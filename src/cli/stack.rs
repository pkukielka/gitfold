//! `gitfold stack`: stacked branches. A branch built on a parent branch
//! (`settings`: `branch.<b>.gitfoldParent`, its parent base kept by
//! `refs/gitfold-stack/<b>`, see `git::stack`) has its own commits copied
//! onto the parent's new ones (restacked): by `gitfold stack sync`, or at the
//! start of its agent's turn (`layer --claude-hook user-prompt-submit`
//! calls `hook_context`), unless `branch.<b>.gitfoldRestack` is `manual`.
//! gitfold never pushes; it says what to force-push.
//!
//! A restack is computed first, moving nothing (`git::stack::restack`).
//! A conflict is recorded (`gitfoldRestackBlocked`) and reported, and the
//! branch stays on the old parent. Otherwise where the branch lives decides
//! how it moves:
//!
//! - no workspace: its ref, with its stack ref, in one transaction (refused
//!   if a git worktree has it checked out);
//! - a workspace, mounted: `git read-tree -m -u C C'` in it (through the
//!   mount, so the kernel sees the files change; it keeps unstaged, staged
//!   and untracked work and refuses if any is on a path the restack
//!   changes), then the refs; the sync thread's ordinary rebase then drops
//!   the files the read-tree copied up;
//! - a workspace, unmounted: only without uncommitted changes, under the
//!   repository lock: its index, its base (`fs::workspace::restack_closed`),
//!   the refs.
//!
//! Both workspace cases record an intent first (`state::restack`), so a
//! crash half-way is finished or undone. Workspaces showing layers aren't
//! restacked (v1).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, ensure};
use clap::{Args, Subcommand};
use gix::ObjectId;
use gix::bstr::BString;

use crate::git::{self, stack};
use crate::refusal::Refusal;
use crate::settings::{self, Local};
use crate::state::StateDir;
use crate::state::lens::{Applied, Lens};
use crate::state::restack::{self, Intent, Mode, Recovery};
use crate::state::retire::{check_name, index_matches_head};
use crate::store::Store;

/// How many paths a line of output names before "and N more".
const SHOWN_PATHS: usize = 5;

#[derive(Args)]
pub struct StackArgs {
    #[command(subcommand)]
    pub op: StackOp,
}

#[derive(Subcommand)]
pub enum StackOp {
    /// Build a branch on PARENT: its own commits then follow PARENT's new
    /// ones (restacked).
    ///
    /// Its own commits are those after the parent base: --base, or where it
    /// and PARENT meet. If PARENT has commits it doesn't, `stack sync`
    /// (or its agent's next turn) restacks it.
    On {
        parent: String,
        /// The branch (default: the workspace you are in).
        #[arg(short = 'w', long, value_name = "BRANCH")]
        workspace: Option<String>,
        /// The commit of PARENT the branch's own commits sit on.
        #[arg(long, value_name = "REV")]
        base: Option<String>,
    },
    /// Forget a branch's parent (its history stays as it is).
    Off {
        /// The branch (default: the workspace you are in).
        #[arg(short = 'w', long, value_name = "BRANCH")]
        workspace: Option<String>,
    },
    /// Restack branches onto their parents' new commits, bottom-up.
    Sync {
        /// The branches (default: the workspace you are in).
        branches: Vec<String>,
        /// Every stacked branch.
        #[arg(long, conflicts_with = "branches")]
        all: bool,
        /// Only say what would be restacked.
        #[arg(long)]
        dry_run: bool,
    },
    /// List the stacks, each parent above its branches.
    List,
}

