//! Busy marks: which branches a request is working on (a removal, a check of
//! a closed workspace, a copy), so they aren't opened, removed or worked on
//! again meanwhile. Readers (a fork's source) share a branch; anything else
//! takes it alone. `creating` is held only while marking, so other
//! workspaces open as usual while a request works.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fuser::Errno;
use parking_lot::Mutex;

use super::Workspaces;
use crate::fs::workspace::Workspace;
use crate::refusal::Refusal;

/// How long opening a workspace waits for requests reading it.
const OPEN_WAIT: Duration = Duration::from_secs(30);

/// How a request marks a branch busy.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    /// Reading it (the source of a copy): any number at once.
    Reader,
    /// Removing, checking or writing it: alone.
    Exclusive,
}

/// A branch's marks in `Workspaces::busy`.
#[derive(Default, PartialEq)]
pub(super) struct Marks {
    /// Requests reading it (the source of a copy).
    pub(super) readers: usize,
    /// Being removed, checked while closed, or copied into.
    pub(super) exclusive: bool,
}

/// A branch marked busy by `Workspaces::mark_busy`, until dropped.
pub(super) struct Busy<'a> {
    set: &'a Mutex<HashMap<String, Marks>>,
    branch: String,
    mark: Mark,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        unmark(self.set, &self.branch, self.mark);
    }
}

/// Undoes a mark of `branch` (see `Workspaces::try_mark`).
pub(super) fn unmark(set: &Mutex<HashMap<String, Marks>>, branch: &str, mark: Mark) {
    let mut busy = set.lock();
    if let Some(marks) = busy.get_mut(branch) {
        match mark {
            Mark::Reader => marks.readers = marks.readers.saturating_sub(1),
            Mark::Exclusive => marks.exclusive = false,
        }
        if *marks == Marks::default() {
            busy.remove(branch);
        }
    }
}

/// The refusal of a branch busy with something else.
pub(super) fn busy_error(branch: &str) -> anyhow::Error {
    let message = format!("{branch} is busy (being removed, checked or copied); retry");
    Refusal::InUse.error(message)
}

impl Workspaces {
    /// Runs `check` for the workspace of `branch` unless something is open
    /// under that name (then `None`), while it can't be opened: lookups of it
    /// answer EBUSY.
    pub fn check_closed<T>(
        &self,
        branch: &str,
        check: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        {
            let _creating = self.creating.lock();
            if self.by_path.lock().contains_key(branch) {
                return Ok(None);
            }
            if !self.try_mark(branch, Mark::Exclusive) {
                return Err(busy_error(branch));
            }
        }
        // Unmarked even if `check` panics.
        let _busy = Busy {
            set: &self.busy,
            branch: branch.to_string(),
            mark: Mark::Exclusive,
        };
        check().map(Some)
    }

    /// Waits (lock-free, until `OPEN_WAIT` after `started`) while requests read
    /// `branch` (a copy of the closed workspace), so an agent's open
    /// doesn't fail because a supervisor read it; EBUSY while it is marked
    /// exclusively (removed, checked, copied into).
    pub(super) fn wait_for_readers(&self, branch: &str, started: Instant) -> Result<(), Errno> {
        loop {
            match self.busy.lock().get(branch) {
                Some(m) if m.exclusive => return Err(Errno::EBUSY),
                Some(m) if m.readers > 0 => {}
                _ => return Ok(()),
            }
            if started.elapsed() > OPEN_WAIT {
                return Err(Errno::EBUSY);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Marks `branch` busy (see `busy`) until the returned guard drops, and
    /// returns its workspace if open. `creating` is held only for this, so
    /// other workspaces open as usual while the request works. A branch busy
    /// already is refused.
    pub(super) fn mark_busy(
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
}
