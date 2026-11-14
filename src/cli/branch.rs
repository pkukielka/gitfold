//! `gitfold branch` (a fork with `-w`), and a workspace's settings:
//! `gitfold prewarm`, `gitfold track-reads`.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use clap::builder::BoolishValueParser;

use crate::daemon::control;
use crate::git;
use crate::prewarm::Prewarm;
use crate::refusal::Refusal;
use crate::settings;
use crate::state::StateDir;
use crate::state::fork;
use crate::state::retire::check_name;

/// `gitfold branch`: creates a branch to open as a workspace.
pub fn branch(repo: &Path, args: BranchArgs) -> Result<()> {
    let BranchArgs {
        branch: name,
        with_changes,
        from,
        prewarm,
        track_reads,
        claude_hook: _,
    } = args;
    let name = name.as_deref().context("name the branch")?;
    let common_dir = git::init(repo)?;
    check_new(&common_dir, name)?;
    if with_changes {
        // `git branch` checks names itself; `update-ref` would take `HEAD`, and
        // refuse other bad names only after the copy.
        let valid = git::cmd::output(&common_dir, &["check-ref-format", "--branch", name]);
        ensure!(valid.is_ok(), "{name:?} is not a valid branch name");
        // A workspace to copy: `from`, or the one we are in.
        let source = super::branch_or_current(from, repo).map_err(|_| {
            anyhow::anyhow!("not in a gitfold workspace: name the source with --from")
        })?;
        check_name(&source)?;
        if !git::branch_exists(&source) {
            let message = format!("--with-changes copies a branch's workspace: no branch {source}");
            return Err(Refusal::NotFound.error(message));
        }
        let request = control::Request::Fork {
            from: source.clone(),
            to: name.to_string(),
        };
        let done = super::local_or_daemon(&common_dir, &request, "branch --with-changes", || {
            fork::fork(&common_dir, &source, name, None, |dst| {
                fork::copy_stored(&common_dir, &source, dst)
            })
        })?;
        set_branch_settings(repo, name, prewarm, track_reads)?;
        println!("{done}; open it as <mountpoint>/{name}");
        return Ok(());
    }
    create(repo, name, from.as_deref())?;
    set_branch_settings(repo, name, prewarm, track_reads)?;
    println!("created branch {name}; open it as <mountpoint>/{name}");
    Ok(())
}

/// Refuses branch name `name` for a new workspace in the repository of
/// `common_dir` (opened): one starting with `@` (a snapshot's), or one whose
/// earlier branch's workspace is still stored (it would be resumed).
pub fn check_new(common_dir: &Path, name: &str) -> Result<()> {
    ensure!(
        !name.starts_with('@'),
        "branch names starting with @ can't be opened as workspaces (@<rev> is a snapshot)"
    );
    ensure!(
        git::branch_exists(name) || !StateDir::of(common_dir, name).dir.exists(),
        "a workspace of an earlier branch {name} is still stored; `gitfold remove` it first"
    );
    Ok(())
}

/// Creates branch `name` at `from`, or at HEAD of the worktree of `repo` (its
/// git directory), as `git branch` does; `check_new` first.
pub fn create(repo: &Path, name: &str, from: Option<&str>) -> Result<()> {
    let mut args = vec!["branch", "--", name];
    args.extend(from);
    git::cmd::output(repo, &args).map(drop)
}

/// `gitfold prewarm`: sets the prewarm mode of workspace `name` (as `branch
/// --prewarm`) and, while mounted, has the daemon apply it at once.
pub fn prewarm(repo: &Path, args: PrewarmArgs) -> Result<()> {
    let PrewarmArgs { branch: name, mode } = args;
    let common_dir = git::init(repo)?;
    git::require_branch(&name)?;
    settings::set_prewarm(repo, &name, mode)?;
    let unmounted = match mode {
        Prewarm::None => format!("set; {name} isn't walked"),
        _ => format!("set; {name} is walked when opened"),
    };
    let older = format!("set; the mounted gitfold is older: {name} is walked when it opens next");
    apply(
        &common_dir,
        control::Request::Prewarm { branch: name },
        &unmounted,
        &older,
    )
}

