//! The requests the daemon serves on workspaces for `gitfold branch
//! --with-changes` while mounted (see ARCHITECTURE.md, "Forks: commands on a
//! workspace's stored state"), and the busy marks and request lock they take.

use std::sync::Arc;
use std::time::Duration;

use log::warn;

use super::{Busy, Mark, Workspaces, busy_error};
use crate::fs::workspace::Workspace;
use crate::state::fork;

/// How long unmounting waits for long requests in progress.
const CLOSE_WAIT: Duration = Duration::from_secs(60);

impl Workspaces {
    /// Marks `branch` busy (see `busy`) until the returned guard drops, and
    /// returns its workspace if open. `creating` is held only for this, so
    /// other workspaces open as usual while the request works. A branch busy
    /// already is refused.
    fn mark_busy(
        &self,
        branch: &str,
        mark: Mark,
    ) -> anyhow::Result<(Option<Arc<Workspace>>, Busy<'_>)> {
        let _creating = self.creating.lock();
        if !self.try_mark(branch, mark) {
            return Err(busy_error(branch));
        }
        let busy = Busy {
            set: &self.busy,
            branch: branch.to_string(),
            mark,
        };
        Ok((self.get_open(branch), busy))
    }

    /// Marks `branch` busy as `mark` says, if that agrees with its marks: any
    /// number of readers, or one exclusive mark.
    pub(super) fn try_mark(&self, branch: &str, mark: Mark) -> bool {
        let mut busy = self.busy.lock();
        let marks = busy.entry(branch.to_string()).or_default();
        match mark {
            Mark::Reader if !marks.exclusive => marks.readers += 1,
            Mark::Exclusive if !marks.exclusive && marks.readers == 0 => marks.exclusive = true,
            _ => return false,
        }
        true
    }

    /// Stops taking long requests and waits for those in progress (unmount),
    /// up to `CLOSE_WAIT`: one stuck in a fetch mustn't keep the daemon alive.
    pub fn close_requests(&self) {
        match self.requests.try_write_for(CLOSE_WAIT) {
            Some(mut closed) => *closed = true,
            None => warn!(
                "a copy of a workspace still running after {} s: exiting anyway",
                CLOSE_WAIT.as_secs()
            ),
        }
    }

    /// Held while a long request works (see `requests`); refused once closed.
    fn request(&self) -> anyhow::Result<parking_lot::RwLockReadGuard<'_, bool>> {
        let open = self.requests.read();
        anyhow::ensure!(!*open, "the mount is being unmounted");
        Ok(open)
    }

    /// `gitfold branch --with-changes` while mounted: creates branch `to` at
    /// `from`'s tip with a copy of `from`'s uncommitted changes (see
    /// `workspace::fork`), copying an open `from` under its lock. Both branches
    /// are busy meanwhile.
    pub fn fork(&self, from: &str, to: &str) -> anyhow::Result<String> {
        anyhow::ensure!(from != to, "a branch can't be created from itself");
        let _request = self.request()?;
        // The target first: once a reader of `from`, it takes `creating` no more
        // (an open of `from` waiting for it may hold that).
        let (_, _to) = self.mark_busy(to, Mark::Exclusive)?;
        let (ws, _from) = self.mark_busy(from, Mark::Reader)?;
        let (common, mount) = (&self.common_dir, Some(self.mount_point.as_path()));
        match ws {
            Some(ws) => fork::fork(common, from, to, mount, |dst| ws.copy_state(dst)),
            None => fork::fork(common, from, to, mount, |dst| {
                fork::copy_stored(common, from, dst)
            }),
        }
    }
}
