//! Proposals: changing a layer's files (a contract) from a workspace that
//! shows it read-only. `gitfold propose` forks the layer's workspace into a
//! proposal branch (`branch --with-changes`), where the files are the
//! proposer's to edit, and shows the proposal in place of the layer;
//! `gitfold consent` checks the change against every workspace that shows
//! the layer (each one's view with the change accepted, and a test run in
//! it); `gitfold accept` writes the proposal's files into the layer's
//! workspace, switches the proposer back to the layer, tells the readers,
//! and removes the proposal; `gitfold reject` removes it untaken. `gitfold
//! proposals` lists them.
//!
//! A proposal is a branch with `branch.<name>.gitfoldProposal` (the layer)
//! and `gitfoldProposedBy` (see `settings`); what it starts from (the
//! layer's work when it was made) is kept by `refs/gitfold-proposal/<id>`.
//! Its changes are its work (a snapshot) against that. What accepting it gives
//! is the layer's work as now with those changes: what consent checks, and
//! what accept writes.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Seek as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::daemon::control;
use crate::git;
use crate::git::layers::{Change, Item};
use crate::mount_table;
use crate::refusal::Refusal;
use crate::settings::{self, Local};
use crate::state::layers::{Layer, Layers, Shows, Updates};
use crate::state::markers::{Kind, Markers};
use crate::state::retire::check_name;
use crate::state::snapshot::IDENTITY;
use crate::state::{self, StateDir, fork, snapshot};
use crate::store::Store;

#[derive(Args)]
pub struct ProposeArgs {
    /// The files to change: relative to the current directory in the
    /// workspace, else to the workspace's root.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// The layer to change (default: the one whose file the first path
    /// shows).
    #[arg(long, value_name = "BRANCH")]
    pub layer: Option<String>,
    /// Take the new versions from this directory (`<DIR>/<PATH>`), instead
    /// of editing them in the proposal's workspace afterwards.
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
    /// The workspace proposing (default: the one you are in).
    #[arg(short = 'w', long, value_name = "BRANCH")]
    pub workspace: Option<String>,
}

#[derive(Args)]
pub struct ProposalsArgs {
    /// Only the proposals to change this layer.
    pub layer: Option<String>,
}

#[derive(Args)]
pub struct ConsentArgs {
    /// The proposal's branch.
    pub proposal: String,
    /// A command run (`sh -c`) in each workspace's view with the change
    /// accepted (a read-only snapshot); it must exit 0. It gets the
    /// workspace's branch as GITFOLD_WORKSPACE and an empty writable
    /// directory, removed after, as GITFOLD_SCRATCH (for build output).
    #[arg(long, value_name = "CMD")]
    pub test: Option<String>,
    /// How long the test may run in each workspace's view, in seconds.
    #[arg(long, value_name = "SECS", default_value_t = 600, requires = "test")]
    pub timeout: u64,
    /// Skip the workspaces whose read records say they didn't read a file
    /// the proposal changes (by default they are checked too: a build reads
    /// files no agent opened).
    #[arg(long)]
    pub only_readers: bool,
}

#[derive(Args)]
pub struct AcceptArgs {
    /// The proposal's branch.
    pub proposal: String,
    /// Accept without a passed consent check of the proposal and the layer
    /// as they are now.
    #[arg(long)]
    pub force: bool,
    /// The layer's workspace, when run outside any workspace (default: the
    /// one you are in).
    #[arg(short = 'w', long, value_name = "BRANCH")]
    pub workspace: Option<String>,
}

#[derive(Args)]
pub struct RejectArgs {
    /// The proposal's branch.
    pub proposal: String,
    /// Why, for the proposer's inbox.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,
    /// The layer's or the proposer's workspace, when run outside any
    /// workspace (default: the one you are in).
    #[arg(short = 'w', long, value_name = "BRANCH")]
    pub workspace: Option<String>,
}

/// Where proposals' starts are kept.
const START_REFS: &str = "refs/gitfold-proposal/";

/// The ref keeping what proposal `id` (a workspace id) started from.
fn start_ref(id: &str) -> String {
    format!("{START_REFS}{id}")
}

/// An open proposal.
struct Proposal {
    name: String,
    /// The layer it would change.
    layer: String,
    /// The workspace that proposed it.
    by: String,
    /// The layer's work when it was proposed.
    start: git::Commit,
    /// Its work as now.
    work: git::Commit,
}

impl Proposal {
    fn load(store: &Store, common_dir: &Path, local: &Local, name: &str) -> Result<Proposal> {
        check_name(name)?;
        let Some((layer, by)) = local.proposal(name).filter(|_| git::branch_exists(name)) else {
            return Err(Refusal::NotFound.error(format!("{name} isn't a proposal")));
        };
        let start = git::resolve(&start_ref(&state::workspace_id(name)))
            .with_context(|| format!("what {name} started from"))?;
        let work = git::resolve(&work(store, common_dir, name)?.to_string())?;
        Ok(Proposal {
            name: name.to_string(),
            layer,
            by,
            start,
            work,
        })
    }

    /// What it changes: paths, files only, in order.
    fn changes(&self) -> Result<Vec<(BString, Change)>> {
        git::with_repo(|repo| git::layers::delta(repo, self.start.tree, self.work.tree))
    }

    /// The proposed paths the layer's work `now` changed since the start:
    /// accepting would undo those changes.
    fn moved(&self, now: &git::Commit, paths: &[BString]) -> Result<Vec<String>> {
        git::with_repo(|repo| {
            let at = |tree, path: &BString| git::layers::lookup(repo, tree, path.as_ref());
            let mut moved = Vec::new();
            for path in paths {
                if at(now.tree, path)? != at(self.start.tree, path)? {
                    moved.push(path.to_string());
                }
            }
            Ok(moved)
        })
    }

    /// The refusal for `moved` paths (see `moved`).
    fn moved_error(&self, moved: &[String]) -> anyhow::Error {
        Refusal::Changes.error(format!(
            "{} changed {} since {} was proposed: accepting it would undo that. Reject it \
             (`gitfold reject {}`) and have {} propose again, from the work as now",
            self.layer,
            moved.join(", "),
            self.name,
            self.name,
            self.by,
        ))
    }

