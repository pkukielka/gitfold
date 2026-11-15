//! `gitfold layer`: other branches' work shown in a workspace, read-only,
//! over its base (see `state::lens`), and the base itself (`layer base`,
//! see `stack`). The layers are stored with the workspace; while mounted,
//! the daemon shows them at once.

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
use crate::state::lens::{Layer, Lens, Shows, Updates, and_list};
use crate::state::retire::check_name;
use crate::state::seal;
use crate::store::Store;

/// How many conflicting paths the listing names before "and N more".
const SHOWN_CONFLICTS: usize = 5;

#[derive(Args)]
pub struct LayerArgs {
    #[command(subcommand)]
    pub op: Option<LayerOp>,
    /// As Claude Code's `UserPromptSubmit` hook (`user-prompt-submit`): bring
    /// the live layers of the workspace the session is in up to date (an
    /// agent's workspace as it was when its last turn ended, one without an
    /// agent as it is now, or a branch's tip), move it onto its base's new
    /// commits if it is built on one (`gitfold layer base`), and say what
    /// changed. As `PreToolUse` on
    /// Edit, Write, MultiEdit and NotebookEdit (`pre-tool-use`): deny
    /// writing a layer's file, saying why.
    #[arg(long, value_name = "EVENT", value_parser = ["user-prompt-submit", "pre-tool-use"])]
    pub claude_hook: Option<String>,
    /// The workspace (default: the one you are in).
    #[arg(short = 'w', long, global = true, value_name = "BRANCH")]
    pub workspace: Option<String>,
}

#[derive(Subcommand)]
pub enum LayerOp {
    /// Show BRANCH's work in the workspace, read-only, kept up to date.
    ///
    /// A branch with a workspace shows its work, uncommitted included;
    /// one without, its commits. A tag or a commit shows as it is. Adding
    /// it again replaces it (and brings it up to date).
    Add {
        /// A branch, or any revision.
        branch: String,
        /// As it is now: it never updates.
        #[arg(long)]
        frozen: bool,
        /// Only what the branch has committed.
        #[arg(long)]
        commits: bool,
    },
    /// Stop showing a layer.
    Remove {
        #[arg(required_unless_present = "all")]
        branch: Option<String>,
        /// Every layer.
        #[arg(long, conflicts_with = "branch")]
        all: bool,
    },
    /// Build the branch on TARGET, its base: its own commits move onto
    /// TARGET now, and onto TARGET's new commits from then on.
    ///
    /// It says first what it moves, and refuses (exit 4) to move commits
    /// already pushed (reachable from its upstream, or origin/<BRANCH>)
    /// without --yes. From then on its agent's next turn moves it (the
    /// `UserPromptSubmit` hook), unless --frozen; running it again moves it
    /// now. A workspace's files move as in a checkout, keeping uncommitted
    /// work; one whose uncommitted work is on a path the move changes waits,
    /// and a conflict leaves the branch as it was, reported (`gitfold list`
    /// shows both). A move that doesn't happen changes nothing, the base
    /// included. Refused while the workspace shows layers (`gitfold layer
    /// remove --all` first). When its base lands (a merge, squash, rebase
    /// merge or fast-forward), the branch moves onto what it landed in, by
    /// itself, or when you name that (`gitfold layer base main`): only its
    /// own commits move. The main branch (main, master, or gitfold.trunk)
    /// as TARGET takes the base away, its own commits moved onto main.
    /// gitfold never pushes: it prints the force-pushes to run. Exit codes:
    /// 3 no such branch, 4 not moved (commits already pushed, a conflict,
    /// uncommitted work in the way, git busy, layers shown), 1 other
    /// errors, 2 usage errors.
    Base {
        /// The branch to build on (the main branch: no base, just main).
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        target: Option<String>,
        /// Build on TARGET as it is now, and don't follow its new commits
        /// (running this again brings it up to date, still frozen).
        #[arg(long)]
        frozen: bool,
        /// Move commits already pushed too.
        #[arg(long)]
        yes: bool,
        /// Bring every branch with a base it follows up to date, bases
        /// first (frozen ones are left as they are).
        #[arg(long)]
        all: bool,
    },
}

