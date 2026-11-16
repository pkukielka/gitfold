//! `gitfold branch` (a fork with `-w`), and a workspace's settings:
//! `gitfold prewarm`, `gitfold track-reads`.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
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
        parent,
        no_follow,
        prewarm,
        track_reads,
        claude_hook: _,
        on: _,
        merge: _,
    } = args;
    let name = name.as_deref().context("name the branch")?;
    if let Some(parent) = &parent {
        // N5: created at P's tip, then built on P. With no upstream from P
        // (`--no-track`, whatever `branch.autoSetupMerge` says): the parent
        // isn't git's upstream (spec, section 2); pull and push stay the
        // branch's own.
        let full = super::tree::check_new_parent(repo, name, parent)?;
        git::cmd::output(repo, &["branch", "--no-track", "--", name, &full])?;
        // Following set either way: a deleted branch's settings may be left
        // (T2). Without its settings the branch goes again, so the command
        // can be retried.
        let set = settings::set_parent(repo, name, &full)
            .and_then(|()| settings::set_follow(repo, name, !no_follow))
            .and_then(|()| set_branch_settings(repo, name, prewarm, track_reads));
        if let Err(e) = set {
            if let Err(undo) = git::cmd::output(repo, &["branch", "-D", "--", name]) {
                log::warn!("deleting branch {name} again: {undo:#}");
            }
            return Err(e);
        }
        println!(
            "created branch {name} on {} ({}); open it as <mountpoint>/{name}",
            crate::tree::short(&full),
            super::tree::following(!no_follow)
        );
        return Ok(());
    }
    if from.len() > 1 && with_changes {
        return Err(super::tree::not_implemented(
            "gitfold branch --with-changes with several --from",
        ));
    }
    let common_dir = git::init(repo)?;
    check_new(&common_dir, name)?;
    if from.len() > 1 {
        return merged(repo, &common_dir, name, &from, prewarm, track_reads);
    }
    let from = from.into_iter().next();
    if with_changes {
        // `git branch` checks names itself; `update-ref` would take `HEAD`, and
        // refuse other bad names only after the copy.
        check_branch_name(&common_dir, name)?;
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

/// `gitfold branch NEW --from A --from B …`: NEW at A, then `git merge B …`
/// run in NEW's workspace, through the mount, so git merges as it does
/// (strategy, octopus, message, `merge.ff`, `merge.log`, signing, hooks),
/// saying so itself. A conflict leaves NEW with git's merge in progress
/// (exit 1); git refusing leaves nothing: NEW's workspace and branch go
/// again. Unmounted, the repository is mounted again where it last was;
/// never mounted, refused. No relation to the sources afterwards.
fn merged(
    repo: &Path,
    common_dir: &Path,
    name: &str,
    from: &[String],
    prewarm: Option<Prewarm>,
    track_reads: Option<bool>,
) -> Result<()> {
    check_branch_name(common_dir, name)?;
    ensure!(
        !git::branch_exists(name),
        "a branch named '{name}' already exists"
    );
    // Where the user stands, before anything is made (or mounted). `-` is
    // the previous branch, as for `git merge -`, except as the start point.
    // Only the merged ones go to git by name.
    let sources = (from.iter().enumerate())
        .map(|(i, rev)| {
            let rev = if rev == "-" && i > 0 { "@{-1}" } else { rev };
            Source::resolve(repo, rev, i > 0)
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(mount) = super::mount::mount_or_again(common_dir)? else {
        bail!(
            "{} isn't mounted: mount it first (`gitfold mount`); git merges several --from \
             in the new branch's workspace",
            common_dir.display()
        )
    };
    let (first, others) = sources.split_first().expect("several --from");
    // Without an upstream: no relation afterwards.
    git::cmd::output(repo, &["branch", "--no-track", "--", name, &first.id])?;
    let made = Made {
        name,
        at: &first.id,
        prewarm: prewarm.is_some(),
        track_reads: track_reads.is_some(),
    };
    let state = StateDir::of(common_dir, name);
    let workspace = mount.join(name);
    // Before the workspace opens: it opens with them.
    let merge = set_branch_settings(repo, name, prewarm, track_reads)
        // Looking it up opens it.
        .and_then(|()| {
            let meta = fs::metadata(&workspace)
                .with_context(|| format!("opening workspace {}", workspace.display()))?;
            ensure!(meta.is_dir(), "{} is not a workspace", workspace.display());
            Ok(())
        })
        .and_then(|()| {
            let revs = (others.iter())
                .map(|s| s.as_in(&state.admin))
                .collect::<Result<Vec<_>>>()?;
            run_merge(&workspace, &revs)
        });
    match merge {
        Ok(true) => {
            println!("created branch {name}; open it as {}", workspace.display());
            Ok(())
        }
        // Stopped by a conflict, or by a hook or an empty message: git says.
        Ok(false) if state.admin.join("MERGE_HEAD").is_file() => bail!(
            "created branch {name} with its merge in progress (see above): finish it in {} \
             with `git merge --continue`, or `git merge --abort`",
            workspace.display()
        ),
        failed => {
            let failed = failed
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("git merge failed"));
            Err(match made.undo(repo, common_dir, &state) {
                Ok(()) => failed.context(format!("branch {name} not created")),
                Err(e) => failed.context(format!("branch {name} {e:#}")),
            })
        }
    }
}

/// What `merged` made: branch `name` at `at`, and which settings it stored.
struct Made<'a> {
    name: &'a str,
    at: &'a str,
    prewarm: bool,
    track_reads: bool,
}

impl Made<'_> {
    /// Removes it again: the workspace if it opened (retried while busy, as
    /// `gitfold remove --wait`), the settings it stored, then the branch
    /// (without it, its workspace couldn't be removed). An error says what is
    /// left, and how to remove it.
    fn undo(&self, repo: &Path, common_dir: &Path, state: &StateDir) -> Result<()> {
        let name = self.name;
        if state.dir.exists() || state.admin.exists() {
            super::remove::remove_waiting(common_dir, name, true, false, UNDO_WAIT).with_context(
                || {
                    format!(
                        "created, but not removed again: `gitfold remove --force {name}`, then \
                         `git branch -D {name}`"
                    )
                },
            )?;
        }
        let branch_left = || format!("created, but not deleted again: `git branch -D {name}`");
        settings::forget_workspace(repo, name, self.prewarm, self.track_reads)
            .with_context(branch_left)?;
        let branch = format!("refs/heads/{name}");
        git::cmd::output(common_dir, &["update-ref", "-d", &branch, self.at])
            .with_context(branch_left)
            .map(drop)
    }
}

