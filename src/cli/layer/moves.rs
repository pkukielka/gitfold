//! Moves of a branch's own commits: what an editable layer means for its
//! history. A branch built on editable layers (`state::layers`, `layer add
//! --edit`) has its own commits sitting on their commits: on one layer's
//! commit, or with several, on a merge of theirs gitfold makes and keeps
//! (`git::stack::merge_parts`, a fixed identity), rebuilt whenever one of
//! them moves. Where its own commits start is kept by `refs/gitfold-own/<b>`.
//!
//! One path moves them, whatever asked (`move_onto`):
//!
//! - `layer add A --edit` (A becomes editable, or is brought up to date);
//! - `layer add A` on an editable A (it becomes see-only), `layer remove A`
//!   (it goes): the commits come off A, onto the other editable layers or
//!   main; refused, listing them, if some change A's lines (they can't
//!   stand without A);
//! - `branch NEW --edit A` (no move: it starts at A's tip);
//! - the start of its agent's turn (`hook_context`): onto its live editable
//!   layers' new commits, and onto where one landed once its branch is gone.
//!
//! It says first what it moves; commits already pushed move only with
//! `--yes` when asked (the hook isn't asked, and says what to force-push).
//! A move is computed first, moving nothing (`git::stack::compute_move`): a
//! conflict, or editable layers conflicting with each other, leaves
//! everything as it was, and is said. Otherwise where the branch lives
//! decides who moves it: its ref alone with no workspace; its workspace,
//! mounted (the daemon) or not (here, under the repository lock), through
//! `fs::workspace::moves`, which moves its files and base with the refs,
//! layers shown or not.
//!
//! A move that changes what the branch builds on records the layers it
//! will have as `layers.next` first (`state::layers::settle`): they become
//! its layers once its own ref is where the move puts it, so a crash
//! half-way never leaves it built on what it was moved off.
//!
//! Landings (`land`): an editable layer A whose commits the branch sits on
//! landed in its target (main, or the one branch A builds on) by a merge,
//! squash, rebase or fast-forward. Followed by itself only once A's branch
//! is gone; until then it is said, and `layer remove A` follows it: only
//! the branch's own commits move, onto the landing commit (or a merge of it
//! and the other editable layers).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use gix::ObjectId;
use gix::bstr::BString;

use super::land;
use crate::cli::plural;
use crate::fs::workspace::Moved;
use crate::git::{self, stack};
use crate::refusal::Refusal;
use crate::settings::Local;
use crate::state::StateDir;
use crate::state::layers::{self, Layer, Layers, Updates};
use crate::state::moves::{self, Intent};
use crate::store::Store;

/// How many paths a line of output names before "and N more".
const SHOWN_PATHS: usize = 5;

/// A part of what a branch builds on: an editable layer's name (or the
/// main branch's, where one landed) and the commit of it the branch sits on.
pub type Part = (String, ObjectId);

/// The hook's last message about why a workspace wasn't moved, in its
/// state directory: said once while it stays the same.
const TOLD: &str = "move-told";

/// The move of a branch that conflicted, in its state directory: `<onto>
/// <commit> <paths...>`, so the hook says it once per commit it conflicts
/// with, and `list` notes it.
const BLOCKED: &str = "move-blocked";

/// What a move did.
pub enum Outcome {
    /// Nothing new to move onto.
    UpToDate,
    /// It has what it would move onto already (rebased by hand): where its
    /// own commits start was moved there.
    BaseMoved,
    /// Moved: how many of its own commits were dropped (made empty), the
    /// paths changed in its workspace (`None`: no workspace).
    Moved {
        dropped: usize,
        files: Option<Vec<BString>>,
    },
    /// One of its commits conflicts with what it would move onto (`onto`,
    /// named); `told` if that had blocked it already.
    Blocked {
        commit: ObjectId,
        paths: Vec<BString>,
        told: bool,
        onto: String,
    },
    /// Its editable layers conflict with each other on `paths`.
    Conflicting(Vec<BString>),
    /// Not now: why (uncommitted work in the way, git busy).
    Waits(String),
    /// Not at all as it stands: why (commits already pushed, commits that
    /// can't stand without a layer, a merge in its commits).
    Refused(String),
}

