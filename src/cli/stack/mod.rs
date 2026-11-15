//! Bases (stacked branches): `gitfold layer base` and `gitfold branch
//! --base`. A branch built on another branch, its base (`settings`:
//! `branch.<b>.gitfoldParent`, where its own commits start kept by
//! `refs/gitfold-stack/<b>`, see `git::stack`), has its own commits copied
//! onto the base's new ones: by `gitfold layer base <BASE>` again, or at the
//! start of its agent's turn (`layer --claude-hook user-prompt-submit` calls
//! `hook_context`), unless `branch.<b>.gitfoldRestack` is `manual` (`layer
//! base --frozen`). `layer base <TRUNK>` takes the base away (the branch is
//! built on the trunk, as one never given a base). gitfold never pushes; it
//! says what to force-push, and moves commits already pushed only when
//! asked (`--yes`). A `layer base` that can't move the branch changes
//! nothing: it is built on a new base only once moved there. The new base
//! is recorded as under way before the move (`gitfoldLanding`, as for a
//! landing), so a move interrupted after the branch moved (a crash, Ctrl-C)
//! is finished by the next `layer base` or turn start (`finish_landing`),
//! never moved back onto the old base.
//!
//! A move is computed first, moving nothing (`git::stack::restack`).
//! A conflict is recorded (`gitfoldRestackBlocked`) and reported, and the
//! branch stays on the old base. Otherwise where the branch lives decides
//! how it moves:
//!
//! - no workspace: its ref, with its stack ref, in one transaction (refused
//!   if a git worktree has it checked out);
//! - a workspace, mounted: `git read-tree -m -u C C'` in it (through the
//!   mount, so the kernel sees the files change; it keeps unstaged, staged
//!   and untracked work and refuses if any is on a path the move changes),
//!   then the refs; the sync thread's ordinary rebase then drops the files
//!   the read-tree copied up;
//! - a workspace, unmounted: only without uncommitted changes, under the
//!   repository lock: its index, its base (`fs::workspace::restack_closed`),
//!   the refs.
//!
//! Both workspace cases record an intent first (`state::restack`), so a
//! crash half-way is finished or undone. Workspaces showing layers aren't
//! moved (not yet): `layer base` refuses, and the commands advised remove
//! the layers first (`advice`).
//!
//! A base that landed (merged, squashed or rebased onto its own base or the
//! trunk, `land`) has its children moved the same way onto the landing
//! commit, with only their own commits (`reparent`): by the hook unasked,
//! or by `layer base <TARGET>` (then commits already pushed need `--yes`).
//! They are built on its target from then on, or, landed in the trunk, have
//! no base any more (`built_on`). A branch on the trunk isn't moved at turn
//! boundaries: following the trunk is on request.
//!
//! - `land`: a base's target, whether a parent base landed there (cached),
//!   the trunk.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use gix::ObjectId;
use gix::bstr::BString;

use super::plural;
use crate::git::{self, stack};
use crate::refusal::Refusal;
use crate::settings::{self, Local};
use crate::state::StateDir;
use crate::state::lens::{Applied, Lens};
use crate::state::restack::{self, Intent, Mode, Recovery};
use crate::state::retire::{check_name, index_matches_head};
use crate::store::Store;

pub(crate) mod land;

/// How many paths a line of output names before "and N more".
const SHOWN_PATHS: usize = 5;

/// `gitfold layer base TARGET`: builds branch `workspace` (default: the
/// workspace you are in) on `target` and moves its own commits onto it now,
/// or, on it already, onto its new commits; `frozen`: it doesn't follow
/// `target` by itself after that. Where its base landed (`target` named as
/// the landing's target) it follows the landing; the trunk as `target`
/// takes its base away (`unstack`). Commits already pushed move only with
/// `yes`. Refused before anything changes: a move that doesn't happen
/// leaves its base as it was. `all`: every branch following its base is
/// brought up to date, bottom-up (`target` unused).
pub fn base(
    repo: &Path,
    workspace: Option<String>,
    target: &str,
    frozen: bool,
    yes: bool,
    all: bool,
) -> Result<()> {
    if all && (frozen || workspace.is_some()) {
        super::usage("--all updates every branch with a base: it takes neither --frozen nor -w");
    }
    let common_dir = git::init(repo)?;
    if all {
        return base_all(&common_dir, yes);
    }
    let name = super::workspace_or_current(workspace, repo)?;
    check_name(&name)?;
    check_name(target)?;
    git::require_branch(&name)?;
    if name == target {
        super::usage(format!("{name} can't be built on itself"));
    }
    // First, so what is said is about where it is now.
    finish_landing(&common_dir, &name)?;
    let local = Local::read(&common_dir);
    let trunk = land::is_trunk(&common_dir, &local, target);
    let parent = local.parent(&name);
    if trunk && parent.is_none() {
        println!("{name}'s base is {target} already; nothing to do");
        return Ok(());
    }
    if let Some(why) = layers_shown(&common_dir, &name, target, frozen && !trunk)? {
        return Err(Refusal::Changes.error(format!("{name} {why}")));
    }
    // Where its base landed: the landing followed, only its own commits
    // moved (onto the landing commit, as the hook does).
    if let Some(parent) = &parent
        && !land::same(&common_dir, &local, parent, target)
        && let Some((landed_in, _)) = landing_of(&common_dir, &local, &name, parent)?
        && land::same(&common_dir, &local, &landed_in.name, target)
    {
        update_one(&common_dir, &name, yes)?;
        return frozen_after(&common_dir, &name, target, frozen && !trunk);
    }
    if trunk {
        return unstack(&common_dir, &name, target, yes);
    }
    match parent {
        // Again: brought up to date (onto where it landed, if it is gone).
        Some(parent) if parent == target => update_one(&common_dir, &name, yes)?,
        parent => rebase_onto(&common_dir, &name, parent.as_deref(), target, frozen, yes)?,
    }
    frozen_after(&common_dir, &name, target, frozen)
}

