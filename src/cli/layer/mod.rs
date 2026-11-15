//! `gitfold layer`: other branches' work shown in a workspace (see
//! `state::layers`), see-only (read-only) or editable (`--edit`: the branch
//! builds on it, its commits sitting on the layer's; see `moves`). The
//! layers are stored with the workspace (or, for a branch without one, its
//! editable layers alone); while mounted, the daemon shows them at once.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, ensure};
use clap::{Args, Subcommand};
use gix::ObjectId;

use crate::daemon::control::{self, Request};
use crate::git;
use crate::refusal::Refusal;
use crate::settings::Local;
use crate::state::StateDir;
use crate::state::layers::{Layer, Layers, Shows, Updates, View, and_list};
use crate::state::retire::check_name;
use crate::state::{follow, snapshot};
use crate::store::Store;

pub(crate) mod land;
pub(super) mod moves;

/// How many conflicting paths the listing names before "and N more".
const SHOWN_CONFLICTS: usize = 5;

#[derive(Args)]
pub struct LayerArgs {
    #[command(subcommand)]
    pub op: Option<LayerOp>,
    /// As Claude Code's `UserPromptSubmit` hook (`user-prompt-submit`):
    /// move the workspace the session is in onto its editable layers' new
    /// commits (and onto where one landed, once its branch is gone), bring
    /// its live layers up to date (an agent's workspace as it was when its
    /// last turn ended, one without an agent as it is now, or a branch's
    /// tip), and say what changed; from then on, only its turns move its
    /// layers. As `PreToolUse` on Edit, Write, MultiEdit and NotebookEdit
    /// (`pre-tool-use`): deny writing a see-only layer's file, saying why
    /// and what to do instead (build on its branch, propose a change, ask
    /// its agent, remove the layer; for an edit that only adds lines, mark
    /// the file `merge=union`).
    #[arg(long, value_name = "EVENT", value_parser = ["user-prompt-submit", "pre-tool-use"])]
    pub claude_hook: Option<String>,
    /// The workspace's branch (default: the workspace you are in). A branch
    /// without a workspace has only editable layers: `add --edit`, and
    /// `remove` of one, work on it too.
    #[arg(short = 'w', long, global = true, value_name = "BRANCH")]
    pub workspace: Option<String>,
}