impl Outcome {
    pub fn done(&self) -> bool {
        matches!(
            self,
            Outcome::UpToDate | Outcome::BaseMoved | Outcome::Moved { .. }
        )
    }

    /// What it did, in a line (after `<branch>: `).
    pub fn said(&self) -> String {
        match self {
            Outcome::UpToDate => "up to date".into(),
            Outcome::BaseMoved => "has it all already; its own commits now start there".into(),
            Outcome::Moved { dropped, files } => {
                let mut line = "moved".to_string();
                if *dropped > 0 {
                    let _ = write!(line, "; {dropped} of its commits made empty, dropped");
                }
                if let Some(files) = files {
                    let _ = write!(
                        line,
                        "; {} in its workspace",
                        plural(files.len(), "file changed", "files changed")
                    );
                }
                line
            }
            Outcome::Blocked {
                commit,
                paths,
                onto,
                ..
            } => format!(
                "blocked: commit {} conflicts on {} with {onto}; nothing moved. Rebase it by \
                 hand, then retry",
                short(*commit),
                shown(paths)
            ),
            Outcome::Conflicting(paths) => format!(
                "not moved: its editable layers conflict with each other on {}; nothing moved",
                shown(paths)
            ),
            Outcome::Waits(why) => format!("waits: {why}"),
            Outcome::Refused(why) => format!("not moved: {why}"),
        }
    }
}

/// What a branch builds on, read: its layers and where its own commits
/// start (`None`: no own ref).
pub struct Built {
    pub layers: Layers,
    pub own: Option<ObjectId>,
    /// The parts its own commits sit on now (`current_parts`).
    pub parts: Vec<Part>,
}

impl Built {
    /// What branch `name` builds on now; a move under way first settled.
    /// `lock` held (`state::layers::lock`).
    pub fn read(common_dir: &Path, name: &str) -> Result<Built> {
        let state = StateDir::of(common_dir, name);
        if state.opened() && moves::pending(&state) {
            let _ = moves::recover(&state);
        }
        layers::settle(&state)?;
        let layers = Layers::read(&state)?;
        let own = stack::rev(common_dir, &stack::own_ref(name))?;
        let parts = current_parts(own, &layers)?;
        Ok(Built { layers, own, parts })
    }

    /// The commit of editable layer `name` the branch sits on, if known.
    pub fn on(&self, name: &str) -> Option<ObjectId> {
        self.parts.iter().find(|(n, _)| n == name).map(|(_, c)| *c)
    }
}

/// The parts a branch's own commits sit on, from its own ref `own`: the
/// merge's (`git::stack::parts_of`), or with one editable layer, the ref
/// itself.
fn current_parts(own: Option<ObjectId>, layers: &Layers) -> Result<Vec<Part>> {
    let Some(own) = own else {
        return Ok(Vec::new());
    };
    if let Some(parts) = git::with_repo(|repo| stack::parts_of(repo, own))? {
        return Ok(parts);
    }
    let editable = layers.editable_names();
    Ok(match &editable[..] {
        [one] => vec![(one.clone(), own)],
        _ => Vec::new(),
    })
}

/// The parts branch `built`'s own commits should sit on: each editable
/// layer's tip if it is live (or named in `refresh`), else the commit it
/// sits on now; then what a landing left (the main branch at its landing
/// commit), while other editable layers remain.
pub fn wanted_parts(built: &Built, refresh: &[&str]) -> Result<Vec<Part>> {
    let mut parts = Vec::new();
    for l in built.layers.editable() {
        let now = built.on(&l.branch);
        let follow = l.updates == Updates::Live || refresh.contains(&l.branch.as_str());
        let commit = match (follow || now.is_none(), git::branch_exists(&l.branch)) {
            (true, true) => git::branch_tip(&l.branch)?.id,
            _ => match now {
                Some(now) => now,
                None => continue,
            },
        };
        parts.push((l.branch.clone(), commit));
    }
    if !parts.is_empty() {
        let names = built.layers.editable_names();
        parts.extend(
            (built.parts.iter())
                .filter(|(n, _)| !names.contains(n))
                .cloned(),
        );
    }
    Ok(parts)
}

