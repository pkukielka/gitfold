//! Live layers of a workspace without an agent, kept up to date by the
//! daemon.
//!
//! A workspace with an agent moves its live layers at its turns' starts
//! (its `UserPromptSubmit` hook, see `state::follow`), and only then: what
//! it shows doesn't change half-way through a turn. One without (a person
//! works in it) has no turns, so the sync thread moves them
//! (`follow_layers`) once:
//! - its live layers' branches have newer work: an agent's newer
//!   end-of-turn snapshot, a branch tip that moved, or a person's workspace
//!   written to since its last snapshot (only then is a new one taken), and
//! - nothing has written to it for `IDLE`, so files don't change under
//!   someone half-way through an edit.
//!
//! It is checked every `CHECK_EVERY`, cheaply: a person's workspace open
//! here is snapshotted only when it was written to (`Activity`, counted by
//! every write through the mount) or its base moved since its last
//! snapshot, and once it has been quiet for `SOURCE_QUIET`. A closed one is
//! snapshotted once per mount, then when its base moves.
//!
//! The move is the hook's (`state::follow::follow_live`), under the layers'
//! lock (`state::lens::try_lock`: a command holding it goes first, and the
//! check waits for the next round), shown through `switch`, as `gitfold
//! layer` has it shown. It is logged, one line; `gitfold layer` lists when
//! each layer last moved.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gix::ObjectId;
use parking_lot::Mutex;

use super::Workspace;
use crate::state::lens::Updates;
use crate::state::{StateDir, follow, lens, seal};
use crate::store::Store;

/// How long nothing must have written to a workspace without an agent
/// before the daemon moves its live layers.
pub const IDLE: Duration = Duration::from_secs(3);

/// How often the sync thread looks for newer work to move them to.
const CHECK_EVERY: Duration = Duration::from_secs(2);

/// How long a person's workspace must have been quiet before a snapshot of
/// it is taken for another's layer: a save in progress isn't half-seen.
const SOURCE_QUIET: Duration = Duration::from_secs(1);

/// Writes through the mount to one workspace: how many, and when the last
/// was.
#[derive(Default)]
pub struct Activity {
    writes: AtomicU64,
    /// Milliseconds since `epoch()`, plus one (0: never written).
    last: AtomicU64,
}

/// The open workspaces' `Activity`, by branch.
pub type Activities = Arc<Mutex<HashMap<String, Arc<Activity>>>>;

/// When the daemon started counting, for `Activity::last`.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

impl Activity {
    /// Counts a write.
    pub fn wrote(&self) {
        let at = epoch().elapsed().as_millis() as u64 + 1;
        self.last.store(at, Ordering::Relaxed);
        self.writes.fetch_add(1, Ordering::Release);
    }

    /// How many writes there were.
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Acquire)
    }

    /// How long since the last write (`Duration::MAX`: none).
    pub fn quiet_for(&self) -> Duration {
        match self.last.load(Ordering::Relaxed) {
            0 => Duration::MAX,
            at => epoch()
                .elapsed()
                .saturating_sub(Duration::from_millis(at - 1)),
        }
    }
}

/// What `follow_layers` keeps between checks.
#[derive(Default)]
pub(super) struct Following {
    checked: Option<Instant>,
    /// Per person's workspace a layer shows: its writes (if open here) and
    /// base when last snapshotted.
    seen: HashMap<String, (Option<u64>, ObjectId)>,
    /// The last error logged, not logged again until another.
    failed: String,
}

impl Workspace {
    /// Its `Activity`, for the registry.
    pub fn activity(&self) -> Arc<Activity> {
        self.activity.clone()
    }

    /// The sync thread's part for a workspace without an agent: moves its
    /// live layers to their newer work once it is idle (see the module
    /// doc). `activities`: those of the open workspaces, to tell whether a
    /// person's was written to. Errors are logged.
    pub fn follow_layers(&self, store: &Store, activities: &Activities) {
        {
            let mut following = self.following.lock();
            if following
                .checked
                .is_some_and(|at| at.elapsed() < CHECK_EVERY)
            {
                return;
            }
            following.checked = Some(Instant::now());
        }
        let followed = self.follow_now(store, activities);
        let mut following = self.following.lock();
        match followed {
            Ok(Some(moved)) => {
                following.failed.clear();
                log::info!(
                    "{}: its layers moved to their latest work ({} s without a write, no \
                     agent): {moved}",
                    self.state.branch,
                    IDLE.as_secs()
                );
            }
            Ok(None) => following.failed.clear(),
            Err(e) => {
                // Snapshotted again next time.
                following.seen.clear();
                let e = format!("{e:#}");
                if e != following.failed {
                    log::warn!("{}: its layers not moved: {e}", self.state.branch);
                    following.failed = e;
                }
            }
        }
    }

    /// `follow_layers` once due: what moved, if anything did.
    fn follow_now(&self, store: &Store, activities: &Activities) -> anyhow::Result<Option<String>> {
        // The cheap checks first.
        if self.activity.quiet_for() < IDLE || self.lens_pending() {
            return Ok(None);
        }
        let applied = self.layers.read().applied.layers()?;
        if !(applied.layers.iter()).any(|l| l.updates == Updates::Live)
            || seal::has_agent(&self.state)
            || !self.git_idle()
        {
            return Ok(None);
        }
        let Some(locked) = lens::try_lock(&self.state)? else {
            return Ok(None);
        };
        let mut seen = std::mem::take(&mut self.following.lock().seen);
        let fresh = |theirs: &StateDir| {
            let activity = activities.lock().get(&theirs.branch).cloned();
            if activity
                .as_ref()
                .is_some_and(|a| a.quiet_for() < SOURCE_QUIET)
            {
                return false;
            }
            let Ok(base) = theirs.read_base() else {
                return true;
            };
            let now = (activity.map(|a| a.writes()), base.id);
            seen.insert(theirs.branch.clone(), now) != Some(now)
        };
        let followed =
            follow::follow_live(store, &self.state, &locked, fresh, || self.switch(store));
        self.following.lock().seen = seen;
        let Some(followed) = followed?.filter(|f| !f.moved.is_empty()) else {
            return Ok(None);
        };
        // With what showing them noted (conflicts), as the hook says it.
        let notes = (followed.applied.lines()).filter(|l| l.starts_with("note: "));
        Ok(Some(
            std::iter::once(followed.moved.join(", "))
                .chain(notes.map(|n| format!("{}: {n}", self.state.branch)))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}