#[derive(Subcommand)]
pub enum LayerOp {
    /// Show BRANCH's work in the workspace, kept up to date; with --edit,
    /// build on it.
    ///
    /// A branch with a workspace shows its work, uncommitted included;
    /// one without, its commits. A tag or a commit shows as it is. Its
    /// files are read-only here. Adding it again replaces it (and brings
    /// it up to date).
    ///
    /// It updates at the start of each of your agent's turns, or, in a
    /// workspace without an agent, while mounted, once nothing has written
    /// to it for 3 seconds.
    ///
    /// With --edit, your branch builds on BRANCH: your commits sit on its
    /// commits (they move there now, and onto its new ones as it updates),
    /// and what it committed is yours to edit. What it hasn't committed yet
    /// shows on top, editable too, but git sees only your lines. Your
    /// changes to its uncommitted lines are waiting edits: kept in the file,
    /// out of git, listed by `gitfold layer`, and yours to commit once
    /// BRANCH commits those lines. A pull request for your branch includes
    /// BRANCH's commits until BRANCH lands. With several editable layers,
    /// your commits sit on a merge of theirs.
    ///
    /// Adding an editable layer without --edit makes it see-only again,
    /// taking your commits off it. A move says first what it moves; commits
    /// already pushed move only with --yes; commits or waiting edits that
    /// change the layer's lines can't come off it (listed, nothing moved).
    /// gitfold never pushes: it prints the force-push to run.
    ///
    /// Exit codes: 3 no such branch (or see-only on a branch without a
    /// workspace), 4 not moved (commits already pushed, a conflict, waiting
    /// edits, uncommitted work in the way, git busy), 2 usage errors (main,
    /// or --edit on a tag or a commit).
    Add {
        /// A branch, or any revision.
        branch: String,
        /// Build on it: your commits sit on its commits.
        #[arg(long)]
        edit: bool,
        /// As it is now: it never updates.
        #[arg(long)]
        frozen: bool,
        /// Only what the branch has committed.
        #[arg(long)]
        commits: bool,
        /// Move commits already pushed too.
        #[arg(long)]
        yes: bool,
    },
    /// Stop showing a layer, or every layer (--all).
    ///
    /// An editable one: your commits come off it, onto your other editable
    /// layers or main (or onto where it landed, if it did), as when it is
    /// made see-only (see `add`): said first, commits already pushed only
    /// with --yes, refused if some of your commits or waiting edits change
    /// its lines.
    Remove {
        /// The layer: the branch or revision it shows.
        branch: Option<String>,
        /// Every layer.
        #[arg(long, conflicts_with = "branch")]
        all: bool,
        /// Move commits already pushed too.
        #[arg(long)]
        yes: bool,
    },
    /// Gone: `add BRANCH --edit` builds on a branch (said so).
    #[command(hide = true)]
    Base {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gitfold layer`.
pub fn layer(repo: &Path, args: LayerArgs) -> Result<()> {
    match &args.op {
        Some(LayerOp::Base { args }) => {
            let branch = args.first().map_or("BRANCH", String::as_str);
            super::usage(format!(
                "`gitfold layer base` is gone: `gitfold layer add {branch} --edit` builds on \
                 {branch}, `gitfold layer remove {branch}` takes your commits off it"
            ));
        }
        Some(LayerOp::Remove {
            branch: None,
            all: false,
            ..
        }) => super::usage("name a layer, or --all"),
        _ => {}
    }
    let name = super::workspace_or_current(args.workspace, repo)?;
    if let Some(LayerOp::Add { branch, .. }) = &args.op
        && *branch == name
    {
        super::usage(format!("{name} can't be a layer of its own workspace"));
    }
    check_name(&name)?;
    let common_dir = git::init(repo)?;
    if let Some(LayerOp::Add {
        branch,
        edit: false,
        ..
    }) = &args.op
    {
        if moves::is_trunk(&common_dir, branch) {
            super::usage(format!(
                "{branch} is the main branch: {name} has its commits already (its newer ones \
                 come with a rebase or a merge)"
            ));
        }
        if git::branch_exists(&name) && !StateDir::of(&common_dir, &name).opened() {
            return Err(Refusal::NotFound.error(format!(
                "{name} has no workspace: see-only layers show in a workspace; to build on \
                 {branch} use --edit"
            )));
        }
    }
    // Building on a branch, or taking a branch's commits off one, works
    // without a workspace too.
    let stored = Layers::read(&StateDir::of(&common_dir, &name)).unwrap_or_default();
    let editable = |b: &str| stored.find(b).is_some_and(|i| stored.layers[i].edit);
    let edits = match &args.op {
        Some(LayerOp::Add { edit, branch, .. }) => *edit || editable(branch),
        Some(LayerOp::Remove { branch, .. }) => {
            (branch.as_deref()).map_or(stored.editable().next().is_some(), editable)
        }
        // The listing of a branch without a workspace: what it builds on.
        None => git::branch_exists(&name) && !StateDir::of(&common_dir, &name).opened(),
        Some(LayerOp::Base { .. }) => unreachable!("refused above"),
    };
    // A branch without a workspace has only its editable layers.
    if let Some(LayerOp::Remove {
        branch: Some(branch),
        ..
    }) = &args.op
        && stored.find(branch).is_none()
        && git::branch_exists(&name)
        && !StateDir::of(&common_dir, &name).opened()
    {
        return Err(not_a_layer(branch, &name));
    }
    let state = match edits {
        true => {
            git::require_branch(&name)?;
            StateDir::of(&common_dir, &name)
        }
        false => super::opened_workspace(&common_dir, &name)?,
    };
    if args.op.is_none() {
        let (shown, _) = crate::state::layers::to_show(&state)?;
        print!("{}", listing(&common_dir, &state, &shown)?);
        return Ok(());
    }
    let local = Local::read(&common_dir);
    std::fs::create_dir_all(&state.dir)?;
    // Read, changed and stored with no other change in between.
    let _lock = crate::state::layers::lock(&state)?;
    let built = moves::Built::read(&common_dir, &name)?;
    let mut layers = built.layers.clone();
    let done = match args.op {
        None | Some(LayerOp::Base { .. }) => unreachable!("handled above"),
        Some(LayerOp::Add {
            branch,
            edit: true,
            frozen,
            commits,
            yes,
        }) => return add_edit(&common_dir, &state, &built, &branch, frozen, commits, yes),
        Some(LayerOp::Add {
            branch,
            frozen,
            commits,
            yes,
            ..
        }) => {
            // A move in progress first: it is what to wait for.
            refuse_if_moving(&state)?;
            let store = Store::default();
            let layer = layer_of(&store, &common_dir, &state, &branch, frozen, commits)?;
            let in_base = in_base(&state, layer.snapshot)?;
            if layers.find(&branch).is_some_and(|i| layers.layers[i].edit) {
                return see_only(&common_dir, &state, &built, layer, yes);
            }
            add(&local, &mut layers, &name, layer, in_base)
        }
        Some(LayerOp::Remove {
            branch: None, yes, ..
        }) => {
            if built.layers.editable().next().is_some() {
                return remove_all_edit(&common_dir, &state, &built, yes);
            }
            layers.layers.clear();
            format!("{name}: every layer removed")
        }
        Some(LayerOp::Remove {
            branch: Some(branch),
            yes,
            ..
        }) => {
            if layers.find(&branch).is_some_and(|i| layers.layers[i].edit) {
                return remove_edit(&common_dir, &state, &built, &branch, yes);
            }
            remove(&local, &mut layers, &name, &branch)?
        }
    };
    let applied = store_and_apply(&common_dir, &state, &layers)?;
    println!("{done}\n{applied}");
    Ok(())
}

/// The warning for a branch built on more than one editable layer: a pull
/// request for it shows all of their commits.
fn several_warning(name: &str, editable: &[String]) -> String {
    let all = and_list(editable.iter().cloned());
    let theirs = and_list(editable.iter().map(|n| format!("{n}'s")));
    let both = if editable.len() == 2 { "both" } else { "all" };
    format!(
        "{name} will be built on {all}. A pull request for {name} shows {theirs} commits as \
         part of {name}'s until {both} land: open it after they land, or build on one of them."
    )
}

/// `gitfold layer add BRANCH --edit`: branch `state.branch` (`built`) builds
/// on BRANCH: its own commits move onto BRANCH's commits (with other
/// editable layers, a merge of theirs), BRANCH an editable layer from then
/// on. Nothing changes if they can't move.
fn add_edit(
    common_dir: &Path,
    state: &StateDir,
    built: &moves::Built,
    branch: &str,
    frozen: bool,
    commits: bool,
    yes: bool,
) -> Result<()> {
    let name = &state.branch;
    if !git::branch_exists(branch) {
        // A revision: refused as one, if it is one.
        git::resolve(branch)
            .map_err(|_| Refusal::NotFound.error(format!("no branch or revision {branch}")))?;
        super::usage(format!(
            "{branch} isn't a branch: a tag or a commit can't be edited on (it never moves); \
             use --edit with a branch, or show it with `gitfold layer add {branch}`"
        ));
    }
    if moves::is_trunk(common_dir, branch) {
        super::usage(format!(
            "{branch} is the main branch: every branch sits on it already"
        ));
    }
    check_name(branch)?;
    moves::check_cycle(common_dir, name, branch)?;
    refuse_if_moving(state)?;
    let store = Store::default();
    // Taken as a live one (a frozen layer of what the branch has already
    // would be refused), then frozen.
    let mut layer = layer_of(&store, common_dir, state, branch, false, commits)?;
    if frozen {
        layer.updates = Updates::Frozen;
    }
    layer.edit = true;
    let was = built
        .layers
        .find(branch)
        .map(|i| built.layers.layers[i].clone());
    let mut next = built.layers.clone();
    match next.find(branch) {
        Some(i) => {
            if next.layers[i].snapshot == layer.snapshot {
                layer.moved = next.layers[i].moved;
            }
            next.layers[i] = layer.clone();
        }
        None => next.layers.push(layer.clone()),
    }
    let editable = next.editable_names();
    if editable.len() > 1 && !was.as_ref().is_some_and(|l| l.edit) {
        println!("{}", several_warning(name, &editable));
    }
    let after = moves::Built {
        layers: next.clone(),
        own: built.own,
        parts: built.parts.clone(),
    };
    let parts = moves::wanted_parts(common_dir, &after, &[branch])?;
    let plan = moves::Plan {
        name,
        said: format!("onto {}", moves::onto_words(common_dir, &parts)),
        parts,
        next: Some(next),
        asked: true,
        yes,
        off: None,
        prefix: Default::default(),
    };
    let outcome = moves::move_onto(common_dir, &plan)?;
    moves::say(common_dir, name, &outcome)?;
    let what = match was {
        Some(_) => "updated",
        None => "added",
    };
    println!("layer {branch} {what}: {}, built on", describe(&layer));
    show_after_move(common_dir, state)
}

/// Refuses taking the branch of `state` off editable layers `off` while it
/// has waiting edits of their lines (`state::waiting`): they can't stand
/// without them, so they are listed, nothing moving.
fn refuse_waiting(state: &StateDir, off: &[String]) -> Result<()> {
    let waiting = crate::state::waiting::of_workspace(state)?;
    let on: Vec<String> = (waiting.iter())
        .filter(|w| w.layers.iter().any(|l| off.contains(l)))
        .map(|w| {
            format!(
                "{} ({})",
                String::from_utf8_lossy(&w.path),
                super::plural(w.edits, "waiting edit", "waiting edits")
            )
        })
        .collect();
    if on.is_empty() {
        return Ok(());
    }
    let off = and_list(off.iter().cloned());
    Err(Refusal::Changes.error(format!(
        "{} has waiting edits of {off}'s uncommitted lines in {}: they can't stand without \
         {off}. Undo them, or keep {off} editable until it commits those lines (they then \
         become yours to commit)",
        state.branch,
        on.join(", ")
    )))
}

/// `gitfold layer add BRANCH` on editable layer BRANCH (`layer`, as it is
/// now): it becomes see-only, the branch's own commits taken off it.
fn see_only(
    common_dir: &Path,
    state: &StateDir,
    built: &moves::Built,
    mut layer: Layer,
    yes: bool,
) -> Result<()> {
    let name = &state.branch;
    let branch = layer.branch.clone();
    refuse_waiting(state, std::slice::from_ref(&branch))?;
    let mut next = built.layers.clone();
    let i = next.find(&branch).expect("an editable layer");
    if next.layers[i].snapshot == layer.snapshot {
        layer.moved = next.layers[i].moved;
    }
    next.layers[i] = layer.clone();
    let after = moves::Built {
        layers: next.clone(),
        own: built.own,
        parts: built.parts.clone(),
    };
    let parts = moves::wanted_parts(common_dir, &after, &[])?;
    let onto = moves::onto_words(common_dir, &parts);
    let plan = moves::Plan {
        name,
        said: format!("off {branch}, onto {onto} ({branch} stays shown, read-only)"),
        parts,
        next: Some(next),
        asked: true,
        yes,
        off: Some(&branch),
        prefix: moves::Prefix {
            moves: format!("{branch} becomes see-only: "),
            would: format!("making {branch} see-only "),
        },
    };
    let outcome = moves::move_onto(common_dir, &plan)?;
    moves::say(common_dir, name, &outcome)?;
    println!("layer {branch} updated: {}, see-only", describe(&layer));
    show_after_move(common_dir, state)
}

/// `gitfold layer remove BRANCH` on editable layer BRANCH: the branch's own
/// commits come off it (onto where it landed, if it did), and it goes.
fn remove_edit(
    common_dir: &Path,
    state: &StateDir,
    built: &moves::Built,
    branch: &str,
    yes: bool,
) -> Result<()> {
    let name = &state.branch;
    refuse_waiting(state, &[branch.to_string()])?;
    let mut next = built.layers.clone();
    next.layers.retain(|l| l.branch != branch);
    let after = moves::Built {
        layers: next.clone(),
        own: built.own,
        parts: built.parts.clone(),
    };
    let wanted = moves::wanted_parts(common_dir, &after, &[])?;
    let landed = match built.on(branch) {
        Some(on) => moves::landed(common_dir, branch, on)?,
        None => None,
    };
    let plan = match &landed {
        // Where it landed: only the own commits move, onto the landing.
        Some(landing) => {
            let parts = moves::wanted_parts(common_dir, built, &[])?;
            let (layers, parts) = moves::after_landing(common_dir, built, landing, &parts)?;
            println!("{}", landing.said());
            moves::Plan {
                name,
                said: format!(
                    "off {branch}, onto {} at {}",
                    landing.target,
                    moves::short(landing.landing.at)
                ),
                parts,
                next: Some(layers),
                asked: true,
                yes,
                off: None,
                prefix: Default::default(),
            }
        }
        None => moves::Plan {
            name,
            said: format!(
                "off {branch}, onto {}",
                moves::onto_words(common_dir, &wanted)
            ),
            parts: wanted,
            next: Some(next),
            asked: true,
            yes,
            off: Some(branch),
            prefix: Default::default(),
        },
    };
    let outcome = moves::move_onto(common_dir, &plan)?;
    moves::say(common_dir, name, &outcome)?;
    println!("layer {branch} removed");
    show_after_move(common_dir, state)
}

/// `gitfold layer remove --all` on a branch with editable layers: its own
/// commits come off them, onto main, and every layer goes. An interrupted
/// move that can't be finished or undone (`Recovery::Stuck`) is forgotten
/// instead, moving nothing more; one that fails to recover otherwise
/// stays, the error said.
fn remove_all_edit(
    common_dir: &Path,
    state: &StateDir,
    built: &moves::Built,
    yes: bool,
) -> Result<()> {
    let name = &state.branch;
    if state.opened()
        && matches!(
            crate::state::moves::recover(state)?,
            crate::state::moves::Recovery::Stuck(_)
        )
        && crate::state::moves::forget(state)?
    {
        crate::state::layers::drop_next(state)?;
        Layers::default().write(state)?;
        git::stack::drop_base(common_dir, name)?;
        println!(
            "{name}: every layer removed; nothing moved, and an interrupted move's intent is \
             forgotten"
        );
        return show_after_move(common_dir, state);
    }
    let editable = built.layers.editable_names();
    refuse_waiting(state, &editable)?;
    let off = and_list(editable.iter().cloned());
    let plan = moves::Plan {
        name,
        said: format!("off {off}, onto {}", moves::onto_words(common_dir, &[])),
        parts: Vec::new(),
        next: Some(Layers::default()),
        asked: true,
        yes,
        off: Some(&off),
        prefix: Default::default(),
    };
    let outcome = moves::move_onto(common_dir, &plan)?;
    moves::say(common_dir, name, &outcome)?;
    println!("{name}: every layer removed");
    show_after_move(common_dir, state)
}

/// `gitfold branch NEW --edit LAYER`: new branch `name`, at `layer`'s tip,
/// builds on it: its editable layer stored, its own commits starting there.
pub fn built_on_new(common_dir: &Path, name: &str, layer: &str) -> Result<()> {
    let state = StateDir::of(common_dir, name);
    std::fs::create_dir_all(&state.dir)?;
    let _lock = crate::state::layers::lock(&state)?;
    let mut edit = layer_of(&Store::default(), common_dir, &state, layer, false, false)?;
    edit.edit = true;
    Layers { layers: vec![edit] }.write(&state)?;
    git::stack::set_base(common_dir, name, git::branch_tip(name)?.id)
}

/// After a move asked for: the layers it stored shown, if the branch has a
/// workspace (the move showed them already when it moved files; this says
/// so, or shows them when it didn't need to).
fn show_after_move(common_dir: &Path, state: &StateDir) -> Result<()> {
    if state.opened() {
        println!("{}", apply(common_dir, &state.branch)?);
    }
    Ok(())
}

/// `gitfold layer add`: `layer` added to `layers` (the layers of workspace
/// `name`), or put in place of the one standing for its branch (`slot`);
/// says which. `in_base`: all of it is in the workspace's base already.
fn add(local: &Local, layers: &mut Layers, name: &str, mut layer: Layer, in_base: bool) -> String {
    let branch = layer.branch.clone();
    let mut what = describe(&layer);
    if layer.updates == Updates::Live && in_base {
        let _ = write!(
            what,
            " (nothing to show yet: {name} already has {branch}'s work (it's in main or what \
             {name} builds on); what {branch} does next shows here)"
        );
    }
    let Some(i) = slot(local, layers, name, &branch) else {
        layers.layers.push(layer);
        return format!("layer {branch} added: {what}");
    };
    let was = &layers.layers[i];
    // Not moved if it is where it was.
    if was.snapshot == layer.snapshot {
        layer.moved = was.moved;
    }
    let note = if was.branch == branch {
        // A proposal again: still in place of its layer.
        layer.instead = was.instead.clone();
        String::new()
    } else if mine(local, &branch, name) {
        // A proposal in place of its layer, put back later.
        layer.instead = Some(Box::new(was.clone()));
        format!(" (in place of {})", was.branch)
    } else {
        format!(
            " (in place of your proposal {0}, which stays: `gitfold layer add {0}` shows it \
             again, `gitfold reject {0}` drops it)",
            was.branch
        )
    };
    layers.layers[i] = layer;
    format!("layer {branch} updated: {what}{note}")
}

/// `gitfold layer remove BRANCH`: its layer removed from `layers` (the layers
/// of workspace `name`), or this workspace's proposal shown in its place; a
/// proposal only itself. Says what it did.
fn remove(local: &Local, layers: &mut Layers, name: &str, branch: &str) -> Result<String> {
    let mine = mine(local, branch, name);
    let i = slot(local, layers, name, branch)
        .filter(|&i| !mine || layers.layers[i].branch == branch)
        .ok_or_else(|| not_a_layer(branch, name))?;
    let mut was = layers.layers.remove(i);
    Ok(match was.branch == branch {
        // Your proposal shown in its layer's place: that layer back.
        true if mine => {
            let back = was.instead.take().map(|instead| {
                let note = format!("; {} shown again as it was", instead.branch);
                layers.layers.insert(i, *instead);
                note
            });
            format!(
                "layer {branch} removed{}; the proposal stays: `gitfold reject {branch}` drops it",
                back.unwrap_or_default()
            )
        }
        true => format!("layer {branch} removed"),
        false => format!(
            "layer {branch} removed (it showed your proposal {0}, which stays: `gitfold reject \
             {0}` drops it)",
            was.branch
        ),
    })
}

/// The refusal of `layer remove BRANCH` when `branch` isn't a layer of
/// `name`'s.
fn not_a_layer(branch: &str, name: &str) -> anyhow::Error {
    Refusal::NotFound.error(format!(
        "{branch} isn't a layer of {name} (`gitfold layer` lists them)"
    ))
}

/// Whether `branch` is a proposal of workspace `name`'s.
fn mine(local: &Local, branch: &str, name: &str) -> bool {
    local.proposal(branch).is_some_and(|(_, by)| by == name)
}

/// The layer of `layers` (the layers of workspace `name`) that stands for
/// `branch`: `branch`'s own, or `name`'s proposal to change it shown in its
/// place. A proposal of `name`'s stands for its layer: the same one.
pub(super) fn slot(local: &Local, layers: &Layers, name: &str, branch: &str) -> Option<usize> {
    let target_of = |b: &str| {
        (local.proposal(b))
            .filter(|(_, by)| by == name)
            .map_or_else(|| b.to_string(), |(layer, _)| layer)
    };
    let target = target_of(branch);
    (layers.layers.iter()).position(|l| l.branch == branch || target_of(&l.branch) == target)
}

/// What `layer` shows and how it updates, as the listing says it.
fn describe(layer: &Layer) -> String {
    format!("{}, {}", shows(layer), updates(layer))
}

/// What `layer` shows: a workspace's work, its commits only (`--commits`),
/// or a branch's commits, it having no workspace (a revision: its commits).
fn shows(layer: &Layer) -> &'static str {
    match layer.shows {
        Shows::Work => "all work",
        Shows::CommitsOnly => "commits only",
        Shows::Commits if layer.updates == Updates::Fixed => "commits",
        Shows::Commits => "commits (no workspace)",
    }
}

/// Whether what live layer `layer` shows is behind its source's latest:
/// the snapshot of its agent's last turn, or a branch's tip. A workspace
/// without an agent isn't told (it would need a snapshot taken).
fn behind(common_dir: &Path, layer: &Layer) -> bool {
    if layer.updates != Updates::Live || !git::branch_exists(&layer.branch) {
        return false;
    }
    let theirs = StateDir::of(common_dir, &layer.branch);
    if layer.shows == Shows::Work && theirs.opened() {
        return match snapshot::driven(&theirs) {
            snapshot::Driven::Agent(Some(last)) => last != layer.snapshot,
            _ => false,
        };
    }
    git::branch_tip(&layer.branch).is_ok_and(|tip| tip.id != layer.snapshot)
}

fn updates(layer: &Layer) -> String {
    match layer.updates {
        Updates::Live => format!("live, last {}", clock(layer.moved)),
        Updates::Frozen => format!("frozen at {}", clock(layer.moved)),
        Updates::Fixed => "fixed".into(),
    }
}

/// A line of the hook's context: whoever reads it may stop early (a closed
/// pipe isn't an error).
fn hook_say(text: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stdout(), "{text}");
}

/// `gitfold layer --claude-hook user-prompt-submit`: at the start of a turn,
/// the workspace the session is in follows its editable layers' new commits
/// (`moves::hook_context`), then its live layers move to their latest work
/// (`state::follow`: an agent's workspace as `gitfold snapshot
/// --claude-hook stop` took it when its last turn ended, one without an
/// agent as it is now), so what shows changes only between turns, never
/// half-way through another's. Records that the workspace has an agent
/// (`snapshot::turn_started`): the daemon leaves its layers to its turns.
/// What changed is printed, which becomes context. Does nothing outside a
/// workspace with layers; errors are only logged.
pub fn layer_hook(event: &str) -> Result<()> {
    if event == "pre-tool-use" {
        return deny_hook();
    }
    if let Err(e) = (|| -> Result<()> {
        let repo = super::repo()?;
        let name = super::branch_or_current(None, &repo)?;
        let common_dir = git::init(&repo)?;
        let state = StateDir::of(&common_dir, &name);
        // Its editable layers' new commits first, at the same boundary.
        let editable = Layers::read(&state)?.editable_names();
        let mut hold = Vec::new();
        if !editable.is_empty() {
            match moves::hook_context(&common_dir, &name) {
                Ok((context, done)) => {
                    if let Some(context) = context {
                        hook_say(&context);
                    }
                    if !done {
                        hold = editable;
                    }
                }
                Err(e) => {
                    log::debug!("gitfold layer --claude-hook: moving {name}: {e:#}");
                    hold = editable;
                }
            }
        }
        if state.opened()
            && let Err(e) = snapshot::turn_started(&state)
        {
            log::debug!("gitfold layer --claude-hook: {e:#}");
        }
        let live = |layers: &Layers| layers.layers.iter().any(|l| l.updates == Updates::Live);
        // Most turns: nothing to lock for.
        if !live(&Layers::read(&state)?) {
            return Ok(());
        }
        let lock = crate::state::layers::lock(&state)?;
        let followed = follow::follow_live(
            &Store::default(),
            &state,
            &lock,
            &hold,
            |_| true,
            || apply(&common_dir, &name),
        )?;
        let Some(followed) = followed.filter(|f| !f.moved.is_empty()) else {
            return Ok(());
        };
        hook_say(&format!(
            "gitfold: the layers shown in this workspace moved to their latest work: {}. \
             Files you read from them may have changed.",
            followed.moved.join(", ")
        ));
        for note in followed.applied.lines().filter(|l| l.starts_with("note: ")) {
            hook_say(&format!("gitfold: {note}"));
        }
        Ok(())
    })() {
        log::debug!("gitfold layer --claude-hook: {e:#}");
    }
    if let Err(e) = turn_notes() {
        log::debug!("gitfold layer --claude-hook: {e:#}");
    }
    Ok(())
}

/// For the daemon's move of a workspace without an agent, once it is idle
/// (`fs::workspace::follow`), as the hook does at an agent's turn start:
/// the branch `name` follows its editable layers' new commits (and where
/// one landed, once its branch is gone), so their committed lines become
/// its own (and its waiting edits on them ordinary changes). `lock` held.
/// Whether it is where its editable layers are, and what it did, if it
/// moved or can't.
pub(crate) fn follow_editable(common_dir: &Path, name: &str) -> Result<(bool, Option<String>)> {
    let (outcome, _, _) = moves::follow(common_dir, name)?;
    let said = match &outcome {
        moves::Outcome::UpToDate | moves::Outcome::BaseMoved => None,
        other => Some(other.said()),
    };
    Ok((outcome.done(), said))
}

/// The turn's notes on the session's workspace, after its layers moved:
/// commits on its branch with a layer's uncommitted lines
/// (`state::guard_check`), and its waiting edits.
fn turn_notes() -> Result<()> {
    let repo = super::repo()?;
    let name = super::branch_or_current(None, &repo)?;
    let common_dir = git::init(&repo)?;
    let state = StateDir::of(&common_dir, &name);
    if !state.opened() {
        return Ok(());
    }
    for alarm in crate::state::guard_check::read(&state) {
        hook_say(&format!("gitfold: WARNING: {alarm}"));
    }
    let waiting = crate::state::waiting::of_workspace(&state)?;
    for w in &waiting {
        hook_say(&format!(
            "gitfold: {} (git doesn't see them: `git commit -a` leaves them waiting).",
            w.describe()
        ));
    }
    for w in waiting.iter().filter(|w| w.conflict) {
        hook_say(&format!("gitfold: Conflict: {}.", w.describe_conflict()));
    }
    Ok(())
}

/// `gitfold layer --claude-hook pre-tool-use`: denies Claude Code's edit
/// tools a file a layer shows (the filesystem would refuse it with a bare
/// EACCES), saying whose it is and what to do instead: for one layer's file,
/// first to build on that branch (`base_offer`), then to propose a change
/// (`gitfold propose`), ask its agent or remove the layer; for an edit that
/// only adds lines, to mark the file `merge=union`. Allows everything else,
/// and logs its errors.
fn deny_hook() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    let denied = (|| -> Result<Option<String>> {
        let Some(file) = super::hook::field_at(&input, &["tool_input", "file_path"])
            .or_else(|| super::hook::field_at(&input, &["tool_input", "notebook_path"]))
        else {
            return Ok(None);
        };
        let file = crate::shell::normalized(&super::claude::cwd(&input)?.join(file));
        let Some((state, path)) = super::workspace_at(&file) else {
            return Ok(None);
        };
        // What is shown, which a pending switch may not have changed yet.
        let layers = crate::state::layers::ShownLayers::read(&state)?.layers()?;
        let Some((owners, view)) = owners_in_view(&state, &layers, path.as_bytes())? else {
            return Ok(None);
        };
        let names: Vec<&str> = (owners.iter())
            .map(|&i| layers.layers[i].branch.as_str())
            .collect();
        let Some(&owner) = names.first() else {
            return Ok(None);
        };
        let local = Local::read(&state.common_dir);
        if let [_] = names[..]
            && let Some((layer, by)) = local.proposal(owner)
            && by == state.branch
        {
            let mount = crate::mount_table::mount_of(&state.common_dir)?.unwrap_or_default();
            let there = mount.join(owner).join(&path);
            return Ok(Some(format!(
                "{path} shows your proposal {owner} (a change to {layer}'s work), read-only \
                 here: edit it there, at {}; your next turn shows your edits here (or now: \
                 `gitfold layer add {owner}`).",
                there.display()
            )));
        }
        let conflict: String = (view.conflicts.iter())
            .filter(|c| c.path == path.as_bytes())
            .map(|c| format!(" Conflict: {}.", c.describe(&layers)))
            .collect();
        let list_hint = match adds_lines_only(&input, &file) {
            true => {
                " If the order of lines in this file doesn't matter (a list, a changelog), mark \
                 it `merge=union` in .gitattributes, and every workspace can add lines to it."
            }
            false => "",
        };
        let see_only: Vec<&Layer> = (owners.iter())
            .map(|&i| &layers.layers[i])
            .filter(|l| !l.edit)
            .collect();
        let (build_on, propose) = match edit_offer(&state, &local, &see_only, &path)? {
            Some(offer) => (offer, "Or run"),
            None => (String::new(), "To change it, run"),
        };
        if let [_] = owners[..] {
            return Ok(Some(format!(
                "{path} shows {owner}'s work (a layer, read-only here): it can't be changed in \
                 this workspace.{conflict}{build_on} {propose} `gitfold propose {path}`: it \
                 makes a copy of {owner}'s work for you to edit (a proposal), shown here in its \
                 place, which {owner}'s agent tests against every workspace that read the file \
                 and accepts. Or leave it to {owner}'s agent (`gitfold send {owner} <text>` \
                 asks it), or run `gitfold layer remove {owner}` to stop showing the layer and \
                 edit your own version.{list_hint}"
            )));
        }
        let all = and_list(names.iter().map(|n| n.to_string()));
        Ok(Some(format!(
            "{path} shows the merge of {all}'s work (layers, read-only here): it can't be \
             changed in this workspace.{conflict}{build_on} {propose} `gitfold propose {path} \
             --layer <one of them>`: it makes a copy of that layer's work for you to edit \
             (a proposal), shown here in its place, which its agent tests and accepts. Or \
             leave it to their agents (`gitfold send {owner} <text>` asks {owner}'s), or \
             run `gitfold layer remove {owner}` (and the others) to stop showing them and \
             edit your own version.{list_hint}"
        )))
    })();
    match denied {
        Ok(Some(reason)) => println!(
            "{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":{}}}}}",
            super::inbox::json_str(&reason)
        ),
        Ok(None) => {}
        Err(e) => log::warn!("gitfold layer --claude-hook pre-tool-use: {e:#}"),
    }
    Ok(())
}

