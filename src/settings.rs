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
use clap::ValueEnum;
use log::warn;

use crate::git;
use crate::prewarm::Prewarm;

/// `branch.<branch>.gitfoldPrewarm`: none, meta or content.
const PREWARM: &str = "gitfoldPrewarm";
/// `branch.<branch>.gitfoldTrackReads`: a boolean.
const TRACK_READS: &str = "gitfoldTrackReads";

/// `gitfold.taskDir`: the task folder's name in a workspace.
pub const TASK_DIR: &str = "taskDir";
/// `gitfold.tasksDir`: where task folders are kept.
pub const TASKS_DIR: &str = "tasksDir";
/// `gitfold.inbox`: the inbox's name in a task folder.
pub const INBOX: &str = "inbox";

/// The repository's own config file, read once: for several settings at a
/// time (a task folder's name and place, a fork's settings).
pub struct Local(Option<gix::config::File>);

impl Local {
    /// The repository's config file as it is now.
    pub fn read(common_dir: &Path) -> Local {
        let path = common_dir.join("config");
        Local(gix::config::File::from_path_no_includes(path, gix::config::Source::Local).ok())
    }

    /// `gitfold.<key>`, trimmed, if set and not empty.
    pub fn repository(&self, key: &str) -> Option<String> {
        let value = self.0.as_ref()?.string_by("gitfold", None, key)?;
        non_empty(value.to_string())
    }

    /// The prewarm mode set for the workspace of `branch`, if any; a value
    /// that isn't one is warned about and counts as unset.
    pub fn prewarm(&self, branch: &str) -> Option<Prewarm> {
        let value = self.branch(branch, PREWARM)?;
        Prewarm::from_str(&value, true)
            .inspect_err(|_| {
                let key = branch_key(branch, PREWARM);
                warn!("{key}: {value:?} is not none, meta or content")
            })
            .ok()
    }

    /// Whether the workspace of `branch` records reads, if set (see
    /// `branch_bool`).
    pub fn track_reads(&self, branch: &str) -> Option<bool> {
        self.branch_bool(branch, TRACK_READS)
    }

    /// `branch.<branch>.<key>`, trimmed, if set and not empty.
    fn branch(&self, branch: &str, key: &str) -> Option<String> {
        let config = self.0.as_ref()?;
        let value = config.string_by("branch", Some(branch.as_bytes().into()), key)?;
        non_empty(value.to_string())
    }

    /// `branch.<branch>.<key>` as a boolean, if set; a value that isn't one is
    /// warned about and counts as unset.
    fn branch_bool(&self, branch: &str, key: &str) -> Option<bool> {
        let config = self.0.as_ref()?;
        match config.boolean_by("branch", Some(branch.as_bytes().into()), key) {
            Ok(value) => value,
            Err(_) => {
                warn!("{} is not a boolean", branch_key(branch, key));
                None
            }
        }
    }
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The full name of setting `key` of `branch`: `branch.<branch>.<key>`.
fn branch_key(branch: &str, key: &str) -> String {
    format!("branch.{branch}.{key}")
}

/// `gitfold.<key>` (see `Local::repository`).
pub fn repository(common_dir: &Path, key: &str) -> Option<String> {
    Local::read(common_dir).repository(key)
}

/// The prewarm mode set for the workspace of `branch` (see `Local::prewarm`).
/// Read when it opens, so a change counts from the next open.
pub fn prewarm(common_dir: &Path, branch: &str) -> Option<Prewarm> {
    Local::read(common_dir).prewarm(branch)
}

/// Whether the workspace of `branch` records reads, if set.
pub fn track_reads(common_dir: &Path, branch: &str) -> Option<bool> {
    Local::read(common_dir).track_reads(branch)
}

/// Stores `mode` as the prewarm mode of the workspace of `branch` (see
/// `set_branch`).
pub fn set_prewarm(git_dir: &Path, branch: &str, mode: Prewarm) -> Result<()> {
    set_branch(git_dir, branch, PREWARM, &mode.to_string())
}

/// Stores whether the workspace of `branch` records reads (see
/// `set_branch`).
pub fn set_track_reads(git_dir: &Path, branch: &str, on: bool) -> Result<()> {
    set_branch(git_dir, branch, TRACK_READS, &on.to_string())
}

/// Sets `branch.<branch>.<key>` to `value`, with git, in the repository's
/// own config file (where it is read), given the repository's or one of its
/// worktrees' git directory.
fn set_branch(git_dir: &Path, branch: &str, key: &str, value: &str) -> Result<()> {
    git::cmd::run(
        git_dir,
        &["config", "--local", &branch_key(branch, key), value],
    )
}

/// Gives branch `to` the settings set for branch `from` (a fork); one that
/// can't be written is warned about.
pub fn copy_branch(common_dir: &Path, from: &str, to: &str) {
    let local = Local::read(common_dir);
    // Every per-branch setting.
    let copied = [
        (
            PREWARM,
            local
                .prewarm(from)
                .map(|mode| set_prewarm(common_dir, to, mode)),
        ),
        (
            TRACK_READS,
            local
                .track_reads(from)
                .map(|on| set_track_reads(common_dir, to, on)),
        ),
    ];
    for (key, result) in copied {
        if let Some(Err(e)) = result {
            warn!("copying {} to {to}: {e:#}", branch_key(from, key));
        }
    }
}
