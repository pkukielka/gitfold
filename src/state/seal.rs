//! A workspace's uncommitted work as a commit: a seal. Its tree is what the
//! workspace shows (its base's tree with the changed files and without the
//! deleted ones, ignored files left out, as `gitfold changes` lists them), its
//! parent the base. A seal can be opened as a snapshot (`@<sha>`), mixed with
//! other workspaces' seals (`gitfold mix`), and is what a layer shows of
//! another workspace (`layers`).
//!
//! Seals are made with a fixed identity and date, so the same work on the same
//! base gives the same commit: sealing an unchanged workspace again makes
//! nothing new. Each new seal is kept by a ref, `refs/gitfold-seal/<id>/<seq>`,
//! numbered from 1 (git would prune it otherwise); the oldest beyond
//! `KEPT` are deleted.

use std::collections::HashMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::BStr;
use gix::objs::tree::EntryKind;

use super::{StateDir, status, upper_path};
use crate::git;
use crate::store::Store;

/// How many seals of one workspace are kept (the newest).
const KEPT: usize = 16;

/// The identity and date seals (and mixes) are made with: the same trees and
/// parents give the same commit.
pub const IDENTITY: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "gitfold"),
    ("GIT_AUTHOR_EMAIL", "gitfold@localhost"),
    ("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "gitfold"),
    ("GIT_COMMITTER_EMAIL", "gitfold@localhost"),
    ("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z"),
];

pub struct Seal {
    pub commit: ObjectId,
    /// Not made now: the newest seal already had this work.
    pub same: bool,
}

/// The ref prefix of workspace `id`'s seals.
pub fn ref_prefix(id: &str) -> String {
    format!("refs/gitfold-seal/{id}/")
}

/// Seals the work of the workspace stored in `state` (see the module doc).
/// Called by the commands themselves (`seal`, `mix`, `layer`, the `Stop`
/// hook), mounted or not, without the daemon or a lock: it reads the stored
/// state (upper, markers, base) as it is, so a write in progress meanwhile
/// may be half-seen. That is why the hooks seal at turn ends, when the
/// workspace's agent isn't writing.
pub fn seal(store: &Store, state: &StateDir) -> Result<Seal> {
    let s = status::status(store, state)?;
    let base = state.read_base()?;
    let blends = blends(state, &base)?;
    let tree = tree_of(state, &base.tree, &s.changed, &s.deleted, &blends)
        .with_context(|| format!("sealing {}", state.branch))?;
    let commit = commit(&state.common_dir, &tree, &base.id, &state.branch)?;
    let seals = list(&state.common_dir, &state.id)?;
    if let Some(&(_, newest)) = seals.last()
        && newest == commit
    {
        return Ok(Seal { commit, same: true });
    }
    let seq = seals.last().map_or(1, |&(seq, _)| seq + 1);
    let name = format!("{}{seq}", ref_prefix(&state.id));
    git::cmd::run(
        &state.common_dir,
        &["update-ref", &name, &commit.to_string(), ""],
    )?;
    // The oldest go, unless a workspace's layers name them.
    let named = super::lens::named_seals(&state.common_dir);
    let old = seals.iter().take((seals.len() + 1).saturating_sub(KEPT));
    for (old, _) in old.filter(|(_, c)| !named.contains(c)) {
        let name = format!("{}{old}", ref_prefix(&state.id));
        git::cmd::run(&state.common_dir, &["update-ref", "-d", &name])?;
    }
    Ok(Seal {
        commit,
        same: false,
    })
}

/// Workspace `id`'s seals, oldest first: (number, commit).
pub fn list(common_dir: &std::path::Path, id: &str) -> Result<Vec<(u64, ObjectId)>> {
    let out = git::cmd::output(
        common_dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &ref_prefix(id),
        ],
    )?;
    let prefix = ref_prefix(id);
    let mut seals: Vec<(u64, ObjectId)> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let (name, oid) = l.split_once(' ')?;
            let seq = name.strip_prefix(&prefix)?.parse().ok()?;
            Some((seq, ObjectId::from_hex(oid.as_bytes()).ok()?))
        })
        .collect();
    seals.sort_unstable();
    Ok(seals)
}