/// The deny hook's offer to make file `path` of the workspace in `state`
/// writable: build on the branches of `see_only`, the see-only layers whose
/// changes show there (with them editable, only editable layers changed
/// it), saying what that means. `None` if one of them can't be built on (a
/// revision, a proposal, the main branch).
fn edit_offer(
    state: &StateDir,
    local: &Local,
    see_only: &[&Layer],
    path: &str,
) -> Result<Option<String>> {
    if see_only.is_empty() {
        return Ok(None);
    }
    let common_dir = &state.common_dir;
    for l in see_only {
        if l.updates == Updates::Fixed
            || !git::branch_exists(&l.branch)
            || local.proposal(&l.branch).is_some()
            || land::is_trunk(common_dir, local, &l.branch)
        {
            return Ok(None);
        }
    }
    let commands = and_list(
        see_only
            .iter()
            .map(|l| format!("`gitfold layer add {} --edit`", l.branch)),
    );
    let mut out = match see_only {
        [one] => {
            let owner = &one.branch;
            format!(
                " To make it yours to edit, build on {owner}: {commands}: your commits will sit \
                 on {owner}'s (a pull request for your branch then includes {owner}'s commits \
                 until {owner} lands)."
            )
        }
        _ => {
            let theirs = and_list(see_only.iter().map(|l| format!("{}'s", l.branch)));
            format!(
                " To make it yours to edit, build on all of them: {commands} (one alone leaves \
                 the others' lines read-only): your commits will sit on {theirs} (a pull \
                 request for your branch then includes their commits until they land)."
            )
        }
    };
    for l in see_only {
        let owner = &l.branch;
        let tip = git::branch_tip(owner)?.id;
        let committed = git::with_repo(|repo| -> Result<bool> {
            let at = |commit: ObjectId| -> Result<_> {
                let tree = git::layers::commit_tree(repo, commit)?;
                git::layers::lookup(repo, tree, path.into())
            };
            Ok(at(l.snapshot)? == at(tip)?)
        })?;
        if !committed {
            let _ = write!(
                out,
                " {owner} hasn't committed its change to this file yet: your edits to \
                 {owner}'s lines wait until {owner} commits them."
            );
        }
    }
    Ok(Some(out))
}

