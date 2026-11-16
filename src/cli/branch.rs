//! `gitfold branch` (a fork with `-w`), and a workspace's settings:
//! `gitfold prewarm`, `gitfold track-reads`.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::builder::BoolishValueParser;

use crate::daemon::control;
use crate::git;
use crate::mount_table;
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
    // Where the user stands, before anything is made (or mounted).
    let sources = (from.iter())
        .map(|rev| Source::resolve(repo, rev))
        .collect::<Result<Vec<_>>>()?;
    let mount = match mount_table::mount_of(common_dir)? {
        Some(mount) => mount,
        None => match settings::Local::read(common_dir).mountpoint() {
            Some(last) => super::mount::mount_again(common_dir, &last)?,
            None => bail!(
                "{} isn't mounted: mount it first (`gitfold mount`); git merges several \
                 --from in the new branch's workspace",
                common_dir.display()
            ),
        },
    };
    fs::metadata(&mount).with_context(|| {
        format!(
            "the gitfold mount at {} doesn't answer (its daemon gone?); `gitfold unmount` it",
            mount.display()
        )
    })?;
    let (first, others) = sources.split_first().expect("several --from");
    // Without an upstream: no relation afterwards.
    git::cmd::output(repo, &["branch", "--no-track", "--", name, &first.id])?;
    let state = StateDir::of(common_dir, name);
    let workspace = mount.join(name);
    // Before the workspace opens: it opens with them.
    let merge = set_branch_settings(repo, name, prewarm, track_reads)
        // Looking it up opens it.
        .and_then(|()| {
            fs::metadata(&workspace)
                .with_context(|| format!("opening workspace {}", workspace.display()))
        })
        .and_then(|_| {
            let revs: Vec<String> = others.iter().map(|s| s.as_in(&state.admin)).collect();
            run_merge(common_dir, &workspace, &revs)
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
            let undone = undo(
                repo,
                common_dir,
                name,
                &first.id,
                &state,
                prewarm,
                track_reads,
            );
            Err(match undone {
                Ok(()) => failed.context(format!("branch {name} not created")),
                Err(e) => failed.context(format!(
                    "branch {name} created, but not removed again ({e:#}): `gitfold remove \
                     --force {name}`, then `git branch -D {name}`"
                )),
            })
        }
    }
}

/// Removes what `merged` made for branch `name` (at `at`): its workspace if
/// it opened, the settings it stored, then the branch (without it, its
/// workspace couldn't be removed).
fn undo(
    repo: &Path,
    common_dir: &Path,
    name: &str,
    at: &str,
    state: &StateDir,
    prewarm: Option<Prewarm>,
    track_reads: Option<bool>,
) -> Result<()> {
    if state.dir.exists() || state.admin.exists() {
        super::remove::remove_once(common_dir, name, true, false)?;
    }
    if prewarm.is_some() || track_reads.is_some() {
        settings::forget_workspace(repo, name)?;
    }
    let branch = format!("refs/heads/{name}");
    git::cmd::output(common_dir, &["update-ref", "-d", &branch, at]).map(drop)
}

/// A `--from` as git takes it where the user stands: as typed, its commit,
/// and the ref it names, shortened as git shortens it (`ui` for `HEAD` on
/// ui), if it names one.
struct Source {
    rev: String,
    id: String,
    name: Option<String>,
}

impl Source {
    /// `rev` in the worktree of git directory `repo`; refused as `git merge`
    /// refuses what isn't a commit.
    fn resolve(repo: &Path, rev: &str) -> Result<Source> {
        let line = |out: Vec<u8>| String::from_utf8_lossy(&out).trim().to_string();
        let commit = format!("{rev}^{{commit}}");
        let id = git::cmd::output(
            repo,
            &["rev-parse", "--verify", "-q", "--end-of-options", &commit],
        )
        .map(line)
        .map_err(|_| anyhow::anyhow!("{rev} - not something we can merge"))?;
        // Names starting with `-` are no refs (`check-ref-format`).
        let name = (!rev.starts_with('-'))
            .then(|| git::cmd::output(repo, &["rev-parse", "--abbrev-ref", rev]).ok())
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
    fn as_in(&self, admin: &Path) -> String {
        let typed = (!self.rev.starts_with('-')).then_some(&self.rev);
        let same = |name: &&String| {
            let commit = format!("{name}^{{commit}}");
            let there = git::cmd::output(admin, &["rev-parse", "--verify", "-q", &commit]);
            there.is_ok_and(|out| String::from_utf8_lossy(&out).trim() == self.id)
        };
        (typed.into_iter().chain(&self.name))
            .find(same)
            .unwrap_or(&self.id)
            .clone()
    }
}

/// Runs `git merge` of `revs` in workspace `workspace`, its messages the
/// user's; whether it made the merge. As in a shell there: without the
/// variables that tie git to another repository (a hook caller's), as `git
/// rev-parse --local-env-vars` lists them, but for the configuration given
/// in the environment (`git -c`, `GIT_CONFIG_COUNT`), which still applies.
fn run_merge(common_dir: &Path, workspace: &Path, revs: &[String]) -> Result<bool> {
    let local = git::cmd::output(common_dir, &["rev-parse", "--local-env-vars"])?;
    let mut git = Command::new("git");
    for var in String::from_utf8_lossy(&local).lines() {
        if !matches!(var, "GIT_CONFIG_PARAMETERS" | "GIT_CONFIG_COUNT") {
            git.env_remove(var);
        }
    }
    let status = (git.current_dir(workspace))
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
