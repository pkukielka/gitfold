//! Claude Code's hooks. A plugin's hooks run in every repository and every
//! session, so each does nothing, or what Claude Code would do without it,
//! where gitfold has no part:
//!
//! - `worktree`: its worktrees as gitfold workspaces (`WorktreeCreate`,
//!   `WorktreeRemove`).
//! - `reads`: the files its Read tool and Bash commands read, for read
//!   tracking (`PreToolUse`).

mod reads;
mod worktree;

use std::path::PathBuf;

use anyhow::Result;

use super::hook::field;

pub use reads::reads_hook;
pub use worktree::{worktree_create, worktree_remove};

/// The hook's working directory: `cwd` in its input, else this process's.
pub(super) fn cwd(input: &str) -> Result<PathBuf> {
    match field(input, "cwd") {
        Some(cwd) => Ok(PathBuf::from(cwd)),
        None => Ok(std::env::current_dir()?),
    }
}
