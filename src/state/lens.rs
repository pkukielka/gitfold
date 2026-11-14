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
use gix::bstr::BStr;
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
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if not_found(&e) => return Ok(Lens::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut lines = text.lines();
        if lines.next() != Some("version 1") {
            bail!("{}: not a layers file this gitfold knows", path.display());
        }
        let mut layers = Vec::new();
        for line in lines {
            let fields: Vec<&str> = line
                .strip_prefix("layer ")
                .unwrap_or("")
                .split('\t')
                .collect();
            let [branch, on, seal, merge_base] = fields[..] else {
                bail!("{}: bad line {line:?}", path.display());
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

/// What a workspace with layers shows below its own changes.
pub struct View {
    pub tree: ObjectId,
    /// The paths shown layers changed (deleted ones too), with the layer (its
    /// index in `Lens::layers`) whose version shows.
    pub mixed: HashMap<Vec<u8>, usize>,
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

/// The view of `lens` over base tree `base_tree`, or `None` if no layer is
/// shown.
pub fn view(
    common_dir: &std::path::Path,
    base_tree: &ObjectId,
    lens: &Lens,
) -> Result<Option<View>> {
    if !lens.is_on() {
        return Ok(None);
    }
    let mut mixed = HashMap::new();
    let mut edits: Vec<Change> = Vec::new();
    for (i, layer) in lens.layers.iter().enumerate().filter(|(_, l)| l.on) {
        for c in changes(common_dir, &layer.merge_base, &layer.seal)
            .with_context(|| format!("layer {}", layer.branch))?
        {
            mixed.insert(c.path.clone(), i);
            edits.push(c);
        }
    }
    // Higher layers last, so they win; deletions of each layer before its
    // additions (`changes`). A path deleted by a lower layer and added by a
    // higher one ends added.
    let tree = git::with_repo(|repo| -> Result<ObjectId> {
        let mut editor = repo.edit_tree(*base_tree)?;
        for c in &edits {
            match c.new {
                None => {
                    editor.remove(BStr::new(&c.path))?;
                }
                Some((kind, oid)) => {
                    editor.upsert(BStr::new(&c.path), kind, oid)?;
                }
            }
        }
        Ok(editor.write()?.detach())
    })?;
    Ok(Some(View { tree, mixed }))
}
