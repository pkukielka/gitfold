//! Moving a workspace's live layers to their branches' latest work (see
//! `lens`). Shared by the two that move them:
//! - a workspace with an agent: its `UserPromptSubmit` hook, at its turns'
//!   starts (`cli::layer::layer_hook`), so what shows changes only between
//!   turns;
//! - one without (a person's): the mounted daemon, once nothing has written
//!   to it for a few seconds (`fs::workspace::follow`).
//!
//! What a live layer follows (`newest`): a workspace's work as `latest_work`
//! says (an agent's as it was at the end of its last turn, a person's as it
//! is now), or, with `--commits` or for a branch without a workspace, the
//! branch's tip. Frozen layers and revisions never move.

use std::path::Path;

use anyhow::Result;
use gix::ObjectId;

use super::StateDir;
use super::lens::{Layer, LayersLock, Lens, Shows, Updates};
use super::seal;
use crate::git;
use crate::settings::Local;
use crate::store::Store;

/// What `follow_live` did.
pub struct Followed {
    /// The layers that moved, with how many files changed: `B (2 files
    /// changed)`.
    pub moved: Vec<String>,
    /// What showing them said (`show`; empty if no layer moved, so nothing
    /// was shown).
    pub applied: String,
    /// The layers that couldn't move now (their branches): an error, only
    /// logged.
    pub failed: Vec<String>,
}

/// Moves the live layers of the workspace in `state` to their latest work
/// (`newest`), stores them and has them shown (`show`, see
/// `lens::store_and_show`). `fresh(theirs)` says whether a person's
/// workspace (no agent, in `theirs`) is to be snapshotted now: the hook
/// always does, the daemon only when it was written to since. One layer that
/// can't move now (its workspace busy, say) leaves the others to move, and
/// is named in `Followed::failed`. `None` if the workspace is being moved
/// onto its base (that move goes first: nothing done). Called under
/// `lens::lock`.
pub fn follow_live(
    store: &Store,
    state: &StateDir,
    _locked: &LayersLock,
    mut fresh: impl FnMut(&StateDir) -> bool,
    show: impl FnOnce() -> Result<String>,
) -> Result<Option<Followed>> {
    if super::restack::pending(state) {
        return Ok(None);
    }
    let mut lens = Lens::read(state)?;
    let (mut moved, mut stored, mut failed) = (Vec::new(), false, Vec::new());
    let local = Local::read(&state.common_dir);
    for layer in &mut lens.layers {
        if layer.updates != Updates::Live {
            continue;
        }
        let files = match follow(store, &state.common_dir, &local, layer, &mut fresh) {
            Ok(Some(files)) => files,
            Ok(None) => continue,
            Err(e) => {
                log::debug!("{}: moving layer {}: {e:#}", state.branch, layer.branch);
                failed.push(layer.branch.clone());
                continue;
            }
        };
        stored = true;
        // The same files (its snapshot of what its tip has): nothing to say,
        // nor a move to show.
        if files == 0 {
            continue;
        }
        layer.moved = crate::fsutil::epoch_secs(std::time::SystemTime::now());
        let what = if files == 1 {
            "file changed"
        } else {
            "files changed"
        };
        moved.push(format!("{} ({files} {what})", layer.branch));
    }
    let applied = match stored {
        true => super::lens::store_and_show(state, &lens, show)?,
        false => String::new(),
    };
    Ok(Some(Followed {
        moved,
        applied,
        failed,
    }))
}

/// Moves live `layer` to where it is now (`newest`): how many files
/// changed, `None` if it didn't move.
fn follow(
    store: &Store,
    common_dir: &Path,
    local: &Local,
    layer: &mut Layer,
    fresh: &mut impl FnMut(&StateDir) -> bool,
) -> Result<Option<usize>> {
    let Some((newest, shows)) = newest(store, common_dir, local, layer, fresh)? else {
        return Ok(None);
    };
    if newest == layer.seal {
        return Ok(None);
    }
    let (from, to) = (
        git::resolve(&layer.seal.to_string())?,
        git::resolve(&newest.to_string())?,
    );
    let files = git::with_repo(|repo| git::lens::delta(repo, from.tree, to.tree))?.len();
    layer.seal = newest;
    layer.shows = shows;
    Ok(Some(files))
}

/// Where live `layer` is now, and what it shows there: its workspace's
/// work (`latest_work`), or with `--commits` or without a workspace, its
/// branch's tip (`None` if the branch is gone, or a person's workspace isn't
/// to be snapshotted now: `fresh`).
fn newest(
    store: &Store,
    common_dir: &Path,
    local: &Local,
    layer: &Layer,
    fresh: &mut impl FnMut(&StateDir) -> bool,
) -> Result<Option<(ObjectId, Shows)>> {
    if !git::branch_exists(&layer.branch) {
        return Ok(None);
    }
    let theirs = StateDir::of(common_dir, &layer.branch);
    if layer.shows != Shows::CommitsOnly && theirs.opened() {
        let work = match seal::driven(&theirs) {
            seal::Driven::Person if !fresh(&theirs) => None,
            _ => latest_work(store, &theirs)?,
        };
        return Ok(work.map(|work| (work, Shows::Work)));
    }
    let shows = tip_shows(local, &layer.branch, layer.shows == Shows::CommitsOnly);
    Ok(Some((git::branch_tip(&layer.branch)?.id, shows)))
}

/// The work of the workspace in `theirs` a live layer shows (see
/// `seal`): an agent's (one whose `Stop` hook ran) as it was at the end of
/// its last turn, never half-way through one; one without an agent (a
/// person's, a proposal edited from another workspace) has no turns, so a
/// fresh snapshot, as it is now. An agent's that wasn't shown when its last
/// turn ended has none to follow (`None`): the layer stays as it was added.
pub fn latest_work(store: &Store, theirs: &StateDir) -> Result<Option<ObjectId>> {
    Ok(match seal::driven(theirs) {
        seal::Driven::Agent(last_turn) => last_turn,
        seal::Driven::Person => Some(seal::seal(store, theirs, seal::By::Other)?.commit),
    })
}

/// What a layer showing the tip of `branch` shows: only its commits with
/// `commits`; all work for a proposal (a workspace to edit, its work shown as
/// all work even before it first opens: its tip, then); else its commits.
pub fn tip_shows(local: &Local, branch: &str, commits: bool) -> Shows {
    match commits {
        true => Shows::CommitsOnly,
        false if local.proposal(branch).is_some() => Shows::Work,
        false => Shows::Commits,
    }
}
