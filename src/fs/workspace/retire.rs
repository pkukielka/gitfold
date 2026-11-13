//! Removing a workspace or a snapshot's registration: the rules shared by the
//! offline `gitfold remove` (`cli::manage`) and the daemon's removal while
//! mounted (`fs::removal`). A name is checked first (`..` would point a removal
//! at the git directory itself); a workspace is removed only without
//! uncommitted or staged changes or a git operation in progress, unless forced;
//! a snapshot's registration only once no open name shows its commit.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Workspace, status};
use crate::git::worktree::workspace_id;
use crate::git::{self, worktree};
use crate::refusal::Refusal;
use crate::store::Store;

/// How many paths an error message lists before "and N more".
const SHOWN_PATHS: usize = 10;

fn shown(paths: &[Vec<u8>]) -> String {
    let mut s: Vec<String> = (paths.iter().take(SHOWN_PATHS))
        .map(|p| format!("  {}", String::from_utf8_lossy(p)))
        .collect();
    if paths.len() > SHOWN_PATHS {
        s.push(format!("  ... and {} more", paths.len() - SHOWN_PATHS));
    }
    s.join("\n")
}

/// Whether the workspace's base is the branch tip, as a note for the user.
pub(crate) fn base_note(branch: &str, base: &gix::ObjectId) -> &'static str {
    match git::resolve(&format!("refs/heads/{branch}")) {
        Err(_) => "branch deleted",
        Ok(tip) if tip.id != *base => "base behind the branch tip (mount to update)",
        Ok(_) => "",
    }
}

/// What `remove --check` says of a workspace that can be removed.
pub fn removable_workspace(branch: &str, ignored: usize) -> String {
    match ignored {
        0 => format!("workspace {branch} can be removed"),
        n => format!("workspace {branch} can be removed (with {n} ignored files in it)"),
    }
}

/// Refuses what can name neither a workspace (a valid branch name) nor a
/// snapshot (`@<rev>`): a name like `..` would point the removal elsewhere.
pub fn check_name(name: &str) -> Result<()> {
    let valid = match name.strip_prefix('@') {
        Some(rev) => !rev.is_empty(),
        None => {
            gix::validate::reference::name(format!("refs/heads/{name}").as_str().into()).is_ok()
        }
    };
    if !valid {
        let message = format!("{name:?} is neither a branch name nor @<rev>");
        return Err(Refusal::NotFound.error(message));
    }
    Ok(())
}

/// Deletes the registration of the snapshot of `commit` (named `@<rev>`)
/// unless `in_use` (another open name shows it); returns what was done. With
/// `dry_run`, only says what it would do.
pub fn remove_snapshot_registration(
    common_dir: &Path,
    rev: &str,
    commit: &gix::ObjectId,
    in_use: bool,
    dry_run: bool,
) -> Result<String> {
    let admin = worktree::snapshot_admin_dir(common_dir, commit);
    Ok(match (dry_run, in_use, admin.exists()) {
        (true, true, _) => format!(
            "@{rev} can be removed; its registration would stay: another open snapshot shows {commit}"
        ),
        (false, true, _) => {
            format!("the registration of @{rev} stays: another open snapshot shows {commit}")
        }
        (true, false, true) => format!("@{rev} can be removed, with its registration"),
        (false, false, true) => {
            remove_dir(&admin)?;
            format!("removed the registration of snapshot @{rev}")
        }
        (_, false, false) => format!("no registration of snapshot @{rev} ({commit}) to remove"),
    })
}

/// Deletes the workspace of `branch` (registration, state and base pin)
/// unless it has uncommitted changes or a git operation in progress (`force`
/// skips the checks). Returns what was done.
pub fn remove_workspace(
    store: &Store,
    common_dir: &Path,
    branch: &str,
    force: bool,
) -> Result<String> {
    let ignored = check_workspace(store, common_dir, branch, force, None)?;
    delete_workspace(common_dir, branch, ignored)
}

