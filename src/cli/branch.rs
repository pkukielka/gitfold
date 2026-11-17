//! `gitfold branch` (a fork with `-w`), and a workspace's settings:
//! `gitfold prewarm`, `gitfold track-reads`.

use std::fs;
use std::path::Path;

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
        // Its own commits start at P's tip.
        let set = git::branch_tip(name)
            .and_then(|tip| crate::follow::set_base(repo, name, tip.id))
            .and_then(|()| settings::set_parent(repo, name, &full))
            .and_then(|()| settings::set_follow(repo, name, !no_follow))
            .and_then(|()| set_branch_settings(repo, name, prewarm, track_reads));
        if let Err(e) = set {
            let undone = git::cmd::output(repo, &["branch", "-D", "--", name])
                .and_then(|_| crate::follow::drop_base(repo, name));
            if let Err(undo) = undone {
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
    let common_dir = git::init(repo)?;
    check_new(&common_dir, name)?;
    if from.len() > 1 {
        return merged(
            repo,
            &common_dir,
            name,
            &from,
            with_changes,
            prewarm,
            track_reads,
        );
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
///
/// `with_changes`: the uncommitted changes of the sources that are local
/// branches with a workspace too (`Changes`), taken before NEW is made, and
/// applied after the merge, unstaged; a merge that doesn't complete is then
/// refused (undone), whatever stopped it.
fn merged(
    repo: &Path,
    common_dir: &Path,
    name: &str,
    from: &[String],
    with_changes: bool,
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
    let sources = (from.iter().enumerate())
        .map(|(i, rev)| {
            let rev = if rev == "-" && i > 0 { "@{-1}" } else { rev };
            Source::resolve(repo, rev, i > 0, with_changes)
        })
        .collect::<Result<Vec<_>>>()?;
    // The workspaces whose changes are taken, checked before mounting too.
    let copied = match with_changes {
        true => Changes::workspaces(common_dir, &sources)?,
        false => Vec::new(),
    };
    let Some(mount) = super::mount::mount_or_again(common_dir)? else {
        bail!(
            "{} isn't mounted: mount it first (`gitfold mount`); git merges several --from \
             in the new branch's workspace",
            common_dir.display()
        )
    };
    let changes = Changes::take(common_dir, &mount, &copied)?;
    let (first, others) = sources.split_first().expect("several --from");
    // Without an upstream: no relation afterwards. The start point as
    // typed: git takes it, or refuses it (a name it finds ambiguous),
    // saying so itself (its warnings too: `resolve` leaves them to it).
    let args = ["branch", "--no-track", "--", name, &first.rev];
    let created = git::cmd::command(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()?;
    if !created.success() {
        bail!("branch {name} not created (see above)");
    }
    // Where git made it (the start point may have moved since).
    let branch = format!("refs/heads/{name}");
    let at = git::cmd::output(repo, &["rev-parse", "--verify", &branch])
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .unwrap_or_else(|_| first.id.clone());
    let made = Made {
        name,
        at: &at,
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
    // With -w, the changes go on the merge git made; one it didn't make
    // (stopped by a conflict or a hook) is refused, as `git cherry-pick`
    // refuses to go on it: that undoes it.
    let merge = merge.and_then(|merged| match merged {
        true => changes.apply(&workspace).map(Some),
        false if !with_changes => Ok(None),
        false => bail!(
            "the merge didn't complete (see above): with --with-changes, the uncommitted \
             changes go only on a merge made"
        ),
    });
    match merge {
        Ok(Some(conflicts)) if conflicts.is_empty() => {
            println!("created branch {name}; open it as {}", workspace.display());
            Ok(())
        }
        // As `git cherry-pick` exits after a conflict.
        Ok(Some(conflicts)) => {
            // Up to `PATHS_SAID` paths in all.
            let (mut said, mut left, mut all) = (Vec::new(), PATHS_SAID, 0);
            for (branch, paths) in &conflicts {
                let shown = paths.len().min(left);
                left -= shown;
                all += paths.len();
                said.push(match shown {
                    0 => format!("{branch}'s"),
                    _ => format!("{branch}'s on {}", paths[..shown].join(", ")),
                });
            }
            let more = match all.saturating_sub(PATHS_SAID) {
                0 => String::new(),
                more => format!(" … and {more} more"),
            };
            bail!(
                "created branch {name}; the uncommitted changes conflict (see above): {}{more}; \
                 in {}",
                and_list(&said),
                workspace.display()
            )
        }
        // Stopped by a conflict, or by a hook or an empty message: git says.
        Ok(None) if state.admin.join("MERGE_HEAD").is_file() => bail!(
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
                Err(e) => failed.context(format!("branch {name} created, but {e:#}")),
            })
        }
    }
}

/// How many of the paths whose changes conflict (`branch -w`) are named.
const PATHS_SAID: usize = 10;

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
                        "not removed again: `gitfold remove --force {name}`, then \
                         `git branch -D {name}`"
                    )
                },
            )?;
        }
        let branch_left = || format!("not deleted again: `git branch -D {name}`");
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
/// and the ref it names (`refs/heads/ui` for `HEAD` on ui), in full and
/// shortened as git shortens it, if it names one (and they are used: `resolve`).
struct Source {
    rev: String,
    id: String,
    full: Option<String>,
    name: Option<String>,
}