/// `gitfold stack`.
pub fn stack(repo: &Path, args: StackArgs) -> Result<()> {
    let common_dir = git::init(repo)?;
    match args.op {
        StackOp::On {
            parent,
            workspace,
            base,
        } => {
            let name = super::branch_or_current(workspace, repo)?;
            println!("{}", declare(&common_dir, &name, &parent, base.as_deref())?);
        }
        StackOp::Off { workspace } => {
            let name = super::branch_or_current(workspace, repo)?;
            check_name(&name)?;
            let Some(parent) = Local::read(&common_dir).parent(&name) else {
                return Err(Refusal::NotFound.error(format!("{name} isn't stacked")));
            };
            settings::set_parent(&common_dir, &name, None)?;
            settings::set_restack_blocked(&common_dir, &name, None)?;
            stack::drop_base(&common_dir, &name)?;
            // The way out of an interrupted restack recovery can't tell apart.
            let forgot = match restack::forget(&StateDir::of(&common_dir, &name))? {
                true => "; an interrupted restack's intent is forgotten",
                false => "",
            };
            println!("{name} is no longer stacked on {parent}; its history is as it was{forgot}");
        }
        StackOp::Sync {
            branches,
            all,
            dry_run,
        } => return sync(repo, &common_dir, branches, all, dry_run),
        StackOp::List => print!("{}", listing(&common_dir)?),
    }
    Ok(())
}

/// Stacks `name` on `parent` with parent base `base` (default: where they
/// meet), refusing a cycle; says where it stands.
pub fn declare(common_dir: &Path, name: &str, parent: &str, base: Option<&str>) -> Result<String> {
    check_name(name)?;
    check_name(parent)?;
    git::require_branch(name)?;
    git::require_branch(parent)?;
    ensure!(name != parent, "a branch can't be stacked on itself");
    let local = Local::read(common_dir);
    let mut above = parent.to_string();
    while let Some(next) = local.parent(&above) {
        ensure!(
            next != name,
            "{parent} is stacked on {name} (through {above}): that would be a cycle"
        );
        above = next;
    }
    let (tip, parent_tip) = (tip_of(common_dir, name)?, tip_of(common_dir, parent)?);
    let base = match base {
        Some(rev) => stack::rev(common_dir, rev)?
            .ok_or_else(|| Refusal::NotFound.error(format!("no commit {rev}")))?,
        None => git::merge_base(parent_tip, tip)?,
    };
    ensure!(
        stack::is_ancestor(common_dir, base, tip)?,
        "the base {base} isn't in {name}'s history"
    );
    settings::set_parent(common_dir, name, Some(parent))?;
    settings::set_restack_blocked(common_dir, name, None)?;
    stack::set_base(common_dir, name, base)?;
    let short = short(base);
    Ok(match stack::is_ancestor(common_dir, parent_tip, tip)? {
        true => format!("{name} is on {parent} (base {short}); up to date"),
        false => {
            let n = stack::count(common_dir, base, parent_tip)?;
            format!(
                "{name} is on {parent} (base {short}); {parent} has {}: `gitfold stack sync \
                 {name}` restacks it (or its agent's next turn)",
                commits(n)
            )
        }
    })
}

/// What one branch's restack did.
enum Outcome {
    /// Its parent has no new commits.
    UpToDate,
    /// It has the parent's tip already (rebased by hand): its parent base
    /// was moved there.
    BaseMoved,
    /// Restacked: onto how many new commits of the parent, how many of its
    /// own were dropped (made empty), the paths changed in its workspace
    /// (`None`: no workspace).
    Restacked {
        new: usize,
        dropped: usize,
        files: Option<Vec<BString>>,
        /// The parent base was guessed (no stack ref, or a stale one).
        guessed: bool,
    },
    /// `--dry-run`: would be restacked onto that many new commits.
    Would(usize),
    /// A commit conflicts with the parent's new ones; `told` if this
    /// parent tip had blocked it already.
    Blocked {
        commit: ObjectId,
        paths: Vec<BString>,
        told: bool,
    },
    /// Not now: why (uncommitted work in the way, git busy, layers shown).
    Waits(String),
    /// Not at all as it stands: why (a merge in its commits, no parent).
    Refused(String),
}

impl Outcome {
    fn done(&self) -> bool {
        !matches!(
            self,
            Outcome::Blocked { .. } | Outcome::Waits(_) | Outcome::Refused(_)
        )
    }
}