/// The tree `base` becomes with upper's `changed` paths and without `deleted`
/// ones: blobs written for the changed files, trees only along changed paths.
fn tree_of(
    state: &StateDir,
    base: &ObjectId,
    changed: &[Vec<u8>],
    deleted: &[Vec<u8>],
    blends: &Blends,
) -> Result<ObjectId> {
    let upper = state.upper();
    git::with_repo(|repo| {
        let mut editor = repo.edit_tree(*base)?;
        // Deletions first: a file can replace a deleted directory.
        for path in deleted {
            editor.remove(BStr::new(path))?;
        }
        for path in changed {
            let file = upper_path(&upper, path);
            let meta = fs::symlink_metadata(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let (kind, blob) = if meta.is_symlink() {
                let target = fs::read_link(&file)?;
                (
                    EntryKind::Link,
                    repo.write_blob(target.as_os_str().as_bytes())?,
                )
            } else if meta.is_dir() {
                // An untracked repository: git adds it as a submodule only
                // when asked, and it has no commit of ours to name.
                log::warn!(
                    "{}: {} is a repository; not sealed",
                    state.branch,
                    String::from_utf8_lossy(path)
                );
                continue;
            } else {
                let kind = match meta.permissions().mode() & 0o111 {
                    0 => EntryKind::Blob,
                    _ => EntryKind::BlobExecutable,
                };
                match blends.get(path) {
                    // A blended file: its own lines only.
                    Some((blend, before)) => {
                        let own =
                            own_lines(&std::env::temp_dir(), &fs::read(&file)?, blend, before)?;
                        (kind, repo.write_blob(own)?)
                    }
                    None => {
                        let f = fs::File::open(&file)
                            .with_context(|| format!("reading {}", file.display()))?;
                        (kind, repo.write_blob_stream(f)?)
                    }
                }
            };
            editor.upsert(BStr::new(path), kind, blob.detach())?;
        }
        Ok(editor.write()?.detach())
    })
}

/// A commit of `tree` with parent `base`, made with `IDENTITY`.
fn commit(
    common_dir: &std::path::Path,
    tree: &ObjectId,
    base: &ObjectId,
    branch: &str,
) -> Result<ObjectId> {
    let message = format!("gitfold seal: {branch}\n\nIts uncommitted work, on its base.");
    let out = git::cmd::command(common_dir)
        .args([
            "commit-tree",
            &tree.to_string(),
            "-p",
            &base.to_string(),
            "-m",
            &message,
        ])
        .envs(IDENTITY)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git commit-tree: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(ObjectId::from_hex(
        String::from_utf8_lossy(&out.stdout).trim().as_bytes(),
    )?)
}

/// Per blended path, the blend and the base's version (`blends`).
type Blends = HashMap<Vec<u8>, (Vec<u8>, Vec<u8>)>;

/// For each file the workspace in `state` shows blended (`state::lens`),
/// the blend and the base's version: a seal of it holds the upper copy with
/// the change from the blend back to the base's version merged in (only its
/// own lines), so a workspace showing it as a layer gets the other layers'
/// work from them, not through it.
fn blends(state: &StateDir, base: &git::Commit) -> Result<Blends> {
    let applied = super::lens::Applied::read(state)?;
    if applied.blended.is_empty() {
        return Ok(HashMap::new());
    }
    let Some(view) = super::lens::view(&state.common_dir, &base.id, &applied.layers()?)? else {
        return Ok(HashMap::new());
    };
    let content = |tree: ObjectId, path: &[u8]| -> Result<Vec<u8>> {
        let item = git::with_repo(|repo| git::lens::lookup(repo, tree, BStr::new(path)))?;
        item.map_or(Ok(Vec::new()), |i| git::read_blob(i.oid))
    };
    let mut out = HashMap::new();
    for path in &applied.blended {
        out.insert(
            path.clone(),
            (content(view.hunk_tree, path)?, content(base.tree, path)?),
        );
    }
    Ok(out)
}

/// `upper`'s own lines: the change from `blend` to `before` merged into it,
/// or if that conflicts (its lines interleave with the layers'), the layers'
/// added lines taken out, once each.
fn own_lines(tmp: &std::path::Path, upper: &[u8], blend: &[u8], before: &[u8]) -> Result<Vec<u8>> {
    if let Some(own) = git::merge_file(tmp, blend, upper, before)? {
        return Ok(own);
    }
    let theirs = git::lens::added_lines(before, blend);
    Ok(git::lens::without_lines(upper, &theirs))
}