/// `layer base TARGET` for branch `name` built on `parent` (or on nothing),
/// not `target`: its own commits moved onto `target`'s tip, then built on
/// it (following it unless `frozen`); nothing changed if they can't be.
fn rebase_onto(
    common_dir: &Path,
    name: &str,
    parent: Option<&str>,
    target: &str,
    frozen: bool,
    yes: bool,
) -> Result<()> {
    let local = Local::read(common_dir);
    git::require_branch(target)?;
    check_cycle(&local, name, target)?;
    let (tip, target_tip) = (tip_of(common_dir, name)?, tip_of(common_dir, target)?);
    // Its own commits: those after its current base's, else those since it
    // and `target` met.
    let base = match parent {
        Some(parent) => {
            let resolved = land::resolve(common_dir, parent)?;
            own_base(common_dir, name, resolved.as_ref(), tip)?.ok_or_else(|| {
                anyhow!(
                    "{parent} is gone, and with it where {name}'s own commits start: rebase it \
                     onto {target} by hand"
                )
            })?
        }
        None => stack::Base {
            commit: git::merge_base(target_tip, tip)?,
            stored: stack_base(common_dir, name),
            guessed: false,
        },
    };
    if on_parent(common_dir, base.commit, target_tip, tip)? {
        declare(common_dir, name, target, base.commit)?;
        return update_one(common_dir, name, yes);
    }
    let blocked = local.restack_blocked(name);
    each(common_dir, &[name.to_string()], Some(target), |name| {
        if let Some(why) = ask(common_dir, name, base.commit, target, yes)? {
            return Ok(Outcome::Refused(why));
        }
        let new = stack::count(common_dir, base.commit, target_tip)?;
        // Recorded first, as for a landing (`reparent`): a branch moved but
        // still built on its old base would be moved back onto it.
        let follows = if frozen { "frozen" } else { "live" };
        let pending = format!("{target} {target_tip} {follows}");
        settings::set_landing(common_dir, name, Some(&pending))?;
        let outcome = copy_onto(
            common_dir,
            name,
            (target, target),
            &base,
            tip,
            target_tip,
            false,
        )?
        .outcome(new, base.guessed);
        match outcome.done() {
            // Built on it once moved there.
            true => {
                declare(common_dir, name, target, target_tip)?;
                settings::set_landing(common_dir, name, None)?;
            }
            false => {
                // Not moved: forgotten (unless an interrupted move may still
                // finish it).
                finish_landing(common_dir, name)?;
                // A conflict with what it isn't built on: not its block.
                settings::set_restack_blocked(common_dir, name, blocked.as_deref())?;
            }
        }
        Ok(outcome)
    })
}

/// Where base `parent` of branch `name` landed, if it did: its target and
/// the landing.
fn landing_of(
    common_dir: &Path,
    local: &Local,
    name: &str,
    parent: &str,
) -> Result<Option<(land::Target, stack::Landing)>> {
    let tip = tip_of(common_dir, name)?;
    let resolved = land::resolve(common_dir, parent)?;
    match own_base(common_dir, name, resolved.as_ref(), tip)? {
        Some(base) => landed(common_dir, local, name, parent, base.commit),
        None => Ok(None),
    }
}

/// Where base `parent` of branch `name`, its own commits starting after
/// `base`, landed, if it did: its target and the landing. A base that is
/// the trunk lands nowhere.
fn landed(
    common_dir: &Path,
    local: &Local,
    name: &str,
    parent: &str,
    base: ObjectId,
) -> Result<Option<(land::Target, stack::Landing)>> {
    if land::is_trunk(common_dir, local, parent) {
        return Ok(None);
    }
    let Some(target) = land::target(common_dir, local, parent)? else {
        return Ok(None);
    };
    let was_on = land::was_on_target(local, name, base, &target.name);
    Ok(land::landed(common_dir, base, &target, was_on)?.map(|landing| (target, landing)))
}

/// After a move asked for: whether `name`, if it still has a base, follows
/// `target` by itself from now on (not if `frozen`), said if not.
fn frozen_after(common_dir: &Path, name: &str, target: &str, frozen: bool) -> Result<()> {
    if Local::read(common_dir).parent(name).is_none() {
        return Ok(());
    }
    settings::set_restack_manual(common_dir, name, frozen)?;
    if frozen {
        println!(
            "frozen: {name} doesn't follow {target}'s new commits; `gitfold layer base \
             {target} --frozen` brings it up to date again"
        );
    }
    Ok(())
}

/// `gitfold layer base --all`: every branch with a base it follows brought
/// up to date, bottom-up; frozen ones, and ones on the trunk, are left.
fn base_all(common_dir: &Path, yes: bool) -> Result<()> {
    // Moves onto a new base interrupted: finished first, so each is
    // updated against the base it moved onto, frozen or not.
    for (branch, _) in Local::read(common_dir).parents() {
        finish_landing(common_dir, &branch)?;
    }
    let local = Local::read(common_dir);
    let (mut branches, frozen): (Vec<String>, Vec<String>) = (local.parents().into_iter())
        .filter(|(_, parent)| !land::is_trunk(common_dir, &local, parent))
        .map(|(b, _)| b)
        .partition(|b| !local.restack_manual(b));
    if !frozen.is_empty() {
        println!(
            "left as they are (frozen): {}; `gitfold layer base <BASE> --frozen -w <BRANCH>` \
             updates one",
            frozen.join(", ")
        );
    }
    if branches.is_empty() {
        println!("no branch with a base to update");
        return Ok(());
    }
    // Bottom-up: a branch moves onto its base's moved commits.
    branches.sort_by_cached_key(|b| (depth(&local, b), b.clone()));
    each(common_dir, &branches, None, |name| {
        update(common_dir, name, yes)
    })
}