/// `gitfold stack sync`: restacks `branches` (default: the workspace's;
/// `all`: every stacked branch), parents before their branches, and says
/// what to force-push.
fn sync(
    repo: &Path,
    common_dir: &Path,
    branches: Vec<String>,
    all: bool,
    dry_run: bool,
) -> Result<()> {
    let local = Local::read(common_dir);
    let mut branches = match (all, branches.is_empty()) {
        (true, _) => local.parents().into_iter().map(|(b, _)| b).collect(),
        (false, true) => vec![super::branch_or_current(None, repo)?],
        (false, false) => branches,
    };
    for name in &branches {
        check_name(name)?;
        if local.parent(name).is_none() {
            return Err(Refusal::NotFound.error(format!("{name} isn't stacked")));
        }
    }
    // Bottom-up: a branch restacks onto its parent's restacked commits.
    branches.sort_by_cached_key(|b| (depth(&local, b), b.clone()));
    branches.dedup();
    let mut failed = 0;
    let mut pushes = Vec::new();
    for name in &branches {
        let parent = local.parent(name).unwrap_or_default();
        let outcome = restack_branch(common_dir, name, dry_run)
            .unwrap_or_else(|e| Outcome::Refused(format!("{e:#}")));
        failed += usize::from(!outcome.done());
        println!("{name}\t{}", describe(&outcome, name, &parent, common_dir));
        if matches!(outcome, Outcome::Restacked { .. })
            && let Some(push) = force_push(common_dir, name)?
        {
            pushes.push(push);
        }
    }
    if !pushes.is_empty() {
        println!("to push (rewritten since last push):");
        for push in pushes {
            println!("  {push}");
        }
    }
    if failed > 0 {
        return Err(Refusal::Changes.error(format!(
            "{failed} of {} not restacked (see above)",
            branches.len()
        )));
    }
    Ok(())
}

/// How many stacked branches are below `branch` in its stack.
fn depth(local: &Local, branch: &str) -> usize {
    let mut at = branch.to_string();
    let mut depth = 0;
    while let Some(parent) = local.parent(&at) {
        depth += 1;
        // A cycle someone wrote into the config by hand.
        if depth > 1000 {
            break;
        }
        at = parent;
    }
    depth
}

/// One line of `stack sync` for `name` (stacked on `parent`).
fn describe(outcome: &Outcome, name: &str, parent: &str, common_dir: &Path) -> String {
    match outcome {
        Outcome::UpToDate => format!("up to date with {parent}"),
        Outcome::BaseMoved => format!("has {parent}'s tip already; its parent base moved there"),
        Outcome::Restacked {
            new,
            dropped,
            files,
            guessed,
        } => {
            let mut line = format!("restacked onto {parent} ({})", commits(*new));
            if *guessed {
                line += "; its parent base was guessed (from the parent's reflog)";
            }
            if *dropped > 0 {
                let _ = write!(line, "; {dropped} of its commits made empty, dropped");
            }
            if let Some(files) = files {
                let n = files.len();
                let _ = write!(
                    line,
                    "; {n} {} in its workspace",
                    if n == 1 {
                        "file changed"
                    } else {
                        "files changed"
                    }
                );
            }
            line
        }
        Outcome::Would(n) => format!("would be restacked onto {parent} ({})", commits(*n)),
        Outcome::Blocked { commit, paths, .. } => {
            let base = stack_base(common_dir, name).map_or("<base>".into(), short);
            format!(
                "blocked: commit {} conflicts on {} with {parent}; nothing moved. Rebase by \
                 hand in its workspace (`git rebase --onto {parent} {base}`), then `gitfold \
                 stack sync {name}`",
                short(*commit),
                shown(paths)
            )
        }
        Outcome::Waits(why) => format!("waits: {why}"),
        Outcome::Refused(why) => format!("not restacked: {why}"),
    }
}

/// The parent base stored for `name`, if any.
fn stack_base(common_dir: &Path, name: &str) -> Option<ObjectId> {
    stack::rev(common_dir, &stack::stack_ref(name))
        .ok()
        .flatten()
}

