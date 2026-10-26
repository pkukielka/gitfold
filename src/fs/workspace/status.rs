//! What a workspace changes relative to its base, read from its state
//! directory without mounting (for `gitfold list` and `gitfold remove`).
//!
//! A changed path is an upper file or symlink whose content or mode differs from
//! the base's file at that path (or that the base doesn't have). A deleted path
//! is a base file or submodule the view no longer shows: hidden by a whiteout or
//! an opaque directory, or below a path upper replaced with a file, and not
//! present in upper as a file. Directories alone are not changes, as for git.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};
use gix::ObjectId;

use super::markers::Markers;
use super::rebase::matches_stored;
use super::{Lower, Workspace, ancestors, join, not_found, read_base};
use crate::store::Store;

pub struct Status {
    pub base: ObjectId,
    pub changed: Vec<Vec<u8>>,
    pub deleted: Vec<Vec<u8>>,
}

/// The changes of the workspace stored in `state_dir`.
pub fn status(store: &Store, state_dir: &Path) -> Result<Status> {
    let base = read_base(state_dir)?;
    let tree = store.load_tree(base.tree)?;
    let (whiteouts, opaque) = Markers::read(&state_dir.join("markers"))?;
    let upper = state_dir.join("upper");
    let mut changed = Vec::new();
    let mut deleted = BTreeSet::new();
    changed_below(store, &upper, b"", Some(tree), &mut changed, &mut deleted)?;
    // Base entries hidden by a marker (one below another marked path is
    // covered by the walk of the outer one).
    let marked = |p: &[u8]| whiteouts.contains(p) || opaque.contains(p);
    for path in whiteouts.iter().chain(&opaque) {
        if !ancestors(path).any(marked)
            && let Some(lower) = Workspace::lower_in(store, tree, path)
        {
            deleted_below(store, &upper, path, lower, &mut deleted)?;
        }
    }
    Ok(Status {
        base: base.id,
        changed,
        deleted: deleted.into_iter().collect(),
    })
}

fn upper_path(upper: &Path, path: &[u8]) -> std::path::PathBuf {
    upper.join(std::ffi::OsStr::from_bytes(path))
}

/// Upper files and symlinks below `dir` (whose base tree is `tree`, if any)
/// that differ from the base's, and base files below upper files that replaced
/// a base directory.
fn changed_below(
    store: &Store,
    upper: &Path,
    dir: &[u8],
    tree: Option<u32>,
    changed: &mut Vec<Vec<u8>>,
    deleted: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    let dir_path = upper_path(upper, dir);
    let entries = match fs::read_dir(&dir_path) {
        Ok(entries) => entries,
        Err(e) if not_found(&e) => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir_path.display())),
    };
    let table = tree.map(|t| store.tree(t));
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let path = join(dir, name.as_bytes());
        let lower = table
            .as_ref()
            .and_then(|t| t.find(name.as_bytes()))
            .map(Lower::of);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            let below = match lower {
                Some(Lower::Tree(t)) => Some(t),
                _ => None,
            };
            changed_below(store, upper, &path, below, changed, deleted)?;
            continue;
        }
        let same = match lower {
            Some(Lower::File(id)) => {
                let rec = store.file(id).context("stored file")?;
                matches_stored(&entry.path(), &meta, &rec)?
            }
            // A file where the base has a directory: its files are gone.
            Some(dir @ (Lower::Tree(_) | Lower::Gitlink)) => {
                deleted_below(store, upper, &path, dir, deleted)?;
                false
            }
            None => false,
        };
        if !same {
            changed.push(path);
        }
    }
    Ok(())
}

/// Base files and submodules at or below `path` (lower entry `lower`) that
/// upper doesn't have as a file (or, for a submodule, as anything).
fn deleted_below(
    store: &Store,
    upper: &Path,
    path: &[u8],
    lower: Lower,
    out: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    let in_upper = fs::symlink_metadata(upper_path(upper, path));
    match lower {
        Lower::Tree(tree) => {
            let table = store.tree(tree);
            for e in table.entries() {
                let child = join(path, table.name(e));
                deleted_below(store, upper, &child, Lower::of(e), out)?;
            }
        }
        Lower::Gitlink => {
            if in_upper.is_err() {
                out.insert(path.to_vec());
            }
        }
        Lower::File(_) => {
            // A directory there (recreated over the deleted file) doesn't
            // replace it.
            if !in_upper.is_ok_and(|m| !m.is_dir()) {
                out.insert(path.to_vec());
            }
        }
    }
    Ok(())
}