/// Why branch `name` can't move onto `target` now, if its workspace shows
/// layers: a branch with a base can't show them (not yet), so a move isn't
/// tried. `frozen`: `target` is asked for with `--frozen`.
fn layers_shown(
    common_dir: &Path,
    name: &str,
    target: &str,
    frozen: bool,
) -> Result<Option<String>> {
    if !lensed(&StateDir::of(common_dir, name))? {
        return Ok(None);
    }
    Ok(Some(format!(
        "can't move while layers are shown: `gitfold layer remove --all`, then {}",
        base_command(target, frozen, true)
    )))
}

/// `gitfold layer base TARGET`, with `--frozen` if `frozen`; `tick`: in
/// backticks.
pub(super) fn base_command(target: &str, frozen: bool, tick: bool) -> String {
    let again = if frozen { " --frozen" } else { "" };
    let q = if tick { "`" } else { "" };
    format!("{q}gitfold layer base {target}{again}{q}")
}

/// Whether a move of branch `name` onto `target` is asked for with
/// `--frozen`: its base is frozen, and `target` isn't the trunk (which takes
/// the base away).
fn frozen_onto(common_dir: &Path, local: &Local, name: &str, target: &str) -> bool {
    local.restack_manual(name) && !land::is_trunk(common_dir, local, target)
}

/// `layers_shown` for a move of branch `name` onto `target` (as users name
/// it) by `restack_branch`, as its outcome.
fn layers_wait(
    common_dir: &Path,
    local: &Local,
    name: &str,
    target: &str,
) -> Result<Option<Outcome>> {
    let frozen = frozen_onto(common_dir, local, name, target);
    Ok(layers_shown(common_dir, name, target, frozen)?.map(Outcome::Waits))
}

/// What to run to bring branch `name` onto `target` (as users name it): `gitfold layer
/// base TARGET`, with `--frozen` for a frozen base (unless `target` is the
/// trunk, which takes the base away), after `gitfold layer remove --all` if
/// its workspace shows layers (a move waits for that), so the advice given
/// is never refused for it. `tick`: each command in backticks.
fn advice(common_dir: &Path, local: &Local, name: &str, target: &str, tick: bool) -> String {
    let base = base_command(target, frozen_onto(common_dir, local, name, target), tick);
    let q = if tick { "`" } else { "" };
    match lensed(&StateDir::of(common_dir, name)).unwrap_or(false) {
        true => format!("{q}gitfold layer remove --all{q}, then {base}"),
        false => base,
    }
}

/// Whether `name` is the trunk (see `land`): a branch on it has no base.
pub fn is_trunk(common_dir: &Path, name: &str) -> bool {
    land::is_trunk(common_dir, &Local::read(common_dir), name)
}

/// Refuses building `name` on `parent` if `parent` is built on `name`.
fn check_cycle(local: &Local, name: &str, parent: &str) -> Result<()> {
    let mut above = parent.to_string();
    while let Some(next) = local.parent(&above) {
        ensure!(
            next != name,
            "{parent} is built on {name} (through {above}): that would be a cycle"
        );
        above = next;
    }
    Ok(())
}

/// Builds `name` on `parent`, its own commits starting after `base`,
/// moving nothing.
pub fn declare(common_dir: &Path, name: &str, parent: &str, base: ObjectId) -> Result<()> {
    check_name(name)?;
    check_name(parent)?;
    git::require_branch(name)?;
    git::require_branch(parent)?;
    ensure!(name != parent, "a branch can't be built on itself");
    check_cycle(&Local::read(common_dir), name, parent)?;
    let tip = tip_of(common_dir, name)?;
    ensure!(
        stack::is_ancestor(common_dir, base, tip)?,
        "the base {base} isn't in {name}'s history"
    );
    settings::set_parent(common_dir, name, Some(parent))?;
    settings::set_restack_blocked(common_dir, name, None)?;
    stack::set_base(common_dir, name, base)?;
    land::record(common_dir, name, parent, base)
}

/// Forgets that `name` is built on another branch: its base, how it
/// follows it, a blocked move and a landing under way.
fn forget_base(common_dir: &Path, name: &str) -> Result<()> {
    settings::set_parent(common_dir, name, None)?;
    settings::set_restack_manual(common_dir, name, false)?;
    settings::set_restack_blocked(common_dir, name, None)?;
    settings::set_landing(common_dir, name, None)?;
    settings::set_base_on_target(common_dir, name, None)?;
    let _ = std::fs::remove_file(StateDir::of(common_dir, name).dir.join(TOLD));
    stack::drop_base(common_dir, name)
}

/// `gitfold layer base <TRUNK>` on a branch with a base: the base is taken
/// away, its own commits moved onto the trunk (named `trunk`; which commit:
/// `land::trunk_tip`).
fn unstack(common_dir: &Path, name: &str, trunk: &str, yes: bool) -> Result<()> {
    let local = Local::read(common_dir);
    let Some(parent) = local.parent(name) else {
        println!("{name}'s base is {trunk} already; nothing to do");
        return Ok(());
    };
    let off = format!("{name} is now built on {trunk} (was {parent})");
    // An interrupted move nobody can finish: the way out, as it was.
    let state = StateDir::of(common_dir, name);
    if state.opened()
        && matches!(restack::recover(&state), Ok(Recovery::Stuck(_)) | Err(_))
        && restack::forget(&state)?
    {
        forget_base(common_dir, name)?;
        println!("{off}; nothing moved, and an interrupted move's intent is forgotten");
        return Ok(());
    }
    let tip = tip_of(common_dir, name)?;
    let resolved = land::resolve(common_dir, &parent)?;
    let Some(base) = own_base(common_dir, name, resolved.as_ref(), tip)? else {
        forget_base(common_dir, name)?;
        println!(
            "{off}; nothing moved: {parent} is gone, and with it where {name}'s own commits \
             start (rebase it onto {trunk} by hand if it needs it)"
        );
        return Ok(());
    };
    let Some((trunk_ref, trunk_tip)) = land::trunk_tip(common_dir, &local, trunk)? else {
        return Err(Refusal::NotFound.error(format!("no branch {trunk}")));
    };
    if on_parent(common_dir, base.commit, trunk_tip, tip)? {
        forget_base(common_dir, name)?;
        println!("{off}; it has {trunk}'s tip already, nothing moved");
        return Ok(());
    }
    each(common_dir, &[name.to_string()], Some(trunk), |name| {
        if let Some(why) = ask(common_dir, name, base.commit, trunk, yes)? {
            return Ok(Outcome::Refused(why));
        }
        let new = stack::count(common_dir, base.commit, trunk_tip)?;
        let onto = (trunk, trunk_ref.as_str());
        // Recorded first (see `rebase_onto`).
        settings::set_landing(common_dir, name, Some(&format!("{trunk} {trunk_tip}")))?;
        let outcome = copy_onto(common_dir, name, onto, &base, tip, trunk_tip, false)?
            .outcome(new, base.guessed);
        if !outcome.done() {
            finish_landing(common_dir, name)?;
        }
        Ok(outcome)
    })?;
    forget_base(common_dir, name)?;
    println!("{off}");
    Ok(())
}

