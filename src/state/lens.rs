//! A workspace's layers: other workspaces' work shown in it, read-only, over
//! its base and under its own changes (a lens). Each layer is another
//! workspace's work as of one seal (`seal`), as the changes it makes since
//! where the two diverged (the merge base, pinned with the layer, so the view
//! doesn't change when this workspace's base moves).
//!
//! What the workspace shows is then the view tree: its base's tree with each
//! shown layer's changes on top, the higher layer winning a path (whole files;
//! deletions too), and upper over all of it as before. A path a shown layer
//! changed is mixed: it is that layer's, so it can't be written here; the
//! workspace's own file at that path, if it has one, wins and stays writable.
//!
//! Stored in `lens` in the state directory, rewritten whole: `version 1`, then
//! one line per layer, bottom first: `layer <branch> TAB <on|off> TAB <seal>
//! TAB <merge base>` (branch names hold no control characters).

use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;

use super::StateDir;
use crate::fsutil::{not_found, write_atomic};
use crate::git;

/// The file holding a workspace's layers.
pub const LENS: &str = "lens";

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub branch: String,
    /// Shown (the eye icon).
    pub on: bool,
    pub seal: ObjectId,
    /// Where the two workspaces' histories diverged: the layer's changes are
    /// its seal's against this.
    pub merge_base: ObjectId,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lens {
    /// Bottom first.
    pub layers: Vec<Layer>,
}

impl Lens {
    /// The layers stored for the workspace in `state` (none if it has no file).
    pub fn read(state: &StateDir) -> Result<Lens> {
        let path = state.dir.join(LENS);
        Lens::parse(&stored_bytes(state)?).with_context(|| format!("reading {}", path.display()))
    }

    /// A layers file's content (empty: no layers).
    pub fn parse(data: &[u8]) -> Result<Lens> {
        if data.is_empty() {
            return Ok(Lens::default());
        }
        let text = std::str::from_utf8(data)?;
        let mut lines = text.lines();
        if lines.next() != Some("version 1") {
            bail!("not a layers file this gitfold knows");
        }
        let mut layers = Vec::new();
        for line in lines {
            let fields: Vec<&str> = line
                .strip_prefix("layer ")
                .unwrap_or("")
                .split('\t')
                .collect();
            let [branch, on, seal, merge_base] = fields[..] else {
                bail!("bad line {line:?}");
            };
            layers.push(Layer {
                branch: branch.to_string(),
                on: on == "on",
                seal: ObjectId::from_hex(seal.as_bytes())?,
                merge_base: ObjectId::from_hex(merge_base.as_bytes())?,
            });
        }
        Ok(Lens { layers })
    }

    /// Stores them (no file when there are none).
    pub fn write(&self, state: &StateDir) -> Result<()> {
        let path = state.dir.join(LENS);
        if self.layers.is_empty() {
            return match fs::remove_file(&path) {
                Err(e) if !not_found(&e) => Err(e.into()),
                _ => Ok(()),
            };
        }
        let mut text = String::from("version 1\n");
        for l in &self.layers {
            let on = if l.on { "on" } else { "off" };
            text += &format!("layer {}\t{on}\t{}\t{}\n", l.branch, l.seal, l.merge_base);
        }
        Ok(write_atomic(&path, text.as_bytes())?)
    }

    /// Whether any layer is shown.
    pub fn is_on(&self) -> bool {
        self.layers.iter().any(|l| l.on)
    }

    pub fn find(&self, branch: &str) -> Option<usize> {
        self.layers.iter().position(|l| l.branch == branch)
    }
}

/// Whether any workspace of the repository at `common_dir` has `branch`'s
/// work as a layer.
pub fn shown_anywhere(common_dir: &std::path::Path, branch: &str) -> Result<bool> {
    let needle = format!("\nlayer {branch}\t");
    let Ok(dirs) = fs::read_dir(super::state_root(common_dir)) else {
        return Ok(false);
    };
    Ok(dirs
        .flatten()
        .any(|d| fs::read_to_string(d.path().join(LENS)).is_ok_and(|text| text.contains(&needle))))
}

