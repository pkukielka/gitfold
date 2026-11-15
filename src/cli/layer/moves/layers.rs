//! Moving a workspace that shows layers onto its base: the layers are taken
//! off, the branch moved as any other (`move_branch`, with its intent), and
//! the same stored layers put back, shown over the branch's new commits
//! once the workspace's base has caught up with them.
//!
//! The layers file is never changed for it (what is stored is the truth); a
//! marker (`state::layers::OFF`) makes the daemon show none meanwhile. The
//! steps, each durable before the next:
//!
//! 1. the layers' lock taken (held throughout: nothing else changes them),
//!    and what would make the move wait anyway checked first (`waits`:
//!    another move to finish, git busy, uncommitted work on a path the move
//!    changes), so a move that waits doesn't switch the view off and on;
//! 2. the marker written, and the switch to no layers made (by the daemon,
//!    or unmounted, here): commits made on the layers commit go onto the
//!    branch, HEAD goes back to it, git gets the layers' paths back, and
//!    the view is the base again, so the move's `git read-tree` meets only
//!    the workspace's own files;
//! 3. the move, worked out again if the switch moved the branch (commits
//!    made on the layers commit went onto it);
//! 4. the marker removed (`state::layers::put_back`), the switch to the
//!    layers asked for: its base first catches up with the moved branch,
//!    then the layers show over it.
//!
//! Interrupted anywhere, the marker stays, and whoever comes next (the
//! daemon's sync thread for an open workspace, the next turn's hook, `layer
//! base`, `layer add`) first finishes or undoes the move as its intent says
//! (`state::moves`), then does step 4 (`resume`): either way the stored
//! layers show again, over wherever the branch is. While a move can't be
//! finished or undone, they stay off. A switch refused at step 2
//! (git busy, a commit that can't go onto the branch, own lines that depend
//! on the layers) moves nothing: the marker goes and the layers stay.

use anyhow::Result;
use gix::bstr::BString;

use super::Moved;
use crate::git::{self, worktree};
use crate::state::layers::{self, Layers, ShownLayers};
use crate::state::moves::{self, Intent, Recovery};
use crate::state::{StateDir, status, upper_path};
use crate::store::Store;

