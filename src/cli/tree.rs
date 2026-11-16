//! The tree of branches: a branch built on another (its parent) takes the
//! parent's new commits. The command-line contract of the design (`gitfold
//! parent`, `branch --parent`, `branch` with several `--from`, `changes --
//! PATH`) and the hints for commands people guess; what isn't built yet
//! parses its arguments as specified and says so (`not_implemented`, exit 1).

use std::path::Path;

use anyhow::{Result, bail};

use crate::follow;
use crate::git;
use crate::refusal::Refusal;
use crate::settings;
use crate::state::retire::check_name;
use crate::tree::{Refused, Tree};

/// A command whose arguments parse but that isn't built yet: printed as
/// `<command>: not implemented yet` (no `Error:` prefix), exit 1.
#[derive(Debug)]
pub struct NotImplemented(String);

impl std::fmt::Display for NotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: not implemented yet", self.0)
    }
}

impl std::error::Error for NotImplemented {}

/// A refusal of the arguments as given: printed as is (no `Error:` prefix),
/// exit 2, like clap's own usage errors.
#[derive(Debug)]
pub struct Usage(pub String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

/// The tree's refusal of `parent` for `branch` in command `what`, with its
/// exit code: 3 not found, 1 no root, 2 the command as typed is invalid.
fn refused(what: &str, branch: &str, parent: &str, refused: Refused) -> anyhow::Error {
    let message = format!("{what}: {}", crate::tree::refusal(branch, parent, &refused));
    match refused {
        Refused::Unknown(_) | Refused::NotBranch { .. } | Refused::OnlyRemote { .. } => {
            Refusal::NotFound.error(message)
        }
        Refused::NoRoot => anyhow::anyhow!(message),
        Refused::Root | Refused::Own | Refused::Cycle | Refused::NoParent => Usage(message).into(),
    }
}

/// The error of a command whose arguments parse but that isn't built yet.
pub fn not_implemented(what: &str) -> anyhow::Error {
    NotImplemented(what.to_string()).into()
}

/// The arguments of `gitfold parent`.
#[derive(clap::Args)]
pub struct ParentArgs {
    /// The branch (default: the workspace you are in).
    pub branch: Option<String>,
    /// Make P its parent: its own commits move onto P's tip, and from then
    /// on onto P's new commits (never creates a branch).
    #[arg(long, value_name = "P", conflicts_with = "none")]
    pub set: Option<String>,
    /// Forget its parent; its history stays as it is (the parent's commits
    /// count as its own from then on).
    #[arg(long, conflicts_with_all = ["follow", "no_follow"])]
    pub none: bool,
    /// Take the parent's new commits by itself (at its agent's turn start,
    /// or once its workspace is idle).
    #[arg(long, conflicts_with = "no_follow")]
    pub follow: bool,
    /// Take them only when asked (`--update`).
    #[arg(long)]
    pub no_follow: bool,
    /// Take the parent's new commits now, once.
    #[arg(long, conflicts_with_all = ["set", "none"])]
    pub update: bool,
    /// With --update: move commits that are pushed already.
    #[arg(long, requires = "update")]
    pub yes: bool,
}

/// `gitfold parent`: the checks of the rules table (S1-S6, F6), then shows
/// the parent (`show`), forgets it (`--none`, F5), turns following on or
/// off, moves the branch onto another parent (`--set`) or onto its parent's
/// new commits (`--update`); see `follow`. Moving a workspace's branch is
/// not implemented yet.
pub fn parent(repo: &Path, args: ParentArgs) -> Result<()> {
    let given = super::branch_or_current(args.branch, repo)?;
    let common_dir = git::init(repo)?;
    check_name(&given)?;
    // S1 before reading the tree: an unknown branch is "not found" even when
    // the config can't be read (T5).
    let Some(name) = crate::tree::local_branch(&given) else {
        let r = Refused::Unknown(given.clone());
        return Err(refused("gitfold parent", &given, "", r));
    };
    let refuse = |parent: &str, r| refused("gitfold parent", &name, parent, r);
    if let Some(parent) = &args.set {
        // Following on with a new parent unless --no-follow (F4); the same
        // parent changes nothing but what a flag says (F3).
        let follow = (args.follow || args.no_follow).then_some(args.follow);
        return set(&common_dir, &name, parent, follow);
    }
    let tree = Tree::read(&common_dir)?;
    if args.follow || args.no_follow || args.update {
        tree.check_follow(&name).map_err(|r| refuse("", r))?;
    }
    let parent = tree.parent(&name).map(crate::tree::short);
    if args.update {
        update(&common_dir, &name, args.yes)?;
        // Changed only once the update went through.
        if args.follow || args.no_follow {
            settings::set_follow(&common_dir, &name, args.follow)?;
        }
        return Ok(());
    }
    if args.none {
        follow::drop_base(&common_dir, &name)?;
        // F5; a parent stored on the root or a deleted branch (T1, T2) goes
        // too.
        settings::forget_parent(&common_dir, &name)?;
        match parent {
            Some(parent) => println!("{name} no longer has a parent (it was {parent})"),
            None => println!("{}", show(&tree, &name, None)),
        }
    } else if args.follow || args.no_follow {
        let parent = parent.expect("check_follow: a branch with a parent (F6)");
        // Written even if unchanged: a malformed value is replaced.
        settings::set_follow(&common_dir, &name, args.follow)?;
        match args.follow {
            true => println!("{name} follows {parent}"),
            false => println!("{name} doesn't follow {parent}"),
        }
    } else {
        println!("{}", show(&tree, &name, parent));
        // A pause of following, only while it follows.
        let paused = match tree.follows(&name) {
            true => paused(&common_dir, &tree, &name),
            false => Ok(None),
        };
        match paused {
            Ok(Some(paused)) => println!("{paused}"),
            Ok(None) => {}
            Err(e) => log::warn!("{name}: whether it can take its parent's commits: {e:#}"),
        }
    }
    Ok(())
}

/// What `gitfold parent` says of local branch `name` with `parent` (short):
/// whether it follows it, and whether the parent is gone (T3).
fn show(tree: &Tree, name: &str, parent: Option<&str>) -> String {
    let Some(parent) = parent else {
        return match tree.is_root(name) {
            true => format!("{name} is the root of the tree; it has no parent"),
            false => format!("{name} has no parent"),
        };
    };
    let gone = match tree.parent_gone(name) {
        true => " (parent gone)",
        false => "",
    };
    let following = following(tree.follows(name));
    format!("{name} is built on {parent}{gone}, {following}")
}

/// Why `name` doesn't take its parent's new commits, if it has some it
/// can't take: a conflict recorded, or commits already pushed.
fn paused(common_dir: &Path, tree: &Tree, name: &str) -> Result<Option<String>> {
    let Some(parent) = tree.parent(name).filter(|_| !tree.parent_gone(name)) else {
        return Ok(None);
    };
    let plan = follow::Plan::new(common_dir, name, Some(parent), &tree.root_refs(), parent)?;
    if plan.up_to_date(common_dir)? {
        return Ok(None);
    }
    let local = settings::Local::read_checked(common_dir)?;
    let short = crate::tree::short(parent);
    if let Some(paths) = plan.conflict(common_dir, &local)? {
        return Ok(Some(format!(
            "following paused: {}",
            conflict_said(name, short, &plan, &paths)
        )));
    }
    let pushed = plan.pushed(common_dir)?;
    Ok(pushed.map(|p| format!("following paused: {}", pushed_said(name, &p))))
}

/// Refuses moving `name` where gitfold doesn't move it (`what`: the flag):
/// in a workspace (not yet), or checked out in another git worktree (never:
/// git rebase there).
fn movable(common_dir: &Path, name: &str, what: &str) -> Result<()> {
    match follow::checked_out(common_dir, name) {
        None => Ok(()),
        Some(follow::CheckedOut::Workspace) => Err(not_implemented(&format!(
            "gitfold parent {what} for a workspace's branch"
        ))),
        Some(follow::CheckedOut::Worktree(path)) => Err(Refusal::InUse.error(format!(
            "{name} is checked out in {path}: gitfold doesn't move it there; take the new \
             commits with git rebase in it"
        ))),
    }
}

/// `gitfold parent NAME --update [--yes]`: its parent's new commits taken
/// now (`follow::Plan::run`).
fn update(common_dir: &Path, name: &str, yes: bool) -> Result<()> {
    movable(common_dir, name, "--update")?;
    let _moving = follow::lock(common_dir, name)?;
    // Read with the branch's moves locked: a --set that ran meanwhile has
    // written its parent.
    let tree = Tree::read(common_dir)?;
    let Some(parent) = tree.parent(name) else {
        return Err(refused("gitfold parent", name, "", Refused::NoParent));
    };
    let short = crate::tree::short(parent);
    if tree.parent_gone(name) {
        bail!(
            "{name}'s parent {short} is gone: no new commits to take (`gitfold parent {name} \
             --none` keeps its history as it is)"
        );
    }
    let plan = follow::Plan::new(common_dir, name, Some(parent), &tree.root_refs(), parent)?;
    match plan.run(common_dir, yes, true, || {})? {
        follow::Moved::UpToDate => {
            println!("{name} is up to date with {short}");
            Ok(())
        }
        moved => said(name, short, &plan, moved),
    }
}

/// `gitfold parent NAME --set P` (P as `given`): P its parent, following it
/// unless `follow` says otherwise (F3, F4), written first (with where its
/// own commits start, on the old parent), under the tree's lock with the
/// check that it makes no cycle (S1-S6); then its own commits moved onto
/// P's tip. A move cut short leaves it built on P with P's commits to take
/// (`--update`). With commits already pushed, it stays built on P without
/// the move (following paused until `--update --yes`). A conflict or a
/// merge moves nothing, and the old parent is taken back.
fn set(common_dir: &Path, name: &str, given: &str, follow: Option<bool>) -> Result<()> {
    let refuse = |r| refused("gitfold parent", name, given, r);
    // Refused as the table says before anything else (a branch checked out
    // elsewhere, say).
    let tree = Tree::read(common_dir)?;
    let full = tree.check(name, given).map_err(refuse)?;
    let short = crate::tree::short(&full);
    if tree.parent(name) == Some(full.as_str()) {
        if let Some(on) = follow {
            settings::set_follow(common_dir, name, on)?;
        }
        println!("{name} is built on {short} already");
        return Ok(());
    }
    let old = tree.parent(name).map(String::from);
    movable(common_dir, name, "--set")?;
    let _moving = follow::lock(common_dir, name)?;
    let (plan, before) = {
        let _tree = follow::tree_lock(common_dir)?;
        let tree = Tree::read(common_dir)?;
        tree.check(name, given).map_err(refuse)?;
        if tree.parent(name) != old.as_deref() {
            bail!("{name}'s parent changed meanwhile; retry");
        }
        let plan = follow::Plan::new(common_dir, name, old.as_deref(), &tree.root_refs(), &full)?;
        // A merge among its own commits: refused before anything changes.
        if plan.merges(common_dir)? {
            return said(name, short, &plan, follow::Moved::Merge);
        }
        let local = settings::Local::read_checked(common_dir)?;
        let before = Before {
            parent: old.clone(),
            follow: local.follow(name),
            conflict: local.conflict(name),
        };
        // Its own commits start where they did on the old parent.
        follow::set_base(common_dir, name, plan.base)?;
        settings::set_parent(common_dir, name, &full)?;
        settings::set_follow(common_dir, name, follow.unwrap_or(true))?;
        settings::set_conflict(common_dir, name, None)?;
        (plan, before)
    };
    let moved = plan.run(common_dir, false, false, || {
        println!("moves {} of {name} onto {short}", commits(plan.own))
    })?;
    match &moved {
        follow::Moved::Merge | follow::Moved::Conflict(_) => {
            take_back(common_dir, name, &before)?;
            said(name, short, &plan, moved)
        }
        follow::Moved::Pushed(pushed) => {
            let paused = match follow.unwrap_or(true) {
                true => "following paused: ",
                false => "",
            };
            println!(
                "{name} is now built on {short}; {paused}{}",
                pushed_said(name, pushed)
            );
            Ok(())
        }
        _ => {
            println!("{name} is now built on {short}");
            said(name, short, &plan, moved)
        }
    }
}

/// A branch's relation before a `--set`, as stored: its parent (a full
/// ref), following, and a conflict recorded.
struct Before {
    parent: Option<String>,
    follow: Option<bool>,
    conflict: Option<(String, Vec<String>)>,
}

/// Builds `name` on its parent from `before` a `--set` that moved nothing,
/// under the tree's lock: unless that would close a cycle by now (another
/// `--set` meanwhile), which is said, and the new parent stays.
fn take_back(common_dir: &Path, name: &str, before: &Before) -> Result<()> {
    let _tree = follow::tree_lock(common_dir)?;
    let Some(old) = &before.parent else {
        follow::drop_base(common_dir, name)?;
        return settings::forget_parent(common_dir, name);
    };
    if Tree::read(common_dir)?.would_cycle(name, old) {
        let old = crate::tree::short(old);
        eprintln!("{name} stays built on its new parent: {old} is built on {name} by now");
        return Ok(());
    }
    settings::set_parent(common_dir, name, old)?;
    settings::set_follow(common_dir, name, before.follow.unwrap_or(true))?;
    let conflict = (before.conflict.as_ref()).map(|(at, paths)| (at.as_str(), paths.as_slice()));
    settings::set_conflict(common_dir, name, conflict)
}

/// What a move of `name` onto `parent` (short) did, or why it didn't, said
/// (on standard output) or refused.
fn said(name: &str, parent: &str, plan: &follow::Plan, moved: follow::Moved) -> Result<()> {
    match moved {
        follow::Moved::UpToDate => Ok(()),
        follow::Moved::Done { kept, push } => {
            println!("moved {} of {name} onto {parent}", commits(kept));
            if plan.own > kept {
                println!(
                    "({} already in {parent}: dropped)",
                    commits(plan.own - kept)
                );
            }
            if let Some(push) = push {
                println!("its history was rewritten; force-push with: {push}");
            }
            Ok(())
        }
        follow::Moved::Pushed(pushed) => bail!("{}", pushed_said(name, &pushed)),
        follow::Moved::Merge => bail!(
            "{name}'s own commits include a merge, which gitfold doesn't move: rebase it by hand \
             ({})",
            plan.by_hand(parent)
        ),
        follow::Moved::Conflict(paths) => Err(Refusal::Changes.error(format!(
            "nothing moved: {}",
            conflict_said(name, parent, plan, &paths)
        ))),
    }
}

/// Why `name` doesn't move: `pushed`.
fn pushed_said(name: &str, pushed: &follow::Pushed) -> String {
    let are = if pushed.count == 1 { "is" } else { "are" };
    format!(
        "{} {are} pushed. `gitfold parent {name} --update --yes` moves them; then force-push \
         with: {}",
        commits(pushed.count),
        pushed.push
    )
}

/// Why `name` doesn't move onto `parent` as `plan` says: its commits
/// conflict on `paths`.
fn conflict_said(name: &str, parent: &str, plan: &follow::Plan, paths: &[String]) -> String {
    format!(
        "{name}'s commits conflict with {parent}'s on {}. Rebase by hand ({}), or wait for \
         {parent} to change",
        paths.join(", "),
        plan.by_hand(parent)
    )
}

/// `n commits`, `1 commit`.
fn commits(n: usize) -> String {
    match n {
        1 => "1 commit".to_string(),
        n => format!("{n} commits"),
    }
}

/// How a branch's following reads after its parent's name.
pub fn following(on: bool) -> &'static str {
    match on {
        true => "following it",
        false => "not following it",
    }
}