/// Every seal a workspace's layers name, stored or applied (hex ids): their
/// refs are kept (`seal`).
pub fn named_seals(common_dir: &std::path::Path) -> std::collections::HashSet<String> {
    let mut named = std::collections::HashSet::new();
    let Ok(dirs) = fs::read_dir(super::state_root(common_dir)) else {
        return named;
    };
    for dir in dirs.flatten() {
        for file in [LENS, APPLIED] {
            let Ok(data) = fs::read(dir.path().join(file)) else {
                continue;
            };
            let text = String::from_utf8_lossy(&data);
            for line in text.lines().filter_map(|l| l.strip_prefix("layer ")) {
                if let Some(seal) = line.split('\t').nth(2) {
                    named.insert(seal.to_string());
                }
            }
        }
    }
    named
}

/// What a workspace with layers shows below its own changes.
pub struct View {
    /// M: the base's tree with the shown layers' changes.
    pub tree: ObjectId,
    /// H: the base's tree with only the blended files blended (see `blend`).
    pub hunk_tree: ObjectId,
    /// The paths the layers' whole files replace or delete, with the layer
    /// (its index in `Lens::layers`) whose version shows: read-only.
    pub mixed: HashMap<Vec<u8>, usize>,
    /// The blended files (see `blend`), with the layers blended in: the
    /// workspace writes them, its own lines on top of theirs.
    pub blended: HashMap<Vec<u8>, Vec<usize>>,
    /// The seals of the layers blended in (L's parents after the base).
    pub hunk_seals: Vec<ObjectId>,
    /// What the agent should hear about it.
    pub notes: Vec<String>,
}

/// One changed path between two trees, as `git diff-tree -r` reports it.
pub struct Change {
    pub path: Vec<u8>,
    /// The new mode and blob; `None` for a deletion.
    pub new: Option<(EntryKind, ObjectId)>,
}

/// The changes from tree-ish `from` to `to` (files and symlinks, recursively;
/// no rename detection), deletions first.
pub fn changes(
    common_dir: &std::path::Path,
    from: &ObjectId,
    to: &ObjectId,
) -> Result<Vec<Change>> {
    let out = git::cmd::output(
        common_dir,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--no-commit-id",
            &from.to_string(),
            &to.to_string(),
        ],
    )?;
    let mut fields = out.split(|&b| b == 0);
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        if meta.is_empty() {
            continue;
        }
        let path = fields.next().context("diff-tree: no path")?.to_vec();
        // `:<old mode> <new mode> <old oid> <new oid> <status>`
        let meta = String::from_utf8_lossy(meta.strip_prefix(b":").unwrap_or(meta)).into_owned();
        let parts: Vec<&str> = meta.split(' ').collect();
        let [_, new_mode, _, new_oid, status] = parts[..] else {
            bail!("diff-tree: unexpected {meta:?}");
        };
        let new = match status {
            "D" => None,
            _ => {
                let kind = match new_mode {
                    "100755" => EntryKind::BlobExecutable,
                    "120000" => EntryKind::Link,
                    "160000" => EntryKind::Commit,
                    _ => EntryKind::Blob,
                };
                Some((kind, ObjectId::from_hex(new_oid.as_bytes())?))
            }
        };
        changes.push(Change { path, new });
    }
    // A file can replace a deleted directory, and the other way round.
    changes.sort_by_key(|c| c.new.is_some());
    Ok(changes)
}

