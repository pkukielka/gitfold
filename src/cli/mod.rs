//! The commands run from the shell, not by the daemon.
//!
//! - `manage`: `branch`, `list`, `changes`, `remove`, `prune`, `prewarm`.
//! - `mount`: `mount` in the background, `unmount`, `version`.

pub mod manage;
pub mod mount;

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
