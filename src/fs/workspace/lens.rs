//! Layers in an open workspace: other workspaces' work shown over its base,
//! read-only, under its own changes (see `state::lens` for the model and its
//! storage).
//!
//! The lower layer becomes the view tree; everything that reads the lower
//! layer reads it. A switch (a layer shown, hidden, or moved to a newer seal)
//! changes what the kernel has cached, so it is told: the attributes and pages
//! of every file whose content changed, the entries added, removed or changed
//! in type, and the listings of their directories. Changed files get a fresh
//! time, so builds see them. Paths the workspace's own changes hide don't
//! change.
//!
//! While layers are shown, git is off in the workspace: git would see the
//! layers' files as changes of its own (and commit them). Its `.git` file names
//! a directory that doesn't exist, whose name tells why; every git command
//! fails with it. And the base doesn't follow the branch meanwhile (`rebase`).

use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};

use super::times::Stamp;
use super::{GITFILE_LOCAL, Layers, Workspace};
use crate::git;
use crate::state::lens::{self, Lens};
use crate::state::{StateDir, ancestors};
use crate::store::{Lower, Store};

/// What the kernel must be told after a switch (local ids of the workspace).
#[derive(Default)]
pub struct Stale {
    /// Nodes whose attributes and pages changed (directories: their listing).
    pub inodes: Vec<u64>,
    /// Names added, removed or changed in type, by their directory's node.
    pub entries: Vec<(u64, Vec<u8>)>,
}

/// The `.git` content while layers are shown: git prints the path it names.
fn surrendered(lens: &Lens) -> Vec<u8> {
    let shown: Vec<&str> = (lens.layers.iter())
        .filter(|l| l.on)
        .map(|l| l.branch.as_str())
        .collect();
    format!(
        "gitdir: /LENS ON ({}): other workspaces' work is shown here, so git is off. \
         Run gitfold lens off to use git\n",
        shown.join("+")
    )
    .into_bytes()
}

/// Sets up the view of the layers stored for the workspace in `state`, if any
/// is shown, when it opens; returns the `.git` content then.
pub(super) fn open_view(
    store: &Store,
    state: &StateDir,
    layers: &mut Layers,
) -> Result<Option<Vec<u8>>> {
    let stored = Lens::read(state)?;
    let Some(view) = lens::view(&state.common_dir, &layers.view, &stored)? else {
        return Ok(None);
    };
    layers.view_tree = store.load_tree(view.tree)?;
    layers.view = view.tree;
    layers.mixed = view.mixed;
    Ok(Some(surrendered(&stored)))
}

impl Workspace {
    /// Whether layers are shown.
    pub fn lensed(&self) -> bool {
        self.gitfile.read().starts_with(b"gitdir: /LENS ON")
    }

    /// Shows the layers stored for it (`state::lens`), as they are now:
    /// what the kernel must be told is returned. Git must be idle: the
    /// switch changes what its index describes.
    pub fn switch(&self, store: &Store) -> Result<Stale> {
        let stored = Lens::read(&self.state)?;
        let base = self.layers.read().base;
        let base_tree = git::resolve(&base.to_string())?.tree;
        let view = lens::view(&self.state.common_dir, &base_tree, &stored)?;
        let (new_view, mixed) = match view {
            Some(v) => (v.tree, v.mixed),
            None => (base_tree, Default::default()),
        };
        let new_tree = store.load_tree(new_view)?;
        let mut layers = self.layers.write();
        let old_view = layers.view;
        let changes = lens::changes(&self.state.common_dir, &old_view, &new_view)
            .context("comparing the views")?;
        let old_tree = layers.view_tree;
        layers.view_tree = new_tree;
        layers.view = new_view;
        layers.mixed = mixed;
        let stale = self.stale(store, &mut layers, old_tree, new_tree, &changes)?;
        layers.times.save()?;
        self.changed();
        *self.gitfile.write() = Arc::new(match stored.is_on() {
            true => surrendered(&stored),
            false => format!("gitdir: {}\n", self.state.admin.display()).into_bytes(),
        });
        Ok(stale)
    }

    /// What changed in the view between lower trees `old` and `new` (the
    /// file `changes` between them), as the kernel holds it; stamps the
    /// changed files and forgets nodes of entries gone or changed in type.
    fn stale(
        &self,
        store: &Store,
        layers: &mut Layers,
        old: u32,
        new: u32,
        changes: &[lens::Change],
    ) -> Result<Stale> {
        let stamp = {
            let now = SystemTime::now();
            Stamp {
                mtime: now,
                ctime: now,
            }
        };
        let kind = |tree: u32, path: &[u8]| match store.entry_at(tree, path) {
            None => 0,
            Some(Lower::Tree(_) | Lower::Gitlink) => 1,
            Some(Lower::File(_)) => 2,
        };
        let mut stale = Stale::default();
        let mut entries = HashSet::new();
        let mut nodes = self.nodes.lock();
        for c in changes {
            if self.shadowed(layers, &c.path) {
                continue;
            }
            // The highest path that appeared, vanished or changed type is an
            // entry change in its directory; below it, nothing was cached.
            let changed_at = ancestors(&c.path)
                .chain(std::iter::once(c.path.as_slice()))
                .find(|p| kind(old, p) != kind(new, p));
            match changed_at {
                Some(at) => {
                    if kind(new, at) == 2 {
                        layers.times.set(at, Some(stamp));
                    }
                    if entries.insert(at.to_vec()) {
                        let (dir, name) = split(at);
                        if let Some(parent) = nodes.held(dir) {
                            nodes.orphan(parent, name);
                            stale.entries.push((parent, name.to_vec()));
                            stale.inodes.push(parent);
                        }
                    }
                }
                None => {
                    layers.times.set(&c.path, Some(stamp));
                    if let Some(local) = nodes.held(&c.path) {
                        stale.inodes.push(local);
                    }
                }
            }
        }
        stale.inodes.push(GITFILE_LOCAL);
        stale.inodes.sort_unstable();
        stale.inodes.dedup();
        Ok(stale)
    }

    /// Whether the workspace's own changes hide `path` from the lower layer:
    /// an upper entry there (or a file where an ancestor would be), or a
    /// whiteout or opaque directory over it.
    fn shadowed(&self, layers: &Layers, path: &[u8]) -> bool {
        Self::lower_hidden(layers, path)
            || match fs::symlink_metadata(self.upper_path(path)) {
                Ok(_) => true,
                Err(e) => e.raw_os_error() == Some(libc::ENOTDIR),
            }
    }

    /// Whether any path below directory `path` shows a layer's file.
    pub(super) fn has_mixed_below(layers: &Layers, path: &[u8]) -> bool {
        if layers.mixed.is_empty() {
            return false;
        }
        let prefix = [path, b"/"].concat();
        path.is_empty() || layers.mixed.keys().any(|p| p.starts_with(&prefix))
    }
}

/// (directory, name) of `path`.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (b"", path),
    }
}