/// Says what moving the own commits of `name` (`base..` its tip) onto
/// `onto` does; why not, if some of them are pushed already (reachable from
/// its upstream, else `origin/<name>`) and `yes` doesn't say to.
fn ask(
    common_dir: &Path,
    name: &str,
    base: ObjectId,
    onto: &str,
    yes: bool,
) -> Result<Option<String>> {
    let tip = tip_of(common_dir, name)?;
    let own = stack::count(common_dir, base, tip)?;
    match own {
        0 => println!("moves {name} onto {onto} (it has no commits of its own)"),
        n => println!(
            "moves {} of {name} onto {onto}",
            plural(n, "commit", "commits")
        ),
    }
    let Some((remote, theirs, at)) = upstream(common_dir, name)?.filter(|_| !yes && own > 0) else {
        return Ok(None);
    };
    let Ok(meet) = git::merge_base(at, tip) else {
        return Ok(None);
    };
    let pushed = stack::count(common_dir, base, meet)?;
    Ok((pushed > 0).then(|| {
        format!(
            "{pushed} of them {} pushed already ({remote}/{theirs}): moving rewrites them, and \
             the branch then needs a push with --force-with-lease; --yes moves them",
            if pushed == 1 { "is" } else { "are" }
        )
    }))
}

/// Brings `name` up to date with its base, asked for: says what moves
/// first, and moves commits already pushed only with `yes`.
fn update(common_dir: &Path, name: &str, yes: bool) -> Result<Outcome> {
    // First, so what is said is about where it is now.
    finish_landing(common_dir, name)?;
    if let Outcome::Would { onto, base } = restack_branch(common_dir, name, true, false)?
        && let Some(why) = ask(common_dir, name, base, &onto, yes)?
    {
        return Ok(Outcome::Refused(why));
    }
    restack_branch(common_dir, name, false, false)
}

/// `update` for `name` alone, said as `each` says it.
fn update_one(common_dir: &Path, name: &str, yes: bool) -> Result<()> {
    each(common_dir, &[name.to_string()], None, |name| {
        update(common_dir, name, yes)
    })
}

/// What one branch's move did.
enum Outcome {
    /// Its base has no new commits.
    UpToDate,
    /// It has the base's tip already (rebased by hand): where its own
    /// commits start was moved there.
    BaseMoved,
    /// Moved onto its base: onto how many new commits of the base, how
    /// many of its own were dropped (made empty), the paths changed in its
    /// workspace (`None`: no workspace).
    Restacked {
        new: usize,
        dropped: usize,
        files: Option<Vec<BString>>,
        /// Where its own commits start was guessed (no stack ref, or a
        /// stale one).
        guessed: bool,
    },
    /// Only asked what it would do: its own commits (after `base`) would
    /// move onto `onto` (named).
    Would { onto: String, base: ObjectId },
    /// Its base landed: it moved onto the landing commit with its `own`
    /// commits (`dropped` made empty), now built on the target.
    Landed {
        landed: Landed,
        own: usize,
        dropped: usize,
        files: Option<Vec<BString>>,
    },
    /// At a turn boundary: its base looks squash-merged, but its branch
    /// is still there, so it is only said (`layer base` moves it).
    LooksLanded(Landed),
    /// A commit conflicts with what it would move onto (`onto`, named);
    /// `told` if that commit had blocked it already; `rebase` the command
    /// that does it by hand.
    Blocked {
        commit: ObjectId,
        paths: Vec<BString>,
        told: bool,
        onto: String,
        rebase: String,
    },
    /// Not now: why (uncommitted work in the way, git busy, layers shown).
    Waits(String),
    /// Not at all as it stands: why (a merge in its commits, no base,
    /// commits already pushed).
    Refused(String),
}

/// A base that landed on its target.
struct Landed {
    parent: String,
    /// The target's name as users know it, and as a pull request's base
    /// (`main` for `origin/main`).
    target: String,
    landing: stack::Landing,
}

impl Landed {
    /// `p1 looks squash- or rebase-merged into main as 1a2b3c4d5e6f`: a
    /// squash is found by content, so main making the same change on its
    /// own looks the same.
    fn said(&self) -> String {
        let (parent, target, at) = (&self.parent, &self.target, short(self.landing.at));
        match self.landing.how {
            stack::How::Merged => format!("{parent} was merged into {target} by {at}"),
            stack::How::Squashed => {
                format!("{parent} looks squash- or rebase-merged into {target} as {at}")
            }
            stack::How::FastForward => {
                format!("{parent} looks fast-forwarded into {target} up to {at}")
            }
        }
    }
}

impl Outcome {
    fn done(&self) -> bool {
        !matches!(
            self,
            Outcome::Blocked { .. } | Outcome::Waits(_) | Outcome::Refused(_)
        )
    }
}