/// Has the daemon apply a setting just stored, with `request`, and says so;
/// `unmounted` and `older` say what happens when nothing is mounted or the
/// mounted gitfold is too old to apply it.
fn apply(common_dir: &Path, request: control::Request, unmounted: &str, older: &str) -> Result<()> {
    match control::request(common_dir, &request, None) {
        Ok(Some(done)) => println!("{done}"),
        Ok(None) => println!("{unmounted}"),
        Err(e) if control::is_older_daemon(&e) => println!("{older}"),
        Err(e) => return Err(e.context("set, but not applied to the mount")),
    }
    Ok(())
}

/// Stores the workspace settings `gitfold branch` was given for `name`.
fn set_branch_settings(
    repo: &Path,
    name: &str,
    prewarm: Option<Prewarm>,
    track_reads: Option<bool>,
) -> Result<()> {
    if let Some(mode) = prewarm {
        settings::set_prewarm(repo, name, mode)?;
    }
    if let Some(on) = track_reads {
        settings::set_track_reads(repo, name, on)?;
    }
    Ok(())
}

/// `gitfold track-reads`: stores whether branch `name`'s workspace records
/// reads, and applies it at once if mounted.
pub fn track_reads(repo: &Path, args: TrackReadsArgs) -> Result<()> {
    let TrackReadsArgs {
        branch: name,
        on,
        claude_hook: _,
    } = args;
    let (Some(name), Some(on)) = (name, on) else {
        anyhow::bail!("name the branch, and on or off");
    };
    let common_dir = git::init(repo)?;
    git::require_branch(&name)?;
    settings::set_track_reads(repo, &name, on)?;
    let unmounted = format!("set; applied when {name} opens");
    let older = format!("set; the mounted gitfold is older: applied when {name} opens next");
    apply(
        &common_dir,
        control::Request::TrackReads { branch: name },
        &unmounted,
        &older,
    )
}

/// The arguments of `gitfold branch`.
#[derive(clap::Args)]
pub struct BranchArgs {
    #[arg(required_unless_present = "claude_hook")]
    pub branch: Option<String>,
    /// Copy the uncommitted changes of the workspace --from names, or of the
    /// one you are in.
    #[arg(short = 'w', long)]
    pub with_changes: bool,
    /// Start point (default: HEAD of the worktree you are in, as for `git
    /// branch`).
    #[arg(long)]
    pub from: Option<String>,
    /// Prewarm mode for this workspace, kept across remounts (in git
    /// config, branch.<BRANCH>.gitfoldPrewarm); a mount's `--prewarm none`
    /// still wins.
    #[arg(long, value_enum)]
    pub prewarm: Option<Prewarm>,
    /// Record the files read in this workspace, for `gitfold overlaps`, or
    /// not, whatever the mount's --track-reads says (in git config,
    /// branch.<BRANCH>.gitfoldTrackReads).
    #[arg(long, value_name = "on|off", value_parser = BoolishValueParser::new())]
    pub track_reads: Option<bool>,
    /// As Claude Code's `WorktreeCreate` hook (`worktree-create`): its
    /// worktree a workspace of branch `worktree-<name>`, the path printed; a
    /// git worktree in a repository not used with gitfold. See
    /// skills/gitfold/SKILL.md.
    #[arg(long, value_enum, value_name = "EVENT", exclusive = true)]
    pub claude_hook: Option<CreateHook>,
}

/// The Claude Code hook `gitfold branch` serves.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CreateHook {
    WorktreeCreate,
}

/// The arguments of `gitfold prewarm`.
#[derive(clap::Args)]
pub struct PrewarmArgs {
    pub branch: String,
    #[arg(value_enum)]
    pub mode: Prewarm,
}

/// The arguments of `gitfold track-reads`.
#[derive(clap::Args)]
pub struct TrackReadsArgs {
    #[arg(required_unless_present = "claude_hook")]
    pub branch: Option<String>,
    #[arg(value_name = "on|off", value_parser = BoolishValueParser::new(), action = clap::ArgAction::Set, required_unless_present = "claude_hook")]
    pub on: Option<bool>,
    /// As Claude Code's `PreToolUse` hook on Read and Bash (`pre-tool-use`):
    /// records the files they read in a tracked workspace, which gitfold
    /// can't see when the kernel serves them from its cache. See
    /// skills/gitfold/SKILL.md.
    #[arg(long, value_enum, value_name = "EVENT", exclusive = true)]
    pub claude_hook: Option<ReadsHook>,
}

/// The Claude Code hook `gitfold track-reads` serves.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ReadsHook {
    PreToolUse,
}
