//! A workspace's stored state, mounted or not: where it is, and its base.
//!
//! The state of the workspace of branch `b` is `.git/gitfold/<id>/`, `<id>`
//! being `b` with `%` and `/` escaped (`worktree::workspace_id`): `base` (the
//! commit its changes are against, written last when it is made, so its
//! presence means the workspace was opened), `branch`, `upper/`, `markers`,
//! `times`, `reads`, `tmp/`. Its worktree registration (the admin directory,
//! with its index and HEAD) is `.git/worktrees/gitfold-ws-<id>/`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::git::{self, worktree};

/// The file naming a workspace's base commit.
pub const BASE: &str = "base";

/// Where the stored state of one workspace is.
pub struct StateDir {
    /// The branch, escaped (`worktree::workspace_id`).
    pub id: String,
    /// `.git/gitfold/<id>`.
    pub dir: PathBuf,
    /// Its worktree registration, `.git/worktrees/gitfold-ws-<id>`.
    pub admin: PathBuf,
}

impl StateDir {
    /// The state of the workspace of `branch` in repository `common_dir`.
    pub fn of(common_dir: &Path, branch: &str) -> StateDir {
        StateDir::of_id(common_dir, &worktree::workspace_id(branch))
    }

    /// The state of the workspace with id `id`.
    pub fn of_id(common_dir: &Path, id: &str) -> StateDir {
        StateDir {
            id: id.to_string(),
            dir: worktree::state_root(common_dir).join(id),
            admin: worktree::ws_admin_dir(common_dir, id),
        }
    }

    /// Whether the workspace was opened (it has a base).
    pub fn opened(&self) -> bool {
        self.dir.join(BASE).exists()
    }

    /// Its base commit.
    pub fn read_base(&self) -> Result<git::Commit> {
        read_base(&self.dir)
    }
}

/// The base commit of the workspace stored in `dir`.
pub fn read_base(dir: &Path) -> Result<git::Commit> {
    let base_file = dir.join(BASE);
    let hex = fs::read_to_string(&base_file)
        .with_context(|| format!("reading {}", base_file.display()))?;
    git::resolve(hex.trim())
}