/// Where a move needs to go.
pub struct Plan<'a> {
    pub name: &'a str,
    /// What the own commits go onto: the merge of these (none: main's tip).
    pub parts: Vec<Part>,
    /// The layers it will have once moved, if they change.
    pub next: Option<Layers>,
    /// Asked for (a command): commits already pushed need `yes`, and
    /// what it moves is said first.
    pub asked: bool,
    pub yes: bool,
    /// A layer the own commits come off: they mustn't change its lines.
    pub off: Option<&'a str>,
    /// What it moves onto, as said (`onto api`, `off api, onto main`).
    pub said: String,
    /// Said before what it moves (`api becomes see-only: `).
    pub prefix: String,
}

/// The commit the parts merge to (none: main's tip), or the paths they
/// conflict on.
pub fn onto_of(
    common_dir: &Path,
    parts: &[Part],
) -> Result<std::result::Result<ObjectId, Vec<BString>>> {
    if parts.is_empty() {
        let local = Local::read(common_dir);
        let main = land::main_name(common_dir, &local);
        return match land::trunk_tip(common_dir, &local, &main)? {
            Some((_, tip)) => Ok(Ok(tip)),
            None => Err(Refusal::NotFound.error(format!("no main branch {main}"))),
        };
    }
    git::with_repo(|repo| stack::merge_parts(repo, parts))
}

/// Moves the own commits of `plan.name` onto `plan.parts` (see the module
/// doc); with `plan.next`, the layers it will have once moved, stored only
/// if it moves. `lock` held on its layers.
pub fn move_onto(common_dir: &Path, plan: &Plan) -> Result<Outcome> {
    let name = plan.name;
    let state = StateDir::of(common_dir, name);
    let tip = tip_of(common_dir, name)?;
    let own = stack::rev(common_dir, &stack::own_ref(name))?;
    let onto = match onto_of(common_dir, &plan.parts)? {
        Ok(onto) => onto,
        Err(paths) => return Ok(Outcome::Conflicting(paths)),
    };
    // Where its own commits start: its own ref, else where it left main, or
    // where it and what it moves onto met, whichever is newer (a branch on
    // main has main's commits, not its own).
    let base = match own {
        Some(own) if stack::is_ancestor(common_dir, own, tip)? => own,
        _ => {
            let met = git::merge_base(onto, tip)?;
            let local = Local::read(common_dir);
            let main = land::main_name(common_dir, &local);
            match land::trunk_tip(common_dir, &local, &main)? {
                Some((_, main_tip)) => match git::merge_base_if_any(main_tip, tip)? {
                    Some(left) if stack::is_ancestor(common_dir, met, left)? => left,
                    _ => met,
                },
                None => met,
            }
        }
    };
    let store_next = |plan: &Plan| -> Result<()> {
        if let Some(next) = &plan.next {
            next.write(&state)?;
            if next.editable().next().is_none() {
                stack::drop_base(common_dir, name)?;
            }
        }
        Ok(())
    };
    if base == onto
        || (stack::is_ancestor(common_dir, base, onto)?
            && stack::is_ancestor(common_dir, onto, tip)?)
    {
        // Nothing to copy: at most where its own commits start moves.
        let up_to_date = own == Some(onto);
        if !up_to_date {
            stack::move_refs(
                common_dir,
                name,
                (tip, tip),
                (own, onto),
                "gitfold: own commits",
            )?;
        }
        store_next(plan)?;
        clear_blocked(&state);
        return Ok(match up_to_date || base == onto {
            true => Outcome::UpToDate,
            false => Outcome::BaseMoved,
        });
    }
    if let Some(off) = plan.off {
        let dependent = git::with_repo(|repo| stack::dependent(repo, base, onto, tip))?;
        if !dependent.is_empty() {
            return Ok(Outcome::Refused(depends_on(common_dir, off, &dependent)?));
        }
    }
    let computed = git::with_repo(|repo| stack::compute_move(repo, base, onto, tip))?;
    let (new_tip, dropped) = match computed {
        stack::Move::Merge(commit) => {
            return Ok(Outcome::Refused(format!(
                "its commit {} is a merge, which isn't moved; rebase by hand",
                short(commit)
            )));
        }
        stack::Move::Conflict { commit, paths } => {
            let told = blocked(&state).is_some_and(|b| b.starts_with(&format!("{onto} {commit}")));
            if plan.asked || !told {
                record_blocked(&state, onto, commit, &paths);
            }
            return Ok(Outcome::Blocked {
                commit,
                paths,
                told,
                onto: plan.said.trim_start_matches("onto ").to_string(),
            });
        }
        stack::Move::Done { tip, dropped, .. } => (tip, dropped.len()),
    };
    let own_count = stack::count(common_dir, base, tip)?;
    if plan.asked {
        println!(
            "{}moves {} of {name} {}",
            plan.prefix,
            plural(own_count, "commit", "commits"),
            plan.said
        );
        if !plan.yes
            && let Some(why) = pushed(common_dir, name, base, tip)?
        {
            return Ok(Outcome::Refused(why));
        }
    }
    if let Some(other) = git::worktree::checked_out_elsewhere(common_dir, name, &state.admin) {
        return Ok(Outcome::Refused(format!(
            "it is checked out in {other}; move it there (`git rebase --onto {onto} {}`)",
            short(base)
        )));
    }
    if let Some(next) = &plan.next {
        layers::write_next(&state, onto, next)?;
    }
    let intent = Intent {
        from: tip,
        to: new_tip,
        own_to: onto,
        own_from: own,
    };
    let moved = move_branch(&state, intent);
    let moved = match moved {
        Ok(moved) => moved,
        Err(e) => {
            // Maybe half-way: settled by whoever comes next.
            return Err(e);
        }
    };
    layers::settle(&state)?;
    Ok(match moved {
        Moved::Done(files) => {
            clear_blocked(&state);
            Outcome::Moved {
                dropped,
                files: state.opened().then_some(files),
            }
        }
        Moved::Already => Outcome::UpToDate,
        Moved::Waits(why) => Outcome::Waits(why),
    })
}