/// Restacks branch `name` onto its parent's tip, if it moved (see the
/// module doc); with `dry_run`, only says whether it would.
fn restack_branch(common_dir: &Path, name: &str, dry_run: bool) -> Result<Outcome> {
    let local = Local::read(common_dir);
    let Some(parent) = local.parent(name) else {
        return Err(Refusal::NotFound.error(format!("{name} isn't stacked")));
    };
    let tip = tip_of(common_dir, name)?;
    let Some(parent_tip) = stack::rev(common_dir, &format!("refs/heads/{parent}"))? else {
        return Ok(Outcome::Refused(format!(
            "its parent {parent} is missing; `gitfold stack on <PARENT>` names another, \
             `gitfold stack off` forgets it"
        )));
    };
    let base = stack::parent_base(common_dir, name, &parent, tip, parent_tip)?;
    if on_parent(common_dir, base.commit, parent_tip, tip)? {
        if dry_run || base.stored == Some(parent_tip) {
            return Ok(Outcome::UpToDate);
        }
        stack::move_refs(
            common_dir,
            name,
            (tip, tip),
            (base.stored, parent_tip),
            "gitfold: stack base",
        )?;
        settings::set_restack_blocked(common_dir, name, None)?;
        return Ok(match base.commit == parent_tip {
            true => Outcome::UpToDate,
            false => Outcome::BaseMoved,
        });
    }
    let new = stack::count(common_dir, base.commit, parent_tip)?;
    let state = StateDir::of(common_dir, name);
    if state.opened() && lensed(&state)? {
        return Ok(Outcome::Waits(
            "layers are shown in its workspace; `gitfold lens off` first".into(),
        ));
    }
    let computed = git::with_repo(|repo| stack::restack(repo, base.commit, parent_tip, tip))?;
    let (new_tip, dropped) = match computed {
        stack::Restack::Merge(commit) => {
            return Ok(Outcome::Refused(format!(
                "its commit {} is a merge, which isn't restacked; rebase by hand",
                short(commit)
            )));
        }
        stack::Restack::Conflict { commit, paths } => {
            let record = format!("{parent_tip} {commit} {}", lossy(&paths).join(" "));
            let told = local
                .restack_blocked(name)
                .is_some_and(|b| b.starts_with(&parent_tip.to_string()));
            if !dry_run && !told {
                settings::set_restack_blocked(common_dir, name, Some(&record))?;
            }
            return Ok(Outcome::Blocked {
                commit,
                paths,
                told,
            });
        }
        stack::Restack::Done { tip, dropped, .. } => (tip, dropped.len()),
    };
    if dry_run {
        return Ok(Outcome::Would(new));
    }
    if let Some(other) = git::worktree::checked_out_elsewhere(common_dir, name, &state.admin) {
        return Ok(Outcome::Refused(format!(
            "it is checked out in {other}; restack it there (`git rebase --onto {parent} {}`)",
            short(base.commit)
        )));
    }
    let intent = Intent {
        mode: Mode::Open,
        from: tip,
        to: new_tip,
        parent_tip,
        stack_old: base.stored,
    };
    let moved = if state.opened() {
        match crate::mount_table::mount_of(common_dir)? {
            Some(mount) => apply_open(&state, &mount.join(name), &intent)?,
            None => match crate::lock::lock(common_dir)? {
                Some(_lock) => apply_closed(
                    &state,
                    &Intent {
                        mode: Mode::Closed,
                        ..intent
                    },
                )?,
                None => Moved::Waits("the repository is being mounted or unmounted; retry".into()),
            },
        }
    } else {
        match stack::move_refs(
            common_dir,
            name,
            (tip, new_tip),
            (base.stored, parent_tip),
            "gitfold: restack",
        ) {
            Ok(()) => Moved::Done(None),
            Err(_) if stack::rev(common_dir, &format!("refs/heads/{name}"))? == Some(new_tip) => {
                Moved::Already
            }
            Err(e) => return Err(e),
        }
    };
    let files = match moved {
        Moved::Done(files) => files,
        Moved::Already => return Ok(Outcome::UpToDate),
        Moved::Waits(why) => return Ok(Outcome::Waits(why)),
    };
    settings::set_restack_blocked(common_dir, name, None)?;
    Ok(Outcome::Restacked {
        new,
        dropped,
        files,
        guessed: base.guessed,
    })
}