/// Checks `gitfold branch NEW --parent P` (in the repository of `repo`):
/// NEW doesn't exist yet (N1, refused as `branch` refuses an existing
/// branch), P resolves (N2), and the tree's rules for a new branch (N3, N4):
/// P's full ref.
pub fn check_new_parent(repo: &Path, branch: &str, parent: &str) -> Result<String> {
    let common_dir = git::init(repo)?;
    super::branch::check_new(&common_dir, branch)?;
    super::branch::check_branch_name(&common_dir, branch)?;
    anyhow::ensure!(
        !git::branch_exists(branch),
        "a branch named '{branch}' already exists"
    );
    let tree = Tree::read(&common_dir)?;
    tree.check_new(branch, parent)
        .map_err(|r| refused("gitfold branch", branch, parent, r))
}

// The arguments of a command people guess (`gitfold sync`, `pull`, `restack`,
// `rebase`): any, ignored (`//`, not `///`: no help text of its own).
#[derive(clap::Args)]
pub struct GuessArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    pub rest: Vec<String>,
}

/// What to say to a guessed command `name`: its right form.
pub fn guessed(name: &str) -> String {
    format!(
        "gitfold {name}: no such command. To take your parent's new commits: gitfold parent --update"
    )
}

/// The hint for `gitfold changes BRANCH -- PATH`: with paths it lists every
/// branch.
pub fn changes_hint(branch: bool) -> Option<String> {
    branch.then(|| "gitfold changes: with -- PATH it lists every branch; drop BRANCH".into())
}

/// The hint for `gitfold branch` flags people guess (`--on`, `--merge`), if
/// one was given.
pub fn branch_hint(on: Option<&[String]>, merge: Option<&[String]>) -> Option<String> {
    if on.is_some() {
        return Some("gitfold branch: no --on; to build on a branch: --parent P".into());
    }
    if let Some(merge) = merge {
        let froms: Vec<String> = match merge {
            [] => vec!["--from A".into(), "--from B".into()],
            _ => merge.iter().map(|m| format!("--from {m}")).collect(),
        };
        return Some(format!(
            "gitfold branch: no --merge; to start at several branches merged: {}",
            froms.join(" ")
        ));
    }
    None
}
