//! The live trees of the workspaces `@live` views merge (`live`), one per
//! workspace, shared by every view it is in: made again once its workspace
//! has been quiet for a while after a change (saves come in bursts), or at
//! the latest some seconds into a burst.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use gix::ObjectId;
use parking_lot::Mutex;

use super::members::Tip;
use super::merge::Private;
use super::watch::{Context, Stamp, stamp};
use crate::state::StateDir;

/// How long a workspace is quiet before its live tree is made again.
const QUIET: Duration = Duration::from_secs(2);
/// How long into a burst of changes at most.
const AT_MOST: Duration = Duration::from_secs(10);

/// The live trees, by branch: each behind a lock of its own, held while
/// git makes it (the others go on meanwhile).
#[derive(Default)]
pub struct Lives {
    trees: Mutex<HashMap<String, Arc<Mutex<Option<Made>>>>>,
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
    /// The live tree of the workspace of `branch` (whose tip is `tip`), as a
    /// commit in the private repository and its tree; `None` if it has no
    /// workspace. Made again if it changed and has been quiet since, or is
    /// changing for long, or `now` asks for it at once (a view woken).
    pub fn tip(
        &self,
        ctx: &Context,
        branch: &str,
        tip: ObjectId,
        now: bool,
    ) -> Result<Option<Tip>> {
        let opened = (ctx.workspaces.open_quietly(&ctx.store, branch))
            .map_err(|e| anyhow!("opening the workspace of {branch}: {e:?}"))?;
        let Some((ws, opened_now)) = opened else {
            self.forget(&ctx.private, branch)?;
            return Ok(None);
        };
        if opened_now {
            // A new name in the root and namespace listings.
            ctx.names.changed(&ctx.workspaces.names_dirs());
        }
        let state = StateDir::of(ctx.workspaces.common_dir(), branch);
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
        let dir = ctx.workspaces.mount_point().join(branch);
        let live = ctx.private.live(branch, &dir, &state.index())?;
        let tip = (live.commit, live.tree);
        *entry = Some(Made {
            at: signal,
            tip,
            changing: None,
        });
        Ok(Some(tip))
    }

    /// Forgets the live tree of the workspace of `branch`, which is gone
    /// (removed), if it had one: its ref too.
    pub fn forget(&self, private: &Private, branch: &str) -> Result<()> {
        if self.trees.lock().remove(branch).is_some() {
            private.drop_live(branch)?;
        }
        Ok(())
    }
}