/// Why commits `dependent` (each with the paths it conflicts on) can't
/// come off layer `off`.
fn depends_on(
    common_dir: &Path,
    off: &str,
    dependent: &[(ObjectId, Vec<BString>)],
) -> Result<String> {
    let mut lines = Vec::new();
    for (commit, paths) in dependent {
        let subject = git::cmd::output(
            common_dir,
            &["log", "-1", "--format=%s", &commit.to_string()],
        )?;
        let subject = String::from_utf8_lossy(&subject).trim().to_string();
        let paths = match paths.is_empty() {
            true => "a merge".to_string(),
            false => shown(paths),
        };
        lines.push(format!("{} {subject} ({paths})", short(*commit)));
    }
    Ok(format!(
        "{} change {off}'s lines, so they can't stand without {off}: {}. Keep {off} \
         editable, or change them not to depend on its lines",
        plural(dependent.len(), "commit", "commits"),
        lines.join("; ")
    ))
}

/// Moves the branch of `state` as `intent` says, where it lives: its
/// workspace mounted (the daemon), unmounted (here, under the repository
/// lock), or only its refs if it has none.
fn move_branch(state: &StateDir, intent: Intent) -> Result<Moved> {
    let (common_dir, name) = (&state.common_dir, &state.branch);
    if !state.opened() {
        let refs = stack::move_refs(
            common_dir,
            name,
            (intent.from, intent.to),
            (intent.own_from, intent.own_to),
            "gitfold: move",
        );
        return match refs {
            Ok(()) => Ok(Moved::Done(Vec::new())),
            Err(_) if stack::rev(common_dir, &format!("refs/heads/{name}"))? == Some(intent.to) => {
                Ok(Moved::Already)
            }
            Err(e) => Err(e),
        };
    }
    if let Some(mount) = crate::mount_table::mount_of(common_dir)? {
        // Opened first: the daemon moves only open workspaces.
        std::fs::metadata(mount.join(name))?;
        let request = crate::daemon::control::Request::Move {
            branch: name.clone(),
            intent,
        };
        // No answer: unmounted meanwhile.
        if let Some(answer) = crate::daemon::control::request(common_dir, &request, None)? {
            return Moved::decode(&answer);
        }
    }
    let Some(_lock) = crate::lock::lock(common_dir)? else {
        return Ok(Moved::Waits(
            "the repository is being mounted or unmounted; retry".into(),
        ));
    };
    let store = Store::default();
    let ws = crate::fs::workspace::Workspace::open(&store, state.clone(), None)?;
    let layers = layers::next_or_stored(state)?;
    ws.move_branch(&store, intent, layers)
}