/// `gitfold layer`.
pub fn layer(repo: &Path, args: LayerArgs) -> Result<()> {
    if let Some(LayerOp::Base {
        target,
        frozen,
        yes,
        all,
    }) = &args.op
    {
        let target = target.as_deref().unwrap_or_default();
        return super::stack::base(repo, args.workspace, target, *frozen, *yes, *all);
    }
    let name = super::workspace_or_current(args.workspace, repo)?;
    if let Some(LayerOp::Add { branch, .. }) = &args.op
        && *branch == name
    {
        super::usage(format!("{name} can't be a layer of its own workspace"));
    }
    check_name(&name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, &name);
    if !state.opened() {
        git::require_branch(&name)?;
        return Err(Refusal::NotFound.error(format!("{name} has no workspace yet")));
    }
    let store = Store::default();
    if args.op.is_none() {
        print!("{}", listing(&common_dir, &state, &Lens::read(&state)?)?);
        return Ok(());
    }
    let local = Local::read(&common_dir);
    // Read, changed and stored with no other change in between.
    let _lock = crate::state::lens::lock(&state)?;
    let mut lens = Lens::read(&state)?;
    let done = match args.op {
        None | Some(LayerOp::Base { .. }) => unreachable!("handled above"),
        Some(LayerOp::Add {
            branch,
            frozen,
            commits,
        }) => {
            // A move in progress first: it is what to wait for.
            refuse_if_moving(&common_dir, &state)?;
            refuse_with_base(&common_dir, &local, &name)?;
            let mut layer = layer_of(&store, &common_dir, &state, &branch, frozen, commits)?;
            let mut what = describe(&layer);
            if layer.updates == Updates::Live && layer.merge_base == layer.seal {
                let _ = write!(
                    what,
                    " (nothing to show yet: all of {branch} is in your base already; what it \
                     does next shows here)"
                );
            }
            match slot(&local, &lens, &name, &branch) {
                Some(i) => {
                    let was = &lens.layers[i];
                    // Not moved if it is where it was.
                    if was.seal == layer.seal {
                        layer.moved = was.moved;
                    }
                    let mine = |b: &str| local.proposal(b).is_some_and(|(_, by)| by == name);
                    let note = if was.branch == branch {
                        // A proposal again: still in place of its layer.
                        layer.instead = was.instead.clone();
                        String::new()
                    } else if mine(&branch) {
                        // A proposal in place of its layer, put back later.
                        layer.instead = Some(Box::new(was.clone()));
                        format!(" (in place of {})", was.branch)
                    } else {
                        format!(
                            " (in place of your proposal {0}, which stays: `gitfold layer add \
                             {0}` shows it again, `gitfold reject {0}` drops it)",
                            was.branch
                        )
                    };
                    lens.layers[i] = layer;
                    format!("layer {branch} updated: {what}{note}")
                }
                None => {
                    lens.layers.push(layer);
                    format!("layer {branch} added: {what}")
                }
            }
        }
        Some(LayerOp::Remove { branch: None, .. }) => {
            lens.layers.clear();
            format!("{name}: every layer removed")
        }
        Some(LayerOp::Remove {
            branch: Some(branch),
            ..
        }) => {
            // Its layer, or this workspace's proposal shown in its place; a
            // proposal only itself.
            let mine = local.proposal(&branch).is_some_and(|(_, by)| by == name);
            let i = slot(&local, &lens, &name, &branch)
                .filter(|&i| !mine || lens.layers[i].branch == branch)
                .ok_or_else(|| {
                    Refusal::NotFound.error(format!("{branch} isn't a layer of {name}"))
                })?;
            let mut was = lens.layers.remove(i);
            match was.branch == branch {
                // Your proposal shown in its layer's place: that layer back.
                true if mine => {
                    let back = was.instead.take().map(|instead| {
                        let note = format!("; {} shown again as it was", instead.branch);
                        lens.layers.insert(i, *instead);
                        note
                    });
                    format!(
                        "layer {branch} removed{}; the proposal stays: `gitfold reject {branch}` \
                         drops it",
                        back.unwrap_or_default()
                    )
                }
                true => format!("layer {branch} removed"),
                false => format!(
                    "layer {branch} removed (it showed your proposal {0}, which stays: `gitfold \
                     reject {0}` drops it)",
                    was.branch
                ),
            }
        }
    };
    let applied = store_and_apply(&common_dir, &state, &lens)?;
    println!("{done}\n{applied}");
    Ok(())
}

