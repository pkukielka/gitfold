//! Removing a workspace or a snapshot's registration: the rules shared by the
//! offline `gitfold remove` (`cli::remove`) and the daemon's removal while
//! mounted (`fs::removal`). A name is checked first (`..` would point a removal
//! at the git directory itself); a workspace is removed only without
//! uncommitted or staged changes or a git operation in progress, unless forced;
//! a snapshot's registration only once no open name shows its commit.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::status;
use crate::git::{self, worktree};
use crate::refusal::Refusal;
use crate::state::StateDir;
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

/// Where a workspace's stored base is, relative to its branch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tip {
    /// The base is the branch tip.
    At,
    /// The branch moved after the daemon last saw it.
    Behind,
    /// The branch is gone.
    Deleted,
}

impl Tip {
    /// Where `base`, the base of the workspace of `branch`, is.
    fn of(branch: &str, base: &gix::ObjectId) -> Tip {
        match git::branch_tip(branch) {
            Err(_) => Tip::Deleted,
            Ok(tip) if tip.id != *base => Tip::Behind,
            Ok(_) => Tip::At,
        }
    }

    /// What to tell the user about it: nothing at the tip.
    fn note(self) -> &'static str {
        match self {
            Tip::At => "",
            Tip::Behind => "base behind the branch tip (mount to update)",
            Tip::Deleted => "branch deleted",
        }
    }
}

/// Whether the workspace's base is the branch tip, as a note for the user.
pub fn base_note(branch: &str, base: &gix::ObjectId) -> &'static str {
    Tip::of(branch, base).note()
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

/// Fails if the workspace of `branch` doesn't exist, or (unless `force`) has
/// uncommitted changes or a git operation in progress; returns how many
/// ignored files it holds. `catch_up` brings its stored base up to the branch
/// tip, if it is behind: `fs::workspace::catch_up` for a closed workspace, or
/// the open workspace's `sync` (`state` can't open one).
pub fn check_workspace(
    store: &Store,
    common_dir: &Path,
    branch: &str,
    force: bool,
    catch_up: &dyn Fn() -> Result<()>,
) -> Result<usize> {
    // `..` would point it at the git directory itself.
    check_name(branch)?;
    let state = StateDir::of(common_dir, branch);
    if !state.dir.exists() && !state.admin.exists() {
        return Err(Refusal::NotFound.error(format!("no workspace for branch {branch}")));
    }
    let changes = |message: String| Refusal::Changes.error(message);
    // Commits reachable only from the worktree (a rebase stopped half-way, a
    // detached HEAD) would be lost with its admin directory.
    if !force
        && state.admin.exists()
        && let Some(why) = state.git_busy()
    {
        // A locked ref is git at work right now: retried by `--wait`.
        return Err(match why == worktree::GitBusy::RefLocked {
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
    if !force && state.index().exists() && !index_matches_head(&state.admin)? {
        return Err(changes(format!(
            "workspace {branch} has staged changes; commit them, or use --force to discard them"
        )));
    }
    let mut ignored = 0;
    if !force && state.opened() {
        let mut s = status::status(store, &state)?;
        // Changes are measured against the base; behind the tip (the branch moved
        // after the daemon last saw it), a file edited back to the base's content
        // would look unchanged. Catch up as the daemon would have.
        if Tip::of(branch, &s.base) == Tip::Behind {
            catch_up().map_err(|e| {
                changes(format!(
                    "workspace {branch}: its base is behind the branch tip and catching up \
                     failed ({e:#}); mount it, open it once, or use --force"
                ))
            })?;
            s = status::status(store, &state)?;
        }
        ignored = s.ignored;
        let tip = Tip::of(branch, &s.base);
        if tip == Tip::Behind {
            return Err(changes(format!(
                "workspace {branch}: {}, so its changes can't be told; mount it, open it \
                 once, or use --force",
                tip.note()
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
    let state = StateDir::of(common_dir, branch);
    // The registration first: without it, the state alone is resumed correctly
    // (the registration is recreated); the reverse is not.
    remove_dir(&state.admin)?;
    remove_dir(&state.dir)?;
    worktree::unpin(common_dir, &state.pin_ref())?;
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

/// Removes directory `dir` and what it holds, if it is there.
pub fn remove_dir(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}