/// Why a move of the own commits of `name` (`base..tip`) needs `--yes`:
/// some are pushed already (reachable from its upstream, else
/// `origin/<name>`).
fn pushed(common_dir: &Path, name: &str, base: ObjectId, tip: ObjectId) -> Result<Option<String>> {
    let Some((remote, theirs, at)) = upstream(common_dir, name)? else {
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

/// What to run to push `name` after a move, if it has a remote-tracking
/// branch the move left behind: a push with a lease on that tip, so a
/// push someone else made meanwhile is refused.
pub fn force_push(common_dir: &Path, name: &str) -> Result<Option<String>> {
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

/// Says `outcome` for `name` (`<name>: ...`) and, if it moved, the
/// force-push to run; fails (exit 4) if it didn't move.
pub fn say(common_dir: &Path, name: &str, outcome: &Outcome) -> Result<()> {
    if !matches!(outcome, Outcome::UpToDate) {
        println!("{name}: {}", outcome.said());
    }
    if matches!(outcome, Outcome::Moved { .. })
        && let Some(push) = force_push(common_dir, name)?
    {
        println!("to push (rewritten since last push):\n  {push}");
    }
    if !outcome.done() {
        return Err(Refusal::Changes.error(format!("{name} not moved (see above)")));
    }
    Ok(())
}

/// Says what parts a branch sits on: `api`, `api and db`, `main`.
pub fn onto_words(common_dir: &Path, parts: &[Part]) -> String {
    match parts.is_empty() {
        true => land::main_name(common_dir, &Local::read(common_dir)),
        false => layers::and_list(parts.iter().map(|(n, _)| n.clone())),
    }
}

/// A landing of an editable layer: where it landed, said.
pub struct Landed {
    pub layer: String,
    /// Its target as users know it (`main` for `origin/main`).
    pub target: String,
    /// The target's name as configured (a branch, or the trunk).
    pub target_name: String,
    pub landing: stack::Landing,
    /// Whether the layer's branch is gone (deleted here, or on its remote).
    pub gone: bool,
}

impl Landed {
    /// `api looks merged into main as 1a2b3c4d5e6f`: a squash is found by
    /// content, so main making the same change on its own looks the same.
    pub fn said(&self) -> String {
        let (layer, target, at) = (&self.layer, &self.target, short(self.landing.at));
        match self.landing.how {
            stack::How::Merged => format!("{layer} was merged into {target} by {at}"),
            stack::How::Squashed => format!("{layer} looks merged into {target} as {at}"),
            stack::How::FastForward => {
                format!("{layer} looks fast-forwarded into {target} up to {at}")
            }
        }
    }
}

/// Where editable layer `layer` (the branch sits on its commit `on`)
/// landed, if it did: in its target, the one branch it builds on itself,
/// else main.
pub fn landed(common_dir: &Path, layer: &str, on: ObjectId) -> Result<Option<Landed>> {
    let local = Local::read(common_dir);
    if land::is_trunk(common_dir, &local, layer) {
        return Ok(None);
    }
    let Some(target) = land::target(common_dir, &local, layer)? else {
        return Ok(None);
    };
    let gone = land::gone(common_dir, &local, layer)?;
    // On main's first-parent line: fast-forwarded, once the branch is gone
    // (while it is there, a fresh branch looks the same).
    let was_on = gone.then_some(false);
    Ok(
        land::landed(common_dir, on, &target, was_on)?.map(|landing| Landed {
            layer: layer.to_string(),
            target: target.pr_base.clone(),
            target_name: target.name.clone(),
            landing,
            gone,
        }),
    )
}

/// The landings of the editable layers of `built`.
pub fn landings(common_dir: &Path, built: &Built) -> Vec<Landed> {
    (built.layers.editable())
        .filter_map(|l| {
            let on = built.on(&l.branch)?;
            landed(common_dir, &l.branch, on).ok().flatten()
        })
        .collect()
}

/// The layers and parts after following `landing` (of one of `built`'s
/// editable layers): the layer goes; its target becomes editable if it is
/// a branch the branch doesn't build on yet; its part becomes the landing
/// commit, under the target's name.
pub fn after_landing(
    common_dir: &Path,
    built: &Built,
    landing: &Landed,
    parts: &[Part],
) -> Result<(Layers, Vec<Part>)> {
    let mut next = built.layers.clone();
    next.layers.retain(|l| l.branch != landing.layer);
    let local = Local::read(common_dir);
    let trunk = land::is_trunk(common_dir, &local, &landing.target_name);
    if !trunk
        && next
            .find(&landing.target_name)
            .is_none_or(|i| !next.layers[i].edit)
    {
        let theirs = editable_layer(common_dir, &landing.target_name)?;
        match next.find(&landing.target_name) {
            Some(i) => next.layers[i] = theirs,
            None => next.layers.push(theirs),
        }
    }
    let mut out: Vec<Part> = parts
        .iter()
        .filter(|(n, _)| *n != landing.layer && *n != landing.target_name)
        .cloned()
        .collect();
    let name = match trunk {
        true => land::main_name(common_dir, &local),
        false => landing.target_name.clone(),
    };
    // With nothing else to build on, the landing commit alone.
    if trunk && next.editable().next().is_none() {
        out.clear();
    }
    out.insert(0, (name, landing.landing.at));
    Ok((next, out))
}

/// Editable layer `branch` as a branch's own layer: its work if it has a
/// workspace, else its commits, live.
fn editable_layer(common_dir: &Path, branch: &str) -> Result<Layer> {
    let theirs = StateDir::of(common_dir, branch);
    let local = Local::read(common_dir);
    let (snapshot, shows) = match theirs.opened() {
        true => match crate::state::follow::latest_work(&Store::default(), &theirs)? {
            Some(work) => (work, layers::Shows::Work),
            None => (git::branch_tip(branch)?.id, layers::Shows::Work),
        },
        false => (
            git::branch_tip(branch)?.id,
            crate::state::follow::tip_shows(&local, branch, false),
        ),
    };
    Ok(Layer {
        branch: branch.to_string(),
        shows,
        updates: Updates::Live,
        moved: crate::fsutil::epoch_secs(std::time::SystemTime::now()),
        snapshot,
        edit: true,
        instead: None,
    })
}

/// Follows what branch `name` builds on, unasked (the turn's hook): onto
/// its live editable layers' new commits, onto where one landed once its
/// branch is gone. `lock` held. With the landings said (those followed and
/// those only noticed).
pub fn follow(common_dir: &Path, name: &str) -> Result<(Outcome, Vec<Landed>, Vec<Part>)> {
    let built = Built::read(common_dir, name)?;
    if built.layers.editable().next().is_none() {
        return Ok((Outcome::UpToDate, Vec::new(), Vec::new()));
    }
    let mut parts = wanted_parts(&built, &[])?;
    let mut next = None;
    let mut seen = Vec::new();
    let mut working = Built {
        layers: built.layers.clone(),
        own: built.own,
        parts: built.parts.clone(),
    };
    for landing in landings(common_dir, &built) {
        if landing.gone {
            let (layers, p) = after_landing(common_dir, &working, &landing, &parts)?;
            parts = p;
            working.layers = layers.clone();
            next = Some(layers);
        }
        seen.push(landing);
    }
    // A landing in main with nothing else to build on: onto the landing
    // commit, as a branch built on nothing (no own ref).
    let plan = Plan {
        name,
        said: format!("onto {}", onto_words(common_dir, &parts)),
        parts: parts.clone(),
        next,
        asked: false,
        yes: true,
        off: None,
        prefix: String::new(),
    };
    let outcome = move_onto(common_dir, &plan)?;
    Ok((outcome, seen, parts))
}

/// `gitfold layer --claude-hook user-prompt-submit`'s part for editable
/// layers: at the start of a turn, the session's workspace follows them
/// (`follow`), and what happened is said as context. Nothing when nothing
/// happened, or when what keeps it from happening was told before (a block
/// per commit, anything else while it says the same: `move-told`). With
/// whether it is where its editable layers are (else their work stays as
/// it shows: their new commits would show as conflicts).
pub fn hook_context(common_dir: &Path, name: &str) -> Result<(Option<String>, bool)> {
    let state = StateDir::of(common_dir, name);
    let _lock = layers::lock(&state)?;
    let before = stack::rev(common_dir, &stack::own_ref(name))?;
    let (outcome, landings, parts) = follow(common_dir, name)?;
    let told_file = state.dir.join(TOLD);
    let say_once = |text: String| -> Result<Option<String>> {
        if std::fs::read_to_string(&told_file).is_ok_and(|told| told == text) {
            return Ok(None);
        }
        if state.dir.exists() {
            crate::fsutil::write_atomic(&told_file, text.as_bytes())?;
        }
        Ok(Some(text))
    };
    let mut out = Vec::new();
    let onto = onto_words(common_dir, &parts);
    match &outcome {
        Outcome::UpToDate | Outcome::BaseMoved => {}
        Outcome::Blocked { told: true, .. } => {}
        Outcome::Moved { files, .. } => {
            let _ = std::fs::remove_file(&told_file);
            let followed: Vec<&Landed> = landings.iter().filter(|l| l.gone).collect();
            let what = match followed.is_empty() {
                true => {
                    let after = stack::rev(common_dir, &stack::own_ref(name))?;
                    let subjects = match (before, after) {
                        (Some(b), Some(a)) => subjects(common_dir, b, a)?,
                        _ => Vec::new(),
                    };
                    let subjects = match subjects.is_empty() {
                        true => String::new(),
                        false => format!(" ({})", subjects.join("; ")),
                    };
                    format!("was moved onto {onto}'s new commits{subjects}")
                }
                false => {
                    let said: Vec<String> = followed.iter().map(|l| l.said()).collect();
                    format!(
                        "{}; it was moved there with only its own commits, and now sits on \
                         {onto}. Its pull request's base should be {}: suggest `gh pr edit \
                         {name} --base {}` to the user after the push below rather than \
                         running it yourself",
                        said.join("; "),
                        followed[0].target,
                        followed[0].target
                    )
                }
            };
            let push = match force_push(common_dir, name)? {
                Some(push) => format!(" Push it with: {push}"),
                None => " Push it with --force-with-lease when you push.".into(),
            };
            out.push(format!(
                "gitfold: this branch, {name}, {what}; {}. Its history was rewritten.{push}",
                under_you(files.clone())
            ));
        }
        other => {
            if let Some(text) = say_once(format!(
                "gitfold: {name} builds on {onto}; {}",
                other.said()
            ))? {
                out.push(text);
            }
        }
    }
    for landing in landings.iter().filter(|l| !l.gone) {
        let text = format!(
            "gitfold: {}, but {} is still there: `gitfold layer remove {}` moves {name} onto \
             {} with only its own commits. Suggest it to the user rather than running it \
             yourself: it rewrites {name}'s history.",
            landing.said(),
            landing.layer,
            landing.layer,
            landing.target
        );
        if let Some(text) = say_once(text)? {
            out.push(text);
        }
    }
    Ok(((!out.is_empty()).then(|| out.join("\n")), outcome.done()))
}

/// The paths a move changed in the session's workspace, for the hook.
fn under_you(files: Option<Vec<BString>>) -> String {
    match files.unwrap_or_default() {
        files if files.is_empty() => "no files changed under you".to_string(),
        files => format!("files changed under you: {}", shown(&files)),
    }
}

/// The notes `gitfold list` and the listing show for branch `name`: an
/// editable layer that landed, a blocked move, one needed, a force-push.
pub fn notes(common_dir: &Path, name: &str) -> Vec<String> {
    let Ok(built) = Built::read_quiet(common_dir, name) else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for landing in landings(common_dir, &built) {
        let how = match landing.landing.how {
            stack::How::Merged => "merged",
            _ => "looks merged",
        };
        notes.push(format!(
            "{} {how} into {}: `gitfold layer remove {}` moves it there",
            landing.layer, landing.target, landing.layer
        ));
    }
    let state = StateDir::of(common_dir, name);
    if let Some(blocked) = blocked(&state) {
        let paths: Vec<&str> = blocked.split(' ').skip(2).collect();
        notes.push(format!("blocked: conflicts on {}", shown_str(&paths)));
    } else if notes.is_empty()
        && let Ok(wanted) = wanted_parts(&built, &[])
        && !wanted.is_empty()
        && wanted != built.parts
    {
        notes.push("needs update: its next turn, or `gitfold layer add <LAYER> --edit`".into());
    }
    if let Ok(Some(push)) = force_push(common_dir, name) {
        notes.push(format!("push with: {push}"));
    }
    notes
}

impl Built {
    /// `read` without settling anything (no lock): for listings.
    pub fn read_quiet(common_dir: &Path, name: &str) -> Result<Built> {
        let state = StateDir::of(common_dir, name);
        let layers = match layers::read_next(&state)? {
            Some(next) if stack::rev(common_dir, &stack::own_ref(name))? == Some(next.onto) => {
                Layers::parse(&next.layers)?
            }
            _ => Layers::read(&state)?,
        };
        let own = stack::rev(common_dir, &stack::own_ref(name))?;
        let parts = current_parts(own, &layers)?;
        Ok(Built { layers, own, parts })
    }
}

/// Refuses building `name` on `layer` if `layer` builds on `name`, through
/// its editable layers.
pub fn check_cycle(common_dir: &Path, name: &str, layer: &str) -> Result<()> {
    let mut todo = vec![layer.to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(at) = todo.pop() {
        if !seen.insert(at.clone()) {
            continue;
        }
        let theirs = Layers::read(&StateDir::of(common_dir, &at)).unwrap_or_default();
        for next in theirs.editable_names() {
            if next == name {
                crate::cli::usage(format!(
                    "{layer} builds on {name} (through {at}): that would be a cycle"
                ));
            }
            todo.push(next);
        }
    }
    Ok(())
}

/// The recorded block of the branch of `state`, if any (see `BLOCKED`).
fn blocked(state: &StateDir) -> Option<String> {
    std::fs::read_to_string(state.dir.join(BLOCKED))
        .ok()
        .map(|s| s.trim().to_string())
}

fn record_blocked(state: &StateDir, onto: ObjectId, commit: ObjectId, paths: &[BString]) {
    let record = format!("{onto} {commit} {}", lossy(paths).join(" "));
    let written = std::fs::create_dir_all(&state.dir)
        .and_then(|()| crate::fsutil::write_atomic(&state.dir.join(BLOCKED), record.as_bytes()));
    if let Err(e) = written {
        log::warn!("{}: recording a blocked move: {e}", state.branch);
    }
}

fn clear_blocked(state: &StateDir) {
    let _ = std::fs::remove_file(state.dir.join(BLOCKED));
}

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
pub fn tip_of(common_dir: &Path, name: &str) -> Result<ObjectId> {
    match stack::rev(common_dir, &format!("refs/heads/{name}"))? {
        Some(tip) => Ok(tip),
        None => Err(Refusal::NotFound.error(format!("no branch {name}"))),
    }
}

/// How many own commits branch `name` has on top of what it builds on.
pub fn own_count(common_dir: &Path, name: &str) -> Result<Option<usize>> {
    let tip = tip_of(common_dir, name)?;
    let Some(own) = stack::rev(common_dir, &stack::own_ref(name))? else {
        return Ok(None);
    };
    if !stack::is_ancestor(common_dir, own, tip)? {
        return Ok(None);
    }
    Ok(Some(stack::count(common_dir, own, tip)?))
}

/// Whether `name` is the main branch (see `land`).
pub fn is_trunk(common_dir: &Path, name: &str) -> bool {
    land::is_trunk(common_dir, &Local::read(common_dir), name)
}

pub fn short(id: ObjectId) -> String {
    id.to_hex_with_len(12).to_string()
}

fn lossy(paths: &[BString]) -> Vec<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

/// Paths for a line of output: the first few, and how many more.
pub fn shown(paths: &[BString]) -> String {
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
    fn answers_read_back() {
        for moved in [
            Moved::Done(vec!["a".into(), "d/b c".into()]),
            Moved::Done(Vec::new()),
            Moved::Already,
            Moved::Waits("why\nnot".into()),
        ] {
            assert_eq!(Moved::decode(&moved.encode()).unwrap(), moved);
        }
    }
}