    /// The layer's work `now` with the proposal's changes: what accepting
    /// it gives (with no moved paths).
    fn accepted(
        &self,
        common_dir: &Path,
        now: &git::Commit,
        changes: &[(BString, Change)],
    ) -> Result<ObjectId> {
        let tree = git::with_repo(|repo| {
            let changes = changes.iter().map(|(p, c)| (p.as_ref(), *c));
            git::layers::edit_tree(repo, now.tree, changes)
        })?;
        let message = format!("gitfold consent: {} with {}", self.layer, self.name);
        commit(common_dir, &tree, &[now.id], &message)
    }
}

/// A commit of `tree` on `parents`, made with `IDENTITY` (the same each time).
fn commit(
    common_dir: &Path,
    tree: &ObjectId,
    parents: &[ObjectId],
    message: &str,
) -> Result<ObjectId> {
    let tree = tree.to_string();
    let mut args = vec!["commit-tree".to_string(), tree, "-m".into(), message.into()];
    for p in parents {
        args.extend(["-p".to_string(), p.to_string()]);
    }
    let out = git::cmd::command(common_dir)
        .args(&args)
        .envs(IDENTITY)
        .output()?;
    ensure!(
        out.status.success(),
        "git commit-tree: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(ObjectId::from_hex(
        String::from_utf8_lossy(&out.stdout).trim().as_bytes(),
    )?)
}

/// The work of `branch`: its workspace's snapshot, or its tip if it has none.
fn work(store: &Store, common_dir: &Path, branch: &str) -> Result<ObjectId> {
    let state = StateDir::of(common_dir, branch);
    Ok(match state.opened() {
        true => snapshot::snapshot(store, &state, snapshot::By::Other)?.commit,
        false => git::branch_tip(branch)?.id,
    })
}

/// The workspace a command is run by: the one the current directory is in;
/// `-w` only from outside every workspace (an agent in one acts as it).
fn caller(repo: &Path, named: Option<String>, what: &str) -> Result<String> {
    match (super::branch_or_current(None, repo), named) {
        (Ok(here), Some(named)) if named != here => Err(Refusal::NotFound.error(format!(
            "you are in workspace {here}: {what} it from {named}'s workspace (-w names one \
             only from outside every workspace)"
        ))),
        (Ok(here), _) => Ok(here),
        (Err(_), Some(named)) => Ok(named),
        (Err(_), None) => bail!("{what} it in its workspace, or name that with -w"),
    }
}

/// `gitfold propose`.
pub fn propose(repo: &Path, args: ProposeArgs) -> Result<()> {
    let name = super::workspace_or_current(args.workspace, repo)?;
    check_name(&name)?;
    let common_dir = git::init(repo)?;
    let state = super::opened_workspace(&common_dir, &name)?;
    let paths = workspace_paths(&state, &args.paths)?;
    let store = Store::default();
    let local = Local::read(&common_dir);
    // Read, changed and stored with no other change in between.
    let _lock = state::layers::lock(&state)?;
    let mut layers = Layers::read(&state)?;
    let layer = match args.layer {
        Some(layer) => layer,
        None => default_layer(&state, &layers, &paths[0])?,
    };
    // The layer, or this workspace's open proposal to change it, shown in
    // its place.
    let mine = |b: &str| {
        local
            .proposal(b)
            .is_some_and(|(t, by)| t == layer && by == name)
    };
    let Some(i) = layers
        .find(&layer)
        .or_else(|| layers.layers.iter().position(|l| mine(&l.branch)))
    else {
        return Err(Refusal::NotFound.error(format!(
            "{layer} isn't a layer of {name}: a proposal changes a layer shown here"
        )));
    };
    let shown = layers.layers[i].branch.clone();
    // Proposing again with the proposal shown: it is the one to edit.
    let (target, proposal) = match local.proposal(&shown) {
        Some((target, by)) if by == name => (target, shown.clone()),
        _ => (shown.clone(), format!("{shown}-prop/{name}")),
    };
    // Read before anything is made: a missing one is an error.
    let mut files = Vec::new();
    if let Some(dir) = &args.from {
        for path in &paths {
            let from = dir.join(path.to_str_lossy().as_ref());
            files.push((path.clone(), Some(read_file(&from)?)));
        }
    }
    let made = match local.proposal(&proposal) {
        Some((t, by)) if t == target && by == name && git::branch_exists(&proposal) => false,
        _ => {
            make(&store, &common_dir, &target, &proposal, &name)?;
            true
        }
    };
    write_files(&common_dir, &proposal, &files)?;
    let copied: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
    let mut out = String::new();
    if made {
        let copy = match StateDir::of(&common_dir, &target).opened() {
            true => format!("a copy of {target}'s work, uncommitted included"),
            false => format!("a copy of {target}'s commits"),
        };
        let _ = writeln!(
            out,
            "proposed a change to {target}: branch {proposal}, {copy}"
        );
    }
    if !copied.is_empty() {
        let _ = writeln!(out, "copied into it: {}", copied.join(", "));
    }
    // The proposal in the layer's place, shown as now.
    let swapped = shown != proposal;
    if swapped || !copied.is_empty() {
        let was = &layers.layers[i];
        let mut shown =
            super::layer::layer_of(&store, &common_dir, &state, &proposal, false, false)?;
        // The layer as it was, put back when the proposal goes.
        shown.instead = match swapped {
            true => Some(Box::new(was.clone())),
            false => was.instead.clone(),
        };
        layers.layers[i] = shown;
        super::layer::store_and_apply(&common_dir, &state, &layers)?;
    }
    if swapped {
        let _ = writeln!(out, "{name} shows {proposal} in place of {target} now");
    }
    let mount = mount_table::mount_of(&common_dir)?;
    let edit = match copied.is_empty() {
        true => "edit",
        false => "edit further",
    };
    out += &next_steps(mount.as_deref(), &name, &target, &proposal, &paths, edit);
    println!("{out}");
    Ok(())
}