/// Whether a branch at `tip` with parent base `base` needs no restack onto
/// parent tip `parent_tip`: it is the base, or it is past the base and in
/// the branch already (rebased by hand). Not a parent rewound behind the
/// base: the commits it dropped go from the branch too, as `git rebase
/// --onto` drops them.
fn on_parent(
    common_dir: &Path,
    base: ObjectId,
    parent_tip: ObjectId,
    tip: ObjectId,
) -> Result<bool> {
    Ok(base == parent_tip
        || (stack::is_ancestor(common_dir, base, parent_tip)?
            && stack::is_ancestor(common_dir, parent_tip, tip)?))
}

/// Whether the workspace in `state` shows layers, as stored or as applied.
fn lensed(state: &StateDir) -> Result<bool> {
    Ok(Lens::read(state)?.is_on() || Applied::read(state)?.layers()?.is_on())
}

/// An interrupted restack of the workspace in `state` finished or undone
/// first (`state::restack`); why not, if it can't be.
fn recovered(state: &StateDir) -> Result<std::result::Result<(), String>> {
    Ok(match restack::recover(state)? {
        Recovery::Nothing | Recovery::Finished | Recovery::Undone => Ok(()),
        Recovery::InProgress => Err("another restack of it is running".into()),
        Recovery::Stuck(why) => Err(why),
    })
}

/// How moving a branch for a restack ended.
enum Moved {
    /// Moved: the paths changed in its workspace (`None`: no workspace).
    Done(Option<Vec<BString>>),
    /// Another restack (the hook and `stack sync` at once) got there first.
    Already,
    /// Not now: why.
    Waits(String),
}

/// With the intent held: whether the branch and its stack ref are still
/// where `intent` starts from (`None`), else how it ended. A restack
/// computed before another one finished would otherwise run its read-tree
/// against the other's result.
fn moved_meanwhile(state: &StateDir, intent: &Intent) -> Result<Option<Moved>> {
    let tip = stack::rev(&state.common_dir, &format!("refs/heads/{}", state.branch))?;
    let stored = stack::rev(&state.common_dir, &stack::stack_ref(&state.branch))?;
    Ok(if tip == Some(intent.from) && stored == intent.stack_old {
        None
    } else if tip == Some(intent.to) {
        Some(Moved::Already)
    } else {
        Some(Moved::Waits(
            "the branch moved meanwhile; retried next time".into(),
        ))
    })
}

/// `git read-tree -m -u <from> <to>` in the workspace at `dir`.
fn read_tree(dir: &Path, from: ObjectId, to: ObjectId) -> Result<std::process::Output> {
    let (from, to) = (from.to_string(), to.to_string());
    Ok(git::cmd::git()
        .arg("-C")
        .arg(dir)
        .args(["read-tree", "-m", "-u", &from, &to])
        .output()?)
}

/// Restacks the mounted workspace in `state`, at `dir`, as `intent` says:
/// its files and index by `git read-tree -m -u`, then the refs.
fn apply_open(state: &StateDir, dir: &Path, intent: &Intent) -> Result<Moved> {
    if let Err(why) = recovered(state)? {
        return Ok(Moved::Waits(why));
    }
    if let Some(why) = state.git_busy() {
        return Ok(Moved::Waits(format!("{why} in its workspace")));
    }
    let paths = restack::changed_paths(intent)?;
    let held = intent.begin(state)?;
    if let Some(moved) = moved_meanwhile(state, intent)? {
        return Ok(moved);
    }
    let out = read_tree(dir, intent.from, intent.to)?;
    if !out.status.success() {
        return Ok(Moved::Waits(refused(&String::from_utf8_lossy(&out.stderr))));
    }
    if let Err(e) = move_refs(state, intent) {
        let tip = stack::rev(&state.common_dir, &format!("refs/heads/{}", state.branch))?;
        if tip == Some(intent.to) {
            held.done()?;
            return Ok(Moved::Already);
        }
        // Moved by git meanwhile: the files go back, as before, if the
        // branch is where they were taken from.
        if tip == Some(intent.from) && read_tree(dir, intent.to, intent.from)?.status.success() {
            held.done()?;
        } else {
            held.keep();
        }
        return Ok(Moved::Waits(format!(
            "the branch moved during the restack ({e:#})"
        )));
    }
    held.done()?;
    Ok(Moved::Done(Some(paths)))
}

