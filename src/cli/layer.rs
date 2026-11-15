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
    /// the live layers of the workspace the session is in up to date (a
    /// workspace's newest snapshot, made when its session stopped, or a
    /// branch's tip), move it onto its base's new commits if it is built on
    /// one (`gitfold layer base`), and say what changed. As `PreToolUse` on
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
    /// Build the branch on TARGET: move its own commits onto it, or bring
    /// them up to date.
    ///
    /// They then follow TARGET's new commits (unless --frozen). The trunk
    /// (main, say) takes the branch off its base.
    Base {
        #[arg(required_unless_present = "all")]
        target: Option<String>,
        /// Build on TARGET as it is now; don't follow it.
        #[arg(long)]
        frozen: bool,
        /// Move commits that were pushed already.
        #[arg(long)]
        yes: bool,
        /// Bring every branch built on another up to date.
        #[arg(long, conflicts_with = "target")]
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
    }) = args.op
    {
        let target = target.unwrap_or_default();
        return super::stack::base(repo, args.workspace, &target, frozen, yes, all);
    }
    let name = super::branch_or_current(args.workspace, repo)?;
    check_name(&name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, &name);
    if !state.opened() {
        git::require_branch(&name)?;
        return Err(Refusal::NotFound.error(format!("{name} has no workspace yet")));
    }
    let store = Store::default();
    let mut lens = Lens::read(&state)?;
    let done = match args.op {
        None => {
            print!("{}", listing(&common_dir, &state, &lens)?);
            return Ok(());
        }
        Some(LayerOp::Base { .. }) => unreachable!("handled above"),
        Some(LayerOp::Add {
            branch,
            frozen,
            commits,
        }) => {
            ensure!(branch != name, "a workspace can't be its own layer");
            let mut layer = layer_of(&store, &common_dir, &state, &branch, frozen, commits)?;
            let what = describe(&layer);
            match lens.find(&branch) {
                Some(i) => {
                    // Not moved if it is where it was.
                    if lens.layers[i].seal == layer.seal {
                        layer.moved = lens.layers[i].moved;
                    }
                    lens.layers[i] = layer;
                    format!("layer {branch} updated: {what}")
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
            let i = lens.find(&branch).ok_or_else(|| {
                Refusal::NotFound.error(format!("{branch} isn't a layer of {name}"))
            })?;
            lens.layers.remove(i);
            format!("layer {branch} removed")
        }
    };
    let applied = store_and_apply(&common_dir, &state, &lens)?;
    println!("{done}\n{applied}");
    Ok(())
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
/// the live layers of the workspace the session is in move to their newest
/// work (a workspace's newest snapshot: `gitfold snapshot --claude-hook
/// stop` makes one when a session stops), so what shows changes only
/// between turns, never half-way through another's; a branch built on a
/// base is moved onto its new commits first (`stack::hook_context`). What
/// changed is printed, which becomes context. Does nothing outside a
/// workspace with layers or a base; errors are only logged.
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
        let mut lens = Lens::read(&state)?;
        let mut moved = Vec::new();
        let local = Local::read(&common_dir);
        for layer in &mut lens.layers {
            if layer.updates != Updates::Live {
                continue;
            }
            // Its own proposal is edited from this session, whose end
            // snapshots only this workspace: snapshotted here, so the edits
            // show.
            let theirs = StateDir::of(&common_dir, &layer.branch);
            if local
                .proposal(&layer.branch)
                .is_some_and(|(_, by)| by == name)
                && theirs.opened()
            {
                seal::seal(&Store::default(), &theirs)?;
            }
            let Some((newest, shows)) = newest(&common_dir, layer)? else {
                continue;
            };
            if newest == layer.seal {
                continue;
            }
            let (from, to) = (
                git::resolve(&layer.seal.to_string())?,
                git::resolve(&newest.to_string())?,
            );
            let files = git::with_repo(|repo| git::lens::delta(repo, from.tree, to.tree))?.len();
            layer.merge_base = git::merge_base(state.read_base()?.id, newest)?;
            layer.seal = newest;
            layer.shows = shows;
            layer.moved = now();
            moved.push(format!(
                "{} ({files} {})",
                layer.branch,
                if files == 1 {
                    "file changed"
                } else {
                    "files changed"
                }
            ));
        }
        if moved.is_empty() {
            return Ok(());
        }
        let applied = store_and_apply(&common_dir, &state, &lens)?;
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

/// Where live `layer` is now, and what it shows there: its workspace's
/// newest snapshot (`None` if it has none), or with `--commits` or without
/// a workspace, its branch's tip (`None` if the branch is gone).
fn newest(common_dir: &Path, layer: &Layer) -> Result<Option<(ObjectId, Shows)>> {
    if !git::branch_exists(&layer.branch) {
        return Ok(None);
    }
    if layer.shows != Shows::CommitsOnly && StateDir::of(common_dir, &layer.branch).opened() {
        let id = crate::state::workspace_id(&layer.branch);
        let newest = seal::list(common_dir, &id)?.last().map(|&(_, s)| s);
        return Ok(newest.map(|s| (s, Shows::Work)));
    }
    let shows = match layer.shows {
        Shows::CommitsOnly => Shows::CommitsOnly,
        _ => Shows::Commits,
    };
    Ok(Some((git::branch_tip(&layer.branch)?.id, shows)))
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
                 here: edit it there, at {}; `gitfold layer add {owner}` then shows your \
                 edits here (your next turn does too).",
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
        let conflict = match crate::state::lens::view(&state.common_dir, &base.id, &lens)? {
            Some(v) if v.conflicts.iter().any(|c| c.path == path.as_bytes()) => {
                " Their changes to it conflict."
            }
            _ => "",
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
            (true, false) => (seal::seal(store, &theirs)?.commit, Shows::Work, updates),
            (_, true) => (git::branch_tip(branch)?.id, Shows::CommitsOnly, updates),
            (false, false) => (git::branch_tip(branch)?.id, Shows::Commits, updates),
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
    Ok(Layer {
        branch: branch.to_string(),
        shows,
        updates,
        moved: now(),
        seal,
        merge_base: git::merge_base(state.read_base()?.id, seal)?,
    })
}

/// Stores `lens` for the workspace in `state` and has the daemon show it
/// now, if mounted; says what happened. If the daemon refuses, the layers
/// stored before are put back, so what is stored is what shows.
pub(super) fn store_and_apply(common_dir: &Path, state: &StateDir, lens: &Lens) -> Result<String> {
    // A move onto its base moves the base a switch builds the view on.
    if crate::state::restack::pending(state) {
        let base = Local::read(common_dir)
            .parent(&state.branch)
            .unwrap_or_else(|| "<base>".into());
        return Err(Refusal::Changes.error(format!(
            "{} is being moved onto {base} (or that was interrupted: `gitfold layer base \
             {base}` finishes it); retry once it is done",
            state.branch
        )));
    }
    let before = crate::state::lens::stored_bytes(state)?;
    lens.write(state)?;
    apply(common_dir, &state.branch).inspect_err(|_| {
        if let Err(e) = crate::state::lens::store_bytes(state, &before) {
            log::warn!("putting the layers back: {e:#}");
        }
    })
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
        let trunk = land::trunk(common_dir, &local).unwrap_or_else(|| "-".into());
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
    let (commits, they) = match own {
        Some(1) => ("your 1 commit sits".to_string(), "it moves"),
        Some(n) => (format!("your {n} commits sit"), "they move"),
        None => ("your commits sit".to_string(), "they move"),
    };
    let when = match frozen || trunk {
        true => format!("when you run `gitfold layer base {parent}`"),
        false => format!("when {parent} gets new commits"),
    };
    let width = head.chars().count().max(15) + 6;
    let mut out = format!("{head:<width$}{commits} on {parent}; {they} {when}\n");
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