/// How long removing a new workspace again waits while it is busy (git's
/// processes still in it).
const UNDO_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A `--from` as git takes it where the user stands: as typed, its commit,
/// and the ref it names, shortened as git shortens it (`ui` for `HEAD` on
/// ui), if it names one.
struct Source {
    rev: String,
    id: String,
    name: Option<String>,
}

impl Source {
    /// `rev` in the worktree of git directory `repo`, and if `named`, the
    /// ref it names; refused as `git merge` refuses what isn't a commit;
    /// git's own error if it fails otherwise.
    fn resolve(repo: &Path, rev: &str, named: bool) -> Result<Source> {
        let line = |out: Vec<u8>| String::from_utf8_lossy(&out).trim().to_string();
        let commit = format!("{rev}^{{commit}}");
        let args = ["rev-parse", "--verify", "-q", "--end-of-options", &commit];
        let out = git::cmd::command(repo).args(args).output()?;
        // `--verify -q`: exit 1 for what names no commit (anything else
        // that fails dies, 128).
        if out.status.code() == Some(1) {
            // What git said first (an ambiguous short id's candidates), as
            // `git merge` shows it.
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            bail!("{rev} - not something we can merge");
        }
        ensure!(
            out.status.success(),
            "git rev-parse {commit} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let id = line(out.stdout);
        // Names starting with `-` are no refs (`check-ref-format`).
        let args = ["rev-parse", "--verify", "-q", "--abbrev-ref", rev];
        let name = (named && !rev.starts_with('-'))
            .then(|| git::cmd::output(repo, &args).ok())
            .flatten()
            .map(line)
            .filter(|n| !n.is_empty() && n != "HEAD");
        Ok(Source {
            rev: rev.to_string(),
            id,
            name,
        })
    }

    /// What to give `git merge` in the worktree of git directory `admin`:
    /// the revision as typed, else the ref it named, if git takes it for
    /// the same commit there (so git's message names it as the user did);
    /// else the commit.
    fn as_in(&self, admin: &Path) -> Result<String> {
        let typed = (!self.rev.starts_with('-')).then_some(&self.rev);
        for name in typed.into_iter().chain(&self.name) {
            let commit = format!("{name}^{{commit}}");
            let args = ["rev-parse", "--verify", "-q", &commit];
            let there = git::cmd::command_in_other(admin)?.args(args).output()?;
            if there.status.success() && String::from_utf8_lossy(&there.stdout).trim() == self.id {
                return Ok(name.clone());
            }
        }
        Ok(self.id.clone())
    }
}

/// Runs `git merge` of `revs` in workspace `workspace`, its messages the
/// user's, as in a shell there (`git::cmd::in_other_worktree`); whether it
/// made the merge.
fn run_merge(workspace: &Path, revs: &[String]) -> Result<bool> {
    let mut git = Command::new("git");
    let status = (git::cmd::in_other_worktree(&mut git)?)
        .current_dir(workspace)
        .args(["merge", "--end-of-options"])
        .args(revs)
        .status()
        .context("running git merge")?;
    Ok(status.success())
}

/// Refuses `name` if git doesn't take it as a branch name, as `git branch`
/// does before anything else.
pub fn check_branch_name(common_dir: &Path, name: &str) -> Result<()> {
    let valid = git::cmd::output(common_dir, &["check-ref-format", "--branch", name]);
    ensure!(valid.is_ok(), "{name:?} is not a valid branch name");
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
    /// Copy the uncommitted changes of the workspaces --from names (each
    /// one's, with several), or of the one you are in.
    #[arg(short = 'w', long)]
    pub with_changes: bool,
    /// Start point (default: HEAD of the worktree you are in, as for `git
    /// branch`). Repeated: the branch starts at the revisions merged (with
    /// -w, each one's uncommitted changes too); conflicts are left in the
    /// files, as a merge in progress. No relation to them afterwards.
    #[arg(long, value_name = "REV", conflicts_with = "parent")]
    pub from: Vec<String>,
    /// Build the branch on branch P (its parent): it starts at P's tip, and
    /// takes P's new commits from then on (see `gitfold parent`).
    #[arg(long, value_name = "P", conflicts_with = "with_changes")]
    pub parent: Option<String>,
    /// With --parent: take P's new commits only when asked (`gitfold parent
    /// --update`).
    #[arg(long, requires = "parent")]
    pub no_follow: bool,
    /// Not a flag: `--parent` (a hint says so).
    #[arg(long, hide = true, num_args = 0.., value_name = "P")]
    pub on: Option<Vec<String>>,
    /// Not a flag: repeated `--from` (a hint says so).
    #[arg(long, hide = true, num_args = 0.., value_name = "REV")]
    pub merge: Option<Vec<String>>,
    /// Prewarm mode for this workspace, kept across remounts (in git
    /// config, branch.<BRANCH>.gitfoldPrewarm); a mount's `--prewarm none`
    /// still wins.
    #[arg(long, value_enum)]
    pub prewarm: Option<Prewarm>,
    /// Record the files Claude Code reads in this workspace (its hook reports
    /// them), for `gitfold overlaps`, or not, whatever the mount's
    /// --track-reads says (in git config, branch.<BRANCH>.gitfoldTrackReads).
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
