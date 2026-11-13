//! A workspace's stored state, mounted or not: where it is, and its base.
//!
//! The state of the workspace of branch `b` is `.git/gitfold/<id>/`, `<id>`
//! being `b` with `%` and `/` escaped (`workspace_id`): `base` (the
//! commit its changes are against, written last when it is made, so its
//! presence means the workspace was opened), `branch`, `upper/`, `markers`,
//! `times`, `reads`, `tmp/`. Its worktree registration (the admin directory,
//! with its index and HEAD) is `.git/worktrees/gitfold-ws-<id>/`.

pub mod fork;
pub mod markers;
pub mod reads;
pub mod retire;
pub mod status;
pub mod task;
pub mod times;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gix::ObjectId;

use crate::fsutil::write_atomic;
use crate::git::{self, worktree};

/// The file naming a workspace's base commit.
pub const BASE: &str = "base";
/// The file naming its branch (for people: the id is escaped).
pub const BRANCH: &str = "branch";
/// The directory of the files that differ from the base.
pub const UPPER: &str = "upper";
/// The log of deleted paths (whiteouts, opaque directories).
pub const MARKERS: &str = "markers";
/// The timestamps of files dropped from upper.
pub const TIMES: &str = "times";
/// Copy-ups in progress.
pub const TMP: &str = "tmp";

/// Where the stored state of one workspace is.
#[derive(Clone)]
pub struct StateDir {
    /// The branch, escaped (`workspace_id`).
    pub id: String,
    /// `.git/gitfold/<id>`.
    pub dir: PathBuf,
    /// Its worktree registration, `.git/worktrees/gitfold-ws-<id>`.
    pub admin: PathBuf,
}

impl StateDir {
    /// The state of the workspace of `branch` in repository `common_dir`.
    pub fn of(common_dir: &Path, branch: &str) -> StateDir {
        StateDir::of_id(common_dir, &workspace_id(branch))
    }

    /// The state of the workspace with id `id`.
    pub fn of_id(common_dir: &Path, id: &str) -> StateDir {
        StateDir {
            id: id.to_string(),
            dir: state_root(common_dir).join(id),
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

    pub fn upper(&self) -> PathBuf {
        self.dir.join(UPPER)
    }

    pub fn markers(&self) -> PathBuf {
        self.dir.join(MARKERS)
    }

    pub fn times(&self) -> PathBuf {
        self.dir.join(TIMES)
    }

    pub fn tmp(&self) -> PathBuf {
        self.dir.join(TMP)
    }

    /// Its registration's git index.
    pub fn index(&self) -> PathBuf {
        self.admin.join("index")
    }
}

/// `<common>/gitfold`: the workspaces' state directories, by id.
pub fn state_root(common_dir: &Path) -> PathBuf {
    common_dir.join("gitfold")
}

/// A workspace's id, naming its state and registration directories: the branch
/// with `%` and `/` escaped.
pub fn workspace_id(branch: &str) -> String {
    branch.replace('%', "%25").replace('/', "%2F")
}

/// The branch of workspace id `id` (inverse of `workspace_id`).
pub fn workspace_branch(id: &str) -> String {
    id.replace("%2F", "/").replace("%25", "%")
}

/// `dir/name`, for paths below a workspace's root (`dir` empty: the root).
pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// The path prefixes of `path` at `/` boundaries, shortest first, excluding
/// `path`.
pub fn ancestors(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, b)| **b == b'/')
        .map(|(i, _)| &path[..i])
}

/// The base commit of the workspace stored in `dir`.
pub fn read_base(dir: &Path) -> Result<git::Commit> {
    let base_file = dir.join(BASE);
    let hex = fs::read_to_string(&base_file)
        .with_context(|| format!("reading {}", base_file.display()))?;
    git::resolve(hex.trim())
}

/// Records `base` as the base of the workspace stored in `dir`, atomically.
pub fn write_base(dir: &Path, base: &ObjectId) -> io::Result<()> {
    write_atomic(&dir.join(BASE), format!("{base}\n").as_bytes())
}

/// Records `branch` as the branch of the workspace stored in `dir`.
pub fn write_branch(dir: &Path, branch: &str) -> io::Result<()> {
    write_atomic(&dir.join(BRANCH), format!("{branch}\n").as_bytes())
}

/// The branch recorded for the workspace stored in `dir`, if readable.
pub fn read_branch(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(BRANCH)).ok()?;
    Some(text.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids name directories: no `/`, and the branch reads back from its id.
    #[test]
    fn workspace_ids_round_trip() {
        for branch in ["main", "agent/task-17", "a%2Fb", "100%", "x/%25/y"] {
            let id = workspace_id(branch);
            assert!(!id.contains('/'), "{id}");
            assert_eq!(workspace_branch(&id), branch);
        }
        assert_eq!(workspace_id("agent/task-17"), "agent%2Ftask-17");
    }

    /// The base and branch files read back as written.
    #[test]
    fn base_and_branch_round_trip() {
        let dir = std::env::temp_dir().join(format!("gitfold-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let id = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        write_base(&dir, &id).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(BASE)).unwrap(),
            format!("{id}\n")
        );
        write_branch(&dir, "agent/x").unwrap();
        assert_eq!(read_branch(&dir).as_deref(), Some("agent/x"));
        fs::remove_dir_all(dir).unwrap();
    }
}