/// What `gitfold propose` says to do next, by workspace `name` with
/// `proposal` to change `target`: `edit` the files at `paths` in the
/// proposal's workspace (under `mount`, if mounted), then hand it over.
fn next_steps(
    mount: Option<&Path>,
    name: &str,
    target: &str,
    proposal: &str,
    paths: &[BString],
    edit: &str,
) -> String {
    let root = mount.map_or_else(
        || format!("<mountpoint>/{proposal}"),
        |m| m.join(proposal).display().to_string(),
    );
    let mut out = format!("{edit} them there (a workspace of its own, where they are yours):\n");
    for path in paths {
        let _ = writeln!(out, "  {root}/{path}");
    }
    let _ = write!(
        out,
        "your next turn shows your edits here (or now: `gitfold layer add {proposal}`); when \
         it is ready, tell the agent of {target} (`gitfold send {target} <text>`): it checks \
         the proposal against every workspace that shows {target} (`gitfold consent \
         {proposal}`) and takes it (`gitfold accept {proposal}`), after which {name} shows \
         {target} again"
    );
    out
}

/// Makes proposal `proposal` by workspace `by` to change layer `layer`: a
/// fork of `layer`'s workspace, through the daemon if mounted, recorded as a
/// proposal with what it starts from.
fn make(store: &Store, common_dir: &Path, layer: &str, proposal: &str, by: &str) -> Result<()> {
    git::require_branch(layer)?;
    super::branch::check_new(common_dir, proposal)?;
    if git::branch_exists(proposal) {
        bail!("branch {proposal} exists already, and isn't {by}'s proposal to change {layer}");
    }
    let request = control::Request::Fork {
        from: layer.to_string(),
        to: proposal.to_string(),
    };
    super::local_or_daemon(common_dir, &request, "branch --with-changes", || {
        fork::fork(common_dir, layer, proposal, None, |dst| {
            fork::copy_stored(common_dir, layer, dst)
        })
    })?;
    settings::set_proposal(common_dir, proposal, layer, by)?;
    let start = work(store, common_dir, proposal)?;
    let name = start_ref(&state::workspace_id(proposal));
    git::cmd::run(common_dir, &["update-ref", &name, &start.to_string()])
}

/// `paths` as paths in the workspace in `state`: relative to the current
/// directory if it is in that workspace (mounted), else to its root.
fn workspace_paths(state: &StateDir, paths: &[PathBuf]) -> Result<Vec<BString>> {
    let cwd = std::env::current_dir()?;
    let inside = super::workspace_at(&cwd).is_some_and(|(s, _)| s.branch == state.branch);
    let mut out = Vec::new();
    for path in paths {
        let path = match inside || path.is_absolute() {
            true => {
                let full = crate::shell::normalized(&cwd.join(path));
                match super::workspace_at(&full) {
                    Some((s, rest)) if s.branch == state.branch => rest,
                    _ => bail!("{}: not in workspace {}", path.display(), state.branch),
                }
            }
            false => {
                let p = crate::shell::normalized(path);
                p.to_str().context("a path that isn't UTF-8")?.to_string()
            }
        };
        ensure!(
            !path.is_empty() && path != ".git" && !path.starts_with("../"),
            "{path:?}: not a file in the workspace"
        );
        out.push(path.into());
    }
    Ok(out)
}

/// The layer whose file shows at `path` in the workspace in `state`, or the
/// only layer.
fn default_layer(state: &StateDir, layers: &Layers, path: &BString) -> Result<String> {
    let applied = crate::state::layers::ShownLayers::read(state)?.layers()?;
    match super::layer::owners(state, &applied, path)?[..] {
        [i] => return Ok(applied.layers[i].branch.clone()),
        [] => {}
        _ => bail!("{path} shows the merge of several layers: name one with --layer"),
    }
    match &layers.layers[..] {
        [one] => Ok(one.branch.clone()),
        [] => Err(Refusal::NotFound.error(format!("{} shows no layers", state.branch))),
        _ => bail!("no layer shows {path} here: name the layer with --layer"),
    }
}

/// A file's content and kind (a blob, an executable or a symlink).
struct File {
    kind: EntryKind,
    data: Vec<u8>,
}

/// The file at `path` (not following a symlink).
fn read_file(path: &Path) -> Result<File> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("{}", path.display()))?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(File {
            kind: EntryKind::Link,
            data: target.as_os_str().as_bytes().to_vec(),
        });
    }
    ensure!(meta.is_file(), "{}: not a file", path.display());
    let kind = match meta.permissions().mode() & 0o111 != 0 {
        true => EntryKind::BlobExecutable,
        false => EntryKind::Blob,
    };
    Ok(File {
        kind,
        data: fs::read(path)?,
    })
}

/// Writes `files` (`None`: delete) into the workspace of `branch`: through
/// the mount, as any write, if mounted; else into its stored state, under
/// the repository lock. Every path is checked first, and deletions go
/// before writes (a file replaced by a directory, or the reverse), so a
/// conflict stops it before anything is written.
fn write_files(common_dir: &Path, branch: &str, files: &[(BString, Option<File>)]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let mut ordered: Vec<&(BString, Option<File>)> = files.iter().collect();
    ordered.sort_by_key(|(_, file)| file.is_some());
    let deleted: HashSet<&[u8]> = (files.iter())
        .filter(|(_, f)| f.is_none())
        .map(|(p, _)| p.as_slice())
        .collect();
    if let Some(_lock) = crate::lock::lock(common_dir)? {
        return write_stored(&StateDir::of(common_dir, branch), &ordered, &deleted);
    }
    let Some(mount) = mount_table::mount_of(common_dir)? else {
        return Err(Refusal::InUse.error(format!(
            "{} is locked by another gitfold command; retry",
            common_dir.display()
        )));
    };
    let root = mount.join(branch);
    let at = |path: &[u8]| root.join(std::ffi::OsStr::from_bytes(path));
    for (path, file) in &ordered {
        if file.is_none() {
            continue;
        }
        for a in state::ancestors(path) {
            let meta = fs::symlink_metadata(at(a)).ok();
            ensure!(
                meta.is_none_or(|m| m.is_dir()) || deleted.contains(a),
                "{path}: {} is a file in {branch}; nothing was written",
                a.as_bstr()
            );
        }
        if fs::symlink_metadata(at(path)).is_ok_and(|m| m.is_dir()) {
            for below in files_below(&at(path), path)? {
                ensure!(
                    deleted.contains(below.as_slice()),
                    "{path}: a directory in {branch} with other files; nothing was written"
                );
            }
        }
    }
    for (path, file) in ordered {
        let at = at(path);
        place(&at, file.as_ref()).with_context(|| format!("writing {}", at.display()))?;
    }
    Ok(())
}