/// Why `git read-tree -m -u` refused, from what it said: the uncommitted
/// (or untracked) files in the way, else its own words (a lock, say).
fn refused(said: &str) -> String {
    let in_the_way: Vec<&str> = (said.lines())
        .filter(|l| l.contains("not uptodate") || l.contains("would be overwritten"))
        .filter_map(|l| l.split('\'').nth(1))
        .collect();
    match in_the_way.is_empty() {
        true => format!("git read-tree refused: {}", said.trim()),
        false => format!(
            "uncommitted changes to {} overlap the restack; commit or stash them",
            shown_str(&in_the_way)
        ),
    }
}

/// Restacks the unmounted workspace in `state` (the repository lock held),
/// only if it has no uncommitted changes: its index, its base, then the
/// refs.
fn apply_closed(state: &StateDir, intent: &Intent) -> Result<Moved> {
    if let Err(why) = recovered(state)? {
        return Ok(Moved::Waits(why));
    }
    if let Some(why) = state.git_busy() {
        return Ok(Moved::Waits(format!("{why} in its workspace")));
    }
    let store = Store::default();
    let base = state.read_base()?.id;
    if base != intent.from {
        // The branch moved while unmounted: catch up as opening it would.
        crate::fs::workspace::catch_up(&store, &state.common_dir, &state.branch)?;
    }
    let s = crate::state::status::status(&store, state)?;
    if !s.changed.is_empty() || !s.deleted.is_empty() || !index_matches_head(&state.admin)? {
        return Ok(Moved::Waits(
            "its workspace has uncommitted changes; restacked once it is mounted".into(),
        ));
    }
    let (from, to) = (
        git::resolve(&intent.from.to_string())?,
        git::resolve(&intent.to.to_string())?,
    );
    let paths = restack::changed_paths(intent)?;
    let held = intent.begin(state)?;
    if let Some(moved) = moved_meanwhile(state, intent)? {
        return Ok(moved);
    }
    let moved = crate::fs::workspace::restack_closed(&store, state.clone(), &from, &to, || {
        restack::move_index(state, intent, false)
    });
    let moved = match moved {
        Ok(moved) => moved,
        // The index or the base may have moved: `recover` sees to it.
        Err(e) => {
            held.keep();
            return Err(e);
        }
    };
    if let Err(in_the_way) = moved {
        held.done()?;
        let in_the_way: Vec<BString> = in_the_way.into_iter().map(BString::from).collect();
        return Ok(Moved::Waits(format!(
            "its workspace has files of its own (ignored ones) at {}",
            shown(&in_the_way)
        )));
    }
    if let Err(e) = move_refs(state, intent) {
        held.keep();
        return Err(e);
    }
    held.done()?;
    Ok(Moved::Done(Some(paths)))
}

/// The branch from C to C' and its stack ref to A, as `intent` says.
fn move_refs(state: &StateDir, intent: &Intent) -> Result<()> {
    stack::move_refs(
        &state.common_dir,
        &state.branch,
        (intent.from, intent.to),
        (intent.stack_old, intent.parent_tip),
        "gitfold: restack",
    )
}

/// What to run to push `name` after a restack, if it has a remote-tracking
/// branch the restack left behind: a push with a lease on that tip, so a
/// push someone else made meanwhile is refused.
fn force_push(common_dir: &Path, name: &str) -> Result<Option<String>> {
    let Some((remote, theirs, at)) = upstream(common_dir, name)? else {
        return Ok(None);
    };
    let tip = tip_of(common_dir, name)?;
    if stack::is_ancestor(common_dir, at, tip)? {
        return Ok(None);
    }
    Ok(Some(format!(
        "git push --force-with-lease={theirs}:{at} {remote} {name}:{theirs}"
    )))
}

