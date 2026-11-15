//! A closed workspace's base moved for a restack of its branch (see
//! `cli::stack`), visibly: unlike a rebase, which keeps what the workspace
//! shows, the restack's changes are meant to show. Done unmounted, under the
//! repository lock, and only for a workspace with no uncommitted changes:
//! upper then holds nothing but ignored files, and the view is the base's
//! tree. The new base's tree shows instead, and its changed files get a
//! fresh time (stamps), so builds see them.

use std::fs;
use std::time::SystemTime;

use anyhow::{Result, bail};
use gix::objs::tree::EntryKind;

use super::Workspace;
use super::times::Stamp;
use crate::git::{self, Commit, layers::Change};
use crate::state::{StateDir, ancestors};
use crate::store::Store;

/// Moves the base of the closed workspace in `state` from `from` (its
/// base) to `to`, after `before` (which moves the index). The paths the
/// restack changes where the workspace has something of its own (an ignored
/// file, a whiteout, a directory hiding the base's) are returned instead,
/// with nothing done.
pub fn restack_closed(
    store: &Store,
    state: StateDir,
    from: &Commit,
    to: &Commit,
    before: impl FnOnce() -> Result<()>,
) -> Result<std::result::Result<(), Vec<Vec<u8>>>> {
    let ws = Workspace::open(store, state, None)?;
    let mut layers = ws.layers.write();
    if layers.base != from.id {
        bail!(
            "its base is {}, not the branch tip {}",
            layers.base,
            from.id
        );
    }
    if layers.layers_on {
        bail!("layers are shown in it");
    }
    let delta = git::with_repo(|repo| git::layers::delta(repo, from.tree, to.tree))?;
    let mut in_the_way = Vec::new();
    for (path, _) in &delta {
        let path: &[u8] = path.as_ref();
        let own = fs::symlink_metadata(ws.upper_path(path)).is_ok()
            || layers.markers.whiteouts.contains(path)
            || ancestors(path).any(|dir| {
                layers.markers.opaque.contains(dir)
                    || fs::symlink_metadata(ws.upper_path(dir)).is_ok_and(|m| !m.is_dir())
            });
        if own {
            in_the_way.push(path.to_vec());
        }
    }
    if !in_the_way.is_empty() {
        return Ok(Err(in_the_way));
    }
    store.load_tree(to.tree)?;
    before()?;
    let now = SystemTime::now();
    let stamp = Stamp {
        mtime: now,
        ctime: now,
    };
    for (path, change) in &delta {
        let file = matches!(change, Change::Set(item) if item.kind != EntryKind::Tree);
        layers.times.set(path.as_ref(), file.then_some(stamp));
    }
    layers.times.save()?;
    ws.state.write_base(&to.id)?;
    layers.base = to.id;
    drop(layers);
    ws.pin_base()?;
    Ok(Ok(()))
}
