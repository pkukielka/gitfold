//! The commands run from the shell, not by the daemon.
//!
//! - `inbox`: `watch`, `inbox`, `send`: messages and watching on task folders.
//! - `manage`: `branch`, `list`, `changes`, `remove`, `prune`, `prewarm`.
//! - `mount`: `mount` in the background, `unmount`, `version`.
//! - `overlaps`: what a workspace read or changed that changed elsewhere since.

pub mod inbox;
pub mod manage;
pub mod mount;
pub mod overlaps;

use std::path::Path;

use anyhow::{Result, bail};

/// `branch` if given, else the branch of the workspace the current directory is
/// in: `repo`, the git directory found from it (`git::discover`), is then the
/// workspace's registration, `<common>/worktrees/gitfold-ws-<id>`.
pub fn branch_or_current(branch: Option<String>, repo: &Path) -> Result<String> {
    if let Some(branch) = branch {
        return Ok(branch);
    }
    let in_worktrees = repo.parent().and_then(Path::file_name) == Some("worktrees".as_ref());
    let id = repo
        .file_name()
        .and_then(|n| n.to_str()?.strip_prefix("gitfold-ws-"));
    match id {
        Some(id) if in_worktrees => Ok(crate::git::worktree::workspace_branch(id)),
        _ => bail!("not in a gitfold workspace: name the branch"),
    }
}

/// Runs `local` under the repository lock if no daemon holds it (unmounted),
/// else sends `request` (named `what` in errors) to the daemon, which does the
/// same while mounted. See ARCHITECTURE.md, "Commands on a workspace's stored
/// state".
pub fn local_or_daemon(
    common_dir: &Path,
    request: &crate::daemon::control::Request,
    what: &str,
    local: impl FnOnce() -> Result<String>,
) -> Result<String> {
    match crate::git::lock::lock(common_dir)? {
        Some(_lock) => local(),
        None => match crate::daemon::control::request(common_dir, request, None)? {
            Some(done) => Ok(done),
            None => Err(crate::refusal::Refusal::InUse.error(format!(
                "{} is locked by another gitfold command, or by a gitfold daemon that doesn't \
                 take {what} requests (an older gitfold, or one starting or exiting); retry, \
                 or remount it",
                common_dir.display()
            ))),
        },
    }
}