/// Whether the edit tool call in hook input `input`, of file `file`, only
/// adds whole lines: Edit's (and each of MultiEdit's) `new_string` is its
/// `old_string` with lines inserted; Write's content is the file's with
/// lines inserted.
fn adds_lines_only(input: &str, file: &Path) -> bool {
    use super::hook::{array_at, field, field_at};
    let edit = |json: &str, at: &[&str]| {
        let old = field_at(json, &[at, &["old_string"]].concat());
        let new = field_at(json, &[at, &["new_string"]].concat());
        matches!((old, new), (Some(old), Some(new)) if inserts_lines(&old, &new))
    };
    match field(input, "tool_name").as_deref() {
        Some("Edit") => edit(input, &["tool_input"]),
        Some("MultiEdit") => array_at(input, &["tool_input", "edits"])
            .is_some_and(|edits| !edits.is_empty() && edits.iter().all(|e| edit(e, &[]))),
        Some("Write") => match (
            std::fs::read_to_string(file),
            field_at(input, &["tool_input", "content"]),
        ) {
            (Ok(now), Some(new)) => inserts_lines(&now, &new),
            _ => false,
        },
        _ => false,
    }
}

/// Whether `new` is `old` (not empty) with whole lines inserted: every
/// line of `old`, in order, is a line of `new`, and `new` has more.
fn inserts_lines(old: &str, new: &str) -> bool {
    if old.is_empty() || new.len() <= old.len() {
        return false;
    }
    let mut old = old.split('\n').peekable();
    for line in new.split('\n') {
        if old.peek() == Some(&line) {
            old.next();
        }
    }
    old.peek().is_none()
}