/// Runs `mv`, the move `intent` of the branch of the workspace in `state`,
/// with its layers off if it shows any (see the module doc).
pub(super) fn with_layers_off(
    state: &StateDir,
    intent: &Intent,
    mv: impl FnOnce() -> Result<Moved>,
) -> Result<Moved> {
    if !state.opened() {
        return mv();
    }
    let _lock = layers::lock(state)?;
    // One interrupted before: back first, so what shows now is known.
    if layers::put_back(state)? {
        log::info!(
            "{}: layers taken off for an interrupted move put back",
            state.branch
        );
    }
    if !shows_layers(state)? {
        return mv();
    }
    if let Some(why) = waits(state, intent)? {
        return Ok(Moved::Waits(why));
    }
    layers::take_off(state)?;
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
/// them shown: for the hook and `layer base`, before anything else. The
/// move is finished or undone first (`state::moves`), as the daemon does:
/// the layers show over where it leaves the branch, and a switch never
/// catches the workspace's base up with a move half-way. One that can't be
/// yet (running, or stuck) leaves them off.
pub(super) fn resume(state: &StateDir) -> Result<()> {
    if !layers::off(state) || !state.opened() {
        return Ok(());
    }
    let _lock = layers::lock(state)?;
    if moves::pending(state) {
        match moves::recover(state) {
            Ok(Recovery::Nothing | Recovery::Finished | Recovery::Undone) => {}
            Ok(Recovery::InProgress | Recovery::Stuck(_)) => return Ok(()),
            Err(e) => {
                log::warn!("{}: finishing an interrupted move: {e:#}", state.branch);
                return Ok(());
            }
        }
    }
    if layers::put_back(state)? {
        show(state);
    }
    Ok(())
}

/// Why the move `intent` of the workspace in `state`, which shows layers,
/// would wait anyway, checked before they are taken off: another move to
/// finish first, git busy, or uncommitted work on a path the move changes
/// (as `git read-tree` refuses it). Commits made on the layers commit that
/// aren't on the branch yet go onto it as the layers come off: the
/// uncommitted work is then judged by the move itself.
fn waits(state: &StateDir, intent: &Intent) -> Result<Option<String>> {
    if let Err(why) = super::recovered(state)? {
        return Ok(Some(why));
    }
    let applied = ShownLayers::read(state)?;
    let busy = match applied.head {
        Some(_) => worktree::git_running(&state.common_dir, &state.admin, &state.branch),
        None => state.git_busy(),
    };
    if let Some(why) = busy {
        return Ok(Some(format!("{why} in its workspace")));
    }
    let head = std::fs::read_to_string(state.admin.join("HEAD")).unwrap_or_default();
    let head = gix::ObjectId::from_hex(head.trim().as_bytes()).ok();
    if applied.head.is_some_and(|l| head != Some(l)) {
        return Ok(None);
    }
    let in_the_way = uncommitted_on(state, intent, &applied.blended)?;
    Ok((!in_the_way.is_empty()).then(|| super::in_the_way(&in_the_way)))
}

/// The paths the move `intent` changes where the workspace in `state` has
/// work of its own its branch hasn't (its files differ from the branch
/// tip's), but for `blended` ones (their upper copies hold the layers'
/// lines until the layers come off).
fn uncommitted_on(
    state: &StateDir,
    intent: &Intent,
    blended: &std::collections::BTreeSet<Vec<u8>>,
) -> Result<Vec<String>> {
    let moved: std::collections::HashSet<BString> =
        moves::changed_paths(intent)?.into_iter().collect();
    let s = status::status(&Store::default(), state)?;
    let tip = git::resolve(&intent.from.to_string())?.tree;
    let mut out = Vec::new();
    let paths = s.changed.iter().chain(&s.deleted);
    for path in paths.filter(|p| moved.contains(p.as_slice()) && !blended.contains(*p)) {
        let at_tip = git::with_repo(|repo| git::layers::lookup(repo, tip, path.as_slice().into()))?;
        let file = upper_path(&state.upper(), path);
        let differs = match (at_tip, std::fs::symlink_metadata(&file)) {
            (None, Ok(_)) | (Some(_), Err(_)) => true,
            (None, Err(_)) => false,
            (Some(item), Ok(meta)) => {
                let exec = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111;
                let kind = match (meta.is_symlink(), exec) {
                    (true, _) => gix::objs::tree::EntryKind::Link,
                    (false, 0) => gix::objs::tree::EntryKind::Blob,
                    (false, _) => gix::objs::tree::EntryKind::BlobExecutable,
                };
                let data = match meta.is_symlink() {
                    true => std::os::unix::ffi::OsStrExt::as_bytes(
                        std::fs::read_link(&file)?.as_os_str(),
                    )
                    .to_vec(),
                    false => std::fs::read(&file)?,
                };
                kind != item.kind || data != git::read_blob(item.oid)?
            }
        };
        if differs {
            out.push(String::from_utf8_lossy(path).into_owned());
        }
    }
    Ok(out)
}

/// Whether the workspace in `state` shows layers, as stored or as applied.
fn shows_layers(state: &StateDir) -> Result<bool> {
    let applied = ShownLayers::read(state)?;
    Ok(Layers::read(state)?.is_on() || applied.layers()?.is_on() || applied.head.is_some())
}

/// Step 4: the layers back, shown. A failure is logged: the marker is gone
/// by then (or stays for the next to put back), and the daemon retries a
/// switch it refused.
fn put_back(state: &StateDir) {
    match layers::put_back(state) {
        Ok(_) => show(state),
        Err(e) => log::warn!("{}: putting its layers back: {e:#}", state.branch),
    }
}

/// Has the daemon show the stored layers, if mounted (else they show when
/// the workspace opens).
fn show(state: &StateDir) {
    if let Err(e) = super::super::apply(&state.common_dir, &state.branch) {
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
            super::super::apply(common_dir, &state.branch)?;
        }
        None => {
            let Some(_repo) = crate::lock::lock(common_dir)? else {
                anyhow::bail!("the repository is being mounted or unmounted; retry");
            };
            let store = Store::default();
            crate::fs::workspace::Workspace::open(&store, state.clone(), None)?.switch(&store)?;
        }
    }
    let applied = ShownLayers::read(state)?;
    anyhow::ensure!(
        !applied.layers()?.is_on() && applied.head.is_none(),
        "they still show"
    );
    Ok(())
}
