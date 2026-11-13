//! gitfold's settings, kept in the repository's git config:
//!
//! - per branch, `branch.<branch>.<key>`, which git keeps with the branch
//!   (renamed and deleted with it): `gitfoldPrewarm` (a workspace's prewarm
//!   mode) and `gitfoldTrackReads` (whether it records reads);
//! - per repository, `gitfold.<key>`: `taskDir`, `tasksDir` and `inbox` (the
//!   task folder and its inbox).
//!
//! They are read from the repository's own config file, where `git config`
//! writes them (not included files), at the moment they are needed: the
//! daemon's repository handle has the config as it was at mount time.

use std::path::Path;

use anyhow::Result;
use log::warn;

use crate::git;

/// `branch.<branch>.gitfoldPrewarm`: none, meta or content.
pub const PREWARM: &str = "gitfoldPrewarm";
/// `branch.<branch>.gitfoldTrackReads`: a boolean.
pub const TRACK_READS: &str = "gitfoldTrackReads";
/// Every per-branch setting (a fork copies them all).
const BRANCH_KEYS: [&str; 2] = [PREWARM, TRACK_READS];

/// `gitfold.taskDir`: the task folder's name in a workspace.
pub const TASK_DIR: &str = "taskDir";
/// `gitfold.tasksDir`: where task folders are kept.
pub const TASKS_DIR: &str = "tasksDir";
/// `gitfold.inbox`: the inbox's name in a task folder.
pub const INBOX: &str = "inbox";

/// The repository's own config file, read now.
fn local(common_dir: &Path) -> Option<gix::config::File> {
    let path = common_dir.join("config");
    gix::config::File::from_path_no_includes(path, gix::config::Source::Local).ok()
}

/// `gitfold.<key>`, trimmed, if set and not empty.
pub fn repository(common_dir: &Path, key: &str) -> Option<String> {
    let value = local(common_dir)?
        .string_by("gitfold", None, key)?
        .to_string();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `branch.<branch>.<key>`, trimmed, if set.
pub fn branch(common_dir: &Path, branch: &str, key: &str) -> Option<String> {
    let config = local(common_dir)?;
    let value = config.string_by("branch", Some(branch.as_bytes().into()), key)?;
    Some(value.to_string().trim().to_string())
}

/// `branch.<branch>.<key>` as a boolean, if set; a value that isn't one is
/// warned about and counts as unset.
pub fn branch_bool(common_dir: &Path, branch: &str, key: &str) -> Option<bool> {
    let config = local(common_dir)?;
    match config.boolean_by("branch", Some(branch.as_bytes().into()), key) {
        Ok(value) => value,
        Err(_) => {
            warn!("branch.{branch}.{key} is not a boolean");
            None
        }
    }
}

/// Sets `branch.<branch>.<key>` to `value`, with git, in repository (or
/// worktree) `git_dir`.
pub fn set_branch(git_dir: &Path, branch: &str, key: &str, value: &str) -> Result<()> {
    git::cmd::run(
        git_dir,
        &["config", &format!("branch.{branch}.{key}"), value],
    )
}

/// Gives branch `to` the settings set for branch `from` (a fork).
pub fn copy_branch(common_dir: &Path, from: &str, to: &str) {
    for key in BRANCH_KEYS {
        if let Some(value) = branch(common_dir, from, key).filter(|v| !v.is_empty()) {
            let to_key = format!("branch.{to}.{key}");
            let _ = git::cmd::run(common_dir, &["config", "--local", &to_key, &value]);
        }
    }
}