/// The layers of `layers` (shown in the workspace in `state`) whose changes
/// show at `path`, read-only there: one, or those whose changes are merged
/// there. None if what shows is the workspace's own or its base's.
pub(super) fn owners(state: &StateDir, layers: &Layers, path: &[u8]) -> Result<Vec<usize>> {
    Ok(owners_in_view(state, layers, path)?.map_or_else(Vec::new, |(owners, _)| owners))
}

/// `owners`, with the view they were found in (`None` if none was built:
/// no owners).
fn owners_in_view(
    state: &StateDir,
    layers: &Layers,
    path: &[u8],
) -> Result<Option<(Vec<usize>, View)>> {
    if !layers.is_on() {
        return Ok(None);
    }
    // Its own file at that path wins: writable.
    if std::fs::symlink_metadata(crate::state::upper_path(&state.upper(), path)).is_ok() {
        return Ok(None);
    }
    git::init(&state.common_dir)?;
    let base = state.read_base()?;
    // Most edits are of files no layer changes: told apart without building
    // the view (the deny hook runs on every edit).
    if !differs_from_base(layers, base.tree, path)? {
        return Ok(None);
    }
    let view = crate::state::layers::view(&state.common_dir, &base.id, layers)?;
    // Only editable layers changed it: writable.
    Ok(view
        .filter(|view| !view.writable.contains(path))
        .map(|view| (view.mixed.get(path).cloned().unwrap_or_default(), view)))
}

