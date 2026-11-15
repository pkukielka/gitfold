//! gitfold's settings, kept in the repository's git config:
//!
//! - per branch, `branch.<branch>.<key>`, which git keeps with the branch
//!   (renamed and deleted with it): `gitfoldPrewarm` (a workspace's prewarm
//!   mode), `gitfoldTrackReads` (whether it records reads), and for a
//!   stacked branch `gitfoldParent` (the branch it is built on),
//!   `gitfoldRestack` (`manual`: restacked by `gitfold stack sync` only) and
//!   `gitfoldRestackBlocked` (the restack that conflicted, see
//!   `cli::stack`);
//! - per repository, `gitfold.<key>`: `taskDir`, `tasksDir` and `inbox` (the
//!   task folder and its inbox), `mountpoint` (where `gitfold mount` last
//!   mounted it, for Claude Code's worktree hook to mount it there again),
//!   and `trunk` (where stacked branches' parents land: `origin/main`, say).
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
/// `branch.<branch>.gitfoldParent`: the branch it is stacked on.
const PARENT: &str = "gitfoldParent";
/// `branch.<branch>.gitfoldRestack`: `manual`, or (unset) at turn boundaries.
const RESTACK: &str = "gitfoldRestack";
/// `branch.<branch>.gitfoldRestackBlocked`: `<parent tip> <commit> <paths>`,
/// the restack onto that parent tip that conflicted.
const RESTACK_BLOCKED: &str = "gitfoldRestackBlocked";

/// `gitfold.taskDir`: the task folder's name in a workspace.
const TASK_DIR: &str = "taskDir";
/// `gitfold.tasksDir`: where task folders are kept.
const TASKS_DIR: &str = "tasksDir";
/// `gitfold.inbox`: the inbox's name in a task folder.
const INBOX: &str = "inbox";
/// `gitfold.mountpoint`: where `gitfold mount` last mounted the repository.
const MOUNTPOINT: &str = "mountpoint";
/// `gitfold.trunk`: where the bottom of a stack lands (a branch or a
/// remote-tracking branch).
const TRUNK: &str = "trunk";
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
        let path = common_dir.join("config");
        Local(gix::config::File::from_path_no_includes(path, gix::config::Source::Local).ok())
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

    /// Where the bottom of a stack lands (`gitfold.trunk`), if set.
    pub fn trunk(&self) -> Option<String> {
        self.repository(TRUNK)
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

    /// The branch `branch` is stacked on, if any.
    pub fn parent(&self, branch: &str) -> Option<String> {
        self.branch(branch, PARENT)
    }

    /// Every stacked branch with its parent, in config order.
    pub fn parents(&self) -> Vec<(String, String)> {
        let Some(sections) = self.0.as_ref().and_then(|c| c.sections_by_name("branch")) else {
            return Vec::new();
        };
        sections
            .filter_map(|s| {
                let branch = s.header().subsection_name()?.to_string();
                let parent = non_empty(s.value(PARENT)?.to_string())?;
                Some((branch, parent))
            })
            .collect()
    }

    /// Whether `branch` is restacked only by `gitfold stack sync`
    /// (`gitfoldRestack` is `manual`), not at its turn boundaries.
    pub fn restack_manual(&self, branch: &str) -> bool {
        self.branch(branch, RESTACK).as_deref() == Some("manual")
    }

    /// The restack of `branch` that conflicted, if any (see `RESTACK_BLOCKED`).
    pub fn restack_blocked(&self, branch: &str) -> Option<String> {
        self.branch(branch, RESTACK_BLOCKED)
    }

    /// The remote `branch` pushes to and its branch there, from git's own
    /// `branch.<branch>.remote` and `.merge`, if both are set.
    pub fn upstream(&self, branch: &str) -> Option<(String, String)> {
        let merge = self.branch(branch, "merge")?;
        let theirs = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
        Some((self.branch(branch, "remote")?, theirs.to_string()))
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

/// Stores `parent` as the branch `branch` is stacked on, or forgets it.
pub fn set_parent(git_dir: &Path, branch: &str, parent: Option<&str>) -> Result<()> {
    match parent {
        Some(parent) => set_branch(git_dir, branch, PARENT, parent),
        None => unset_branch(git_dir, branch, PARENT),
    }
}

/// Stores the restack mode of `branch`: `manual` or not (unset).
pub fn set_restack_manual(git_dir: &Path, branch: &str, manual: bool) -> Result<()> {
    match manual {
        true => set_branch(git_dir, branch, RESTACK, "manual"),
        false => unset_branch(git_dir, branch, RESTACK),
    }
}

/// Records the restack of `branch` that conflicted, or forgets it.
pub fn set_restack_blocked(git_dir: &Path, branch: &str, blocked: Option<&str>) -> Result<()> {
    match blocked {
        Some(blocked) => set_branch(git_dir, branch, RESTACK_BLOCKED, blocked),
        None => unset_branch(git_dir, branch, RESTACK_BLOCKED),
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

/// Unsets `branch.<branch>.<key>` (see `set_branch`), if it is set.
fn unset_branch(git_dir: &Path, branch: &str, key: &str) -> Result<()> {
    let key = branch_key(branch, key);
    let status = git::cmd::command(git_dir)
        .args(["config", "--local", "--unset-all", &key])
        .status()?;
    // 5: it wasn't set.
    match status.code() {
        Some(0 | 5) => Ok(()),
        _ => anyhow::bail!("git config --unset-all {key} failed ({status})"),
    }
}

/// Gives branch `to` the settings set for branch `from` (a fork); one that
/// can't be written is warned about.
pub fn copy_branch(local: &Local, common_dir: &Path, from: &str, to: &str) {
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
        // A fork of a stacked branch is built on the same parent.
        (
            PARENT,
            local
                .parent(from)
                .map(|parent| set_parent(common_dir, to, Some(&parent))),
        ),
        (
            RESTACK,
            (local.restack_manual(from)).then(|| set_restack_manual(common_dir, to, true)),
        ),
    ];
    for (key, result) in copied {
        if let Some(Err(e)) = result {
            warn!("copying {} to {to}: {e:#}", branch_key(from, key));
        }
    }
    // And the parent base, if it is in the fork's history (not a ref, but
    // what makes its own commits known rather than guessed).
    if local.parent(from).is_some()
        && let Err(e) = copy_stack_base(common_dir, from, to)
    {
        warn!("copying the parent base of {from} to {to}: {e:#}");
    }
}

/// Points `to`'s stack ref where `from`'s is, if that is in `to`'s history.
fn copy_stack_base(common_dir: &Path, from: &str, to: &str) -> Result<()> {
    use git::stack;
    let (Some(base), Some(tip)) = (
        stack::rev(common_dir, &stack::stack_ref(from))?,
        stack::rev(common_dir, &format!("refs/heads/{to}"))?,
    ) else {
        return Ok(());
    };
    if stack::is_ancestor(common_dir, base, tip)? {
        stack::set_base(common_dir, to, base)?;
    }
    Ok(())
}