/// Fails if the workspace of `branch` doesn't exist, or (unless `force`) has
/// uncommitted changes or a git operation in progress; returns how many
/// ignored files it holds. `open`: the workspace, if the daemon has it open
/// (its base kept at the branch tip), else it is opened to catch up if needed.
pub fn check_workspace(
    store: &Store,
    common_dir: &Path,
    branch: &str,
    force: bool,
    open: Option<&Workspace>,
) -> Result<usize> {
    // `..` would point it at the git directory itself.
    check_name(branch)?;
    let id = workspace_id(branch);
    let state_dir = worktree::state_root(common_dir).join(&id);
    let admin = worktree::ws_admin_dir(common_dir, &id);
    if !state_dir.exists() && !admin.exists() {
        return Err(Refusal::NotFound.error(format!("no workspace for branch {branch}")));
    }
    let changes = |message: String| Refusal::Changes.error(message);
    // Commits reachable only from the worktree (a rebase stopped half-way, a
    // detached HEAD) would be lost with its admin directory.
    if !force
        && admin.exists()
        && let Some(why) = worktree::git_busy(common_dir, &admin, branch)
    {
        // A locked ref is git at work right now: retried by `--wait`.
        return Err(match why == worktree::REF_LOCKED {
            true => Refusal::InUse.error(format!(
                "workspace {branch}: {why} (git is updating it); try again, or use --wait"
            )),
            false => Refusal::Changes.error(format!(
                "workspace {branch}: {why}; finish it, or use --force to discard it"
            )),
        });
    }
    // Staged changes live in the worktree's index, which upper may not show (a
    // file staged, then edited back).
    if !force && admin.join("index").exists() && !index_matches_head(&admin)? {
        return Err(changes(format!(
            "workspace {branch} has staged changes; commit them, or use --force to discard them"
        )));
    }
    let mut ignored = 0;
    if !force && state_dir.join("base").exists() {
        let mut s = status::status(store, common_dir, &state_dir)?;
        // Changes are measured against the base; behind the tip (the branch moved
        // after the daemon last saw it), a file edited back to the base's content
        // would look unchanged. Catch up as the daemon would have.
        let tip = git::resolve(&format!("refs/heads/{branch}")).ok();
        if tip.is_some_and(|tip| tip.id != s.base) {
            let catch_up = || {
                if let Some(ws) = open {
                    return ws.sync(store);
                }
                let ws = Workspace::open(
                    store,
                    common_dir.to_path_buf(),
                    state_dir.clone(),
                    admin.clone(),
                    branch,
                    None,
                    Vec::new(),
                )?;
                ws.sync(store)
            };
            catch_up().map_err(|e| {
                changes(format!(
                    "workspace {branch}: its base is behind the branch tip and catching up \
                     failed ({e:#}); mount it, open it once, or use --force"
                ))
            })?;
            s = status::status(store, common_dir, &state_dir)?;
        }
        ignored = s.ignored;
        let note = base_note(branch, &s.base);
        if !note.is_empty() && note != "branch deleted" {
            return Err(changes(format!(
                "workspace {branch}: {note}, so its changes can't be told; mount it, open \
                 it once, or use --force"
            )));
        }
        if !s.changed.is_empty() || !s.deleted.is_empty() {
            let mut msg = format!(
                "workspace {branch} has uncommitted changes ({} changed, {} deleted):",
                s.changed.len(),
                s.deleted.len()
            );
            for (what, paths) in [("changed", &s.changed), ("deleted", &s.deleted)] {
                if !paths.is_empty() {
                    msg += &format!("\n{what}:\n{}", shown(paths));
                }
            }
            return Err(changes(format!(
                "{msg}\ncommit them, or use --force to discard them"
            )));
        }
    }
    Ok(ignored)
}

/// Deletes the workspace of `branch` (checked by `check_workspace`, which
/// counted `ignored` files in it); returns what was done.
pub fn delete_workspace(common_dir: &Path, branch: &str, ignored: usize) -> Result<String> {
    check_name(branch)?;
    let id = workspace_id(branch);
    let state_dir = worktree::state_root(common_dir).join(&id);
    let admin = worktree::ws_admin_dir(common_dir, &id);
    // The registration first: without it, the state alone is resumed correctly
    // (the registration is recreated); the reverse is not.
    remove_dir(&admin)?;
    remove_dir(&state_dir)?;
    worktree::unpin(common_dir, &worktree::pin_ref(&id))?;
    Ok(match ignored {
        0 => format!("removed workspace {branch}"),
        n => format!("removed workspace {branch} (and {n} ignored files in it)"),
    })
}

/// Whether the index of the worktree with admin directory `admin` matches its
/// HEAD commit.
fn index_matches_head(admin: &Path) -> Result<bool> {
    let status = git::cmd::command(admin)
        .args(["diff-index", "--cached", "--quiet", "HEAD", "--"])
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git diff-index in {} failed ({status})", admin.display()),
    }
}

pub(crate) fn remove_dir(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}
