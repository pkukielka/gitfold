//! A workspace's uncommitted work as a commit: a seal. Its tree is what the
//! workspace shows (its base's tree with the changed files and without the
//! deleted ones, ignored files left out, as `gitfold changes` lists them), its
//! parent the base. A seal can be opened as a snapshot (`@<sha>`), merged with
//! other workspaces' seals (`gitfold snapshot`), and is what a layer shows of
//! another workspace (`layers`).
//!
//! Seals are made with a fixed identity and date, so the same work on the same
//! base gives the same commit: sealing an unchanged workspace again makes
//! nothing new. Each new seal is kept by a ref, `refs/gitfold-seal/<id>/<seq>`,
//! numbered from 1 (git would prune it otherwise); the oldest beyond
//! `KEPT` are deleted.
//!
//! A workspace's own Claude Code `Stop` hook, at the end of its agent's
//! turn, records in its state directory (`SEALED`) that it has an agent,
//! with a seal of its work then if another workspace shows it as a layer
//! (`By::StopHook`). The record goes with the workspace. A live layer of a
//! workspace with an agent follows that seal (`driven`): it shows its work
//! as it was at the end of the agent's last turn, never half-way through
//! one. A workspace without (a person's) has no turns, so its layers take a
//! fresh seal when they move (`state::follow`). Its `UserPromptSubmit` hook
//! records that it has an agent too (`TURNS`), from its first turn on: the
//! daemon leaves such a workspace's own live layers to its turns
//! (`has_agent`). The refs outlive the
//! workspace (`gitfold remove` keeps them while layers name them, and the
//! id comes from the branch name), so the newest ref may hold an earlier
//! workspace's work: the record never does.
//!
//! Numbering, the ref and the record are written under a lock on the
//! workspace's `SEAL_LOCK`, so two seals at once (its `Stop` hook and
//! another workspace's turn start) don't take the same number.

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

/// The file in a workspace's state directory recording its own `Stop`
/// hook's last run: `<sha> stop`, the seal it made, or `- stop`, none (no
/// layer showed it). (Earlier builds wrote a bare `<sha>` for any seal: not
/// one of its hook's.)
const SEALED: &str = "sealed";

/// The file in a workspace's state directory whose presence records that
/// its own `UserPromptSubmit` hook ran (`turn_started`).
const TURNS: &str = "turns";

/// The file locked while a seal is numbered and recorded.
const SEAL_LOCK: &str = "sealed.lock";

/// Who makes a seal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The workspace's own Claude Code `Stop` hook, at the end of its
    /// agent's turn: recorded (see the module doc).
    StopHook,
    /// Its owner, at a point of its choosing (a proposal accepted into it):
    /// recorded as the `Stop` hook's if it has an agent, so its layers show
    /// what it accepted from their next turn.
    Owner,
    /// A command (`snapshot`, `layer`, a proposal): not recorded.
    Other,
}

/// The identity and date seals (and snapshots' merges) are made with: the
/// same trees and parents give the same commit.
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

/// The parent of a snapshot of the workspace in `state` on `base`: the
/// branch's tip when the workspace committed and its base hasn't caught up
/// yet (the tip has the base, no move onto another base is under way: the
/// tip's tree is then part of what the workspace shows), else `base`. A
/// snapshot on the stale base would count the just-committed lines as the
/// workspace's own new work, which layers then merge as such.
fn parent(state: &StateDir, base: ObjectId) -> ObjectId {
    let Ok(tip) = git::branch_tip(&state.branch) else {
        return base;
    };
    if tip.id == base || super::restack::pending(state) {
        return base;
    }
    match git::merge_base_if_any(base, tip.id) {
        Ok(Some(mb)) if mb == base => tip.id,
        _ => base,
    }
}