/// The view of `lens` over base commit `base`, or `None` if no layer is
/// shown.
///
/// A file is blended, rather than shown whole, if the attribute
/// `gitfold-blend` is `hunk` for it (in the base's `.gitattributes` or
/// `info/attributes`), the base has it and no shown layer deletes it: every
/// layer's change to it is merged into the base's version, lines both sides
/// add kept both (registries, manifests: files several agents append to).
pub fn view(common_dir: &std::path::Path, base: &ObjectId, lens: &Lens) -> Result<Option<View>> {
    if !lens.is_on() {
        return Ok(None);
    }
    let shown: Vec<usize> = (0..lens.layers.len())
        .filter(|&i| lens.layers[i].on)
        .collect();
    let stack: Vec<git::lens::Layer> = (shown.iter())
        .map(|&i| git::lens::Layer {
            seal: lens.layers[i].seal,
            merge_base: lens.layers[i].merge_base,
        })
        .collect();
    let base_tree = git::resolve(&base.to_string())?.tree;
    let hunk = hunk_paths(common_dir, base, &base_tree, &stack)?;
    let view = git::with_repo(|repo| {
        git::lens::build_view(repo, *base, &stack, &|p| hunk.contains(p.as_bytes()))
    })?;
    let layer = |ix: usize| shown[ix];
    let name = |ix: usize| lens.layers[shown[ix]].branch.clone();
    let notes = (view.notes.iter())
        .map(|n| match n {
            git::lens::Note::NotBlended { layer, path } => format!(
                "{}: {}'s change conflicts with what is below it, so it isn't blended in",
                path,
                name(*layer)
            ),
            git::lens::Note::HidesBase { layer, path } => format!(
                "{}: {}'s version hides a change on your base since it diverged",
                path,
                name(*layer)
            ),
        })
        .collect();
    Ok(Some(View {
        tree: view.view_tree,
        hunk_tree: view.hunk_tree,
        mixed: (view.mixed.iter())
            .map(|(p, &ix)| (p.to_vec(), layer(ix)))
            .collect(),
        blended: (view.blended.iter())
            .map(|(p, ixs)| (p.to_vec(), ixs.iter().map(|&ix| layer(ix)).collect()))
            .collect(),
        hunk_seals: (view.hunk_layers().into_iter())
            .map(|ix| lens.layers[shown[ix]].seal)
            .collect(),
        notes,
    }))
}

