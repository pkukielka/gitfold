//! Removing a workspace or snapshot while mounted (requests from `gitfold
//! remove` through the control socket, see `crate::daemon::control`).
//!
//! Names otherwise never disappear while mounted (their entries are cached for
//! a year), so a removal tells the kernel to drop the entry, and refuses while
//! a process uses the name: its working directory, an open or mapped file.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;

use super::NameChanges;
use super::snapshots::Snapshots;
use super::workspace::Workspace;
use super::workspaces::Workspaces;
use crate::fs::workspace;
use crate::git;
use crate::refusal::Refusal;
use crate::state::retire;
use crate::store::Store;
use crate::{mount_table, prewarm, procs};

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
        self.refuse_if_unmounting(*closed)?;
        let path = self.mount_point.join(name);
        // Taken out, it must not be in use: its prewarm walk is stopped then
        // (the checks before ignore it), and restarted if it is refused.
        let walk_stopped = Cell::new(false);
        let refuse_if_used = || {
            walk_stopped.set(prewarm::stop_under(&path));
            self.refuse_if_used(&path, name)
        };
        let (parent, leaf) = self.workspaces.parent_and_leaf(name);
        let removed = match name.strip_prefix('@') {
            Some(rev) => {
                // While it is open, refusals come before it leaves the root.
                if self.snapshots.commit_of(name.as_bytes()).is_some() {
                    self.refuse_if_used(&path, name)?;
                }
                let resolve = || git::resolve(rev).ok().map(|c| c.id);
                self.snapshots
                    .remove(
                        name.as_bytes(),
                        resolve,
                        refuse_if_used,
                        |commit, shared| {
                            retire::remove_snapshot_registration(
                                &self.common_dir,
                                rev,
                                commit,
                                shared,
                                false,
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
        // Refused while open (not taken out by another removal meanwhile).
        if removed.is_err() && walk_stopped.get() {
            match name.starts_with('@') {
                true => self.snapshots.prewarm(name.as_bytes()),
                false => self.workspaces.prewarm(name),
            }
        }
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
        // Open: the daemon keeps its base at the tip; closed: it catches up.
        let check = |ws: Option<&Workspace>| {
            let sync = |s: &Store, _: &Path, _: &str| ws.map_or(Ok(()), |ws| ws.sync(s));
            let catch_up: &retire::CatchUp = match ws {
                Some(_) => &sync,
                None => &workspace::catch_up,
            };
            retire::check_workspace(store, common_dir, branch, force, catch_up)
        };
        if let Some(open) = self.workspaces.get_open(branch) {
            // Changes first, as `check` reports them.
            check(Some(&open))?;
            self.refuse_if_used(&self.mount_point.join(branch), branch)?;
            // `open` dropped here: the wait for requests in flight counts references.
        }
        self.workspaces.remove(branch, refuse_if_used, |ws| {
            let ignored = check(ws)?;
            retire::delete_workspace(common_dir, branch, ignored)
        })
    }

    /// What `remove` would refuse, without changing anything (`remove --check`).
    pub fn check(&self, name: &str, force: bool) -> Result<String> {
        let closed = self.closed.lock();
        self.refuse_if_unmounting(*closed)?;
        let path = self.mount_point.join(name);
        if let Some(rev) = name.strip_prefix('@') {
            let open = self.snapshots.commit_of(name.as_bytes());
            if open.is_some() {
                self.refuse_if_used(&path, name)?;
            }
            let commit = open.or_else(|| git::resolve(rev).ok().map(|c| c.id));
            let commit =
                commit.ok_or_else(|| Refusal::NotFound.error(format!("no snapshot {name}")))?;
            let shared = self.snapshots.open_names_of(&commit) > usize::from(open.is_some());
            return retire::remove_snapshot_registration(
                &self.common_dir,
                rev,
                &commit,
                shared,
                true,
            );
        }
        let (store, common_dir) = (&*self.store, &self.common_dir);
        // Not open: checked while it can't be opened (catching up may write).
        let closed = self.workspaces.check_closed(name, || {
            retire::check_workspace(store, common_dir, name, force, &workspace::catch_up)
        })?;
        let ignored = match closed {
            Some(ignored) => ignored,
            None => {
                let open = (self.workspaces.get_open(name)).ok_or_else(|| {
                    Refusal::NotFound
                        .error(format!("{name} is a namespace directory, not a workspace"))
                })?;
                // Changes first, both reported: a shell sitting in it must not
                // hide uncommitted work from the check.
                let changes =
                    retire::check_workspace(store, common_dir, name, force, &|s, _, _| {
                        open.sync(s)
                    });
                match (changes, self.refuse_if_used(&path, name)) {
                    (Ok(ignored), Ok(())) => ignored,
                    (Err(changes), Err(used))
                        if Refusal::of(&changes) == Some(Refusal::Changes) =>
                    {
                        return Err(Refusal::Changes.error(format!("{changes:#}\n{used:#}")));
                    }
                    (Err(e), _) | (Ok(_), Err(e)) => return Err(e),
                }
            }
        };
        Ok(retire::removable_workspace(name, ignored))
    }

    /// Refuses once the mount is being unmounted (`closed`), or detached
    /// (unmounted while busy): who uses what can't be told any more.
    fn refuse_if_unmounting(&self, closed: bool) -> Result<()> {
        if closed || mount_table::gitfold_source(&self.mount_point)?.is_none() {
            let shown = self.mount_point.display();
            return Err(Refusal::InUse.error(format!("{shown} is being unmounted")));
        }
        Ok(())
    }

    /// Fails, naming them, if processes use something at or below `path`
    /// (workspace or snapshot `name`). This daemon's own children don't
    /// count: its prewarm walkers (stopped by a removal), also one just started
    /// and not listed yet; nothing else it starts uses the mount.
    fn refuse_if_used(&self, path: &Path, name: &str) -> Result<()> {
        let daemon_pid = std::process::id();
        let mut users = procs::users(&self.mount_point, path);
        users.retain(|u| procs::parent(u.pid) != Some(daemon_pid));
        if users.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = users.iter().map(|u| format!("\n  {u}")).collect();
        Err(Refusal::InUse.error(format!(
            "{name} is in use; close or `cd` out of it first:{}",
            list.concat()
        )))
    }
}