/// The files below directory `dir`, at `path` in the workspace.
fn files_below(dir: &Path, path: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let below = state::join(path, entry.file_name().as_bytes());
        match entry.file_type()?.is_dir() {
            true => out.extend(files_below(&entry.path(), &below)?),
            false => out.push(below),
        }
    }
    Ok(out)
}

/// Puts `file` at `at` (`None`: removes what is there), replacing a file or
/// symlink, or an empty directory, there; removes directories a deletion
/// leaves empty.
fn place(at: &Path, file: Option<&File>) -> Result<()> {
    let existing = fs::symlink_metadata(at).ok();
    let Some(file) = file else {
        if existing.is_some() {
            fs::remove_file(at)?;
        }
        return Ok(());
    };
    if existing.as_ref().is_some_and(|m| m.is_dir()) {
        // Its files were deleted first (`write_files` checked).
        fs::remove_dir_all(at)?;
    } else if let Some(parent) = at.parent() {
        fs::create_dir_all(parent)?;
    }
    let replace = existing
        .as_ref()
        .is_some_and(|m| m.file_type().is_symlink() || file.kind == EntryKind::Link);
    if replace && !existing.as_ref().is_some_and(|m| m.is_dir()) {
        fs::remove_file(at)?;
    }
    match file.kind {
        EntryKind::Link => {
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&file.data), at)?;
        }
        EntryKind::Blob | EntryKind::BlobExecutable => {
            fs::write(at, &file.data)?;
            let mode = match file.kind == EntryKind::BlobExecutable {
                true => 0o755,
                false => 0o644,
            };
            fs::set_permissions(at, fs::Permissions::from_mode(mode))?;
        }
        _ => bail!("not a file or symlink"),
    }
    Ok(())
}

/// `write_files` for a closed workspace: its upper files and deletion
/// markers. One showing layers, or a path where its base has a directory or
/// below a deleted one, is left to the daemon (mount it); checked first.
fn write_stored(
    state: &StateDir,
    files: &[&(BString, Option<File>)],
    deleted: &HashSet<&[u8]>,
) -> Result<()> {
    let branch = &state.branch;
    if !state.opened() {
        return Err(Refusal::NotFound.error(format!(
            "{branch} has no workspace yet: mount the repository to write into it"
        )));
    }
    ensure!(
        !Layers::read(state)?.is_on(),
        "{branch} shows layers: mount the repository to write into it"
    );
    let base = state.read_base()?;
    let mut markers = Markers::open(&state.markers())?;
    let lookup =
        |path: &[u8]| git::with_repo(|repo| git::layers::lookup(repo, base.tree, path.into()));
    let mount = |path: &BString, why: &str| {
        anyhow::anyhow!(
            "{path}: {why} in {branch}; mount the repository to write it (nothing was written)"
        )
    };
    for (path, file) in files {
        if state::ancestors(path).any(|a| markers.whiteouts.contains(a)) {
            return Err(mount(path, "below a deleted directory"));
        }
        if file.is_some() {
            if lookup(path)?.is_some_and(|i| i.kind == EntryKind::Tree) {
                return Err(mount(path, "a directory of its base"));
            }
            for a in state::ancestors(path) {
                let upper = fs::symlink_metadata(state::upper_path(&state.upper(), a));
                let file = match upper {
                    Ok(m) => !m.is_dir(),
                    Err(_) => lookup(a)?.is_some_and(|i| i.kind != EntryKind::Tree),
                };
                if file && !deleted.contains(a) {
                    return Err(mount(path, "below a file"));
                }
            }
            let at = state::upper_path(&state.upper(), path);
            if fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir())
                && !(files_below(&at, path)?.iter()).all(|b| deleted.contains(b.as_slice()))
            {
                return Err(mount(path, "a directory with other files"));
            }
        }
    }
    let written = (|| -> Result<()> {
        for (path, file) in files {
            let at = state::upper_path(&state.upper(), path);
            place(&at, file.as_ref()).with_context(|| format!("writing {path} into {branch}"))?;
            // Deleted over the base's file: a whiteout hides it.
            let hide = file.is_none() && lookup(path)?.is_some();
            markers.set(Kind::Whiteout, path, hide)?;
        }
        Ok(())
    })();
    // What was written is described, whatever stopped it.
    markers.sync()?;
    written
}

/// `gitfold proposals`: the open proposals (to change `layer`, if given):
/// who proposed, what they change, and the last consent check. One that
/// can't be read is said so, the others still listed.
pub fn proposals(repo: &Path, args: ProposalsArgs) -> Result<()> {
    let common_dir = git::init(repo)?;
    let local = Local::read(&common_dir);
    let store = Store::default();
    let mut out = String::new();
    let mut layers = BTreeMap::new();
    for (name, layer) in local.proposals() {
        if args.layer.as_ref().is_some_and(|l| *l != layer) || !git::branch_exists(&name) {
            continue;
        }
        let listed = (|| -> Result<String> {
            let p = Proposal::load(&store, &common_dir, &local, &name)?;
            let now = match layers.get(&layer) {
                Some(&now) => now,
                None => {
                    let now = work(&store, &common_dir, &layer)?;
                    layers.insert(layer.clone(), now);
                    now
                }
            };
            let mut out = format!("{name}: proposed by {}, to change {layer}\n", p.by);
            let (start, work) = (p.start.id.to_string(), p.work.id.to_string());
            let stat = git::cmd::output(&common_dir, &["diff", "--stat", &start, &work])?;
            let stat = String::from_utf8_lossy(&stat);
            match stat.trim().is_empty() {
                true => out += "   no changes yet\n",
                false => {
                    for line in stat.lines() {
                        let _ = writeln!(out, "  {line}");
                    }
                }
            }
            let _ = writeln!(
                out,
                "  consent: {}",
                says(Consent::of(&local, &p).as_ref(), &p, now)
            );
            Ok(out)
        })();
        match listed {
            Ok(listed) => out += &listed,
            Err(e) => {
                let _ = writeln!(out, "{name}: to change {layer}, can't be read: {e:#}");
            }
        }
    }
    print!("{out}");
    Ok(())
}

