//! The live trees of the workspaces `@live` views merge (`live`), one per
//! workspace, shared by every view it is in: made again once its workspace
//! has been quiet for a while after a change (saves come in bursts), or at
//! the latest some seconds into a burst.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use gix::ObjectId;
use log::error;
use parking_lot::Mutex;

use super::members::Tip;
use super::merge::Private;
use super::watch::{Stamp, stamp};
use crate::fs::NameChanges;
use crate::fs::workspaces::Workspaces;
use crate::state::StateDir;
use crate::store::Store;

/// How long a workspace is quiet before its live tree is made again.
const QUIET: Duration = Duration::from_secs(2);
/// How long into a burst of changes at most.
const AT_MOST: Duration = Duration::from_secs(10);
/// How many live trees `tips` makes at once.
const AT_ONCE: usize = 4;

/// The live trees, by branch: each behind a lock of its own, held while
/// git makes it (the others go on meanwhile); and the mount's parts making
/// one needs.
pub struct Lives {
    trees: Mutex<HashMap<String, Arc<Mutex<Option<Made>>>>>,
    store: Arc<Store>,
    names: Arc<NameChanges>,
    workspaces: Arc<Workspaces>,
}

/// A live tree, and what it was made at.
struct Made {
    at: Signal,
    tip: Tip,
    /// A change seen since: what it is at now, when it first and last
    /// changed.
    changing: Option<(Signal, Instant, Instant)>,
}

/// What tells a workspace changed: writes through the mount, its base, its
/// branch's tip, its index (staging, a commit).
type Signal = (u64, ObjectId, ObjectId, Stamp);

impl Lives {
    pub fn new(store: Arc<Store>, names: Arc<NameChanges>, workspaces: Arc<Workspaces>) -> Lives {
        Lives {
            trees: Mutex::default(),
            store,
            names,
            workspaces,
        }
    }

    /// The live tree of the workspace of `branch` (whose tip is `tip`), as a
    /// commit in the private repository and its tree; `None` if it has no
    /// workspace. Made again if it changed and has been quiet since, or is
    /// changing for long, or `now` asks for it at once (a view woken).
    pub fn tip(
        &self,
        private: &Private,
        branch: &str,
        tip: ObjectId,
        now: bool,
    ) -> Result<Option<Tip>> {
        let opened = (self.workspaces.open_quietly(&self.store, branch))
            .map_err(|e| anyhow!("opening the workspace of {branch}: {e:?}"))?;
        let Some((ws, opened_now)) = opened else {
            self.forget(private, branch);
            return Ok(None);
        };
        if opened_now {
            // A new name in the root and namespace listings.
            self.names.changed(&self.workspaces.names_dirs());
        }
        let state = StateDir::of(self.workspaces.common_dir(), branch);
        let signal = (ws.writes(), ws.base().0, tip, stamp(&state.index()));
        let entry = self
            .trees
            .lock()
            .entry(branch.to_string())
            .or_default()
            .clone();
        let mut entry = entry.lock();
        if let Some(made) = &mut *entry {
            if made.at == signal {
                made.changing = None;
                return Ok(Some(made.tip));
            }
            let at = Instant::now();
            let (first, last) = match &made.changing {
                Some((s, first, last)) if *s == signal => (*first, *last),
                // Changing on: the burst goes on, unless it ended long ago
                // (unbuilt: the view rested).
                Some((_, first, last)) if at - *last < AT_MOST => (*first, at),
                _ => (at, at),
            };
            made.changing = Some((signal, first, last));
            if !now && at - last < QUIET && at - first < AT_MOST {
                return Ok(Some(made.tip));
            }
        }
        // The signal was read before: a change meanwhile is seen next time.
        let dir = self.workspaces.mount_point().join(branch);
        let live = private.live(branch, &dir, &state.index())?;
        let tip = (live.commit, live.tree);
        *entry = Some(Made {
            at: signal,
            tip,
            changing: None,
        });
        Ok(Some(tip))
    }

    /// Forgets the live tree of the workspace of `branch`, which is gone
    /// (removed), if it had one: its ref too, once one being made is (a
    /// failure only logged: it is dropped at the next mount anyway).
    pub fn forget(&self, private: &Private, branch: &str) {
        let Some(entry) = self.trees.lock().remove(branch) else {
            return;
        };
        let _made = entry.lock();
        if let Err(e) = private.drop_live(branch) {
            error!("views: dropping the live tree of {branch}: {e:#}");
        }
    }

    /// The live trees of the workspaces of `asked` (branch, tip), in its order
    /// (`tip`): made side by side, at most `AT_ONCE` at a time (each is git
    /// reading a workspace through the mount, mostly waiting on it), this
    /// thread making some too. Threads of their own, not a rayon pool: opening
    /// a workspace loads trees with `par_iter` under `Workspaces::creating`,
    /// and a pool thread waiting there could take up another live tree, which
    /// waits for that very lock.
    pub fn tips(
        &self,
        private: &Private,
        asked: &[(&str, ObjectId)],
        now: bool,
    ) -> Vec<Result<Option<Tip>>> {
        let next = AtomicUsize::new(0);
        let made: Vec<Mutex<Option<Result<Option<Tip>>>>> =
            asked.iter().map(|_| Mutex::default()).collect();
        let work = || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(branch, tip)) = asked.get(i) else {
                    return;
                };
                *made[i].lock() = Some(self.tip(private, branch, tip, now));
            }
        };
        thread::scope(|scope| {
            for _ in 1..asked.len().min(AT_ONCE) {
                // Without a thread (process limits), fewer at once.
                let spawned = thread::Builder::new().name("live trees".into());
                if spawned.spawn_scoped(scope, work).is_err() {
                    break;
                }
            }
            work();
        });
        (made.into_iter())
            .map(|m| m.into_inner().expect("each made"))
            .collect()
    }
}