/// The layer of `lens` (the layers of workspace `name`) that stands for
/// `branch`: `branch`'s own, or `name`'s proposal to change it shown in its
/// place. A proposal of `name`'s stands for its layer: the same one.
pub(super) fn slot(local: &Local, lens: &Lens, name: &str, branch: &str) -> Option<usize> {
    let target_of = |b: &str| {
        (local.proposal(b))
            .filter(|(_, by)| by == name)
            .map_or_else(|| b.to_string(), |(layer, _)| layer)
    };
    let target = target_of(branch);
    (lens.layers.iter()).position(|l| l.branch == branch || target_of(&l.branch) == target)
}

/// Refuses layers in the workspace of `name` if it is built on a base (not
/// the trunk): moving it onto its base's new commits with layers shown
/// isn't supported yet.
fn refuse_with_base(common_dir: &Path, local: &Local, name: &str) -> Result<()> {
    let Some(parent) = local.parent(name) else {
        return Ok(());
    };
    if super::stack::land::is_trunk(common_dir, local, &parent) {
        return Ok(());
    }
    let trunk = super::stack::land::main_name(common_dir, local);
    Err(Refusal::Changes.error(format!(
        "{name} is built on {parent}; layers on a branch with a base aren't supported yet: \
         show {parent}'s work with a snapshot (`gitfold snapshot {parent}`), or take {name} \
         off its base first (`gitfold layer base {trunk}`)"
    )))
}

/// What `layer` shows and how it updates, as the listing says it.
fn describe(layer: &Layer) -> String {
    format!("{}, {}", shows(layer.shows), updates(layer))
}

fn shows(shows: Shows) -> &'static str {
    match shows {
        Shows::Work => "all work",
        Shows::CommitsOnly => "commits only",
        Shows::Commits => "commits",
    }
}

fn updates(layer: &Layer) -> String {
    match layer.updates {
        Updates::Live => format!("live, last {}", clock(layer.moved)),
        Updates::Frozen => format!("frozen at {}", clock(layer.moved)),
        Updates::Fixed => "fixed".into(),
    }
}