/// The last consent check of a proposal, as recorded (see
/// `settings::CONSENT`).
struct Consent {
    result: String,
    /// The proposal's work checked.
    work: Option<ObjectId>,
    /// The layer's work it was checked on.
    layer: Option<ObjectId>,
    summary: String,
}

impl Consent {
    fn of(local: &Local, p: &Proposal) -> Option<Consent> {
        let record = local.consent(&p.name)?;
        let mut parts = record.splitn(4, ' ');
        let mut next = || parts.next().unwrap_or_default().to_string();
        let oid = |s: String| ObjectId::from_hex(s.as_bytes()).ok();
        Some(Consent {
            result: next(),
            work: oid(next()),
            layer: oid(next()),
            summary: next(),
        })
    }

    /// Whether it checked `p` as it is now on the layer's work `now`.
    fn current(&self, p: &Proposal, now: ObjectId) -> bool {
        self.work == Some(p.work.id) && self.layer == Some(now)
    }

    fn passed(&self, p: &Proposal, now: ObjectId) -> bool {
        self.result == "passed" && self.current(p, now)
    }
}

/// What the user is told of `consent` (the last check of `p`, if any) with
/// the layer's work `now`.
fn says(consent: Option<&Consent>, p: &Proposal, now: ObjectId) -> String {
    let Some(c) = consent else {
        return "not checked (`gitfold consent`)".to_string();
    };
    let (result, summary) = (&c.result, &c.summary);
    match (c.work == Some(p.work.id), c.layer == Some(now)) {
        (true, true) => format!("{result}: {summary}"),
        (false, _) => format!("{result}, but the proposal changed since: check again"),
        (true, false) => format!("{result}, but {} changed since: check again", p.layer),
    }
}

/// What a workspace's read log says about a proposal's paths.
#[derive(Clone, Copy, PartialEq)]
enum Read {
    Yes,
    No,
    /// Its reads aren't recorded (`gitfold track-reads`).
    Unknown,
}

impl Read {
    fn of(state: &StateDir, paths: &[BString]) -> Result<Read> {
        Ok(match state::reads::recorded(state)? {
            None => Read::Unknown,
            Some(read) if paths.iter().any(|p| read.contains(p.as_slice())) => Read::Yes,
            Some(_) => Read::No,
        })
    }

    fn word(self) -> &'static str {
        match self {
            Read::Yes => "yes",
            Read::No => "no",
            Read::Unknown => "unknown",
        }
    }
}

/// The workspaces showing `p`'s layer, `p`, or another proposal to change
/// that layer, but the layer's own and `p`'s.
fn followers(common_dir: &Path, local: &Local, p: &Proposal) -> Result<Vec<StateDir>> {
    let mut out = Vec::new();
    for s in state::list(common_dir)? {
        if s.branch == p.layer || s.branch == p.name {
            continue;
        }
        let layers = Layers::read(&s.state).unwrap_or_default();
        if layers.layers.iter().any(|l| of_layer(local, p, &l.branch)) {
            out.push(s.state);
        }
    }
    Ok(out)
}

/// Whether a layer `branch` stands for `p`'s layer: it, or a proposal to
/// change it.
fn of_layer(local: &Local, p: &Proposal, branch: &str) -> bool {
    branch == p.layer || local.proposal(branch).is_some_and(|(l, _)| l == p.layer)
}

/// What the workspace in `state` (showing `layers`) would show with `accepted`
/// (`p` accepted) in place of `p`'s layer and its proposals: its view tree
/// with its own work on top, as a commit on its base. Union files show the
/// workspace's own version, if it has one.
fn view_with(
    store: &Store,
    local: &Local,
    p: &Proposal,
    state: &StateDir,
    mut layers: Layers,
    accepted: ObjectId,
) -> Result<ObjectId> {
    let common_dir = &state.common_dir;
    let base = state.read_base()?;
    let mut replaced = false;
    let mut kept = Vec::new();
    for l in layers.layers {
        if !of_layer(local, p, &l.branch) {
            kept.push(l);
        } else if !replaced {
            replaced = true;
            kept.push(Layer {
                branch: p.layer.clone(),
                snapshot: accepted,
                ..l
            });
        }
    }
    layers.layers = kept;
    let view = match state::layers::view(common_dir, &base.id, &layers)? {
        Some(view) => view.tree,
        None => base.tree,
    };
    let own = snapshot::snapshot(store, state, snapshot::By::Other)?.commit;
    let own = git::resolve(&own.to_string())?;
    let tree = git::with_repo(|repo| {
        let changes = git::layers::delta(repo, base.tree, own.tree)?;
        git::layers::edit_tree(repo, view, changes.iter().map(|(p, c)| (p.as_ref(), *c)))
    })?;
    let message = format!("gitfold consent: {} with {}", state.branch, p.name);
    commit(common_dir, &tree, &[base.id], &message)
}

