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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

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
/// conflicted with, then the paths (quoted as git quotes them), one per
/// line.
const CONFLICT: &str = "gitfoldConflict";
/// `branch.<branch>.gitfoldMoving`: the outcome of a move under way (see
/// `follow`).
const MOVING: &str = "gitfoldMoving";
/// `branch.<branch>.gitfoldLanding`: what was checked of the landing of its
/// parent (see `follow::landing`).
const LANDING: &str = "gitfoldLanding";

/// `gitfold.taskDir`: the task folder's name in a workspace.
const TASK_DIR: &str = "taskDir";
/// `gitfold.tasksDir`: where task folders are kept.
const TASKS_DIR: &str = "tasksDir";
/// `gitfold.inbox`: the inbox's name in a task folder.
const INBOX: &str = "inbox";
/// `gitfold.mountpoint`: where `gitfold mount` last mounted the repository.
const MOUNTPOINT: &str = "mountpoint";
/// `gitfold.mountOptions`: the flags `gitfold mount` last mounted it with,
/// one per line (`--prewarm=meta`).
const MOUNT_OPTIONS: &str = "mountOptions";
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

    /// The flags `gitfold mount` last mounted the repository with
    /// (`gitfold.mountOptions`), if recorded.
    pub fn mount_options(&self) -> Vec<String> {
        let options = self.repository(MOUNT_OPTIONS).unwrap_or_default();
        options.lines().map(String::from).collect()
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
    /// (`gitfoldFollow`), if set (`Tree::follows` applies the rules); a value
    /// that isn't a boolean an error saying so (warned about by `Tree` only
    /// where that branch's following is asked).
    pub fn follow(&self, branch: &str) -> Result<Option<bool>, String> {
        self.branch_bool_checked(branch, FOLLOW)
    }

    /// The conflict recorded for `branch` (`gitfoldConflict`): the parent's
    /// tip its commits conflicted with, and the paths.
    pub fn conflict(&self, branch: &str) -> Option<(String, Vec<String>)> {
        let value = self.branch(branch, CONFLICT)?;
        let mut lines = value.lines().map(String::from);
        Some((lines.next()?, lines.collect()))
    }

    /// The outcome of a move of `branch` under way, as recorded
    /// (`gitfoldMoving`).
    pub fn moving(&self, branch: &str) -> Option<String> {
        self.branch(branch, MOVING)
    }

    /// What was checked of the landing of the parent of `branch`
    /// (`gitfoldLanding`).
    pub fn landing(&self, branch: &str) -> Option<String> {
        self.branch(branch, LANDING)
    }

    /// `branch.<branch>.<key>`, trimmed, if set and not empty.
    fn branch(&self, branch: &str, key: &str) -> Option<String> {
        let config = self.0.as_ref()?;
        let value = config.string_by("branch", Some(branch.as_bytes().into()), key)?;
        non_empty(value.to_string())
    }

    /// `branch.<branch>.<key>` as a boolean, if set; a value that isn't one is
    /// warned about (`warn_once`) and counts as unset.
    fn branch_bool(&self, branch: &str, key: &str) -> Option<bool> {
        self.branch_bool_checked(branch, key).unwrap_or_else(|e| {
            warn_once(&e);
            None
        })
    }

    /// `branch_bool`, a value that isn't a boolean an error saying so.
    fn branch_bool_checked(&self, branch: &str, key: &str) -> Result<Option<bool>, String> {
        let Some(config) = self.0.as_ref() else {
            return Ok(None);
        };
        (config.boolean_by("branch", Some(branch.as_bytes().into()), key)).map_err(|_| {
            format!(
                "{} is not a boolean (taken as unset)",
                branch_key(branch, key)
            )
        })
    }
}

