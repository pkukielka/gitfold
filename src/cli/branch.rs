//! `gitfold branch` (a fork with `-w`), and a workspace's settings:
//! `gitfold prewarm`, `gitfold track-reads`.

use std::path::Path;

use anyhow::{Result, ensure};

use crate::daemon::control;
use crate::daemon::prewarm::Prewarm;
use crate::git;
use crate::refusal::Refusal;
use crate::settings;
use crate::state::StateDir;
use crate::state::fork;
use crate::state::retire::check_name;

/// `gitfold branch`: creates a branch to open as a workspace.
pub fn branch(
    repo: &Path,
    name: &str,
    from: Option<&str>,
    prewarm: Option<Prewarm>,
    track_reads: Option<bool>,
    with_changes: bool,
) -> Result<()> {
    // Mount names starting with `@` are snapshots; such a branch would be
    // unreachable as a workspace.
    ensure!(
        !name.starts_with('@'),
        "branch names starting with @ can't be opened as workspaces (@<rev> is a snapshot)"
    );
    // A workspace left from a deleted branch of that name would be resumed.
    let common_dir = git::init(repo)?;
    ensure!(
        git::branch_exists(name) || !StateDir::of(&common_dir, name).dir.exists(),
        "a workspace of an earlier branch {name} is still stored; `gitfold remove` it first"
    );
    if with_changes {
        // `git branch` checks names itself; `update-ref` would take `HEAD`, and
        // refuse other bad names only after the copy.
        let valid = git::cmd::output(&common_dir, &["check-ref-format", "--branch", name]);
        ensure!(valid.is_ok(), "{name:?} is not a valid branch name");
        // A workspace to copy: `from`, or the one we are in.
        let source = super::branch_or_current(from.map(str::to_string), repo).map_err(|_| {
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
    let mut args = vec!["branch", "--", name];
    args.extend(from);
    git::cmd::run(repo, &args)?;
    set_branch_settings(repo, name, prewarm, track_reads)?;
    println!("created branch {name}; open it as <mountpoint>/{name}");
    Ok(())
}

/// `gitfold prewarm`: sets the prewarm mode of workspace `name` (as `branch
/// --prewarm`) and, while mounted, has the daemon apply it at once.
pub fn prewarm(repo: &Path, name: &str, mode: Prewarm) -> Result<()> {
    let common_dir = git::init(repo)?;
    git::require_branch(name)?;
    set_prewarm(repo, name, mode)?;
    let request = control::Request::Prewarm {
        branch: name.to_string(),
    };
    match control::request(&common_dir, &request, None) {
        Ok(Some(done)) => println!("{done}"),
        Ok(None) if mode == Prewarm::None => println!("set; {name} isn't walked"),
        Ok(None) => println!("set; {name} is walked when opened"),
        Err(e) if control::is_older_daemon(&e) => {
            println!("set; the mounted gitfold is older: {name} is walked when it opens next")
        }
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
        set_prewarm(repo, name, mode)?;
    }
    if let Some(on) = track_reads {
        set_track_reads(repo, name, on)?;
    }
    Ok(())
}

/// `gitfold track-reads`: stores whether branch `name`'s workspace records
/// reads, and applies it at once if mounted.
pub fn track_reads(repo: &Path, name: &str, on: bool) -> Result<()> {
    let common_dir = git::init(repo)?;
    git::require_branch(name)?;
    set_track_reads(repo, name, on)?;
    let request = control::Request::TrackReads {
        branch: name.to_string(),
    };
    match control::request(&common_dir, &request, None) {
        Ok(Some(done)) => println!("{done}"),
        Ok(None) => println!("set; applied when {name} opens"),
        Err(e) if control::is_older_daemon(&e) => {
            println!("set; the mounted gitfold is older: applied when {name} opens next")
        }
        Err(e) => return Err(e.context("set, but not applied to the mount")),
    }
    Ok(())
}

/// Stores whether branch `name`'s workspace records reads, in git config.
fn set_track_reads(repo: &Path, name: &str, on: bool) -> Result<()> {
    let value = &on.to_string();
    settings::set_branch(repo, name, settings::TRACK_READS, value)
}

/// Stores `mode` as branch `name`'s prewarm mode, in git config.
fn set_prewarm(repo: &Path, name: &str, mode: Prewarm) -> Result<()> {
    settings::set_branch(repo, name, settings::PREWARM, &mode.to_string())
}

/// The arguments of `gitfold branch` (see `main`).
#[derive(clap::Args)]
pub struct BranchArgs {
    pub branch: String,
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
    pub prewarm: Option<crate::daemon::prewarm::Prewarm>,
    /// Record the files read in this workspace, for `gitfold overlaps`, or
    /// not, whatever the mount's --track-reads says (in git config,
    /// branch.<BRANCH>.gitfoldTrackReads).
    #[arg(long, value_name = "on|off", value_parser = clap::builder::BoolishValueParser::new())]
    pub track_reads: Option<bool>,
}