/// `gitfold consent`: checks what accepting the proposal gives (the layer's
/// work as now with the proposal's changes) with every workspace that shows
/// the layer (or a proposal to change it): that its own work merges with it,
/// and that the test passes in its view with it. Returns the exit code: 0
/// all passed, 4 any failed or none was checked.
pub fn consent(repo: &Path, args: ConsentArgs) -> Result<i32> {
    let common_dir = git::init(repo)?;
    let local = Local::read(&common_dir);
    let store = Store::default();
    let p = Proposal::load(&store, &common_dir, &local, &args.proposal)?;
    let changes = p.changes()?;
    let paths: Vec<BString> = changes.iter().map(|(path, _)| path.clone()).collect();
    let mount = mount_table::mount_of(&common_dir)?;
    if args.test.is_some() && mount.is_none() {
        bail!("--test runs in each workspace's view, a snapshot: mount the repository");
    }
    let now = git::resolve(&work(&store, &common_dir, &p.layer)?.to_string())?;
    let record = |result: &str, summary: &str| {
        let record = format!("{result} {} {} {summary}", p.work.id, now.id);
        settings::set_consent(&common_dir, &p.name, &record)
    };
    let moved = p.moved(&now, &paths)?;
    if !moved.is_empty() {
        record("failed", &format!("{} changed since", moved.join(", ")))?;
        return Err(p.moved_error(&moved));
    }
    let accepted = p.accepted(&common_dir, &now, &changes)?;
    let mut table = String::from("workspace\tread\tmerges\ttest\n");
    let (mut checked, mut failing) = (0, Vec::new());
    for state in followers(&common_dir, &local, &p)? {
        let branch = &state.branch;
        let read = Read::of(&state, &paths)?;
        if read == Read::No && args.only_readers {
            let _ = writeln!(table, "{branch}\tno\t-\t-");
            continue;
        }
        checked += 1;
        let ours = snapshot::snapshot(&store, &state, snapshot::By::Other)?.commit;
        match super::snapshot::merge(&common_dir, &ours, &accepted, &p.name) {
            Ok(_) => {}
            Err(e) if Refusal::of(&e) == Some(Refusal::Changes) => {
                let conflicts: Vec<String> = format!("{e:#}")
                    .lines()
                    .skip(1)
                    .map(|l| l.trim().to_string())
                    .collect();
                let _ = writeln!(
                    table,
                    "{branch}\t{}\tno\t-\tconflicts: {}",
                    read.word(),
                    conflicts.join(", ")
                );
                failing.push(branch.clone());
                continue;
            }
            Err(e) => return Err(e),
        }
        let (Some(test), Some(mount)) = (&args.test, &mount) else {
            let _ = writeln!(table, "{branch}\t{}\tyes\t-", read.word());
            continue;
        };
        let layers = Layers::read(&state).unwrap_or_default();
        let view = view_with(&store, &local, &p, &state, layers, accepted)?;
        let at = mount.join(format!("@{view}"));
        let ran = run_test(test, &at, branch, Duration::from_secs(args.timeout));
        // The snapshot's registration goes (it is made again if opened).
        let _ = super::remove::remove_once(&common_dir, &format!("@{view}"), false, false);
        match ran? {
            Ran::Passed => {
                let _ = writeln!(table, "{branch}\t{}\tyes\tpassed", read.word());
            }
            Ran::Failed(why) => {
                failing.push(branch.clone());
                let _ = writeln!(
                    table,
                    "{branch}\t{}\tyes\tfailed\t{why} ({})",
                    read.word(),
                    at.display()
                );
            }
        }
    }
    print!("{table}");
    let (result, summary) = match (checked, failing.is_empty()) {
        (0, _) => (
            "unchecked",
            "no workspace was checked (none shows the layer, or none read the files)".to_string(),
        ),
        (n, true) => ("passed", format!("{n} workspaces checked")),
        (n, false) => (
            "failed",
            format!("{} of {n} failing: {}", failing.len(), failing.join(", ")),
        ),
    };
    println!("{}: {result} ({summary})", p.name);
    record(result, &summary)?;
    Ok(if result == "passed" { 0 } else { 4 })
}

/// How a test ran.
enum Ran {
    Passed,
    /// Why: the exit code (or a signal, or the timeout) and its last line.
    Failed(String),
}

/// The bytes of the test's output kept, from its end.
const OUTPUT_KEPT: u64 = 4096;