impl Source {
    /// `rev` in the worktree of git directory `repo`; refused as `git merge`
    /// refuses what isn't a commit; git's own error if it fails otherwise.
    /// With the ref it names: short if `merged` (given to `git merge`), in
    /// full if `named` (`-w`). git's warnings of it said if `merged`.
    fn resolve(repo: &Path, rev: &str, merged: bool, named: bool) -> Result<Source> {
        let line = |out: Vec<u8>| String::from_utf8_lossy(&out).trim().to_string();
        let commit = format!("{rev}^{{commit}}");
        // Without `-q`: git's warnings too.
        let verify = |quiet: &[&str]| {
            (git::cmd::command(repo).args(["rev-parse", "--verify"]))
                .args(quiet)
                .args(["--end-of-options", &commit])
                .output()
        };
        let out = verify(&[])?;
        if !out.status.success() {
            let out = verify(&["-q"])?;
            // `--verify -q`: exit 1 for what names no commit (anything
            // else that fails dies, 128).
            if out.status.code() == Some(1) {
                // What git said first (an ambiguous short id's candidates),
                // as `git merge` shows it.
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
                bail!("{rev} - not something we can merge");
            }
            bail!(
                "git rev-parse {commit} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // git's warnings (a name it finds ambiguous), said once: here, or
        // by `git branch` given the start point.
        if merged {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
        }
        let id = line(out.stdout);
        // Names starting with `-` are no refs (`check-ref-format`); a name
        // git finds ambiguous names none.
        let ref_name = |how: &str| {
            let args = ["rev-parse", "--verify", "-q", how, rev];
            (!rev.starts_with('-'))
                .then(|| git::cmd::output(repo, &args).ok())
                .flatten()
                .map(line)
                .filter(|n| !n.is_empty() && n != "HEAD")
        };
        let full = (named.then(|| ref_name("--symbolic-full-name")).flatten())
            .filter(|full| full.starts_with("refs/"));
        let name = merged.then(|| ref_name("--abbrev-ref")).flatten();
        Ok(Source {
            rev: rev.to_string(),
            id,
            full,
            name,
        })
    }

    /// The local branch it names, if any.
    fn branch(&self) -> Option<&str> {
        self.full.as_deref()?.strip_prefix("refs/heads/")
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

/// The uncommitted changes of the sources' workspaces (`branch --from A
/// --from B -w`), each as git takes it there: a commit of its files (tracked
/// and untracked, not ignored) on its HEAD, made with a copy of its index
/// (`git add -A`, `write-tree`), its own index untouched. Taken through the
/// mount while the workspaces may change: not atomic, as `git stash create`
/// isn't.
#[derive(Default)]
struct Changes(Vec<(String, String)>);

impl Changes {
    /// The workspaces of `sources` to take changes from: the local
    /// branches' that have one, each once (a source that isn't one, or has
    /// none, has no changes). Refused if git is busy in one (a merge, a
    /// rebase), or its index has unmerged paths, as `git stash` refuses.
    fn workspaces(common_dir: &Path, sources: &[Source]) -> Result<Vec<StateDir>> {
        let mut states: Vec<StateDir> = Vec::new();
        for branch in sources.iter().filter_map(Source::branch) {
            let state = StateDir::of(common_dir, branch);
            if !state.opened() || states.iter().any(|s| s.branch == branch) {
                continue;
            }
            let then = "branch from it once that is done";
            crate::git::worktree::refuse_if_busy(common_dir, &state.admin, branch, then)?;
            if !crate::follow::unmerged(&state.admin)?.is_empty() {
                let message = format!("{branch}: its index has unmerged paths; {then}");
                return Err(Refusal::Changes.error(message));
            }
            states.push(state);
        }
        Ok(states)
    }

    /// The changes of the workspaces `states` under `mount`, those that have
    /// any.
    fn take(common_dir: &Path, mount: &Path, states: &[StateDir]) -> Result<Changes> {
        let index = common_dir.join(format!("gitfold-snapshot-{}", std::process::id()));
        let mut changes = Changes::default();
        for state in states {
            let taken = Changes::of(state, &mount.join(&state.branch), &index);
            let _ = fs::remove_file(&index);
            if let Some(commit) = taken? {
                changes.0.push((state.branch.clone(), commit));
            }
        }
        Ok(changes)
    }

    /// The changes of workspace `workspace` (stored in `state`), if any,
    /// with `index` for a copy of its index.
    fn of(state: &StateDir, workspace: &Path, index: &Path) -> Result<Option<String>> {
        // On the copy, safely for the repository (nothing of it written
        // there; its untracked cache and fsmonitor used as git uses them);
        // its throwaway commit unsigned and by gitfold, as `git stash
        // create` makes one by git where no one is configured.
        let git = |args: &[&str]| -> Result<String> {
            let mut git = git::cmd::in_workspace(workspace)?;
            git.env("GIT_INDEX_FILE", index);
            for (var, value) in [("NAME", "gitfold"), ("EMAIL", "gitfold@localhost")] {
                git.env(format!("GIT_COMMITTER_{var}"), value);
                git.env(format!("GIT_AUTHOR_{var}"), value);
            }
            let unsigned = ["-c", "commit.gpgSign=false"];
            let args: Vec<&str> = (git::cmd::REPOSITORY_SAFE.iter().chain(&unsigned))
                .chain(args)
                .copied()
                .collect();
            let out = git::cmd::output_of(&mut git, &args, None)?;
            Ok(String::from_utf8_lossy(&out).trim().to_string())
        };
        match fs::copy(state.index(), index) {
            // No registration (deleted since; its next open writes one): an
            // index of HEAD, its files read anew.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                drop(git(&["read-tree", "HEAD"])?)
            }
            copied => drop(copied.context("copying its git index")?),
        }
        git(&["add", "-A"])?;
        let tree = git(&["write-tree"])?;
        let head = git(&["rev-parse", "HEAD^{commit}", "HEAD^{tree}"])?;
        let (head, head_tree) = head.split_once('\n').context("git rev-parse: no HEAD")?;
        if tree == head_tree {
            return Ok(None);
        }
        let message = format!("uncommitted changes of {}", state.branch);
        git(&["commit-tree", &tree, "-p", head, "-m", &message]).map(Some)
    }

    /// Applies them in workspace `workspace`, each as `git cherry-pick -n`
    /// does, then unstages them (`git reset`); the branches whose changes
    /// conflicted, with the paths git said conflict (each left as git
    /// leaves it). Refused if git refuses one.
    fn apply(&self, workspace: &Path) -> Result<Vec<(&str, Vec<String>)>> {
        if self.0.is_empty() {
            return Ok(Vec::new());
        }
        let git = || git::cmd::in_workspace(workspace);
        let mut conflicts = Vec::new();
        for (n, (branch, commit)) in self.0.iter().enumerate() {
            // git's messages the user's, as for the merge.
            let args = ["cherry-pick", "-n", "--end-of-options", commit];
            if git()?.args(args).status()?.success() {
                continue;
            }
            // A conflict leaves unmerged paths; a refusal, none.
            let args = ["diff", "--name-only", "-z", "--diff-filter=U"];
            let unmerged = git::cmd::output_of(&mut git()?, &args, None)?;
            ensure!(
                !unmerged.is_empty(),
                "git cherry-pick of the uncommitted changes of {branch} failed"
            );
            let paths = (git::cmd::nul_paths(&unmerged).iter())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            // Staged as they are, so the next one merges with them
            // (`cherry-pick -n` staged the rest).
            if n + 1 < self.0.len() {
                let args = [
                    "--literal-pathspecs",
                    "add",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ];
                git::cmd::output_of(&mut git()?, &args, Some(unmerged))?;
            }
            conflicts.push((branch.as_str(), paths));
        }
        // Unstaged; only the index is reset (no refresh of every file).
        git::cmd::output_of(&mut git()?, &["reset", "-q", "--no-refresh"], None)?;
        Ok(conflicts)
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Runs `git merge` of `revs` in workspace `workspace`, its messages the
/// user's, as in a shell there (`git::cmd::in_other_worktree`); whether it
/// made the merge.
fn run_merge(workspace: &Path, revs: &[String]) -> Result<bool> {
    let status = (git::cmd::in_workspace(workspace)?)
        // git's warnings of the names said once, as they were resolved (for
        // the git its hooks run too).
        .args([
            "-c",
            "core.warnAmbiguousRefs=false",
            "merge",
            "--end-of-options",
        ])
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
