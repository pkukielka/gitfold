//! Removing a workspace or snapshot while mounted (requests from `gitfold
//! remove` through the control socket, see `crate::control`).
//!
//! Names otherwise never disappear while mounted (their entries are cached for
//! a year), so a removal tells the kernel to drop the entry, and refuses while
//! a process uses the name: its working directory, an open or mapped file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail, ensure};
use parking_lot::Mutex;

use super::NameChanges;
use super::snapshots::Snapshots;
use super::workspace::Workspace;
use super::workspaces::Workspaces;
use crate::admin;
use crate::store::Store;
use crate::{git, mountpoint, prewarm};

pub struct Removal {
    pub(super) common_dir: PathBuf,
    pub(super) mount_point: PathBuf,
    pub(super) store: Arc<Store>,
    pub(super) snapshots: Arc<Snapshots>,
    pub(super) workspaces: Arc<Workspaces>,
    pub(super) names: Arc<NameChanges>,
    /// Set once the file system is being unmounted; held during a removal, so
    /// the daemon doesn't exit in the middle of one.
    pub(super) closed: Arc<Mutex<bool>>,
}

impl Removal {
    /// Removes workspace `name`, or snapshot `@<rev>`, as `gitfold remove`
    /// does unmounted; returns what was done.
    pub fn remove(&self, name: &str, force: bool) -> Result<String> {
        let closed = self.closed.lock();
        // Detached (unmounted while busy): who uses it can't be told any more.
        ensure!(
            !*closed && mountpoint::gitfold_source(&self.mount_point)?.is_some(),
            "{} is being unmounted",
            self.mount_point.display()
        );
        let path = self.mount_point.join(name);
        let refuse_if_used = || {
            // Prewarm walkers would count.
            prewarm::stop_under(&path);
            self.refuse_if_used(&path, name)
        };
        let (parent, leaf) = self.workspaces.parent_and_leaf(name);
        let removed = match name.strip_prefix('@') {
            Some(rev) => {
                let resolve = || git::resolve(rev).ok().map(|c| c.id);
                self.snapshots
                    .remove(
                        name.as_bytes(),
                        resolve,
                        refuse_if_used,
                        |commit, shared| {
                            admin::remove_snapshot_registration(
                                &self.common_dir,
                                rev,
                                commit,
                                shared,
                            )
                        },
                    )
                    .map(|(done, open)| match open {
                        true => (format!("closed snapshot {name}; {done}"), true),
                        false => (done, false),
                    })
            }
            None => self.remove_workspace(name, force, refuse_if_used),
        };
        match removed {
            Ok((done, open)) => {
                if open {
                    self.names.removed(parent, leaf.as_bytes());
                }
                Ok(done)
            }
            Err(e) => {
                // A listing taken while it was out of the registry lacks it.
                self.names.changed(&[parent]);
                Err(e)
            }
        }
    }

    /// Removes the workspace of `branch`. If open, the checks run first while
    /// it is live, so a removal they refuse never takes it out (requests into
    /// it get EBUSY meanwhile); taken out, they run again, authoritative.
    /// Returns what was done, and whether it was open.
    fn remove_workspace(
        &self,
        branch: &str,
        force: bool,
        refuse_if_used: impl FnOnce() -> Result<()>,
    ) -> Result<(String, bool)> {
        let (store, common_dir) = (&*self.store, &self.common_dir);
        let check =
            |ws: Option<&Workspace>| admin::check_workspace(store, common_dir, branch, force, ws);
        if let Some(open) = self.workspaces.get_open(branch) {
            let path = self.mount_point.join(branch);
            prewarm::stop_under(&path);
            self.refuse_if_used(&path, branch)?;
            check(Some(&open))?;
            // `open` dropped here: the wait for requests in flight counts references.
        }
        self.workspaces.remove(branch, refuse_if_used, |ws| {
            let ignored = check(ws)?;
            admin::delete_workspace(common_dir, branch, ignored)
        })
    }

    /// Fails, naming them, if processes use something at or below `path`
    /// (workspace or snapshot `name`).
    fn refuse_if_used(&self, path: &Path, name: &str) -> Result<()> {
        let users = mountpoint::users(&self.mount_point, path);
        if users.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = users.iter().map(|u| format!("\n  {u}")).collect();
        bail!(
            "{name} is in use; close or `cd` out of it first:{}",
            list.concat()
        )
    }
}