/// Runs `test` (`sh -c`) in `at` for workspace `branch`, its output in a
/// file (only its end is read), with a writable scratch directory, killed
/// (its process group) after `timeout`.
fn run_test(test: &str, at: &Path, branch: &str, timeout: Duration) -> Result<Ran> {
    let scratch = std::env::temp_dir().join(format!(
        "gitfold-consent-{}-{}",
        std::process::id(),
        state::workspace_id(branch)
    ));
    let log = scratch.with_extension("log");
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)?;
    let ran = (|| -> Result<Ran> {
        let output = fs::File::create(&log)?;
        let mut child = std::process::Command::new("sh")
            .args(["-c", test])
            .current_dir(at)
            .env("GITFOLD_WORKSPACE", branch)
            .env("GITFOLD_SCRATCH", &scratch)
            .stdin(std::process::Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .process_group(0)
            .spawn()
            .with_context(|| format!("running the test in {}", at.display()))?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if started.elapsed() > timeout {
                // SAFETY: a plain syscall; the group is the test's own.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut file = fs::File::open(&log)?;
        let len = file.metadata()?.len();
        file.seek(std::io::SeekFrom::Start(len.saturating_sub(OUTPUT_KEPT)))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        let tail = String::from_utf8_lossy(&tail);
        let last = tail.lines().rev().find(|l| !l.trim().is_empty());
        let last: String = last.unwrap_or_default().trim().chars().take(100).collect();
        let last = match last.is_empty() {
            true => last,
            false => format!(": {last}"),
        };
        Ok(match status {
            Some(s) if s.success() => Ran::Passed,
            Some(s) => {
                let code = (s.code()).map_or_else(|| "a signal".to_string(), |c| c.to_string());
                Ran::Failed(format!("exit {code}{last}"))
            }
            None => Ran::Failed(format!("timed out after {} s{last}", timeout.as_secs())),
        })
    })();
    let _ = fs::remove_dir_all(&scratch);
    let _ = fs::remove_file(&log);
    ran
}

/// `gitfold accept`: writes the proposal's changed files into its layer's
/// workspace, shows the layer again where the proposal showed, tells every
/// workspace that may have read them, and removes the proposal. Refused if
/// the layer changed those files since the proposal was made, or (unless
/// `--force`) without a passed consent check of the proposal and the layer
/// as they are now.
pub fn accept(repo: &Path, args: AcceptArgs) -> Result<()> {
    let common_dir = git::init(repo)?;
    let local = Local::read(&common_dir);
    let store = Store::default();
    let p = Proposal::load(&store, &common_dir, &local, &args.proposal)?;
    let here = caller(repo, args.workspace, "accept")?;
    if here != p.layer {
        return Err(Refusal::NotFound.error(format!(
            "{} proposes a change to {}, not to {here}: only {}'s workspace accepts it",
            p.name, p.layer, p.layer
        )));
    }
    let changes = p.changes()?;
    let paths: Vec<BString> = changes.iter().map(|(path, _)| path.clone()).collect();
    let now = git::resolve(&work(&store, &common_dir, &p.layer)?.to_string())?;
    let moved = p.moved(&now, &paths)?;
    if !moved.is_empty() {
        return Err(p.moved_error(&moved));
    }
    let consent = Consent::of(&local, &p);
    if !args.force && !consent.as_ref().is_some_and(|c| c.passed(&p, now.id)) {
        return Err(Refusal::Changes.error(format!(
            "consent: {}. Run `gitfold consent {}` (or accept --force)",
            says(consent.as_ref(), &p, now.id),
            p.name
        )));
    }
    let mut files = Vec::new();
    for (path, change) in &changes {
        let file = match change {
            Change::Delete => None,
            Change::Set(Item { kind, oid }) => {
                ensure!(
                    matches!(
                        kind,
                        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
                    ),
                    "{path}: a submodule can't be proposed"
                );
                Some(File {
                    kind: *kind,
                    data: git::read_blob(*oid)?,
                })
            }
        };
        files.push((path.clone(), file));
    }
    write_files(&common_dir, &p.layer, &files)?;
    let layer_state = StateDir::of(&common_dir, &p.layer);
    if layer_state.opened() {
        // Followers move to it now and at their next turn, even those of a
        // workspace whose agent's turn is under way.
        snapshot::snapshot(&store, &layer_state, snapshot::By::Owner)?;
    }
    let mut out = String::new();
    let shown: Vec<&str> = paths.iter().map(|p| p.to_str().unwrap_or("?")).collect();
    let _ = writeln!(
        out,
        "accepted {} into {}: {}",
        p.name,
        p.layer,
        match shown.is_empty() {
            true => "no changes".to_string(),
            false => shown.join(", "),
        }
    );
    let _ = writeln!(out, "consent: {}", says(consent.as_ref(), &p, now.id));
    let diff = short_diff(&common_dir, &p, &changes)?;
    let mut told = Vec::new();
    for state in followers(&common_dir, &local, &p)? {
        let branch = state.branch.clone();
        out += &show_layer_again(&store, &p, &state, true)?;
        if !paths.is_empty() && Read::of(&state, &paths)? != Read::No {
            let layers = Layers::read(&state)?;
            let body = format!(
                "contract {} changed: {}'s proposal {} was accepted into {}; {}.\n{diff}",
                shown.join(", "),
                p.by,
                p.name,
                p.layer,
                super::layer::when_shown(
                    layers.find(&p.layer).map(|i| &layers.layers[i]),
                    &p.layer
                ),
            );
            super::inbox::deliver(
                &common_dir,
                &branch,
                &p.layer,
                Some("contract changed"),
                &body,
            )?;
            told.push(branch);
        }
    }
    if !told.is_empty() {
        let _ = writeln!(
            out,
            "told {} (they read the files, or their reads aren't recorded)",
            told.join(", ")
        );
    }
    // Edits made to the proposal while it was accepted stay in it, as a
    // proposal of their own from what was accepted.
    if work(&store, &common_dir, &p.name)? != p.work.id {
        let r = start_ref(&state::workspace_id(&p.name));
        git::cmd::run(&common_dir, &["update-ref", &r, &p.work.id.to_string()])?;
        let _ = writeln!(
            out,
            "{} changed while it was accepted: kept, with only those later changes (`gitfold \
             proposals`)",
            p.name
        );
    } else {
        out += &retire(&common_dir, &p.name);
    }
    println!("{}", out.trim_end());
    Ok(())
}

/// `gitfold reject`: removes a proposal without taking it (by its layer's
/// workspace, or withdrawn by its proposer's), showing the layer again where
/// it showed, and tells the proposer why.
pub fn reject(repo: &Path, args: RejectArgs) -> Result<()> {
    let common_dir = git::init(repo)?;
    let local = Local::read(&common_dir);
    let store = Store::default();
    let p = Proposal::load(&store, &common_dir, &local, &args.proposal)?;
    let here = caller(repo, args.workspace, "reject")?;
    if here != p.layer && here != p.by {
        return Err(Refusal::NotFound.error(format!(
            "{} is {}'s proposal to change {}: only those two workspaces reject it",
            p.name, p.by, p.layer
        )));
    }
    let mut out = format!("rejected {} (to change {})\n", p.name, p.layer);
    for state in followers(&common_dir, &local, &p)? {
        out += &show_layer_again(&store, &p, &state, false)?;
    }
    if here != p.by && git::branch_exists(&p.by) {
        let why = args.reason.as_deref().unwrap_or("no reason given");
        let body = format!(
            "your proposal {} to change {} was rejected: {why}\nIt is removed; your workspace \
             shows {} again.",
            p.name, p.layer, p.layer
        );
        super::inbox::deliver(&common_dir, &p.by, &here, Some("proposal rejected"), &body)?;
        let _ = writeln!(out, "told {}", p.by);
    }
    out += &retire(&common_dir, &p.name);
    println!("{}", out.trim_end());
    Ok(())
}

/// Shows `p`'s layer again in the workspace in `state` if it shows `p`, as
/// it was shown before (live or frozen, all work or commits only); says so.
/// A live one shows the layer's work as now; a frozen one is put back as it
/// was if `p` was rejected, and taken again (with the change) if `accepted`.
fn show_layer_again(
    store: &Store,
    p: &Proposal,
    state: &StateDir,
    accepted: bool,
) -> Result<String> {
    let _lock = state::layers::lock(state)?;
    let mut layers = Layers::read(state)?;
    let Some(i) = layers.find(&p.name) else {
        return Ok(String::new());
    };
    let common_dir = &state.common_dir;
    let again = |frozen: bool, commits: bool| {
        super::layer::layer_of(store, common_dir, state, &p.layer, frozen, commits)
    };
    match layers.find(&p.layer) {
        Some(_) => drop(layers.layers.remove(i)),
        None => {
            layers.layers[i] = match layers.layers[i].instead.take().map(|was| *was) {
                None => again(false, false)?,
                Some(was) if was.updates == Updates::Live => {
                    again(false, was.shows == Shows::CommitsOnly)?
                }
                Some(was) if accepted => {
                    again(true, was.shows == Shows::CommitsOnly).unwrap_or(was)
                }
                Some(was) => was,
            };
        }
    }
    // Stored even if the switch is refused now: the workspace switches to it
    // when it can.
    layers.write(state)?;
    let branch = &state.branch;
    Ok(match super::layer::apply(common_dir, branch) {
        Ok(_) => format!("{branch} shows {} again\n", p.layer),
        Err(e) => format!(
            "{branch} shows {} again once it can switch ({e:#})\n",
            p.layer
        ),
    })
}

/// The proposal's changes as changed lines, a few, each file named as it is
/// (not as git quotes it), said to be the proposer's.
fn short_diff(common_dir: &Path, p: &Proposal, changes: &[(BString, Change)]) -> Result<String> {
    const LINES: usize = 20;
    let (start, work) = (p.start.id.to_string(), p.work.id.to_string());
    let mut lines = vec![format!("the change, as {} proposed it:", p.by)];
    for (path, change) in changes {
        let was = git::with_repo(|repo| git::layers::lookup(repo, p.start.tree, path.as_ref()))?;
        let what = match (change, was) {
            (Change::Delete, _) => " (deleted)",
            (_, None) => " (new)",
            _ => "",
        };
        lines.push(format!("{path}{what}:"));
        let spec = format!(":(literal){path}");
        let args = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-U0",
            &start,
            &work,
            "--",
            &spec,
        ];
        let out = git::cmd::output(common_dir, &args)?;
        lines.extend(changed_lines(out.as_bstr()));
    }
    let more = lines.len().saturating_sub(LINES);
    lines.truncate(LINES);
    if more > 0 {
        lines.push(format!("  ... ({more} more lines)"));
    }
    Ok(lines.join("\n"))
}