/// Warns `message` once in this process: the config is read again and again.
pub fn warn_once(message: &str) {
    static WARNED: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if warned.insert(message.to_string()) {
        warn!("{message}");
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

/// Forgets the workspace settings of `branch` asked for: its prewarm mode,
/// its read tracking (a branch made and deleted again by one command).
pub fn forget_workspace(
    git_dir: &Path,
    branch: &str,
    prewarm: bool,
    track_reads: bool,
) -> Result<()> {
    if prewarm {
        unset_branch(git_dir, branch, PREWARM)?;
    }
    if track_reads {
        unset_branch(git_dir, branch, TRACK_READS)?;
    }
    Ok(())
}

/// Makes `parent` (a full ref) the parent of `branch` (see `set_branch`);
/// following is `set_follow`'s. What was checked of the landing of the
/// parent before is forgotten: it was of another parent maybe.
pub fn set_parent(git_dir: &Path, branch: &str, parent: &str) -> Result<()> {
    set_branch(git_dir, branch, PARENT, parent)?;
    unset_branch(git_dir, branch, LANDING)
}

/// Forgets the parent of `branch`, whether it follows it (F5), and a
/// conflict with it.
pub fn forget_parent(git_dir: &Path, branch: &str) -> Result<()> {
    unset_branch(git_dir, branch, PARENT)?;
    unset_branch(git_dir, branch, FOLLOW)?;
    unset_branch(git_dir, branch, LANDING)?;
    set_conflict(git_dir, branch, None)
}

/// Records the outcome of a move of `branch` under way, or with `None`
/// forgets it.
pub fn set_moving(git_dir: &Path, branch: &str, outcome: Option<&str>) -> Result<()> {
    match outcome {
        Some(outcome) => set_branch(git_dir, branch, MOVING, outcome),
        None => unset_branch(git_dir, branch, MOVING),
    }
}

/// Records what was checked of the landing of the parent of `branch`.
pub fn set_landing(git_dir: &Path, branch: &str, checked: &str) -> Result<()> {
    set_branch(git_dir, branch, LANDING, checked)
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
/// (`gitfold.mountpoint`), with `flags`, the `gitfold mount` flags it was
/// mounted with (`gitfold.mountOptions`, one per line), given its git
/// directory or one of its worktrees'.
pub fn set_mountpoint(git_dir: &Path, mountpoint: &Path, flags: &[String]) -> Result<()> {
    let value = mountpoint
        .to_str()
        .context("a mount point that isn't UTF-8");
    // Each written even if the other can't be.
    let write = |key: &str, value: &str| config(git_dir, &[&format!("gitfold.{key}"), value]);
    let place = value.and_then(|value| write(MOUNTPOINT, value));
    write(MOUNT_OPTIONS, &flags.join("\n")).and(place)
}

/// How long writing a setting is retried while it fails (the config locked).
const CONFIG_WAIT: Duration = Duration::from_secs(2);

/// Runs `git config --local <args>`: writes the repository's own config file
/// (where settings are read), given the repository's or one of its
/// worktrees' git directory. Retried a while if git can't write the file:
/// another git command (the user's, the mount's follow thread) may hold its
/// lock.
fn config(git_dir: &Path, args: &[&str]) -> Result<()> {
    let started = Instant::now();
    loop {
        let Err(e) = git_config(git_dir, args) else {
            return Ok(());
        };
        match exit_code(&e) {
            // The file not written (4, git config's exit code) or its lock
            // held (255, what git exits with then): retried; any other
            // failure waiting doesn't change.
            Some(4 | 255) if started.elapsed() < CONFIG_WAIT => {
                sleep(Duration::from_millis(100));
            }
            _ => return Err(e),
        }
    }
}

/// `git config --local <args>` (see `config`), what it printed.
fn git_config(git_dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let args = [&["config", "--local"], args].concat();
    git::cmd::output_of(&mut git::cmd::command(git_dir), &args, None)
}

/// Sets `branch.<branch>.<key>` to `value` (see `config`).
fn set_branch(git_dir: &Path, branch: &str, key: &str, value: &str) -> Result<()> {
    config(git_dir, &[&branch_key(branch, key), value])
}

/// Removes `branch.<branch>.<key>` (every value it has), if set; if not,
/// the config isn't written (git takes its lock even then).
fn unset_branch(git_dir: &Path, branch: &str, key: &str) -> Result<()> {
    let key = branch_key(branch, key);
    // Exit 1: not set.
    if let Err(e) = git_config(git_dir, &["--get-all", &key])
        && exit_code(&e) == Some(1)
    {
        return Ok(());
    }
    match config(git_dir, &["--unset-all", &key]) {
        // Exit 5: unset meanwhile.
        Err(e) if exit_code(&e) == Some(5) => Ok(()),
        done => done,
    }
}

/// The exit code of git, if `e` is git failing (`cmd::Failed`).
fn exit_code(e: &anyhow::Error) -> Option<i32> {
    e.downcast_ref::<git::cmd::Failed>()?.status.code()
}

/// Gives branch `to` the settings set for branch `from` (a fork); those that
/// can't be written, said (what and why).
pub fn copy_branch(local: &Local, common_dir: &Path, from: &str, to: &str) -> Vec<String> {
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
    (copied.into_iter())
        .filter_map(|(key, result)| match result {
            Some(Err(e)) => Some(format!("{} ({e:#})", branch_key(from, key))),
            _ => None,
        })
        .collect()
}
