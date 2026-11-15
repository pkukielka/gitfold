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
//! every write through the mount), or its base or its branch moved (a
//! commit), since its layer last moved to a snapshot of it, and once it has
//! been quiet for `SOURCE_QUIET`. A closed one is snapshotted once per
//! mount, then when its base or branch moves. One whose layer couldn't move
//! is tried again after `BACK_OFF`; so is a move that failed as a whole (its
//! switch refused: git busy, or own lines in a union file that depend on
//! lines that would go), or sooner if the workspace is written to.
//!
//! Its branch first follows its editable layers' new commits, as the hook
//! moves an agent's at its turn start (`cli::layer::follow_editable`), once
//! their tips or its own moved since it last looked: so a person's waiting
//! edits become ordinary changes too once the layer commits those lines.
//!
//! The move is the hook's (`state::follow::follow_live`), under the layers'
//! lock (`state::layers::try_lock`: a command holding it goes first, and the
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
use crate::git;
use crate::state::layers::Updates;
use crate::state::{StateDir, follow, layers, snapshot};
use crate::store::Store;

/// How long nothing must have written to a workspace without an agent
/// before the daemon moves its live layers.
pub const IDLE: Duration = Duration::from_secs(3);

/// How often the sync thread looks for newer work to move them to.
const CHECK_EVERY: Duration = Duration::from_secs(2);

/// How long a person's workspace must have been quiet before a snapshot of
/// it is taken for another's layer: a save in progress isn't half-seen.
const SOURCE_QUIET: Duration = Duration::from_secs(1);

/// How long a move that failed waits before it is tried again (see the
/// module doc).
const BACK_OFF: Duration = Duration::from_secs(10);

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

/// A person's workspace as a layer last moved to a snapshot of it: its
/// writes (if open here), its base and its branch's tip.
type Seen = (Option<u64>, ObjectId, Option<ObjectId>);

/// What `follow_layers` keeps between checks.
#[derive(Default)]
pub(super) struct Following {
    checked: Option<Instant>,
    /// Per person's workspace a layer shows, as the layer last moved to a
    /// snapshot of it.
    seen: HashMap<String, Seen>,
    /// Per person's workspace whose layer couldn't move to a snapshot of
    /// it: when (tried again after `BACK_OFF`).
    source_failed: HashMap<String, Instant>,
    /// The last move that failed as a whole: when, and this workspace's
    /// writes then (tried again after `BACK_OFF`, or once written to).
    refused: Option<(Instant, u64)>,
    /// The last error logged, not logged again until another.
    failed: String,
    /// The tips of its editable layers' branches and its own when it last
    /// followed them, and whether it got where they are.
    editable: Option<(Vec<Option<ObjectId>>, bool)>,
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
                // The same again next round, most likely: after a while.
                following.refused = Some((Instant::now(), self.activity.writes()));
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
        if self.activity.quiet_for() < IDLE || self.layers_pending() {
            return Ok(None);
        }
        let backing_off = (self.following.lock().refused).is_some_and(|(at, writes)| {
            at.elapsed() < BACK_OFF && self.activity.writes() == writes
        });
        if backing_off {
            return Ok(None);
        }
        let applied = self.layers.read().applied.layers()?;
        if !(applied.layers.iter()).any(|l| l.updates == Updates::Live)
            || snapshot::has_agent(&self.state)
            || !self.git_idle()
        {
            return Ok(None);
        }
        let Some(locked) = layers::try_lock(&self.state)? else {
            return Ok(None);
        };
        // Its editable layers' new commits first, as an agent's turn start
        // moves its branch (the move is a switch of this workspace, asked
        // of this daemon); those it can't move onto are held as they show.
        // Looked at again only once a tip moved (theirs or its own).
        let mut hold = Vec::new();
        let editable = applied.editable_names();
        let tips: Vec<Option<ObjectId>> = (editable.iter())
            .chain(std::iter::once(&self.state.branch))
            .map(|b| git::branch_tip(b).ok().map(|t| t.id))
            .collect();
        let last = self.following.lock().editable.clone();
        match last {
            _ if editable.is_empty() => {}
            Some((seen, true)) if seen == tips => {}
            Some((seen, false)) if seen == tips => hold = editable,
            _ => {
                let followed =
                    crate::cli::layer::follow_editable(&self.state.common_dir, &self.state.branch);
                let done = match followed {
                    Ok((done, said)) => {
                        if let Some(said) = said {
                            log::info!(
                                "{}: following its editable layers: {said}",
                                self.state.branch
                            );
                        }
                        done
                    }
                    Err(e) => {
                        log::warn!(
                            "{}: following its editable layers: {e:#}",
                            self.state.branch
                        );
                        false
                    }
                };
                if !done {
                    hold = editable;
                }
                // Its own tip as it is after the move.
                let mut tips = tips;
                if let Some(own) = tips.last_mut() {
                    *own = git::branch_tip(&self.state.branch).ok().map(|t| t.id);
                }
                self.following.lock().editable = Some((tips, done));
            }
        }
        let (seen, source_failed) = {
            let following = self.following.lock();
            (following.seen.clone(), following.source_failed.clone())
        };
        // The person's workspaces snapshotted, as they were: marked seen
        // once their layers moved.
        let mut tried = Vec::new();
        let fresh = |theirs: &StateDir| {
            let activity = activities.lock().get(&theirs.branch).cloned();
            if activity
                .as_ref()
                .is_some_and(|a| a.quiet_for() < SOURCE_QUIET)
                || (source_failed.get(&theirs.branch)).is_some_and(|at| at.elapsed() < BACK_OFF)
            {
                return false;
            }
            let Ok(base) = theirs.read_base() else {
                return true;
            };
            let tip = git::branch_tip(&theirs.branch).ok().map(|t| t.id);
            let now = (activity.map(|a| a.writes()), base.id, tip);
            if seen.get(&theirs.branch) == Some(&now) {
                return false;
            }
            tried.push((theirs.branch.clone(), now));
            true
        };
        let followed = follow::follow_live(store, &self.state, &locked, &hold, fresh, || {
            self.switch(store)
        })?;
        let Some(followed) = followed else {
            return Ok(None);
        };
        {
            let mut following = self.following.lock();
            following.refused = None;
            for (branch, now) in tried {
                if followed.failed.contains(&branch) {
                    following.source_failed.insert(branch, Instant::now());
                } else {
                    following.source_failed.remove(&branch);
                    following.seen.insert(branch, now);
                }
            }
        }
        if followed.moved.is_empty() {
            return Ok(None);
        }
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