/// Runs `restack` for each of `branches`, says what it did, one line each,
/// then the force-pushes to run; fails (exit 4) if any wasn't brought up to
/// date. `onto` names what they move onto in what is said (default: each
/// one's base).
fn each(
    common_dir: &Path,
    branches: &[String],
    onto: Option<&str>,
    restack: impl Fn(&str) -> Result<Outcome>,
) -> Result<()> {
    let mut failed = 0;
    let mut pushes = Vec::new();
    for name in branches {
        let outcome = restack(name).unwrap_or_else(|e| Outcome::Refused(format!("{e:#}")));
        // As now: finishing an interrupted move onto a landing changes it.
        let parent = (onto.map(str::to_string))
            .unwrap_or_else(|| Local::read(common_dir).parent(name).unwrap_or_default());
        failed += usize::from(!outcome.done());
        println!("{name}: {}", describe(common_dir, &outcome, name, &parent));
        if matches!(outcome, Outcome::Restacked { .. } | Outcome::Landed { .. })
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
        let what = match branches {
            [one] => format!("{one} not moved"),
            _ => format!("{failed} of {} not moved", branches.len()),
        };
        return Err(Refusal::Changes.error(format!("{what} (see above)")));
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

/// One line of `layer base` for `name` (built on `parent`).
fn describe(common_dir: &Path, outcome: &Outcome, name: &str, parent: &str) -> String {
    match outcome {
        Outcome::UpToDate => format!("up to date with {parent}"),
        Outcome::BaseMoved => {
            format!("has {parent}'s tip already; its own commits now start there")
        }
        Outcome::Restacked {
            new,
            dropped,
            files,
            guessed,
        } => {
            let mut line = format!("moved onto {parent}");
            if *new > 0 {
                let _ = write!(line, " ({})", commits(*new));
            }
            if *guessed {
                line += "; where its own commits start was guessed (from the base's reflog)";
            }
            line + &moved_details(*dropped, files)
        }
        Outcome::Would { onto, .. } => format!("would move onto {onto}"),
        Outcome::Landed {
            landed,
            own,
            dropped,
            files,
        } => {
            let mut line = format!(
                "{}; moved there with its own {} and now built on {}",
                landed.said(),
                plural(*own, "commit", "commits"),
                landed.target
            );
            line += &moved_details(*dropped, files);
            let _ = write!(
                line,
                ". Its pull request's base should now be {}: push it, then `gh pr edit {name} --base {}`",
                landed.target, landed.target
            );
            line
        }
        Outcome::LooksLanded(landed) => format!(
            "{}, but {} is still there; {} moves {name} there with only its own commits",
            landed.said(),
            landed.parent,
            advice(
                common_dir,
                &Local::read(common_dir),
                name,
                &landed.target,
                true
            ),
        ),
        Outcome::Blocked {
            commit,
            paths,
            onto,
            rebase,
            ..
        } => format!(
            "blocked: commit {} conflicts on {} with {onto}; nothing moved. Rebase by hand in \
             its workspace (`{rebase}`), then {}",
            short(*commit),
            shown(paths),
            advice(common_dir, &Local::read(common_dir), name, parent, true),
        ),
        Outcome::Waits(why) => format!("waits: {why}"),
        Outcome::Refused(why) => format!("not moved: {why}"),
    }
}

/// The end of a line of `describe` for a move: how many of its own commits
/// were made empty and dropped, how many files changed in its workspace.
fn moved_details(dropped: usize, files: &Option<Vec<BString>>) -> String {
    let mut out = String::new();
    if dropped > 0 {
        let _ = write!(out, "; {dropped} of its commits made empty, dropped");
    }
    if let Some(files) = files {
        let _ = write!(
            out,
            "; {} in its workspace",
            plural(files.len(), "file changed", "files changed")
        );
    }
    out
}

/// The paths a move changed in the session's workspace, for the hook.
fn under_you(files: Option<Vec<BString>>) -> String {
    match files.unwrap_or_default() {
        files if files.is_empty() => "no files changed under you".to_string(),
        files => format!("files changed under you: {}", shown(&files)),
    }
}

/// The parent base stored for `name`, if any.
fn stack_base(common_dir: &Path, name: &str) -> Option<ObjectId> {
    stack::rev(common_dir, &stack::stack_ref(name))
        .ok()
        .flatten()
}

/// Moves branch `name` onto its base's tip, if it moved, or onto where its
/// parent base landed, if its base was merged (see the module doc); with
/// `dry_run`, only says what it would do. `auto` (the hook, not
/// asked by anyone): a parent that only looks squash-merged (found by
/// content) is followed only if its branch is gone, here or on its remote;
/// else it is only said. Its callers finish an interrupted move first
/// (`finish_landing`).
fn restack_branch(common_dir: &Path, name: &str, dry_run: bool, auto: bool) -> Result<Outcome> {
    let local = Local::read(common_dir);
    let Some(parent) = local.parent(name) else {
        return Err(Refusal::NotFound.error(format!("{name} isn't built on another branch")));
    };
    let tip = tip_of(common_dir, name)?;
    let resolved = land::resolve(common_dir, &parent)?;
    let base = own_base(common_dir, name, resolved.as_ref(), tip)?;
    if let Some(base) = &base
        && let Some((target, landing)) = landed(common_dir, &local, name, &parent, base.commit)?
    {
        // Found by content, or by a record that may be stale (a target
        // reset to a later commit, say): followed unasked only once the
        // parent is gone.
        let guessed = matches!(landing.how, stack::How::Squashed | stack::How::FastForward);
        if auto && guessed && !land::gone(common_dir, &local, &parent)? {
            return Ok(Outcome::LooksLanded(Landed {
                parent: parent.clone(),
                target: target.pr_base.clone(),
                landing,
            }));
        }
        if let Some(why) = layers_wait(common_dir, &local, name, &target.pr_base)? {
            return Ok(why);
        }
        return reparent(
            common_dir, name, &parent, &target, landing, base, tip, dry_run,
        );
    }
    let (Some((_, parent_tip)), Some(base)) = (resolved, base) else {
        let trunk = land::main_name(common_dir, &local);
        return Ok(Outcome::Refused(format!(
            "its base {parent} is missing: `gitfold layer base <BRANCH>` builds it on another, \
             `gitfold layer base {trunk}` on {trunk} with no other base"
        )));
    };
    if on_parent(common_dir, base.commit, parent_tip, tip)? {
        if dry_run || base.stored == Some(parent_tip) {
            return Ok(Outcome::UpToDate);
        }
        stack::move_refs(
            common_dir,
            name,
            (tip, tip),
            (base.stored, parent_tip),
            "gitfold: base",
        )?;
        settings::set_restack_blocked(common_dir, name, None)?;
        land::record(common_dir, name, &parent, parent_tip)?;
        return Ok(match base.commit == parent_tip {
            true => Outcome::UpToDate,
            false => Outcome::BaseMoved,
        });
    }
    if let Some(why) = layers_wait(common_dir, &local, name, &parent)? {
        return Ok(why);
    }
    let new = stack::count(common_dir, base.commit, parent_tip)?;
    let onto = (parent.as_str(), parent.as_str());
    let copied = copy_onto(common_dir, name, onto, &base, tip, parent_tip, dry_run)?;
    match copied {
        Copied::Moved { .. } => land::record(common_dir, name, &parent, parent_tip)?,
        Copied::Dry => {
            return Ok(Outcome::Would {
                onto: parent,
                base: base.commit,
            });
        }
        Copied::Stop(_) => {}
    }
    Ok(copied.outcome(new, base.guessed))
}

/// Where the own commits of branch `name` (at `tip`) start, on its parent
/// (`resolved`: its full ref name and tip, if it exists). A merged parent is
/// often deleted: then its stack ref, if it is in the branch's history.
fn own_base(
    common_dir: &Path,
    name: &str,
    resolved: Option<&(String, ObjectId)>,
    tip: ObjectId,
) -> Result<Option<stack::Base>> {
    if let Some((full, parent_tip)) = resolved {
        return Ok(Some(stack::parent_base(
            common_dir,
            name,
            full,
            tip,
            *parent_tip,
        )?));
    }
    Ok(match stack_base(common_dir, name) {
        Some(commit) if stack::is_ancestor(common_dir, commit, tip)? => Some(stack::Base {
            commit,
            stored: Some(commit),
            guessed: false,
        }),
        _ => None,
    })
}

/// Moves branch `name`, whose parent `parent` landed on `target`, onto
/// the landing commit with only its own commits, and makes `target` its
/// parent.
///
/// Onto the landing commit S, not the target's tip: it is the smallest
/// rewrite (only the branch's own commits move, onto exactly what its
/// parent became), so nothing unrelated the target got since appears in
/// an agent's workspace mid-task, and a conflict can only be with the
/// parent's landed form, not with whatever else main did. The branch's
/// pull request shows the same diff either way (against the merge base).
/// Following the target's tip is a later, separate `layer base`.
#[allow(clippy::too_many_arguments)]
fn reparent(
    common_dir: &Path,
    name: &str,
    parent: &str,
    target: &land::Target,
    landing: stack::Landing,
    base: &stack::Base,
    tip: ObjectId,
    dry_run: bool,
) -> Result<Outcome> {
    let landed = Landed {
        parent: parent.to_string(),
        target: target.pr_base.clone(),
        landing,
    };
    let onto = format!("{} at {}", target.pr_base, short(landing.at));
    let at = short(landing.at);
    // Recorded first: the parent can't join the ref transaction, and a
    // branch moved but still stacked on its old parent would be restacked
    // back onto the parent's commits (`finish_landing`).
    let marker = format!("{} {}", target.name, landing.at);
    if !dry_run {
        settings::set_landing(common_dir, name, Some(&marker))?;
    }
    let copied = copy_onto(
        common_dir,
        name,
        (&onto, &at),
        base,
        tip,
        landing.at,
        dry_run,
    );
    let (files, dropped) = match copied {
        Ok(Copied::Moved { files, dropped }) => (files, dropped),
        Ok(Copied::Dry) => {
            return Ok(Outcome::Would {
                onto: format!("{} ({})", target.pr_base, landed.said()),
                base: base.commit,
            });
        }
        // Not moved, or moved there by another sync first (up to date).
        Ok(Copied::Stop(outcome)) => {
            finish_landing(common_dir, name)?;
            return Ok(outcome);
        }
        // Maybe half-way: `finish_landing` sorts it out next time.
        Err(e) => return Err(e),
    };
    built_on(common_dir, name, &target.name, landing.at)?;
    let own = stack::count(common_dir, landing.at, tip_of(common_dir, name)?)?;
    Ok(Outcome::Landed {
        landed,
        own,
        dropped,
        files,
    })
}

/// Finishes or forgets the move of branch `name` onto a new base (its
/// parent's landing, or another base) that `gitfoldLanding` records: its
/// stack ref at the recorded commit means the refs moved, so the target
/// becomes its parent (following it or not, as recorded); else the move
/// didn't happen, unless a workspace's interrupted move (its intent) may
/// still finish it: that is finished or undone first, if it can be.
fn finish_landing(common_dir: &Path, name: &str) -> Result<()> {
    let Some(marker) = Local::read(common_dir).landing(name) else {
        return Ok(());
    };
    let words: Vec<&str> = marker.split(' ').collect();
    let (&[target, at] | &[target, at, _]) = &words[..] else {
        return settings::set_landing(common_dir, name, None);
    };
    let state = StateDir::of(common_dir, name);
    if restack::pending(&state) && state.opened() {
        // Ours, or another process's running: then left to it.
        if let Err(e) = restack::recover(&state) {
            log::warn!("{name}: finishing an interrupted move: {e:#}");
        }
    }
    if stack_base(common_dir, name)
        .map(|b| b.to_string())
        .as_deref()
        == Some(at)
        && let Ok(at) = ObjectId::from_hex(at.as_bytes())
    {
        built_on(common_dir, name, target, at)?;
        if let Some(&follows) = words.get(2)
            && Local::read(common_dir).parent(name).is_some()
        {
            settings::set_restack_manual(common_dir, name, follows == "frozen")?;
        }
        return Ok(());
    } else if restack::pending(&state) {
        return Ok(());
    }
    settings::set_landing(common_dir, name, None)
}

/// Branch `name`, moved onto where its base landed (`at`, on `target`),
/// is built on `target` from now on: but a branch that landed in the trunk
/// has no base any more, as one never stacked.
fn built_on(common_dir: &Path, name: &str, target: &str, at: ObjectId) -> Result<()> {
    if land::is_trunk(common_dir, &Local::read(common_dir), target) {
        return forget_base(common_dir, name);
    }
    settings::set_parent(common_dir, name, Some(target))?;
    land::record(common_dir, name, target, at)?;
    settings::set_landing(common_dir, name, None)
}

/// What `copy_onto` did.
enum Copied {
    /// Moved: the paths changed in its workspace (`None`: no workspace),
    /// how many of its commits were made empty and dropped.
    Moved {
        files: Option<Vec<BString>>,
        dropped: usize,
    },
    /// `dry_run`: it would move.
    Dry,
    /// It didn't move: why.
    Stop(Outcome),
}

impl Copied {
    /// As a move onto `new` new commits of the base did (`guessed`: where
    /// its own commits start was guessed).
    fn outcome(self, new: usize, guessed: bool) -> Outcome {
        match self {
            Copied::Moved { files, dropped } => Outcome::Restacked {
                new,
                dropped,
                files,
                guessed,
            },
            Copied::Dry => Outcome::UpToDate,
            Copied::Stop(outcome) => outcome,
        }
    }
}

/// Copies the own commits of branch `name` (at `tip`, from `base`) onto
/// `onto` (named `onto_name` in messages, `onto_rev` in commands), and
/// moves the branch, its
/// stack ref (to `onto`) and its workspace's files there.
fn copy_onto(
    common_dir: &Path,
    name: &str,
    (onto_name, onto_rev): (&str, &str),
    base: &stack::Base,
    tip: ObjectId,
    onto: ObjectId,
    dry_run: bool,
) -> Result<Copied> {
    let local = Local::read(common_dir);
    let state = StateDir::of(common_dir, name);
    if lensed(&state)? {
        return Ok(Copied::Stop(Outcome::Waits(
            "can't move while layers are shown: `gitfold layer remove --all` first".into(),
        )));
    }
    let computed = git::with_repo(|repo| stack::restack(repo, base.commit, onto, tip))?;
    let (new_tip, dropped) = match computed {
        stack::Restack::Merge(commit) => {
            return Ok(Copied::Stop(Outcome::Refused(format!(
                "its commit {} is a merge, which isn't moved; rebase by hand",
                short(commit)
            ))));
        }
        stack::Restack::Conflict { commit, paths } => {
            let record = format!("{onto} {commit} {}", lossy(&paths).join(" "));
            let told = local
                .restack_blocked(name)
                .is_some_and(|b| b.starts_with(&onto.to_string()));
            if !dry_run && !told {
                settings::set_restack_blocked(common_dir, name, Some(&record))?;
            }
            return Ok(Copied::Stop(Outcome::Blocked {
                commit,
                paths,
                told,
                onto: onto_name.to_string(),
                rebase: format!("git rebase --onto {onto_rev} {}", short(base.commit)),
            }));
        }
        stack::Restack::Done { tip, dropped, .. } => (tip, dropped.len()),
    };
    if dry_run {
        return Ok(Copied::Dry);
    }
    if let Some(other) = git::worktree::checked_out_elsewhere(common_dir, name, &state.admin) {
        return Ok(Copied::Stop(Outcome::Refused(format!(
            "it is checked out in {other}; move it there (`git rebase --onto {onto_rev} {}`)",
            short(base.commit)
        ))));
    }
    let intent = Intent {
        mode: Mode::Open,
        from: tip,
        to: new_tip,
        parent_tip: onto,
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
            (base.stored, onto),
            "gitfold: move onto base",
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
        Moved::Already => return Ok(Copied::Stop(Outcome::UpToDate)),
        Moved::Waits(why) => return Ok(Copied::Stop(Outcome::Waits(why))),
    };
    settings::set_restack_blocked(common_dir, name, None)?;
    Ok(Copied::Moved { files, dropped })
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

/// Whether the workspace in `state`, if it has opened, shows layers, as
/// stored or as applied.
fn lensed(state: &StateDir) -> Result<bool> {
    Ok(state.opened() && (Lens::read(state)?.is_on() || Applied::read(state)?.layers()?.is_on()))
}

/// An interrupted restack of the workspace in `state` finished or undone
/// first (`state::restack`); why not, if it can't be.
fn recovered(state: &StateDir) -> Result<std::result::Result<(), String>> {
    Ok(match restack::recover(state)? {
        Recovery::Nothing | Recovery::Finished | Recovery::Undone => Ok(()),
        Recovery::InProgress => Err("another move of it is running".into()),
        Recovery::Stuck(why) => Err(why),
    })
}

/// How moving a branch for a restack ended.
enum Moved {
    /// Moved: the paths changed in its workspace (`None`: no workspace).
    Done(Option<Vec<BString>>),
    /// Another move (the hook and `layer base` at once) got there first.
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
        return Ok(Moved::Waits(format!("the branch moved meanwhile ({e:#})")));
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
            "uncommitted changes to {} overlap the move; commit or stash them",
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
            "its workspace has uncommitted changes; moved once it is mounted".into(),
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
        "gitfold: move onto base",
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

/// The notes `gitfold list` shows for stacked branch `name`: its base's
/// state (landed, missing), and whether it needs an update or a
/// force-push. A branch on the trunk isn't said to need one each time the
/// trunk moves: following it is `layer base`'s, on request.
pub fn notes(common_dir: &Path, local: &Local, name: &str) -> Vec<String> {
    let Some(parent) = local.parent(name) else {
        return Vec::new();
    };
    let Ok(tip) = tip_of(common_dir, name) else {
        return Vec::new();
    };
    let resolved = land::resolve(common_dir, &parent).ok().flatten();
    let base = own_base(common_dir, name, resolved.as_ref(), tip)
        .ok()
        .flatten();
    let trunk = land::is_trunk(common_dir, local, &parent);
    let mut notes = Vec::new();
    if let Some(base) = &base
        && let Ok(Some((target, landing))) = landed(common_dir, local, name, &parent, base.commit)
    {
        let landed = match landing.how {
            stack::How::Squashed | stack::How::FastForward => "looks landed",
            _ => "landed",
        };
        let advice = advice(common_dir, local, name, &target.pr_base, false);
        notes.push(format!(
            "{parent} {landed} in {}: {advice} moves it there",
            target.pr_base
        ));
    } else if let Some(blocked) = local.restack_blocked(name) {
        // `<onto> <commit> <paths>`.
        let paths: Vec<&str> = blocked.split(' ').skip(2).collect();
        notes.push(format!("blocked: conflicts on {}", shown_str(&paths)));
    } else if let (Some((_, parent_tip)), Some(base)) = (&resolved, &base) {
        if !trunk && !on_parent(common_dir, base.commit, *parent_tip, tip).unwrap_or(true) {
            let advice = advice(common_dir, local, name, &parent, false);
            notes.push(format!("needs update: {advice}"));
        }
    } else if resolved.is_none() {
        notes.push(format!("base {parent} missing"));
    }
    if let Ok(Some(push)) = force_push(common_dir, name) {
        notes.push(format!("push with: {push}"));
    }
    notes
}

/// `gitfold layer --claude-hook user-prompt-submit`'s part for bases: at
/// the start of a turn, the session's workspace is moved onto its base's
/// new commits (unless it is frozen), and what happened is said as context.
/// Nothing when nothing happened, or when what keeps it from happening was
/// told before (a block per base tip, anything else while it says the same:
/// `restack-told` in the state directory).
pub fn hook_context(common_dir: &Path, name: &str) -> Result<Option<String>> {
    // A move onto a new base interrupted: finished first, so the base it is
    // checked against is the one it moved onto.
    finish_landing(common_dir, name)?;
    let local = Local::read(common_dir);
    let Some(parent) = local.parent(name) else {
        return Ok(None);
    };
    // Not onto the trunk's every move: that is `layer base`'s, on request.
    if local.restack_manual(name) || land::is_trunk(common_dir, &local, &parent) {
        return Ok(None);
    }
    let told_file = StateDir::of(common_dir, name).dir.join(TOLD);
    let before = stack_base(common_dir, name);
    let outcome = restack_branch(common_dir, name, false, true)?;
    let say = |text: String| -> Result<Option<String>> {
        if std::fs::read_to_string(&told_file).is_ok_and(|told| told == text) {
            return Ok(None);
        }
        if told_file.parent().is_some_and(Path::exists) {
            crate::fsutil::write_atomic(&told_file, text.as_bytes())?;
        }
        Ok(Some(text))
    };
    if outcome.done() && !matches!(outcome, Outcome::LooksLanded(_)) {
        let _ = std::fs::remove_file(&told_file);
    }
    Ok(match outcome {
        Outcome::UpToDate | Outcome::BaseMoved | Outcome::Would { .. } => None,
        Outcome::Blocked { told: true, .. } => None,
        Outcome::Landed {
            landed, own, files, ..
        } => {
            let files = under_you(files);
            Some(format!(
                "gitfold: this branch's base {}; {name} is now built on {} with only its own \
                 {}; {files}. Its history was rewritten, so it needs a push with \
                 --force-with-lease, and then its pull request's base should be {}: suggest \
                 `gh pr edit {name} --base {}` to the user after that push rather than running \
                 it yourself.",
                landed.said(),
                landed.target,
                plural(own, "commit", "commits"),
                landed.target,
                landed.target
            ))
        }
        Outcome::Restacked {
            new,
            files,
            guessed,
            ..
        } => {
            let guessed = match guessed {
                true => {
                    " Where its own commits start wasn't recorded, so it was guessed from the \
                     base's reflog: check that only its own commits were moved."
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
            let files = under_you(files);
            Some(format!(
                "gitfold: this branch, {name}, was moved onto its base {parent}'s {}{what}; \
                 {files}. Its history was rewritten: push it with --force-with-lease when you \
                 push.{guessed}",
                commits(new)
            ))
        }
        Outcome::Blocked { .. } => Some(format!(
            "gitfold: {name} is built on {parent}, which has new commits; {}",
            describe(common_dir, &outcome, name, &parent)
        )),
        Outcome::LooksLanded(_) => say(format!(
            "gitfold: {name}'s base {}; suggest it to the user rather than running it \
             yourself: it rewrites {name}'s history.",
            describe(common_dir, &outcome, name, &parent)
        ))?,
        other => say(format!(
            "gitfold: {name} is built on {parent}, which has new commits; {}",
            describe(common_dir, &other, name, &parent)
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
    plural(n, "new commit", "new commits")
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
            "uncommitted changes to d/c.txt, n.txt overlap the move; commit or stash them"
        );
        let locked = "fatal: Unable to create '/x/index.lock': File exists.";
        assert_eq!(refused(locked), format!("git read-tree refused: {locked}"));
    }
}
