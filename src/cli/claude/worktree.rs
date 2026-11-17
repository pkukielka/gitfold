//! Claude Code's worktrees as gitfold workspaces: its `WorktreeCreate` and
//! `WorktreeRemove` hooks (`gitfold branch --claude-hook worktree-create`,
//! `gitfold remove --claude-hook worktree-remove`), which replace its own
//! `git worktree` logic.
//!
//! For a repository not used with gitfold they do what Claude Code would: a
//! git worktree in `.claude/worktrees/<name>`. One used with gitfold but not
//! mounted is mounted again where `gitfold mount` last mounted it, or else
//! refused: a full checkout of it is what gitfold is there to avoid.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::cwd;
use crate::cli::hook::field;
use crate::git;
use crate::mount_table;
use crate::state::state_root;

/// Claude Code's branch name for its worktree `name` (as bin/gitfold's
/// fallback names it too), kept so a worktree
/// named like a branch of the repository (`main`) is never that branch's
/// workspace.
fn branch_of(name: &str) -> String {
    format!("worktree-{name}")
}

/// `WorktreeCreate`: makes worktree `name` of the repository of the session's
/// directory and prints its path, the only thing on stdout, which Claude Code
/// reads. A workspace of branch `worktree-<name>` (created from the session's
/// HEAD if new; from the mount root or a view, the repository's HEAD) when the
/// repository is mounted, or was.
pub fn worktree_create() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    let name = field(&input, "name").context("no \"name\" in the hook's input")?;
    let cwd = cwd(&input)?;
    let repo = crate::cli::repo_at(&cwd)?;
    let common_dir = git::init(&repo)?;
    let branch = branch_of(&name);
    let exists = git::branch_exists(&branch);
    let mount = crate::cli::mount::mount_or_again(&common_dir)?;
    // Used with gitfold, but no mount point recorded (an older gitfold's).
    ensure!(
        mount.is_some() || !state_root(&common_dir).exists(),
        "{} is used with gitfold but not mounted: mount it (`gitfold mount`) for a workspace, \
         rather than a full checkout",
        common_dir.display()
    );
    let path = match mount {
        Some(mount) => {
            let path = mount.join(&branch);
            if !exists {
                crate::cli::branch::check_new(&common_dir, &branch)?;
                crate::cli::branch::create(&repo, &branch, None)?;
            }
            // Looking it up opens it.
            let meta = fs::metadata(&path)
                .with_context(|| format!("opening workspace {}", path.display()))?;
            ensure!(meta.is_dir(), "{} is not a workspace", path.display());
            path
        }
        // Kept as bin/gitfold's fallback does it, for when gitfold can't run.
        None => {
            // As Claude Code: below the top of the worktree the session is in.
            let top = git::work_tree().context("the session is in no worktree")?;
            let path = top.join(".claude/worktrees").join(&name);
            if !path.join(".git").exists() {
                let at = path.to_str().context("a worktree path that isn't UTF-8")?;
                let args: &[&str] = match exists {
                    true => &["worktree", "add", at, &branch],
                    false => &["worktree", "add", "-b", &branch, at],
                };
                git::cmd::output(&repo, args)?;
            }
            path
        }
    };
    println!("{}", path.display());
    Ok(())
}

/// `WorktreeRemove`: removes the worktree at `worktree_path` (Claude Code asks
/// once the user chose to, or the worktree is clean), discarding its changes
/// as Claude Code does; its branch stays. A workspace in use (the session's
/// shell still in it) is waited for briefly. Unmounted, a workspace is
/// removed all the same, if the session's repository (`cwd`) was last
/// mounted where it is (`unmounted_workspace`).
pub fn worktree_remove() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    let path = PathBuf::from(
        field(&input, "worktree_path").context("no \"worktree_path\" in the hook's input")?,
    );
    let session = cwd(&input);
    // The hook starts in the worktree; from in there, it is in use.
    std::env::set_current_dir("/")?;
    if !path.exists() {
        // Gone, unless its repository isn't mounted.
        let Some((repo, branch)) = session.ok().and_then(|s| unmounted_workspace(&s, &path)) else {
            return Ok(());
        };
        return crate::cli::remove::remove(&repo, removal(branch));
    }
    let repo = crate::cli::repo_at(&path)?;
    let common_dir = git::init(&repo)?;
    let mount = mount_table::mount_of(&common_dir)?;
    if mount.is_some_and(|m| path.starts_with(m)) {
        let branch = crate::cli::branch_or_current(None, &repo)?;
        return crate::cli::remove::remove(&repo, removal(branch));
    }
    let path = path.to_str().context("a worktree path that isn't UTF-8")?;
    git::cmd::output(&common_dir, &["worktree", "remove", "--force", path]).map(drop)
}

/// The workspace of `branch` removed, its changes discarded (as Claude Code
/// does), waiting briefly while it is in use.
fn removal(branch: String) -> crate::cli::remove::RemoveArgs {
    crate::cli::remove::RemoveArgs {
        name: Some(branch),
        force: true,
        check: false,
        wait: 10,
        claude_hook: None,
    }
}

/// The repository of directory `session` and the branch whose workspace
/// `path` is, if that repository isn't mounted, `path` is a workspace's
/// path where it was last mounted (`gitfold.mountpoint`), and that workspace
/// is stored.
fn unmounted_workspace(session: &Path, path: &Path) -> Option<(PathBuf, String)> {
    let repo = crate::cli::repo_at(session).ok()?;
    let common_dir = git::init(&repo).ok()?;
    if mount_table::mount_of(&common_dir).ok()?.is_some() {
        return None;
    }
    let last = crate::settings::Local::read(&common_dir).mountpoint()?;
    // A workspace's name below it, not the mount point itself or a way out.
    let rel = path.strip_prefix(&last).ok()?;
    let names = rel.components().map(|c| match c {
        std::path::Component::Normal(name) => name.to_str(),
        _ => None,
    });
    let branch = names.collect::<Option<Vec<_>>>()?.join("/");
    if branch.starts_with('@') || crate::state::retire::check_name(&branch).is_err() {
        return None;
    }
    let stored = crate::state::StateDir::of(&common_dir, &branch)
        .dir
        .exists();
    stored.then_some((repo, branch))
}