/// `gitfold layer --claude-hook user-prompt-submit`: at the start of a turn,
/// the live layers of the workspace the session is in move to their latest
/// work (`latest_work`: an agent's workspace as `gitfold snapshot
/// --claude-hook stop` took it when its last turn ended, one without an
/// agent as it is now), so what shows changes only between turns, never
/// half-way through another's; a branch built on a base is moved onto its
/// new commits first (`stack::hook_context`). What changed is printed,
/// which becomes context. Does nothing outside a workspace with layers or a
/// base; errors are only logged.
pub fn layer_hook(event: &str) -> Result<()> {
    if event == "pre-tool-use" {
        return deny_hook();
    }
    if let Err(e) = (|| -> Result<()> {
        let repo = super::repo()?;
        let name = super::branch_or_current(None, &repo)?;
        let common_dir = git::init(&repo)?;
        // A branch built on a base first follows its new commits, at the
        // same boundary (see `stack`).
        match super::stack::hook_context(&common_dir, &name) {
            Ok(Some(context)) => println!("{context}"),
            Ok(None) => {}
            Err(e) => log::debug!("gitfold layer --claude-hook: moving {name}: {e:#}"),
        }
        let state = StateDir::of(&common_dir, &name);
        let live = |lens: &Lens| lens.layers.iter().any(|l| l.updates == Updates::Live);
        // Most turns: nothing to lock for.
        if !live(&Lens::read(&state)?) {
            return Ok(());
        }
        let _lock = crate::state::lens::lock(&state)?;
        let mut lens = Lens::read(&state)?;
        let (mut moved, mut stored) = (Vec::new(), false);
        let local = Local::read(&common_dir);
        let base = state.read_base()?.id;
        for layer in &mut lens.layers {
            if layer.updates != Updates::Live {
                continue;
            }
            // One layer that can't move now (its workspace busy, say) leaves
            // the others to move.
            let files = match follow(&common_dir, &local, base, layer) {
                Ok(Some(files)) => files,
                Ok(None) => continue,
                Err(e) => {
                    log::debug!("gitfold layer --claude-hook: layer {}: {e:#}", layer.branch);
                    continue;
                }
            };
            stored = true;
            // The same files (its snapshot of what its tip has): nothing to
            // say, nor a move to show.
            if files == 0 {
                continue;
            }
            layer.moved = now();
            moved.push(format!(
                "{} ({})",
                layer.branch,
                super::plural(files, "file changed", "files changed")
            ));
        }
        if !stored {
            return Ok(());
        }
        let applied = store_and_apply(&common_dir, &state, &lens)?;
        if moved.is_empty() {
            return Ok(());
        }
        println!(
            "gitfold: the layers shown in this workspace moved to their latest work: {}. \
             Files you read from them may have changed.",
            moved.join(", ")
        );
        for note in applied.lines().filter(|l| l.starts_with("note: ")) {
            println!("gitfold: {note}");
        }
        Ok(())
    })() {
        log::debug!("gitfold layer --claude-hook: {e:#}");
    }
    Ok(())
}

/// Moves live `layer` (of a workspace on `base`) to where it is now
/// (`newest`): how many files changed, `None` if it didn't move.
fn follow(
    common_dir: &Path,
    local: &Local,
    base: ObjectId,
    layer: &mut Layer,
) -> Result<Option<usize>> {
    let Some((newest, shows)) = newest(common_dir, local, layer)? else {
        return Ok(None);
    };
    if newest == layer.seal {
        return Ok(None);
    }
    let (from, to) = (
        git::resolve(&layer.seal.to_string())?,
        git::resolve(&newest.to_string())?,
    );
    let files = git::with_repo(|repo| git::lens::delta(repo, from.tree, to.tree))?.len();
    layer.merge_base = git::merge_base(base, newest)?;
    layer.seal = newest;
    layer.shows = shows;
    Ok(Some(files))
}

/// Where live `layer` is now, and what it shows there: its workspace's
/// work (`latest_work`), or with `--commits` or without a workspace, its
/// branch's tip (`None` if the branch is gone).
fn newest(common_dir: &Path, local: &Local, layer: &Layer) -> Result<Option<(ObjectId, Shows)>> {
    if !git::branch_exists(&layer.branch) {
        return Ok(None);
    }
    let theirs = StateDir::of(common_dir, &layer.branch);
    if layer.shows != Shows::CommitsOnly && theirs.opened() {
        let work = latest_work(&Store::default(), &theirs)?;
        return Ok(work.map(|work| (work, Shows::Work)));
    }
    let shows = match layer.shows {
        Shows::CommitsOnly => Shows::CommitsOnly,
        _ if is_proposal(local, &layer.branch) => Shows::Work,
        _ => Shows::Commits,
    };
    Ok(Some((git::branch_tip(&layer.branch)?.id, shows)))
}

/// The work of the workspace in `theirs` a live layer shows (see
/// `state::seal`): an agent's (one whose `Stop` hook ran) as it was at the
/// end of its last turn, never half-way through one; one without an agent
/// (a person's, a proposal edited from another workspace) has no turns, so
/// a fresh snapshot, as it is now. An agent's that wasn't shown when its
/// last turn ended has none to follow (`None`): the layer stays as it was
/// added.
fn latest_work(store: &Store, theirs: &StateDir) -> Result<Option<ObjectId>> {
    Ok(match seal::driven(theirs) {
        seal::Driven::Agent(last_turn) => last_turn,
        seal::Driven::Person => Some(seal::seal(store, theirs, seal::By::Other)?.commit),
    })
}

