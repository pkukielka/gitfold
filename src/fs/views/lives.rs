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
use crate::fs::NameChanges;
use crate::fs::workspaces::Workspaces;
use crate::inode;
use crate::state::StateDir;
use crate::store::Store;

/// How long a workspace is quiet before its live tree is made again.
const QUIET: Duration = Duration::from_secs(2);
/// How long into a burst of changes at most.
const AT_MOST: Duration = Duration::from_secs(10);

/// The live trees, by branch.
#[derive(Default)]
pub struct Lives {
    trees: Mutex<HashMap<String, Made>>,
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
type Signal = (u64, ObjectId, ObjectId, Option<std::time::SystemTime>, u64);

/// What `Lives` needs from the mount.
pub struct Mount<'a> {
    pub store: &'a Arc<Store>,
    pub workspaces: &'a Workspaces,
    pub names: &'a NameChanges,
    pub private: &'a Private,
}

impl Lives {
    /// The live tree of the workspace of `branch` (whose tip is `tip`), as a
    /// commit in the private repository and its tree; `None` if it has no
    /// workspace. Made again if it changed and has been quiet since, or is
    /// changing for long, or `now` asks for it at once (a view woken).
    pub fn tip(
        &self,
        mount: &Mount,
        branch: &str,
        tip: ObjectId,
        now: bool,
    ) -> Result<Option<Tip>> {
        let opened = (mount.workspaces.open_quietly(mount.store, branch))
            .map_err(|e| anyhow!("opening the workspace of {branch}: {e:?}"))?;
        let Some((ws, opened_now)) = opened else {
            self.trees.lock().remove(branch);
            return Ok(None);
        };
        if opened_now {
            // A new name in the root and namespace listings.
            let mut dirs = vec![inode::ROOT];
            dirs.extend(mount.workspaces.namespace_inodes());
            mount.names.changed(&dirs);
        }
        let state = StateDir::of(mount.workspaces.common_dir(), branch);
        let index = std::fs::metadata(state.index()).ok();
        let signal = (
            ws.writes(),
            ws.base().0,
            tip,
            index.as_ref().and_then(|m| m.modified().ok()),
            index.map_or(0, |m| m.len()),
        );
        let mut trees = self.trees.lock();
        if let Some(made) = trees.get_mut(branch) {
            if made.at == signal {
                made.changing = None;
                return Ok(Some(made.tip));
            }
            let at = Instant::now();
            let (first, last) = match &made.changing {
                Some((s, first, last)) if *s == signal => (*first, *last),
                Some((_, first, _)) => (*first, at),
                None => (at, at),
            };
            made.changing = Some((signal, first, last));
            if !now && at - last < QUIET && at - first < AT_MOST {
                return Ok(Some(made.tip));
            }
        }
        // The signal was read before: a change meanwhile is seen next time.
        let dir = mount.workspaces.mount_point().join(branch);
        let live = mount.private.live(branch, &dir, &state.index())?;
        let tip = (live.commit, live.tree);
        let made = Made {
            at: signal,
            tip,
            changing: None,
        };
        trees.insert(branch.to_string(), made);
        Ok(Some(tip))
    }
}