/// Seals the work of the workspace stored in `state` (see the module doc),
/// made `by` its `Stop` hook, its owner or another command. Called by the
/// commands themselves (`snapshot`, `layer`, the `Stop` hook), mounted or
/// not, without the daemon: it reads the stored state (upper, markers,
/// base) as it is, so a write in progress meanwhile may be half-seen. That
/// is why the hooks seal at turn ends, when the workspace's agent isn't
/// writing.
pub fn seal(store: &Store, state: &StateDir, by: By) -> Result<Seal> {
    let s = status::status(store, state)?;
    let base = state.read_base()?;
    let blends = blends(state, &base)?;
    let tree = tree_of(state, &base.tree, &s.changed, &s.deleted, &blends)
        .with_context(|| format!("taking a snapshot of {}", state.branch))?;
    let commit = commit(
        &state.common_dir,
        &tree,
        &parent(state, base.id),
        &state.branch,
    )?;
    let _lock = crate::lock::wait(&state.dir.join(SEAL_LOCK))?;
    let seals = list(&state.common_dir, &state.id)?;
    let recorded = match by {
        By::StopHook => true,
        By::Owner => matches!(driven(state), Driven::Agent(_)),
        By::Other => false,
    };
    let note = || match recorded {
        true => record(state, Some(commit)),
        false => Ok(()),
    };
    if let Some(&(_, newest)) = seals.last()
        && newest == commit
    {
        note()?;
        return Ok(Seal { commit, same: true });
    }
    let seq = seals.last().map_or(1, |&(seq, _)| seq + 1);
    let name = format!("{}{seq}", ref_prefix(&state.id));
    git::cmd::run(
        &state.common_dir,
        &["update-ref", &name, &commit.to_string(), ""],
    )?;
    note()?;
    // The oldest go, unless a workspace's layers name them, or a live layer
    // of this one would follow it.
    let mut named = super::lens::named_seals(&state.common_dir);
    if let Driven::Agent(Some(followed)) = driven(state) {
        named.insert(followed);
    }
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

/// Who drives a workspace, as its live layers follow it (see the module
/// doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driven {
    /// No `Stop` hook of its own ran (a person's): no turns to wait for.
    Person,
    /// An agent, whose `Stop` hook ran: the seal it made at the end of its
    /// last turn (this workspace's, never an earlier one of its branch), if
    /// a layer showed it then.
    Agent(Option<ObjectId>),
}

/// Who drives the workspace in `state` (see `Driven`).
pub fn driven(state: &StateDir) -> Driven {
    let Ok(text) = fs::read_to_string(state.dir.join(SEALED)) else {
        return Driven::Person;
    };
    match text.trim().split_once(' ') {
        Some((hex, "stop")) => Driven::Agent(ObjectId::from_hex(hex.as_bytes()).ok()),
        _ => Driven::Person,
    }
}

/// The `Stop` hook of the workspace in `state` ran without sealing it (no
/// layer shows it): it has an agent, and an earlier seal of its hook's is
/// stale now.
pub fn stopped(state: &StateDir) -> Result<()> {
    let _lock = crate::lock::wait(&state.dir.join(SEAL_LOCK))?;
    if driven(state) == Driven::Agent(None) {
        return Ok(());
    }
    record(state, None)
}

/// Records that the workspace in `state` has an agent of its own: its
/// Claude Code `UserPromptSubmit` hook ran (a turn started). Its live layers
/// then move at its turns' starts only, never by the daemon in between
/// (`has_agent`), from its first turn on, before any `Stop` hook ran.
pub fn turn_started(state: &StateDir) -> Result<()> {
    let path = state.dir.join(TURNS);
    if path.exists() {
        return Ok(());
    }
    crate::fsutil::write_atomic(&path, b"")
        .with_context(|| format!("recording a turn of {}", state.branch))
}

/// Whether the workspace in `state` has an agent of its own: one of its
/// turn hooks ran (`turn_started`, or its `Stop` hook: `driven`). The
/// records go with the workspace.
pub fn has_agent(state: &StateDir) -> bool {
    state.dir.join(TURNS).exists() || matches!(driven(state), Driven::Agent(_))
}

/// Records a run of the `Stop` hook of the workspace in `state`, with the
/// seal it made.
fn record(state: &StateDir, seal: Option<ObjectId>) -> Result<()> {
    let seal = seal.map_or("-".to_string(), |s| s.to_string());
    crate::fsutil::write_atomic(&state.dir.join(SEALED), format!("{seal} stop").as_bytes())
        .with_context(|| format!("recording the snapshot of {}", state.branch))
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
                    "{}: {} is a repository; left out",
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
    let message = format!("gitfold snapshot: {branch}\n\nIts uncommitted work, on its base.");
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