/// Whether `branch` is a proposal: a workspace to edit, its work shown as
/// all work even before it first opens (its tip, then).
fn is_proposal(local: &Local, branch: &str) -> bool {
    local.proposal(branch).is_some()
}

/// `gitfold layer --claude-hook pre-tool-use`: denies Claude Code's edit
/// tools a file a layer shows (the filesystem would refuse it with a bare
/// EACCES), saying whose it is and what to do instead: first, propose a
/// change (`gitfold propose`). Allows everything else, and logs its errors.
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
        let lens = crate::state::lens::Applied::read(&state)?.layers()?;
        let owners = owners(&state, &lens, path.as_bytes())?;
        let names: Vec<&str> = (owners.iter())
            .map(|&i| lens.layers[i].branch.as_str())
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
        if let [_] = names[..] {
            return Ok(Some(format!(
                "{path} shows {owner}'s work (a layer, read-only here): it can't be changed in \
                 this workspace. To change it, run `gitfold propose {path}`: it makes a copy \
                 of {owner}'s work for you to edit (a proposal), shown here in its place, \
                 which {owner}'s agent tests against every workspace that read the file and \
                 accepts. Or leave it to {owner}'s agent (`gitfold send {owner} <text>` asks \
                 it), or run `gitfold layer remove {owner}` to stop showing the layer and \
                 edit your own version."
            )));
        }
        let all = and_list(names.iter().map(|n| n.to_string()));
        let base = state.read_base()?;
        let view = crate::state::lens::view(&state.common_dir, &base.id, &lens)?;
        let conflict =
            match view.and_then(|v| v.conflicts.into_iter().find(|c| c.path == path.as_bytes())) {
                Some(c) => {
                    let names = c.layers.iter().map(|&i| lens.layers[i].branch.clone());
                    format!(" The changes of {} conflict there.", and_list(names))
                }
                None => String::new(),
            };
        Ok(Some(format!(
            "{path} shows the merge of {all}'s work (layers, read-only here): it can't be \
             changed in this workspace.{conflict} To change it, run `gitfold propose {path} \
             --layer <one of them>`: it makes a copy of that layer's work for you to edit \
             (a proposal), shown here in its place, which its agent tests and accepts. Or \
             leave it to their agents (`gitfold send {owner} <text>` asks {owner}'s), or \
             run `gitfold layer remove {owner}` (and the others) to stop showing them and \
             edit your own version."
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

/// The layers of `lens` (shown in the workspace in `state`) whose changes
/// show at `path`, read-only there: one, or those whose changes are merged
/// there. None if what shows is the workspace's own or its base's.
pub(super) fn owners(state: &StateDir, lens: &Lens, path: &[u8]) -> Result<Vec<usize>> {
    if !lens.is_on() {
        return Ok(Vec::new());
    }
    // Its own file at that path wins: writable.
    if std::fs::symlink_metadata(crate::state::upper_path(&state.upper(), path)).is_ok() {
        return Ok(Vec::new());
    }
    git::init(&state.common_dir)?;
    // Most edits are of files no layer changes: told apart without building
    // the view (the deny hook runs on every edit).
    if !changed_by_any(lens, path)? {
        return Ok(Vec::new());
    }
    let base = state.read_base()?;
    let Some(view) = crate::state::lens::view(&state.common_dir, &base.id, lens)? else {
        return Ok(Vec::new());
    };
    Ok(view.mixed.get(path).cloned().unwrap_or_default())
}

/// Whether a layer of `lens` changes `path`: its entry there differs
/// between the layer's merge base and its seal.
fn changed_by_any(lens: &Lens, path: &[u8]) -> Result<bool> {
    git::with_repo(|repo| {
        let at = |commit: ObjectId| -> Result<Option<git::lens::Item>> {
            let tree = repo.find_commit(commit)?.tree_id()?.detach();
            git::lens::lookup(repo, tree, path.into())
        };
        for layer in &lens.layers {
            if at(layer.merge_base)? != at(layer.seal)? {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

/// The layer of `branch` (a branch, or any revision) as it is now, for the
/// workspace in `state`: a workspace's work (its seal) unless `commits`,
/// else a branch's tip or the revision; live unless `frozen` (a revision is
/// fixed); with where it and the workspace's base diverged.
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
    let (seal, shows, updates) = if git::branch_exists(branch) {
        check_name(branch)?;
        let theirs = StateDir::of(common_dir, branch);
        match (theirs.opened(), commits) {
            // Live: as it follows it (`latest_work`), else as it is now.
            (true, false) => {
                let work = match (frozen, seal::driven(&theirs)) {
                    (false, seal::Driven::Agent(Some(last_turn))) => last_turn,
                    _ => seal::seal(store, &theirs, seal::By::Other)?.commit,
                };
                (work, Shows::Work, updates)
            }
            (_, true) => (git::branch_tip(branch)?.id, Shows::CommitsOnly, updates),
            (false, false) => {
                let shows = match is_proposal(&Local::read(common_dir), branch) {
                    true => Shows::Work,
                    false => Shows::Commits,
                };
                (git::branch_tip(branch)?.id, shows, updates)
            }
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
    let merge_base = git::merge_base(state.read_base()?.id, seal)?;
    // A layer shows changes on top of the base: one that never moves and is
    // part of the base already would show nothing.
    if updates != Updates::Live && merge_base == seal {
        return Err(Refusal::Changes.error(format!(
            "{branch} is part of {}'s base already, so as a layer it shows nothing (a layer \
             shows changes on top of the base). To look at {branch} as it was: `gitfold \
             snapshot {branch}`",
            state.branch
        )));
    }
    Ok(Layer {
        branch: branch.to_string(),
        shows,
        updates,
        moved: now(),
        seal,
        merge_base,
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

/// Stores `lens` for the workspace in `state` and has the daemon show it
/// now, if mounted; says what happened. If the daemon refuses, the layers
/// stored before are put back, so what is stored is what shows.
pub(super) fn store_and_apply(common_dir: &Path, state: &StateDir, lens: &Lens) -> Result<String> {
    refuse_if_moving(common_dir, state)?;
    let before = crate::state::lens::stored_bytes(state)?;
    lens.write(state)?;
    apply(common_dir, &state.branch).inspect_err(|_| {
        if let Err(e) = crate::state::lens::store_bytes(state, &before) {
            log::warn!("putting the layers back: {e:#}");
        }
    })
}

/// Refuses changing the layers of the workspace in `state` while it is
/// being moved onto its base: the move moves the base a switch builds the
/// view on.
fn refuse_if_moving(common_dir: &Path, state: &StateDir) -> Result<()> {
    if !crate::state::restack::pending(state) {
        return Ok(());
    }
    let base = Local::read(common_dir)
        .parent(&state.branch)
        .unwrap_or_else(|| "<base>".into());
    Err(Refusal::Changes.error(format!(
        "{} is being moved onto its base {base} (or that was interrupted: `gitfold layer base \
         {base}` finishes it); retry once it is done",
        state.branch
    )))
}

/// Has the daemon show the stored layers now, if mounted; says what happened.
pub(super) fn apply(common_dir: &Path, name: &str) -> Result<String> {
    let request = Request::Lens {
        branch: name.to_string(),
    };
    match control::request(common_dir, &request, None)? {
        Some(done) => Ok(done),
        None => Ok(format!(
            "{name}: not mounted; its layers show when it opens"
        )),
    }
}

/// `gitfold layer`: the base line, then the layers and their conflicts.
fn listing(common_dir: &Path, state: &StateDir, lens: &Lens) -> Result<String> {
    let mut out = base_line(common_dir, &state.branch)?;
    if lens.layers.is_empty() {
        out += "No layers.\n";
        return Ok(out);
    }
    out += "\n";
    let rows: Vec<[String; 3]> =
        std::iter::once(["LAYER".into(), "SHOWS".into(), "UPDATES".into()])
            .chain(
                lens.layers
                    .iter()
                    .map(|l| [l.branch.clone(), shows(l.shows).into(), updates(l)]),
            )
            .collect();
    let width = |col: usize| {
        rows.iter()
            .map(|r| r[col].chars().count())
            .max()
            .unwrap_or(0)
            + 3
    };
    let (w0, w1) = (width(0), width(1));
    for [layer, shows, updates] in &rows {
        let _ = writeln!(out, "{layer:<w0$}{shows:<w1$}{updates}");
    }
    let conflicts = match lens.layers.len() {
        1 => Vec::new(),
        _ => match crate::state::lens::view(common_dir, &state.read_base()?.id, lens)? {
            Some(view) => view.conflicts,
            None => Vec::new(),
        },
    };
    if !conflicts.is_empty() {
        out += "\n";
    }
    for c in conflicts.iter().take(SHOWN_CONFLICTS) {
        let names: Vec<String> = c
            .layers
            .iter()
            .map(|&i| lens.layers[i].branch.clone())
            .collect();
        let both = if names.len() == 2 { "both " } else { "" };
        let shown = match c.markers {
            true => "",
            false => " (they can't be merged: it shows your base's version)",
        };
        let _ = writeln!(
            out,
            "Conflict: {} is changed by {both}{}{shown}",
            c.path,
            and_list(names)
        );
    }
    if conflicts.len() > SHOWN_CONFLICTS {
        let _ = writeln!(out, "and {} more", conflicts.len() - SHOWN_CONFLICTS);
    }
    Ok(out)
}

/// The listing's base line (and the lines below it): what `branch` is built
/// on, whether it follows it, and its notes (see `stack::notes`).
fn base_line(common_dir: &Path, branch: &str) -> Result<String> {
    use super::stack::land;
    let local = Local::read(common_dir);
    let Some(parent) = local.parent(branch) else {
        let trunk = land::trunk_name(common_dir, &local).unwrap_or_else(|| "-".into());
        return Ok(format!("Base: {trunk}\n"));
    };
    let trunk = land::is_trunk(common_dir, &local, &parent);
    let frozen = local.restack_manual(branch);
    let head = format!("Base: {parent}, {}", if frozen { "frozen" } else { "live" });
    let tip = git::branch_tip(branch)?.id;
    let own = match land::resolve(common_dir, &parent)? {
        Some((parent_ref, parent_tip)) => {
            let base = git::stack::parent_base(common_dir, branch, &parent_ref, tip, parent_tip)?;
            Some(git::stack::count(common_dir, base.commit, tip)?)
        }
        None => None,
    };
    let commits = match own {
        Some(0) => "no commits of your own yet".to_string(),
        Some(1) => format!("your 1 commit sits on {parent}"),
        Some(n) => format!("your {n} commits sit on {parent}"),
        None => format!("your commits sit on {parent}"),
    };
    let how = match (frozen || trunk, own) {
        (true, _) => format!(
            "{} updates it",
            super::stack::base_command(&parent, frozen, true)
        ),
        (false, Some(0)) => format!("it follows {parent}"),
        (false, Some(1)) => format!("it moves when {parent} gets new commits"),
        (false, _) => format!("they move when {parent} gets new commits"),
    };
    let width = head.chars().count().max(15) + 6;
    let mut out = format!("{head:<width$}{commits}; {how}\n");
    // When the stack ref last moved, while git keeps it loose.
    let stack_ref = common_dir.join(git::stack::stack_ref(branch));
    if let Ok(at) = std::fs::metadata(stack_ref).and_then(|m| m.modified()) {
        let secs = at
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let _ = writeln!(out, "{:width$}last moved {}", "", clock(secs));
    }
    for note in super::stack::notes(common_dir, &local, branch) {
        let _ = writeln!(out, "{:width$}{note}", "");
    }
    Ok(out)
}

/// Now, in seconds since the epoch.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