/// Whether a layer of `layers` has another entry at `path` than base tree
/// `base`: if none has, the view shows the base's there (each layer's
/// change, merged in turn, is then the base's own version).
fn differs_from_base(layers: &Layers, base: ObjectId, path: &[u8]) -> Result<bool> {
    git::with_repo(|repo| {
        let at = |tree: ObjectId| git::layers::lookup(repo, tree, path.into());
        let in_base = at(base)?;
        for layer in &layers.layers {
            if at(git::layers::commit_tree(repo, layer.snapshot)?)? != in_base {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

/// Whether commit `snapshot` is in the base of the workspace in `state`: as a
/// layer it shows nothing (yet).
fn in_base(state: &StateDir, snapshot: ObjectId) -> Result<bool> {
    let base = view_base(state)?;
    Ok(git::merge_base_if_any(base, snapshot)? == Some(snapshot))
}

/// The commit the view of the branch of `state` is built on: its
/// workspace's base, or without a workspace, its tip.
fn view_base(state: &StateDir) -> Result<ObjectId> {
    match state.opened() {
        true => Ok(state.read_base()?.id),
        false => Ok(git::branch_tip(&state.branch)?.id),
    }
}

/// The layer of `branch` (a branch, or any revision) as it is now, for the
/// workspace in `state`: a workspace's work (its snapshot) unless `commits`,
/// else a branch's tip or the revision; live unless `frozen` (a revision is
/// fixed). Refused if it shares no history with the workspace's base, or,
/// never moving, is in it already.
pub(super) fn layer_of(
    store: &Store,
    common_dir: &Path,
    state: &StateDir,
    branch: &str,
    frozen: bool,
    commits: bool,
) -> Result<Layer> {
    let updates = match frozen {
        true => Updates::Frozen,
        false => Updates::Live,
    };
    let (snapshot, shows, updates) = if git::branch_exists(branch) {
        check_name(branch)?;
        let theirs = StateDir::of(common_dir, branch);
        if theirs.opened() && !commits {
            // Live: as it follows it (`follow::latest_work`), else as it is now.
            let work = match (frozen, snapshot::driven(&theirs)) {
                (false, snapshot::Driven::Agent(Some(last_turn))) => last_turn,
                _ => snapshot::snapshot(store, &theirs, snapshot::By::Other)?.commit,
            };
            (work, Shows::Work, updates)
        } else {
            let shows = follow::tip_shows(&Local::read(common_dir), branch, commits);
            (git::branch_tip(branch)?.id, shows, updates)
        }
    } else {
        // Stored in a line of tab-separated fields.
        ensure!(
            !branch.is_empty() && !branch.chars().any(|c| c.is_whitespace() || c.is_control()),
            "{branch:?}: not a branch or a revision gitfold can show"
        );
        let commit = git::resolve(branch)
            .map_err(|_| Refusal::NotFound.error(format!("no branch or revision {branch}")))?;
        (commit.id, Shows::Commits, Updates::Fixed)
    };
    // A layer shows its changes since it and the base diverged: without a
    // commit in common there is no telling them apart.
    let Some(merge_base) = git::merge_base_if_any(view_base(state)?, snapshot)? else {
        return Err(Refusal::Changes.error(format!(
            "{branch} shares no history with {}, so its changes can't be told apart: a layer \
             must start from a commit your branch also has",
            state.branch
        )));
    };
    // A layer shows changes on top of the base: one that never moves and is
    // part of the base already would show nothing.
    if updates != Updates::Live && merge_base == snapshot {
        let name = &state.branch;
        return Err(Refusal::Changes.error(format!(
            "{name} already has {branch} (it's in main or what {name} builds on), so as a layer \
             it shows nothing: a layer shows what {name} doesn't have. To look at {branch} as \
             it was: `gitfold snapshot {branch}`"
        )));
    }
    Ok(Layer {
        branch: branch.to_string(),
        shows,
        updates,
        moved: now(),
        snapshot,
        edit: false,
        instead: None,
    })
}

/// For a workspace told `branch`'s work changed: when its layer `layer` of
/// it shows that (`None`: it shows something else in its place).
pub(super) fn when_shown(layer: Option<&Layer>, branch: &str) -> String {
    match layer {
        Some(l) if l.updates == Updates::Frozen => format!(
            "your layer of {branch} is frozen: `{}` takes its work as it is now",
            add_command(l)
        ),
        Some(l) if l.shows == Shows::CommitsOnly => format!(
            "your layer of {branch} shows only its commits: it shows the change once {branch} \
             commits it"
        ),
        _ => format!(
            "your layer of {branch} shows it from your next turn (or now: `gitfold layer add \
             {branch}`)"
        ),
    }
}

/// The command that adds `layer` again as it updates.
fn add_command(layer: &Layer) -> String {
    let mut out = format!("gitfold layer add {}", layer.branch);
    if layer.updates == Updates::Frozen {
        out += " --frozen";
    }
    if layer.shows == Shows::CommitsOnly {
        out += " --commits";
    }
    out
}

/// Stores `layers` for the workspace in `state` and has the daemon show it
/// now, if mounted; says what happened. If the daemon refuses, the layers
/// stored before are put back, so what is stored is what shows.
pub(super) fn store_and_apply(
    common_dir: &Path,
    state: &StateDir,
    layers: &Layers,
) -> Result<String> {
    refuse_if_moving(state)?;
    crate::state::layers::store_and_show(state, layers, || apply(common_dir, &state.branch))
}

/// Refuses changing the layers of the workspace in `state` while its
/// branch is being moved: the move moves the base a switch builds the view
/// on.
fn refuse_if_moving(state: &StateDir) -> Result<()> {
    if !crate::state::moves::pending(state) {
        return Ok(());
    }
    Err(Refusal::Changes.error(format!(
        "{} is being moved (or a move of it was interrupted: its next turn, or the mount, \
         finishes it); retry once it is done",
        state.branch
    )))
}

/// Has the daemon show the stored layers now, if mounted; says what happened.
pub(super) fn apply(common_dir: &Path, name: &str) -> Result<String> {
    let request = Request::Layers {
        branch: name.to_string(),
    };
    match control::request(common_dir, &request, None)? {
        Some(done) => Ok(done),
        None => Ok(format!(
            "{name}: not mounted; its layers show when it opens"
        )),
    }
}

/// `gitfold layer`: the layers, what your commits sit on, the conflicts and
/// notes.
fn listing(common_dir: &Path, state: &StateDir, layers: &Layers) -> Result<String> {
    let mut out = String::new();
    let own = moves::own_count(common_dir, &state.branch).ok().flatten();
    let editable = layers.editable_names();
    // The first editable layer's row says where your commits sit; with
    // others, naming them.
    let sits = |n: Option<usize>| {
        let (on, with) = match &editable[..] {
            [_] => ("it", String::new()),
            [_, others @ ..] => (
                "them",
                format!(" (with {})", and_list(others.iter().cloned())),
            ),
            [] => ("it", String::new()),
        };
        match n {
            Some(0) => format!("built on{with}: no commits of yours yet"),
            Some(1) => format!("built on{with}: your 1 commit sits on {on}"),
            Some(n) => format!("built on{with}: your {n} commits sit on {on}"),
            None => format!("built on{with}: your commits sit on {on}"),
        }
    };
    // Shown live, but moved only at its agent's turns: one behind says so.
    let turns = state.opened() && snapshot::has_agent(state);
    if layers.layers.is_empty() {
        out += "No layers.\n";
    } else {
        let header = ["LAYER", "SHOWS", "UPDATES", "YOUR COMMITS"].map(String::from);
        let rows: Vec<[String; 4]> = std::iter::once(header)
            .chain(layers.layers.iter().map(|l| {
                let commits = match (l.edit, editable.first() == Some(&l.branch)) {
                    (false, _) => String::new(),
                    (true, true) => sits(own),
                    (true, false) => "built on".into(),
                };
                let mut updates = updates(l);
                if turns && behind(common_dir, l) {
                    updates += "; updates at your next turn";
                }
                [l.branch.clone(), shows(l).into(), updates, commits]
            }))
            .collect();
        let width = |col: usize| {
            rows.iter()
                .map(|r| r[col].chars().count())
                .max()
                .unwrap_or(0)
                + 3
        };
        let (w0, w1, w2) = (width(0), width(1), width(2));
        for [layer, shows, updates, commits] in &rows {
            let line = format!("{layer:<w0$}{shows:<w1$}{updates:<w2$}{commits}");
            let _ = writeln!(out, "{}", line.trim_end());
        }
    }
    if editable.is_empty() {
        let main = land::main_name(common_dir, &Local::read(common_dir));
        let _ = writeln!(out, "Your commits sit on {main}.");
    }
    let notes = moves::notes(common_dir, &state.branch);
    let (conflicts, mut warnings) = match state.opened() {
        true => {
            let base = state.read_base()?;
            match crate::state::layers::view(common_dir, &base.id, layers)? {
                Some(view) => {
                    let own = crate::state::layers::own_among(state, &base, layers, &view)?;
                    (view.conflicts, [view.warnings, own].concat())
                }
                None => (Vec::new(), Vec::new()),
            }
        }
        false => (Vec::new(), Vec::new()),
    };
    let waiting = crate::state::waiting::of_workspace(state).unwrap_or_else(|e| {
        log::warn!("{}: its waiting edits: {e:#}", state.branch);
        Vec::new()
    });
    let alarms = crate::state::guard_check::read(state);
    if !conflicts.is_empty()
        || !warnings.is_empty()
        || !notes.is_empty()
        || !waiting.is_empty()
        || !alarms.is_empty()
    {
        out += "\n";
    }
    for alarm in &alarms {
        let _ = writeln!(out, "WARNING: {alarm}");
    }
    for w in &waiting {
        let _ = writeln!(out, "{}", w.describe());
    }
    for w in waiting.iter().filter(|w| w.conflict) {
        let _ = writeln!(out, "Conflict: {}", w.describe_conflict());
    }
    for note in &notes {
        let _ = writeln!(out, "Note: {note}");
    }
    for c in conflicts.iter().take(SHOWN_CONFLICTS) {
        let _ = writeln!(out, "Conflict: {}", c.describe(layers));
    }
    if conflicts.len() > SHOWN_CONFLICTS {
        let _ = writeln!(out, "and {} more", conflicts.len() - SHOWN_CONFLICTS);
    }
    warnings.dedup();
    for w in &warnings {
        let _ = writeln!(out, "{w}");
    }
    Ok(out)
}

/// Now, in seconds since the epoch.
fn now() -> i64 {
    crate::fsutil::epoch_secs(std::time::SystemTime::now())
}

/// `secs` (since the epoch) as local time: `HH:MM` today, else with the
/// date (`YYYY-MM-DD HH:MM`).
fn clock(secs: i64) -> String {
    let local = |t: i64| {
        // SAFETY: localtime_r only writes the `tm` given.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&(t as libc::time_t), &mut tm) };
        tm
    };
    let (tm, today) = (local(secs), local(now()));
    let time = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
    match (tm.tm_year, tm.tm_yday) == (today.tm_year, today.tm_yday) {
        true => time,
        false => format!(
            "{}-{:02}-{:02} {time}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::inserts_lines;

    /// Whole lines inserted anywhere count; text added inside a line, a
    /// changed or removed line, or an empty old text don't.
    #[test]
    fn edits_that_only_add_lines() {
        assert!(inserts_lines("net\n", "net\nui\n"));
        assert!(inserts_lines("core", "auth\ncore"));
        assert!(inserts_lines("a\nb", "a\nx\ny\nb"));
        assert!(inserts_lines("core\nnet\n", "core\nnet\nbilling\nui\n"));
        assert!(!inserts_lines("net", "net2"), "inside a line");
        assert!(!inserts_lines("net\n", "network\n"), "a changed line");
        assert!(!inserts_lines("a\nb\n", "a\nx\n"), "a removed line");
        assert!(!inserts_lines("a\n", "a\nb"), "b runs into the next line");
        assert!(!inserts_lines("", "a\n"), "no old text");
        assert!(!inserts_lines("a\n", "a\n"), "nothing added");
    }
}
