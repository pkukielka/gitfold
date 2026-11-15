//! Moving a workspace that shows layers onto its base: the layers are taken
//! off, the branch moved as any other (`move_branch`, with its intent), and
//! the same stored layers put back, each one's merge base pinned again
//! against the branch's new commits.
//!
//! The layers file is never changed for it (what is stored is the truth); a
//! marker (`state::lens::OFF`) makes the daemon show none meanwhile. The
//! steps, each durable before the next:
//!
//! 1. the layers' lock taken (held throughout: nothing else changes them);
//! 2. the marker written, and the switch to no layers made (by the daemon,
//!    or unmounted, here): commits made on the layers commit go onto the
//!    branch, HEAD goes back to it, git gets the layers' paths back, and
//!    the view is the base again, so the move's `git read-tree` meets only
//!    the workspace's own files;
//! 3. the move;
//! 4. the merge bases pinned again (`state::lens::put_back`), the marker
//!    removed, the switch to the layers asked for: its base first catches
//!    up with the moved branch, then the layers show over it.
//!
//! Interrupted anywhere, the marker stays, and whoever comes next (the
//! daemon's sync thread for an open workspace, the next turn's hook, `layer
//! base`, `layer add`) first finishes or undoes the move as its intent says
//! (`state::restack`), then does step 4: either way the stored layers show
//! again, pinned against wherever the branch is. A switch refused at step 2
//! (git busy, a commit that can't go onto the branch, own lines that depend
//! on the layers) moves nothing: the marker goes and the layers stay.

use anyhow::Result;

use super::Moved;
use crate::state::StateDir;
use crate::state::lens::{self, Applied, Lens};
use crate::store::Store;

/// Runs `mv`, a move of the branch of the workspace in `state`, with its
/// layers off if it shows any (see the module doc).
pub(super) fn with_layers_off(
    state: &StateDir,
    mv: impl FnOnce() -> Result<Moved>,
) -> Result<Moved> {
    if !state.opened() {
        return mv();
    }
    let _lock = lens::lock(state)?;
    // One interrupted before: back first, so what shows now is known.
    if lens::put_back(state)? {
        log::info!(
            "{}: layers taken off for an interrupted move put back",
            state.branch
        );
    }
    if !shows_layers(state)? {
        return mv();
    }
    lens::take_off(state)?;
    if let Err(why) = switch_off(state) {
        put_back(state);
        return Ok(Moved::Waits(format!(
            "its layers can't be taken off for the move: {why:#}"
        )));
    }
    let moved = mv();
    put_back(state);
    moved
}

/// Puts back layers a move left off (it was interrupted), if any, and has
/// them shown: for the hook and `layer base`, before anything else.
pub(super) fn resume(state: &StateDir) -> Result<()> {
    if !lens::off(state) || !state.opened() {
        return Ok(());
    }
    let _lock = lens::lock(state)?;
    if lens::put_back(state)? {
        show(state);
    }
    Ok(())
}

/// Whether the workspace in `state` shows layers, as stored or as applied.
fn shows_layers(state: &StateDir) -> Result<bool> {
    let applied = Applied::read(state)?;
    Ok(Lens::read(state)?.is_on() || applied.layers()?.is_on() || applied.head.is_some())
}

/// Step 4: the layers back, shown. A failure is logged: the marker is gone
/// by then (or stays for the next to put back), and the daemon retries a
/// switch it refused.
fn put_back(state: &StateDir) {
    match lens::put_back(state) {
        Ok(_) => show(state),
        Err(e) => log::warn!("{}: putting its layers back: {e:#}", state.branch),
    }
}

/// Has the daemon show the stored layers, if mounted (else they show when
/// the workspace opens).
fn show(state: &StateDir) {
    if let Err(e) = super::super::layer::apply(&state.common_dir, &state.branch) {
        log::warn!("{}: showing its layers again: {e:#}", state.branch);
    }
}

/// Step 2: no layers shown in the workspace in `state` (the marker
/// written): switched by the daemon if mounted, else here, under the
/// repository lock. Fails if they still show.
fn switch_off(state: &StateDir) -> Result<()> {
    let common_dir = &state.common_dir;
    match crate::mount_table::mount_of(common_dir)? {
        Some(mount) => {
            // Opened first: the daemon switches only open workspaces.
            std::fs::metadata(mount.join(&state.branch))?;
            super::super::layer::apply(common_dir, &state.branch)?;
        }
        None => {
            let Some(_repo) = crate::lock::lock(common_dir)? else {
                anyhow::bail!("the repository is being mounted or unmounted; retry");
            };
            let store = Store::default();
            crate::fs::workspace::Workspace::open(&store, state.clone(), None)?.switch(&store)?;
        }
    }
    let applied = Applied::read(state)?;
    anyhow::ensure!(
        !applied.layers()?.is_on() && applied.head.is_none(),
        "they still show"
    );
    Ok(())
}