/// The paths the layers in `stack` change that are blended: `gitfold-blend`
/// is `hunk` for them in the base's attributes, the base has them, and no
/// layer deletes them.
fn hunk_paths(
    common_dir: &std::path::Path,
    base: &ObjectId,
    base_tree: &ObjectId,
    stack: &[git::lens::Layer],
) -> Result<std::collections::HashSet<Vec<u8>>> {
    let mut candidates = std::collections::BTreeSet::new();
    let mut deleted = std::collections::HashSet::new();
    git::with_repo(|repo| -> Result<()> {
        let tree_of =
            |c: ObjectId| -> Result<ObjectId> { Ok(repo.find_commit(c)?.tree_id()?.detach()) };
        for layer in stack {
            let (mb, seal) = (tree_of(layer.merge_base)?, tree_of(layer.seal)?);
            for (path, change) in git::lens::delta(repo, mb, seal)? {
                match change {
                    git::lens::Change::Delete => {
                        deleted.insert(path.to_vec());
                    }
                    git::lens::Change::Set(_) => {
                        if git::lens::lookup(repo, *base_tree, path.as_ref())?.is_some() {
                            candidates.insert(path.to_vec());
                        }
                    }
                }
            }
        }
        Ok(())
    })?;
    candidates.retain(|p| !deleted.contains(p));
    if candidates.is_empty() {
        return Ok(Default::default());
    }
    let mut child = git::cmd::command(common_dir)
        .args([
            "check-attr",
            "-z",
            "--stdin",
            &format!("--source={base}"),
            "gitfold-blend",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut input = Vec::new();
    for p in &candidates {
        input.extend_from_slice(p);
        input.push(0);
    }
    let mut stdin = child.stdin.take().context("stdin")?;
    let writer = std::thread::spawn(move || std::io::Write::write_all(&mut stdin, &input));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    anyhow::ensure!(out.status.success(), "git check-attr failed");
    // path NUL attribute NUL value NUL ...
    let fields: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
    Ok((fields.chunks(3))
        .filter(|f| f.len() == 3 && f[2] == b"hunk")
        .map(|f| f[0].to_vec())
        .collect())
}

/// The file recording what the daemon last applied of a workspace's layers
/// (`Applied`).
pub const APPLIED: &str = "lens-applied";

/// What was last applied of a workspace's layers, as opposed to what is
/// stored (`Lens`): the layers file then, and what git was told. The view
/// and git always describe the same layers: a workspace opens with these,
/// and switches to the stored layers when they differ (a change made while
/// it was closed, or a switch that failed half-way). Stored in `lens-applied`:
/// `version 1`, `head <oid or ->`, a line `lens <bytes>` giving the length of
/// the layers file that follows, then NUL-terminated records: `S<path>` a
/// path git was told to skip (skip-worktree), `B<path>` a blended path whose
/// index entry git was given the blend for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Applied {
    /// The layers file's content when applied (empty: none).
    pub lens: Vec<u8>,
    /// The lens commit HEAD was detached at.
    pub head: Option<ObjectId>,
    pub skipped: std::collections::BTreeSet<Vec<u8>>,
    pub blended: std::collections::BTreeSet<Vec<u8>>,
}

impl Applied {
    pub fn read(state: &StateDir) -> Result<Applied> {
        let path = state.dir.join(APPLIED);
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if not_found(&e) => return Ok(Applied::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let bad = || anyhow::anyhow!("{}: not a file this gitfold knows", path.display());
        let mut rest = data.strip_prefix(b"version 1\nhead ").ok_or_else(bad)?;
        let nl = rest.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
        let head = match &rest[..nl] {
            b"-" => None,
            hex => Some(ObjectId::from_hex(hex)?),
        };
        rest = rest[nl + 1..].strip_prefix(b"lens ").ok_or_else(bad)?;
        let nl = rest.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
        let len: usize = std::str::from_utf8(&rest[..nl])?.parse()?;
        rest = &rest[nl + 1..];
        anyhow::ensure!(rest.len() >= len, bad());
        let mut applied = Applied {
            lens: rest[..len].to_vec(),
            head,
            ..Default::default()
        };
        for record in rest[len..].split(|&b| b == 0).filter(|r| !r.is_empty()) {
            match record.split_first() {
                Some((b'S', path)) => applied.skipped.insert(path.to_vec()),
                Some((b'B', path)) => applied.blended.insert(path.to_vec()),
                _ => return Err(bad()),
            };
        }
        Ok(applied)
    }

    pub fn write(&self, state: &StateDir) -> Result<()> {
        let head = self.head.map_or("-".to_string(), |h| h.to_string());
        let mut data = format!("version 1\nhead {head}\nlens {}\n", self.lens.len()).into_bytes();
        data.extend_from_slice(&self.lens);
        for (tag, set) in [(b'S', &self.skipped), (b'B', &self.blended)] {
            for path in set {
                data.push(tag);
                data.extend_from_slice(path);
                data.push(0);
            }
        }
        Ok(write_atomic(&state.dir.join(APPLIED), &data)?)
    }

    /// The layers as applied.
    pub fn layers(&self) -> Result<Lens> {
        Lens::parse(&self.lens)
    }
}

/// The content of the layers file of the workspace in `state` (empty if none).
pub fn stored_bytes(state: &StateDir) -> Result<Vec<u8>> {
    match fs::read(state.dir.join(LENS)) {
        Ok(data) => Ok(data),
        Err(e) if not_found(&e) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Puts back the layers file content `before` (empty: none), after the daemon
/// refused to apply a change.
pub fn restore(state: &StateDir, before: &[u8]) -> Result<()> {
    let path = state.dir.join(LENS);
    if before.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if !not_found(&e) => Err(e.into()),
            _ => Ok(()),
        };
    }
    Ok(write_atomic(&path, before)?)
}
