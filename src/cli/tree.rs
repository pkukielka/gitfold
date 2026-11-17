//! The tree of branches: a branch built on another (its parent) takes the
//! parent's new commits. The command-line contract of the design (`gitfold
//! parent`, `branch --parent`, `branch` with several `--from`, `changes --
//! PATH`) and the hints for commands people guess.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gix::ObjectId;

use crate::follow;
use crate::git;
use crate::refusal::Refusal;
use crate::settings;
use crate::state::retire::check_name;
use crate::tree::{Refused, Tree};

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
    // As the daemon and agents' turn starts take them (`follow_once`).
    #[arg(long, hide = true, requires = "update", conflicts_with = "yes")]
    pub auto: bool,
    // As Claude Code's hooks (`session-start`, `session-end`,
    // `subagent-stop`, `user-prompt-submit`): an agent's presence in its
    // workspace, and following at its turn start (`cli::claude::parent`).
    #[arg(long, value_enum, value_name = "EVENT", exclusive = true, hide = true)]
    pub claude_hook: Option<super::claude::ParentHook>,
}

/// `gitfold parent`: the checks of the rules table (S1-S6, F6), then shows
/// the parent (`show`), forgets it (`--none`, F5), turns following on or
/// off, moves the branch onto another parent (`--set`) or onto its parent's
/// new commits (`--update`); see `follow`.
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
    if args.update {
        match args.auto {
            true => update_auto(&common_dir, &name)?,
            false => update(&common_dir, &name, args.yes)?,
        }
        // Changed only once the update went through.
        if args.follow || args.no_follow {
            settings::set_follow(&common_dir, &name, args.follow)?;
        }
        return Ok(());
    }
    let tree = Tree::read(&common_dir)?;
    if args.follow || args.no_follow {
        tree.check_follow(&name).map_err(|r| refuse("", r))?;
    }
    let parent = tree.parent(&name).map(crate::tree::short);
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
            Ok(Some((_, full))) => println!("{full}"),
            Ok(None) => {}
            Err(e) => log::warn!("{name}: whether it can take its parent's commits: {e:#}"),
        }
        let local = settings::Local::read_checked(&common_dir)?;
        // Computed, not recorded: showing writes nothing.
        match landed_in(&common_dir, &tree, &local, &name, None, false) {
            Ok(Some(to)) => {
                let gone = tree.parent_gone(&name);
                let parent = tree.parent(&name).expect("landed: a parent");
                let itself = !gone && tree.follows(&name) && tree.root_refs().contains(&to);
                println!("{}", landed_hint(&name, parent, &to, gone, itself));
            }
            Ok(None) => {}
            Err(e) => log::warn!("{name}: whether its parent landed: {e:#}"),
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
/// can't take: a conflict recorded, or commits already pushed. In a few
/// words (`gitfold list`), and in full.
pub(crate) fn paused(
    common_dir: &Path,
    tree: &Tree,
    name: &str,
) -> Result<Option<(String, String)>> {
    let Some(parent) = tree.parent(name).filter(|_| !tree.parent_gone(name)) else {
        return Ok(None);
    };
    let plan = follow::Plan::new(common_dir, name, Some(parent), &tree.root_refs(), parent)?;
    if plan.up_to_date() {
        return Ok(None);
    }
    let local = settings::Local::read_checked(common_dir)?;
    let short = crate::tree::short(parent);
    if let Some(paths) = plan.conflict(common_dir, &local)? {
        let full = conflict_said(name, short, &plan, &paths);
        let few = format!("conflict on {}", paths.join(", "));
        return Ok(Some((few, format!("following paused: {full}"))));
    }
    let pushed = plan.pushed(common_dir)?;
    Ok(pushed.map(|p| {
        let few = "following paused: pushed".to_string();
        (few, format!("following paused: {}", pushed_said(name, &p)))
    }))
}

/// Where `name` moves: its workspace, through the mount (mounted again
/// where it last was if it isn't now and `again`: `mount::mount_or_again`),
/// or `None` (no worktree has it). Refused checked out in another git
/// worktree (git rebase there), with a git operation under way or a
/// conflict left unresolved in its workspace, or if the repository isn't
/// mounted (and never was, or not `again`).
/// (With `--set`, its parent is written by then: each refusal names
/// `--update` to finish.)
fn movable(common_dir: &Path, name: &str, again: bool) -> Result<Option<PathBuf>> {
    match follow::checked_out(common_dir, name) {
        None => Ok(None),
        Some(follow::CheckedOut::Workspace) => {
            let mount = match again {
                true => super::mount::mount_or_again(common_dir)?,
                false => crate::mount_table::mount_of(common_dir)?,
            };
            let Some(mount) = mount else {
                bail!(
                    "{} isn't mounted: mount it first (`gitfold mount`), then: `gitfold parent \
                     {name} --update` ({name} moves in its workspace)",
                    common_dir.display()
                );
            };
            let state = crate::state::StateDir::of(common_dir, name);
            if let Some(why) = state.git_busy() {
                return Err(Refusal::Changes.error(format!(
                    "{name}: {why} in its workspace; once it is done: `gitfold parent {name} --update`"
                )));
            }
            let dir = mount.join(name);
            // Neither committed nor stashed while paths are unmerged: resolved.
            let unmerged = follow::unmerged(&dir)?;
            if !unmerged.is_empty() {
                return Err(Refusal::Changes.error(format!(
                    "{name} has a conflict left unresolved in its workspace on {}: resolve it, \
                     then: `gitfold parent {name} --update`",
                    follow::quoted(&unmerged).join(", ")
                )));
            }
            Ok(Some(dir))
        }
        Some(follow::CheckedOut::Worktree(path)) => Err(Refusal::InUse.error(format!(
            "{name} is checked out in {path}: gitfold doesn't move it there; take the new \
             commits with git rebase in it"
        ))),
    }
}

/// `gitfold parent NAME --update [--yes]`: its parent's new commits taken
/// now (`follow::Plan::run`).
fn update(common_dir: &Path, name: &str, yes: bool) -> Result<()> {
    let _moving = follow::lock(common_dir, name)?;
    // Read with the branch's moves locked: a --set that ran meanwhile has
    // written its parent.
    let tree = Tree::read(common_dir)?;
    // F6, S4: only a branch with a parent.
    let refuse = |r| refused("gitfold parent", name, "", r);
    tree.check_follow(name).map_err(refuse)?;
    let parent = tree.parent(name).expect("check_follow: a parent (F6)");
    let short = crate::tree::short(parent);
    if tree.parent_gone(name) {
        bail!(
            "{name}'s parent {short} is gone: no new commits to take (`gitfold parent {name} \
             --none` keeps its history as it is)"
        );
    }
    let plan = follow::Plan::new(common_dir, name, Some(parent), &tree.root_refs(), parent)?;
    match plan.run(common_dir, yes, || movable(common_dir, name, true))? {
        follow::Moved::UpToDate => {
            println!("{name} is up to date with {short}");
            Ok(())
        }
        moved => said(name, short, &plan, moved),
    }
}

/// `gitfold parent NAME --update --auto`: its parent's new commits taken as
/// the automation takes them (`follow_once`), if it follows its parent: a
/// move said, a pause refused (exit 4).
fn update_auto(common_dir: &Path, name: &str) -> Result<()> {
    // F6, S4: only a branch with a parent.
    let tree = Tree::read(common_dir)?;
    let refuse = |r| refused("gitfold parent", name, "", r);
    tree.check_follow(name).map_err(refuse)?;
    match follow_once(common_dir, name) {
        Followed::Nothing => Ok(()),
        Followed::Moved(said) | Followed::Landed(said) => {
            print!("{said}");
            Ok(())
        }
        Followed::Noted(said) => {
            println!("{said}");
            Ok(())
        }
        Followed::Paused { full, .. } => {
            Err(Refusal::Changes.error(format!("following paused: {full}")))
        }
    }
}

/// What following did for a branch at a safe moment (`follow_once`).
pub enum Followed {
    /// Nothing to take, or it doesn't follow (F1, F2).
    Nothing,
    /// Its parent's new commits taken: what `--update` says.
    Moved(String),
    /// Its parent gone and landed: built where it landed, and what moved.
    Landed(String),
    /// Following paused: why in full, and in one line.
    Paused { full: String, short: String },
    /// Nothing moved, but its parent looks merged elsewhere: what to do.
    Noted(String),
}

/// Takes its parent's new commits for `name`, if it follows its parent
/// (spec section 3): the automation's policy (`--update --auto`, the
/// daemon, an agent's turn start). As `--update` without `--yes`, but a
/// conflict recorded against the parent's tip isn't tried again (paused
/// until the parent moves on, or it is rebased by hand; forgotten once it
/// is up to date), a move under way is waited for (up to a minute), and an
/// unmounted repository isn't mounted again. Its parent landed (`landed_in`,
/// spec section 4): once the parent's branch is deleted, it is built on
/// where the parent landed and moves there; while it exists, noted. A parent
/// gone without landing is noted (section 5).
pub fn follow_once(common_dir: &Path, name: &str) -> Followed {
    try_follow(common_dir, name).unwrap_or_else(|e| {
        let full = format!("{e:#}");
        let short = full.lines().next().unwrap_or_default().to_string();
        Followed::Paused { full, short }
    })
}

fn try_follow(common_dir: &Path, name: &str) -> Result<Followed> {
    if !Tree::read(common_dir)?.follows(name) {
        return Ok(Followed::Nothing);
    }
    // A move under way (the daemon's, begun before the agent came): waited
    // for, so it doesn't go on into the turn.
    let started = std::time::Instant::now();
    let _moving = loop {
        match follow::lock(common_dir, name) {
            Err(e) if Refusal::of(&e) == Some(Refusal::InUse) => {
                if started.elapsed() >= std::time::Duration::from_secs(60) {
                    let short = "a move is under way".to_string();
                    let full = format!("a move of {name} is under way; `gitfold parent` once done");
                    return Ok(Followed::Paused { full, short });
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            lock => break lock?,
        }
    };
    let tree = Tree::read(common_dir)?;
    let Some(parent) = tree.parent(name).filter(|_| tree.follows(name)) else {
        return Ok(Followed::Nothing);
    };
    let local = settings::Local::read_checked(common_dir)?;
    let gone = tree.parent_gone(name);
    // A parent gone: where it landed becomes the parent (spec section 4),
    // written first as `--set` does; else it keeps its commits (section 5).
    let (onto, landed) = match gone {
        false => (parent.to_string(), String::new()),
        // (A check that fails finds none: only noted, as when it finds none.)
        true => match landed_in(common_dir, &tree, &local, name, None, true).unwrap_or_else(|e| {
            log::warn!("{name}: whether its parent landed: {e:#}");
            None
        }) {
            Some(to) => {
                let landed = landed_said(parent, &to, true);
                let said = format!(
                    "{landed}; {name} is now built on {}\n",
                    crate::tree::short(&to)
                );
                (to, said)
            }
            None => {
                let parent = crate::tree::short(parent);
                return Ok(Followed::Noted(format!(
                    "{name}'s parent {parent} is gone, with no landing of it found: \
                     {name} keeps its commits as they are (`gitfold parent {name} --set P` moves \
                     its own onto P; `gitfold parent {name} --none` counts {parent}'s as its own)"
                )));
            }
        },
    };
    let short = crate::tree::short(&onto);
    let plan = follow::Plan::new(common_dir, name, Some(parent), &tree.root_refs(), &onto)?;
    if gone {
        let _tree = follow::tree_lock(common_dir)?;
        let now = Tree::read(common_dir)?;
        if now.parent(name) != Some(parent) || now.check(name, &onto).is_err() {
            let why = format!("{name}'s parent or {short} changed meanwhile; retry");
            return Err(Refusal::InUse.error(why));
        }
        follow::set_base(common_dir, name, plan.base)?;
        settings::set_parent(common_dir, name, &onto)?;
        settings::set_conflict(common_dir, name, None)?;
    }
    // Nothing moved: whether its parent looks merged somewhere, said once.
    let nothing = || -> Result<Followed> {
        if gone {
            return Ok(Followed::Landed(landed.clone()));
        }
        // Only a note: its failure doesn't pause following.
        Ok(
            match landed_in(common_dir, &tree, &local, name, Some(plan.base), true) {
                Ok(Some(to)) => {
                    let itself = tree.root_refs().contains(&to);
                    Followed::Noted(landed_hint(name, parent, &to, false, itself))
                }
                Ok(None) => Followed::Nothing,
                Err(e) => {
                    log::warn!("{name}: whether its parent landed: {e:#}");
                    Followed::Nothing
                }
            },
        )
    };
    if plan.up_to_date() {
        // Rebased by hand: its base recorded and a conflict forgotten, as
        // `--update` does (written only then).
        if follow::recorded_base(name) != Some(plan.onto) || local.conflict(name).is_some() {
            plan.run(common_dir, false, || Ok(None))?;
        }
        return nothing();
    }
    // (A conflict recorded is against the parent before a landing.)
    let moved = match plan.conflict(common_dir, &local)?.filter(|_| !gone) {
        Some(paths) => follow::Moved::Conflict(paths),
        None => plan
            .run(common_dir, false, || movable(common_dir, name, false))
            .map_err(|e| match gone {
                // Built there all the same: said.
                true => e.context(landed.trim_end().to_string()),
                false => e,
            })?,
    };
    let summary = match &moved {
        follow::Moved::UpToDate => return nothing(),
        follow::Moved::Done { .. } => None,
        follow::Moved::Conflict(paths) => Some(format!("conflict on {}", paths.join(", "))),
        follow::Moved::Pushed(pushed) => Some(pushed_count(pushed)),
        follow::Moved::Work(paths) => Some(format!(
            "uncommitted work in the way on {}",
            paths.join(", ")
        )),
        follow::Moved::Merge => Some("a merge among its own commits".to_string()),
        follow::Moved::Refused(_) => Some("git refused in its workspace".to_string()),
    };
    // A conflict as `gitfold parent` says it.
    let full = match &moved {
        follow::Moved::Conflict(paths) => Some(conflict_said(name, short, &plan, paths)),
        _ => None,
    };
    let mut out = landed.clone();
    let said = said_to(&mut out, name, short, &plan, moved).map_err(|e| format!("{e:#}"));
    Ok(match summary {
        // Moved, its work put back conflicting maybe.
        None => {
            let said = out + &said.err().map(|e| e + "\n").unwrap_or_default();
            match gone {
                true => Followed::Landed(said),
                false => Followed::Moved(said),
            }
        }
        Some(short) => Followed::Paused {
            full: landed + &(full.or(said.err())).unwrap_or_else(|| short.clone()),
            short,
        },
    })
}

/// Where the commits of the parent of `name` that it sits on landed (spec
/// section 4), if they did: the parent's own parent while it is known (its
/// settings kept), else the root (local, then origin's), each one `name`
/// may be built on. A parent gone counts only with where its own commits
/// start recorded. `base`: where they start, if known; what is checked is
/// recorded if `record` (`follow::landing`).
fn landed_in(
    common_dir: &Path,
    tree: &Tree,
    local: &settings::Local,
    name: &str,
    base: Option<ObjectId>,
    record: bool,
) -> Result<Option<String>> {
    let Some(parent) = tree.parent(name) else {
        return Ok(None);
    };
    let roots = tree.root_refs();
    if roots.iter().any(|r| r == parent) {
        return Ok(None);
    }
    let gone = tree.parent_gone(name);
    let base = match base {
        Some(base) => base,
        None => follow::own_start(common_dir, name, Some(parent), &roots)?,
    };
    if gone && follow::recorded_base(name) != Some(base) {
        return Ok(None);
    }
    // Its stored parent, as the config holds it (a deleted branch's
    // settings may outlive it, T2; `git branch -D` drops them).
    let own = (parent.strip_prefix("refs/heads/"))
        .and_then(|p| local.parents().remove(p))
        .map(|p| match p.starts_with("refs/") {
            true => p,
            false => format!("refs/heads/{p}"),
        });
    let mut targets: Vec<String> = Vec::new();
    for target in own.into_iter().chain(roots) {
        if target != parent && !targets.contains(&target) && tree.check(name, &target).is_ok() {
            targets.push(target);
        }
    }
    follow::landing(common_dir, local, name, base, &targets, !gone, record)
}

/// What is said of `parent` landing in `to` (full refs): it landed
/// (`gone`), or looks merged while its branch exists.
pub(crate) fn landed_said(parent: &str, to: &str, gone: bool) -> String {
    let (parent, to) = (crate::tree::short(parent), crate::tree::short(to));
    match gone {
        true => format!("{parent} landed in {to}"),
        false => format!("{parent} looks merged into {to}"),
    }
}

/// `landed_said`, with how `name` gets there: `--set`, or (`itself`: it
/// follows its parent, which landed in the root and still exists) the
/// parent's branch deleted (its own parent is forgotten with it).
fn landed_hint(name: &str, parent: &str, to: &str, gone: bool, itself: bool) -> String {
    let landed = landed_said(parent, to, gone);
    let (short, to) = (crate::tree::short(parent), crate::tree::short(to));
    let set =
        format!("`gitfold parent {name} --set {to}` moves {name} there with only its own commits");
    match itself {
        true => format!(
            "{landed}: {set} (or delete {short}, `git branch -d {short}`, and {name} moves there \
             by itself)"
        ),
        false => format!("{landed}: {set}"),
    }
}

/// `gitfold parent NAME --set P` (P as `given`): P its parent, following it
/// unless `follow` says otherwise (F3, F4), written first (with where its
/// own commits start, on the old parent), under the tree's lock with the
/// check that it makes no cycle (S1-S6); then its own commits moved onto
/// P's tip. A move cut short leaves it built on P with P's commits to take
/// (`--update`). With commits already pushed, it stays built on P without
/// the move (following paused until `--update --yes`); with a conflict too
/// (recorded against P: paused until P changes), exit 4. A merge among its
/// own commits is refused before anything is written. Going back is `--set`
/// the old parent.
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
    // Checked out in another git worktree: refused before anything changes
    // (where its workspace is is found once it moves).
    if let Some(follow::CheckedOut::Worktree(_)) = follow::checked_out(common_dir, name) {
        movable(common_dir, name, true)?;
    }
    let _moving = follow::lock(common_dir, name)?;
    // The plan first (git walks history; the tree lock isn't held for
    // that), on the old parent; a merge among its own commits is refused
    // before anything changes.
    let plan = follow::Plan::new(common_dir, name, old.as_deref(), &tree.root_refs(), &full)?;
    if !plan.up_to_date() && plan.merges(common_dir)? {
        return said(name, short, &plan, follow::Moved::Merge);
    }
    {
        // P checked again (it resolves the same, no cycle) and written
        // under the tree's lock, with where its own commits start.
        let _tree = follow::tree_lock(common_dir)?;
        let tree = Tree::read(common_dir)?;
        let now = tree.check(name, given).map_err(refuse)?;
        if now != full || tree.parent(name) != old.as_deref() {
            let why = format!("{name}'s parent or {given} changed meanwhile; retry");
            return Err(Refusal::InUse.error(why));
        }
        follow::set_base(common_dir, name, plan.base)?;
        settings::set_parent(common_dir, name, &full)?;
        settings::set_follow(common_dir, name, follow.unwrap_or(true))?;
        settings::set_conflict(common_dir, name, None)?;
    }
    // Refused or failed from here, it is built on P all the same.
    let moved = plan
        .run(common_dir, false, || {
            let workspace = movable(common_dir, name, true)?;
            println!("moves {} of {name} onto {short}", commits(plan.own));
            Ok(workspace)
        })
        .inspect_err(|_| println!("{name} is now built on {short}"))?;
    let paused = match follow.unwrap_or(true) {
        true => "; following paused",
        false => "",
    };
    match &moved {
        follow::Moved::Pushed(pushed) => {
            println!(
                "{name} is now built on {short}{paused}: {}",
                pushed_said(name, pushed)
            );
            Ok(())
        }
        // Built on P all the same, its conflict recorded: going back is
        // `--set` the old parent.
        follow::Moved::Conflict(_) => {
            println!("{name} is now built on {short}{paused}");
            said(name, short, &plan, moved)
        }
        // Refused before anything was written (above), the count kept.
        follow::Moved::Merge => unreachable!("a merge refused before the write"),
        // (With its work in the way, or git refusing: built on P, not
        // recorded.)
        follow::Moved::Work(_)
        | follow::Moved::Refused(_)
        | follow::Moved::UpToDate
        | follow::Moved::Done { .. } => {
            println!("{name} is now built on {short}");
            said(name, short, &plan, moved)
        }
    }
}

/// What a move of `name` onto `parent` (short) did, or why it didn't, said
/// (on standard output) or refused.
fn said(name: &str, parent: &str, plan: &follow::Plan, moved: follow::Moved) -> Result<()> {
    let mut out = String::new();
    let result = said_to(&mut out, name, parent, plan, moved);
    print!("{out}");
    result
}

/// `said`, its lines written to `out`.
fn said_to(
    out: &mut String,
    name: &str,
    parent: &str,
    plan: &follow::Plan,
    moved: follow::Moved,
) -> Result<()> {
    use std::fmt::Write;
    match moved {
        follow::Moved::UpToDate => Ok(()),
        follow::Moved::Done {
            kept,
            push,
            unmerged,
        } => {
            writeln!(out, "moved {} of {name} onto {parent}", commits(kept))?;
            if plan.own > kept {
                let dropped = commits(plan.own - kept);
                writeln!(out, "({dropped} already in {parent}: dropped)")?;
            }
            if let Some(push) = push {
                writeln!(out, "its history was rewritten; force-push with: {push}")?;
            }
            match unmerged.is_empty() {
                true => Ok(()),
                false => Err(Refusal::Changes.error(format!(
                    "its uncommitted work, put back, conflicts on {}: resolve it in its \
                     workspace (git keeps the work in the stash, `git stash list`)",
                    unmerged.join(", ")
                ))),
            }
        }
        follow::Moved::Work(paths) => Err(Refusal::Changes.error(format!(
            "nothing moved: {name}'s uncommitted work is in the way of {parent}'s commits on {}: \
             commit or stash it, then: `gitfold parent {name} --update`",
            paths.join(", ")
        ))),
        follow::Moved::Refused(said) => Err(Refusal::Changes.error(format!(
            "nothing moved: git refused in {name}'s workspace{}{said}\nonce what git says is \
             out of the way: `gitfold parent {name} --update`",
            if said.is_empty() { "" } else { ":\n" }
        ))),
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

/// How many of its commits are `pushed`: `2 commits are pushed`.
fn pushed_count(pushed: &follow::Pushed) -> String {
    let are = if pushed.count == 1 { "is" } else { "are" };
    format!("{} {are} pushed", commits(pushed.count))
}

/// Why `name` doesn't move: `pushed`.
fn pushed_said(name: &str, pushed: &follow::Pushed) -> String {
    format!(
        "{}. `gitfold parent {name} --update --yes` moves them; then force-push with: {}",
        pushed_count(pushed),
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