/// The remote, branch and remote-tracking tip `name` pushes to: its
/// upstream, else `origin`'s branch of the same name, if fetched.
fn upstream(common_dir: &Path, name: &str) -> Result<Option<(String, String, ObjectId)>> {
    let local = Local::read(common_dir);
    if let Some(at) = stack::rev(common_dir, &format!("refs/heads/{name}@{{upstream}}"))?
        && let Some((remote, theirs)) = local.upstream(name)
    {
        return Ok(Some((remote, theirs, at)));
    }
    let tracking = format!("refs/remotes/origin/{name}");
    Ok(stack::rev(common_dir, &tracking)?.map(|at| ("origin".into(), name.into(), at)))
}

/// The notes `gitfold list` shows for stacked branch `name`: the parent's
/// state, and whether it needs a restack or a force-push.
pub fn notes(common_dir: &Path, local: &Local, name: &str) -> Vec<String> {
    let Some(parent) = local.parent(name) else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    let (Ok(tip), Ok(Some(parent_tip))) = (
        tip_of(common_dir, name),
        stack::rev(common_dir, &format!("refs/heads/{parent}")),
    ) else {
        return vec!["parent missing".into()];
    };
    if local.restack_blocked(name).is_some() {
        notes.push("restack blocked".into());
    } else if let Ok(base) = stack::parent_base(common_dir, name, &parent, tip, parent_tip)
        && !on_parent(common_dir, base.commit, parent_tip, tip).unwrap_or(true)
    {
        let n = stack::count(common_dir, base.commit, parent_tip).unwrap_or(0);
        notes.push(format!("needs restack ({n})"));
    }
    if force_push(common_dir, name).is_ok_and(|p| p.is_some()) {
        notes.push("needs force-push".into());
    }
    notes
}

/// `gitfold stack list`: each stack from its bottom (a branch that isn't
/// stacked), every branch indented below its parent, with its notes.
fn listing(common_dir: &Path) -> Result<String> {
    let local = Local::read(common_dir);
    let edges = local.parents();
    if edges.is_empty() {
        return Ok("no stacked branches\n".into());
    }
    let mut roots: Vec<&str> = (edges.iter())
        .map(|(_, p)| p.as_str())
        .filter(|p| local.parent(p).is_none())
        .collect();
    roots.sort();
    roots.dedup();
    let mut out = String::new();
    for root in roots {
        let missing = match git::branch_exists(root) {
            true => "",
            false => "  (missing)",
        };
        let _ = writeln!(out, "{root}{missing}");
        tree(common_dir, &local, &edges, root, 1, &mut out);
    }
    Ok(out)
}

fn tree(
    common_dir: &Path,
    local: &Local,
    edges: &[(String, String)],
    parent: &str,
    depth: usize,
    out: &mut String,
) {
    // A cycle written into the config by hand.
    if depth > 100 {
        return;
    }
    for (branch, _) in edges.iter().filter(|(_, p)| p == parent) {
        let notes = notes(common_dir, local, branch);
        let notes = match notes.is_empty() {
            true => String::new(),
            false => format!("  ({})", notes.join("; ")),
        };
        let _ = writeln!(out, "{}{branch}{notes}", "  ".repeat(depth));
        tree(common_dir, local, edges, branch, depth + 1, out);
    }
}

