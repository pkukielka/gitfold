//! gitfold's settings, kept in the repository's git config:
//!
//! - per branch, `branch.<branch>.<key>`, which git keeps with the branch
//!   (renamed and deleted with it): `gitfoldPrewarm` (a workspace's prewarm
//!   mode), `gitfoldTrackReads` (whether it records reads), `gitfoldParent`
//!   (the branch it is built on, a full ref, see `tree`) and `gitfoldFollow`
//!   (whether it takes its parent's new commits by itself; on unless set
//!   false). A parent is a setting of the child: renaming or deleting the
//!   parent with git leaves the child naming the old branch ("parent gone");
//! - per repository, `gitfold.<key>`: `taskDir`, `tasksDir` and `inbox` (the
//!   task folder and its inbox), and `mountpoint` (where `gitfold mount` last
//!   mounted it, for Claude Code's worktree hook to mount it there again).
//!
//! They are read from the repository's own config file, where `git config`
//! writes them (not included files), at the moment they are needed: the
//! daemon's repository handle has the config as it was at mount time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use log::warn;

use crate::git;
use crate::prewarm::Prewarm;

/// `branch.<branch>.gitfoldPrewarm`: none, meta or content.
const PREWARM: &str = "gitfoldPrewarm";
/// `branch.<branch>.gitfoldTrackReads`: a boolean.
const TRACK_READS: &str = "gitfoldTrackReads";
/// `branch.<branch>.gitfoldParent`: the branch it is built on.
const PARENT: &str = "gitfoldParent";
/// `branch.<branch>.gitfoldFollow`: a boolean; unset means on.
const FOLLOW: &str = "gitfoldFollow";
/// `branch.<branch>.gitfoldConflict`: the parent's tip its commits
/// conflicted with, then the paths, one per line.
const CONFLICT: &str = "gitfoldConflict";

/// `gitfold.taskDir`: the task folder's name in a workspace.
const TASK_DIR: &str = "taskDir";
/// `gitfold.tasksDir`: where task folders are kept.
const TASKS_DIR: &str = "tasksDir";
/// `gitfold.inbox`: the inbox's name in a task folder.
const INBOX: &str = "inbox";
/// `gitfold.mountpoint`: where `gitfold mount` last mounted the repository.
const MOUNTPOINT: &str = "mountpoint";
/// The task folder's name unless `gitfold.taskDir` says otherwise.
const DEFAULT_TASK_DIR: &str = ".task";
/// The inbox's name unless `gitfold.inbox` says otherwise.
const DEFAULT_INBOX: &str = "inbox";

/// The repository's own config file, read once: for several settings at a
/// time (a task folder's name and place, a fork's settings).
pub struct Local(Option<gix::config::File>);

impl Local {
    /// The repository's config file as it is now.
    pub fn read(common_dir: &Path) -> Local {
        Local::read_checked(common_dir).unwrap_or(Local(None))
    }

    /// The repository's config file as it is now, failing (naming it) if it
    /// can't be read or parsed: for settings whose absence would be wrong
    /// rather than a default (the tree of parents, whose cycle check an
    /// empty config would silently disable).
    pub fn read_checked(common_dir: &Path) -> Result<Local> {
        let path = common_dir.join("config");
        let file =
            gix::config::File::from_path_no_includes(path.clone(), gix::config::Source::Local)
                .with_context(|| format!("reading {}", path.display()))?;
        Ok(Local(Some(file)))
    }

    /// `gitfold.<key>`, trimmed, if set and not empty.
    fn repository(&self, key: &str) -> Option<String> {
        let value = self.0.as_ref()?.string_by("gitfold", None, key)?;
        non_empty(value.to_string())
    }

    /// The task folder's name in a workspace: `gitfold.taskDir` if it is one
    /// name in the workspace root, not one of its special names; else (warned
    /// about if set) `.task`.
    pub fn task_dir(&self) -> String {
        match self.repository(TASK_DIR) {
            Some(name) if !name.contains('/') && ![".", "..", ".git"].contains(&name.as_str()) => {
                name
            }
            Some(name) => {
                warn!("gitfold.taskDir {name:?} isn't a single name: using {DEFAULT_TASK_DIR}");
                DEFAULT_TASK_DIR.to_string()
            }
            None => DEFAULT_TASK_DIR.to_string(),
        }
    }

    /// Where task folders are kept (`gitfold.tasksDir`, `~` expanded), if set:
    /// absolute, as relative paths would depend on each process's current
    /// directory; another value is warned about and counts as unset.
    pub fn tasks_dir(&self) -> Option<PathBuf> {
        let dir = expand_home(&self.repository(TASKS_DIR)?);
        if !dir.is_absolute() {
            let shown = dir.display();
            warn!("gitfold.tasksDir {shown} isn't absolute: using the default");
            return None;
        }
        Some(dir)
    }