/// The added and removed lines of a one-file `git diff -U0`: those in its
/// hunks (after a `@@` line), whatever they start with.
fn changed_lines(diff: &BStr) -> Vec<String> {
    let mut in_hunk = false;
    let mut out = Vec::new();
    for line in diff.lines() {
        if line.starts_with(b"@@") {
            in_hunk = true;
        } else if line.starts_with(b"diff --git ") {
            in_hunk = false;
        } else if in_hunk && (line.starts_with(b"+") || line.starts_with(b"-")) {
            out.push(format!("  {}", line.to_str_lossy()));
        } else if line.starts_with(b"Binary files") {
            out.push("  (binary)".to_string());
        }
    }
    out
}

/// Removes proposal `name`: its workspace (its changes are accepted), its
/// branch, its start and its snapshots no layer names; then sweeps what earlier
/// removals had to leave (snapshots a pending switch still named, starts of
/// proposals gone). Says what was done; what failed is said, not an error:
/// the proposal is accepted already. A workspace in use stays, as a branch
/// that is no longer a proposal, to remove by hand.
fn retire(common_dir: &Path, name: &str) -> String {
    let id = state::workspace_id(name);
    let removed = match super::remove::remove_once(common_dir, name, true, false) {
        Ok(_) => Ok(()),
        Err(e) if Refusal::of(&e) == Some(Refusal::NotFound) => Ok(()),
        Err(e) => Err(e),
    };
    if let Err(e) = removed {
        let forgot = settings::clear_proposal(common_dir, name)
            .and_then(|()| git::cmd::run(common_dir, &["update-ref", "-d", &start_ref(&id)]));
        let forgot = match forgot {
            Ok(()) => "it is no longer a proposal".to_string(),
            Err(e) => format!("forgetting it as a proposal: {e:#}"),
        };
        return format!(
            "{name}: its workspace wasn't removed ({e:#}); {forgot}. Later: `gitfold remove \
             --force {name}`, then `git branch -D {name}`\n"
        );
    }
    let done = git::cmd::run(common_dir, &["branch", "-q", "-D", name])
        .and_then(|()| git::cmd::run(common_dir, &["update-ref", "-d", &start_ref(&id)]))
        .and_then(|()| sweep(common_dir));
    match done {
        Ok(()) => format!("removed {name} (its workspace and branch)\n"),
        Err(e) => format!("removing branch {name}: {e:#}\n"),
    }
}

/// Deletes the snapshot refs of proposals that are gone (no branch, no
/// workspace) which no layers, stored or applied, name, and the starts of
/// proposals whose branch is gone.
fn sweep(common_dir: &Path) -> Result<()> {
    let local = Local::read(common_dir);
    let out = git::cmd::output(
        common_dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/gitfold-snapshot/",
            START_REFS,
        ],
    )?;
    let named = crate::state::layers::named_snapshots(common_dir);
    let gone = |id: &str| {
        let branch = state::workspace_branch(id);
        !git::branch_exists(&branch)
            && !StateDir::of(common_dir, &branch).dir.exists()
            && (branch.contains("-prop/") || local.proposal(&branch).is_some())
    };
    for line in String::from_utf8_lossy(&out).lines() {
        let Some((name, oid)) = line.split_once(' ') else {
            continue;
        };
        let stale = if let Some(id) = name.strip_prefix(START_REFS) {
            !git::branch_exists(&state::workspace_branch(id))
        } else {
            let rest = name
                .strip_prefix("refs/gitfold-snapshot/")
                .unwrap_or_default();
            let id = rest.rsplit_once('/').map_or("", |(id, _)| id);
            let oid = ObjectId::from_hex(oid.as_bytes()).ok();
            gone(id) && oid.is_some_and(|o| !named.contains(&o))
        };
        if stale {
            git::cmd::run(common_dir, &["update-ref", "-d", name])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_lines_are_read_by_hunk() {
        let diff = b"diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n\
            ---old\n+++new\n@@ -5,0 +6 @@\n+-- comment\n";
        assert_eq!(
            changed_lines(diff.as_bstr()),
            ["  ---old", "  +++new", "  +-- comment"]
        );
    }
}