/// `gitfold layer --claude-hook user-prompt-submit`'s part for stacks: at
/// the start of a turn, the session's workspace is restacked onto its
/// parent's new commits (unless its restacks are manual), and what happened
/// is said as context. Nothing when nothing happened, or when what keeps it
/// from happening was told before (a block per parent tip, anything else
/// while it says the same: `restack-told` in the state directory).
pub fn hook_context(common_dir: &Path, name: &str) -> Result<Option<String>> {
    let local = Local::read(common_dir);
    let Some(parent) = local.parent(name) else {
        return Ok(None);
    };
    if local.restack_manual(name) {
        return Ok(None);
    }
    let told_file = StateDir::of(common_dir, name).dir.join(TOLD);
    let before = stack_base(common_dir, name);
    let outcome = restack_branch(common_dir, name, false)?;
    let say = |text: String| -> Result<Option<String>> {
        if std::fs::read_to_string(&told_file).is_ok_and(|told| told == text) {
            return Ok(None);
        }
        if told_file.parent().is_some_and(Path::exists) {
            crate::fsutil::write_atomic(&told_file, text.as_bytes())?;
        }
        Ok(Some(text))
    };
    if outcome.done() {
        let _ = std::fs::remove_file(&told_file);
    }
    Ok(match outcome {
        Outcome::UpToDate | Outcome::BaseMoved | Outcome::Would(_) => None,
        Outcome::Blocked { told: true, .. } => None,
        Outcome::Restacked {
            new,
            files,
            guessed,
            ..
        } => {
            let guessed = match guessed {
                true => {
                    " Its parent base (where its own commits start) was guessed from \
                     the parent's reflog, its stack ref being missing: check that only \
                     its own commits were moved."
                }
                false => "",
            };
            let parent_tip = tip_of(common_dir, &parent)?;
            let subjects =
                before.map_or(Ok(Vec::new()), |b| subjects(common_dir, b, parent_tip))?;
            let what = match subjects.is_empty() {
                true => String::new(),
                false => format!(" ({})", subjects.join("; ")),
            };
            let files = files.unwrap_or_default();
            let files = match files.is_empty() {
                true => "no files changed under you".to_string(),
                false => format!("files changed under you: {}", shown(&files)),
            };
            Some(format!(
                "gitfold: this branch, {name}, was restacked onto {parent}'s {}{what}; {files}. \
                 Its history was rewritten: push it with --force-with-lease when you push.\
                 {guessed}",
                commits(new)
            ))
        }
        Outcome::Blocked { .. } => Some(format!(
            "gitfold: {name} is stacked on {parent}, which has new commits; {}",
            describe(&outcome, name, &parent, common_dir)
        )),
        other => say(format!(
            "gitfold: {name} is stacked on {parent}, which has new commits; {}",
            describe(&other, name, &parent, common_dir)
        ))?,
    })
}

/// The hook's last message about why a workspace wasn't restacked, in its
/// state directory: said once while it stays the same.
const TOLD: &str = "restack-told";

/// The subjects of `from..to`, oldest first, at most three.
fn subjects(common_dir: &Path, from: ObjectId, to: ObjectId) -> Result<Vec<String>> {
    let range = format!("{from}..{to}");
    let out = git::cmd::output(common_dir, &["log", "--reverse", "--format=%s", &range])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .take(3)
        .map(str::to_string)
        .collect())
}

/// The commit branch `name` points at.
fn tip_of(common_dir: &Path, name: &str) -> Result<ObjectId> {
    match stack::rev(common_dir, &format!("refs/heads/{name}"))? {
        Some(tip) => Ok(tip),
        None => Err(Refusal::NotFound.error(format!("no branch {name}"))),
    }
}

fn short(id: ObjectId) -> String {
    id.to_hex_with_len(12).to_string()
}

fn commits(n: usize) -> String {
    match n {
        1 => "1 new commit".into(),
        n => format!("{n} new commits"),
    }
}

fn lossy(paths: &[BString]) -> Vec<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

/// Paths for a line of output: the first few, and how many more.
fn shown(paths: &[BString]) -> String {
    let paths = lossy(paths);
    shown_str(&paths.iter().map(String::as_str).collect::<Vec<_>>())
}

fn shown_str(paths: &[&str]) -> String {
    let mut out = paths[..paths.len().min(SHOWN_PATHS)].join(", ");
    if paths.len() > SHOWN_PATHS {
        let _ = write!(out, " and {} more", paths.len() - SHOWN_PATHS);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_shown() {
        assert_eq!(shown_str(&["a", "b"]), "a, b");
        assert_eq!(
            shown_str(&["1", "2", "3", "4", "5", "6", "7"]),
            "1, 2, 3, 4, 5 and 2 more"
        );
    }

    #[test]
    fn read_tree_refusals_name_the_files_in_the_way() {
        let said = "error: Entry 'd/c.txt' not uptodate. Cannot merge.\n\
                    error: Untracked working tree file 'n.txt' would be overwritten by merge.\n";
        assert_eq!(
            refused(said),
            "uncommitted changes to d/c.txt, n.txt overlap the restack; commit or stash them"
        );
        let locked = "fatal: Unable to create '/x/index.lock': File exists.";
        assert_eq!(refused(locked), format!("git read-tree refused: {locked}"));
    }
}