    /// Where `gitfold mount` last mounted the repository
    /// (`gitfold.mountpoint`), if recorded.
    pub fn mountpoint(&self) -> Option<PathBuf> {
        self.repository(MOUNTPOINT).map(PathBuf::from)
    }

    /// The name of a task folder's inbox: `gitfold.inbox` if it is one plain
    /// name (not starting with a dot: files being written); else (warned about
    /// if set) `inbox`.
    pub fn inbox(&self) -> String {
        match self.repository(INBOX) {
            Some(name) if !name.contains('/') && !name.starts_with('.') => name,
            Some(name) => {
                warn!("gitfold.inbox {name:?} isn't a plain name: using {DEFAULT_INBOX}");
                DEFAULT_INBOX.to_string()
            }
            None => DEFAULT_INBOX.to_string(),
        }
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

    /// Every branch's parent (`gitfoldParent`), by branch.
    pub fn parents(&self) -> std::collections::BTreeMap<String, String> {
        let branches = (self.0.as_ref())
            .and_then(|config| config.sections_by_name("branch"))
            .into_iter()
            .flatten()
            .filter_map(|section| Some(section.header().subsection_name()?.to_string()));
        branches
            .filter_map(|branch| Some((branch.clone(), self.branch(&branch, PARENT)?)))
            .collect()
    }

    /// Whether `branch` takes its parent's new commits by itself
    /// (`gitfoldFollow`), if set (`Tree::follows` applies the rules).
    pub fn follow(&self, branch: &str) -> Option<bool> {
        self.branch_bool(branch, FOLLOW)
    }

    /// The conflict recorded for `branch` (`gitfoldConflict`): the parent's
    /// tip its commits conflicted with, and the paths.
    pub fn conflict(&self, branch: &str) -> Option<(String, Vec<String>)> {
        let value = self.branch(branch, CONFLICT)?;
        let mut lines = value.lines().map(String::from);
        Some((lines.next()?, lines.collect()))
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

/// `dir` with a leading `~` as the home directory.
fn expand_home(dir: &str) -> PathBuf {
    let home = || std::env::home_dir().unwrap_or_default();
    match dir.strip_prefix("~") {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => home().join(&rest[1..]),
        _ => PathBuf::from(dir),
    }
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

/// Makes `parent` (a full ref) the parent of `branch` (see `set_branch`);
/// following is `set_follow`'s.
pub fn set_parent(git_dir: &Path, branch: &str, parent: &str) -> Result<()> {
    set_branch(git_dir, branch, PARENT, parent)
}

/// Forgets the parent of `branch`, whether it follows it (F5), and a
/// conflict with it.
pub fn forget_parent(git_dir: &Path, branch: &str) -> Result<()> {
    unset_branch(git_dir, branch, PARENT)?;
    unset_branch(git_dir, branch, FOLLOW)?;
    set_conflict(git_dir, branch, None)
}

/// Records that the commits of `branch` conflict with its parent's tip
/// `at` on `paths`, or with `None` that they don't (any more).
pub fn set_conflict(
    git_dir: &Path,
    branch: &str,
    conflict: Option<(&str, &[String])>,
) -> Result<()> {
    match conflict {
        Some((at, paths)) => {
            let value = std::iter::once(at)
                .chain(paths.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            set_branch(git_dir, branch, CONFLICT, &value)
        }
        None => unset_branch(git_dir, branch, CONFLICT),
    }
}

/// Stores whether `branch` takes its parent's new commits by itself; on is
/// the default, so it is stored as unset.
pub fn set_follow(git_dir: &Path, branch: &str, on: bool) -> Result<()> {
    match on {
        true => unset_branch(git_dir, branch, FOLLOW),
        false => set_branch(git_dir, branch, FOLLOW, "false"),
    }
}

/// Records `mountpoint` as where the repository is mounted
/// (`gitfold.mountpoint`), given its git directory or one of its worktrees'.
pub fn set_mountpoint(git_dir: &Path, mountpoint: &Path) -> Result<()> {
    let key = format!("gitfold.{MOUNTPOINT}");
    let value = mountpoint
        .to_str()
        .context("a mount point that isn't UTF-8")?;
    git::cmd::run(git_dir, &["config", "--local", &key, value])
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

/// Removes `branch.<branch>.<key>` (every value it has), if set.
fn unset_branch(git_dir: &Path, branch: &str, key: &str) -> Result<()> {
    let key = branch_key(branch, key);
    let out = git::cmd::command(git_dir)
        .args(["config", "--local", "--unset-all", &key])
        .output()?;
    // Exit 5: it wasn't set.
    match out.status.code() {
        Some(0 | 5) => Ok(()),
        _ => anyhow::bail!(
            "git config --unset-all {key}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Gives branch `to` the settings set for branch `from` (a fork); one that
/// can't be written is warned about.
pub fn copy_branch(local: &Local, common_dir: &Path, from: &str, to: &str) {
    // Every per-branch setting but the parent: a fork starts at its source,
    // with no relation to it.
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
