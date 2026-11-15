//! Proposals: changing a layer's files (a contract) from a workspace that
//! shows it read-only. `gitfold propose` forks the layer's workspace into a
//! proposal branch (`branch --with-changes`), where the files are the
//! proposer's to edit, and shows the proposal in place of the layer;
//! `gitfold consent` checks the proposal against every workspace that read
//! what it changes (each one's work mixed with it, `gitfold mix`, and a test
//! run in the mix); `gitfold accept` writes the proposal's files into the
//! layer's workspace, switches the proposer back to the layer, tells the
//! readers, and removes the proposal; `gitfold reject` removes it untaken.
//! `gitfold proposals` lists them.
//!
//! A proposal is a branch with `branch.<name>.gitfoldProposal` (the layer)
//! and `gitfoldProposedBy` (see `settings`); what it starts from (the
//! layer's work when it was made) is kept by `refs/gitfold-proposal/<id>`.
//! Its changes are its work (a seal) against that.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::daemon::control;
use crate::git;
use crate::git::lens::{Change, Item};
use crate::mount_table;
use crate::refusal::Refusal;
use crate::settings::{self, Local};
use crate::state::lens::Lens;
use crate::state::markers::{Kind, Markers};
use crate::state::retire::check_name;
use crate::state::{self, StateDir, fork, seal};
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
    /// A command run (`sh -c`) in each mix's snapshot; it must exit 0. It
    /// gets the workspace's branch as GITFOLD_WORKSPACE.
    #[arg(long, value_name = "CMD")]
    pub test: Option<String>,
}

#[derive(Args)]
pub struct AcceptArgs {
    /// The proposal's branch.
    pub proposal: String,
    /// The layer's workspace (default: the one you are in).
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
    /// The layer's or the proposer's workspace (default: the one you are in).
    #[arg(short = 'w', long, value_name = "BRANCH")]
    pub workspace: Option<String>,
}

/// The ref keeping what proposal `id` (a workspace id) started from.
fn start_ref(id: &str) -> String {
    format!("refs/gitfold-proposal/{id}")
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
        git::with_repo(|repo| git::lens::delta(repo, self.start.tree, self.work.tree))
    }
}

/// The work of `branch`: its workspace's seal, or its tip if it has none.
fn work(store: &Store, common_dir: &Path, branch: &str) -> Result<ObjectId> {
    let state = StateDir::of(common_dir, branch);
    Ok(match state.opened() {
        true => seal::seal(store, &state)?.commit,
        false => git::branch_tip(branch)?.id,
    })
}

/// `gitfold propose`.
pub fn propose(repo: &Path, args: ProposeArgs) -> Result<()> {
    let name = super::branch_or_current(args.workspace, repo)?;
    check_name(&name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, &name);
    if !state.opened() {
        git::require_branch(&name)?;
        return Err(Refusal::NotFound.error(format!("{name} has no workspace yet")));
    }
    let paths = workspace_paths(&state, &args.paths)?;
    let store = Store::default();
    let local = Local::read(&common_dir);
    let mut lens = Lens::read(&state)?;
    let layer = match args.layer {
        Some(layer) => layer,
        None => default_layer(&state, &lens, &paths[0])?,
    };
    let Some(i) = lens.find(&layer) else {
        return Err(Refusal::NotFound.error(format!(
            "{layer} isn't a layer of {name}: a proposal changes a layer shown here"
        )));
    };
    // Proposing again with the proposal shown: it is the one to edit.
    let (target, proposal) = match local.proposal(&layer) {
        Some((target, by)) if by == name => (target, layer.clone()),
        _ => (layer.clone(), format!("{layer}-prop/{name}")),
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
        let _ = writeln!(
            out,
            "proposed a change to {target}: branch {proposal}, a copy of workspace {target} \
             (its uncommitted work too)"
        );
    }
    if !copied.is_empty() {
        let _ = writeln!(out, "copied into it: {}", copied.join(", "));
    }
    // The proposal in the layer's place, shown as now.
    let swapped = lens.layers[i].branch != proposal;
    if swapped || !copied.is_empty() {
        lens.layers[i] = super::layer::layer_of(&store, &common_dir, &state, &proposal)?;
        super::layer::store_and_apply(&common_dir, &state, &lens)?;
    }
    if swapped {
        let _ = writeln!(out, "{name} shows {proposal} in place of {target} now");
    }
    let root = mount_table::mount_of(&common_dir)?.map_or_else(
        || format!("<mountpoint>/{proposal}"),
        |m| m.join(&proposal).display().to_string(),
    );
    let edit = match copied.is_empty() {
        true => "edit",
        false => "edit further",
    };
    let _ = writeln!(
        out,
        "{edit} them there (a workspace of its own, where they are yours):"
    );
    for path in &paths {
        let _ = writeln!(out, "  {root}/{path}");
    }
    let _ = write!(
        out,
        "then: `gitfold layer update` shows your edits here (your next turn does too); when \
         it is ready, tell the agent of {target} (`gitfold send {target} <text>`): it checks \
         the proposal against \
         every workspace that read the files (`gitfold consent {proposal}`) and takes it \
         (`gitfold accept {proposal}`), after which {name} shows {target} again"
    );
    println!("{out}");
    Ok(())
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
fn default_layer(state: &StateDir, lens: &Lens, path: &BString) -> Result<String> {
    let applied = crate::state::lens::Applied::read(state)?.layers()?;
    if let Some(i) = super::layer::owner(state, &applied, path)? {
        return Ok(applied.layers[i].branch.clone());
    }
    match &lens.layers[..] {
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
/// the repository lock.
fn write_files(common_dir: &Path, branch: &str, files: &[(BString, Option<File>)]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    if let Some(_lock) = crate::lock::lock(common_dir)? {
        return write_stored(&StateDir::of(common_dir, branch), files);
    }
    let Some(mount) = mount_table::mount_of(common_dir)? else {
        return Err(Refusal::InUse.error(format!(
            "{} is locked by another gitfold command; retry",
            common_dir.display()
        )));
    };
    let root = mount.join(branch);
    for (path, file) in files {
        let at = root.join(path.to_str_lossy().as_ref());
        place(&at, file.as_ref()).with_context(|| format!("writing {}", at.display()))?;
    }
    Ok(())
}

/// Puts `file` at `at` (`None`: removes what is there), replacing a file or
/// symlink there.
fn place(at: &Path, file: Option<&File>) -> Result<()> {
    let existing = fs::symlink_metadata(at).ok();
    if existing.as_ref().is_some_and(|m| m.is_dir()) {
        bail!("a directory is there");
    }
    let Some(file) = file else {
        return match existing {
            Some(_) => Ok(fs::remove_file(at)?),
            None => Ok(()),
        };
    };
    if let Some(parent) = at.parent() {
        fs::create_dir_all(parent)?;
    }
    let replace = existing
        .as_ref()
        .is_some_and(|m| m.file_type().is_symlink() || file.kind == EntryKind::Link);
    if replace {
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
/// markers. One showing layers, or with a path below a deleted directory,
/// is left to the daemon (mount it).
fn write_stored(state: &StateDir, files: &[(BString, Option<File>)]) -> Result<()> {
    let branch = &state.branch;
    if !state.opened() {
        return Err(Refusal::NotFound.error(format!(
            "{branch} has no workspace yet: mount the repository to write into it"
        )));
    }
    ensure!(
        !Lens::read(state)?.is_on(),
        "{branch} shows layers: mount the repository to write into it"
    );
    let base = state.read_base()?;
    let mut markers = Markers::open(&state.markers())?;
    for (path, file) in files {
        if state::ancestors(path).any(|a| markers.whiteouts.contains(a)) {
            bail!("{path}: below a directory {branch} deleted; mount the repository to write it");
        }
        let at = state::upper_path(&state.upper(), path);
        place(&at, file.as_ref()).with_context(|| format!("writing {path} into {branch}"))?;
        let in_base = git::with_repo(|repo| git::lens::lookup(repo, base.tree, path.as_ref()))?;
        // Deleted over the base's file: a whiteout hides it.
        markers.set(Kind::Whiteout, path, file.is_none() && in_base.is_some())?;
    }
    markers.sync()?;
    Ok(())
}

/// `gitfold proposals`: the open proposals (to change `layer`, if given):
/// who proposed, what they change, and the last consent check.
pub fn proposals(repo: &Path, args: ProposalsArgs) -> Result<()> {
    let common_dir = git::init(repo)?;
    let local = Local::read(&common_dir);
    let store = Store::default();
    let mut out = String::new();
    for (name, layer) in local.proposals() {
        if args.layer.as_ref().is_some_and(|l| *l != layer) || !git::branch_exists(&name) {
            continue;
        }
        let p = Proposal::load(&store, &common_dir, &local, &name)?;
        let _ = writeln!(out, "{name}: proposed by {}, to change {layer}", p.by);
        let stat = git::cmd::output(
            &common_dir,
            &[
                "diff",
                "--stat",
                &p.start.id.to_string(),
                &p.work.id.to_string(),
            ],
        )?;
        let stat = String::from_utf8_lossy(&stat);
        match stat.trim().is_empty() {
            true => out += "   no changes yet\n",
            false => {
                for line in stat.lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }
        let _ = writeln!(out, "  consent: {}", consent_of(&local, &p));
    }
    print!("{out}");
    Ok(())
}

/// The last consent check of `p`, as said to the user.
fn consent_of(local: &Local, p: &Proposal) -> String {
    let Some(c) = local.consent(&p.name) else {
        return "not checked (`gitfold consent`)".to_string();
    };
    let mut parts = c.splitn(3, ' ');
    let (result, work, summary) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    match ObjectId::from_hex(work.as_bytes()).ok() == Some(p.work.id) {
        true => format!("{result}: {summary}"),
        false => format!("{result} on an earlier version: check again"),
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

/// The workspaces showing `layer` or `proposal` as a layer, but those two.
fn followers(common_dir: &Path, layer: &str, proposal: &str) -> Result<Vec<(StateDir, Lens)>> {
    let mut out = Vec::new();
    for s in state::list(common_dir)? {
        if s.branch == layer || s.branch == proposal {
            continue;
        }
        let lens = Lens::read(&s.state).unwrap_or_default();
        if lens.find(layer).is_some() || lens.find(proposal).is_some() {
            out.push((s.state, lens));
        }
    }
    Ok(out)
}

/// `gitfold consent`: checks the proposal against every workspace that
/// shows its layer (or it) and read a file it changes, or whose reads aren't
/// recorded: each one's work mixed with it, and the test run in the mix.
/// Returns the exit code: 0 all passed, 4 any failed.
pub fn consent(repo: &Path, args: ConsentArgs) -> Result<i32> {
    let common_dir = git::init(repo)?;
    let local = Local::read(&common_dir);
    let store = Store::default();
    let p = Proposal::load(&store, &common_dir, &local, &args.proposal)?;
    let paths: Vec<BString> = p.changes()?.into_iter().map(|(path, _)| path).collect();
    let mount = mount_table::mount_of(&common_dir)?;
    if args.test.is_some() && mount.is_none() {
        bail!("--test runs in the mix's snapshot: mount the repository");
    }
    let mut table = String::from("workspace\tread\tmerges\ttest\n");
    let (mut checked, mut failing) = (0, Vec::new());
    for (state, _) in followers(&common_dir, &p.layer, &p.name)? {
        let branch = &state.branch;
        let read = Read::of(&state, &paths)?;
        if read == Read::No {
            let _ = writeln!(table, "{branch}\tno\t-\t-");
            continue;
        }
        checked += 1;
        let ours = seal::seal(&store, &state)?.commit;
        let mixed = match super::mix::merge(&common_dir, &ours, &p.work.id, &p.name) {
            Ok(mixed) => mixed,
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
        };
        let (Some(test), Some(mount)) = (&args.test, &mount) else {
            let _ = writeln!(table, "{branch}\t{}\tyes\t-", read.word());
            continue;
        };
        let at = mount.join(format!("@{mixed}"));
        let out = std::process::Command::new("sh")
            .args(["-c", test])
            .current_dir(&at)
            .env("GITFOLD_WORKSPACE", branch)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("running the test in {}", at.display()))?;
        if out.status.success() {
            let _ = writeln!(table, "{branch}\t{}\tyes\tpassed", read.word());
            continue;
        }
        failing.push(branch.clone());
        let text = [&out.stdout[..], &out.stderr[..]].concat();
        let text = String::from_utf8_lossy(&text);
        let last = text.lines().rev().find(|l| !l.trim().is_empty());
        let last: String = last.unwrap_or_default().trim().chars().take(100).collect();
        let code = (out.status.code()).map_or_else(|| "a signal".to_string(), |c| c.to_string());
        let _ = writeln!(
            table,
            "{branch}\t{}\tyes\tfailed\texit {code}: {last} ({})",
            read.word(),
            at.display()
        );
    }
    print!("{table}");
    let (result, summary) = match (checked, failing.is_empty()) {
        (0, _) => (
            "passed",
            "no workspace read the files it changes".to_string(),
        ),
        (n, true) => ("passed", format!("{n} workspaces checked")),
        (n, false) => (
            "failed",
            format!("{} of {n} failing: {}", failing.len(), failing.join(", ")),
        ),
    };
    println!("{}: {result} ({summary})", p.name);
    let record = format!("{result} {} {summary}", p.work.id);
    settings::set_consent(&common_dir, &p.name, &record)?;
    Ok(if failing.is_empty() { 0 } else { 4 })
}

/// `gitfold accept`: writes the proposal's changed files into its layer's
/// workspace, shows the layer again where the proposal showed, tells every
/// workspace that read them, and removes the proposal. Refused if the layer
/// changed those files since the proposal was made.
pub fn accept(repo: &Path, args: AcceptArgs) -> Result<()> {
    let common_dir = git::init(repo)?;
    let local = Local::read(&common_dir);
    let store = Store::default();
    let p = Proposal::load(&store, &common_dir, &local, &args.proposal)?;
    let here = super::branch_or_current(args.workspace, repo).map_err(|_| {
        anyhow::anyhow!("accept a proposal in its layer's workspace, or name it with -w")
    })?;
    if here != p.layer {
        return Err(Refusal::NotFound.error(format!(
            "{} proposes a change to {}, not to {here}: accept it in {}'s workspace",
            p.name, p.layer, p.layer
        )));
    }
    let changes = p.changes()?;
    let paths: Vec<BString> = changes.iter().map(|(path, _)| path.clone()).collect();
    // The layer's work as now, on the proposal's paths, must be what the
    // proposal started from: else its version of them would be lost.
    let now = git::resolve(&work(&store, &common_dir, &p.layer)?.to_string())?;
    let moved: Vec<String> = git::with_repo(|repo| -> Result<Vec<String>> {
        let at = |tree, path: &BString| git::lens::lookup(repo, tree, path.as_ref());
        let mut moved = Vec::new();
        for path in &paths {
            if at(now.tree, path)? != at(p.start.tree, path)? {
                moved.push(path.to_string());
            }
        }
        Ok(moved)
    })?;
    if !moved.is_empty() {
        return Err(Refusal::Changes.error(format!(
            "{} changed {} since {} was proposed: accepting it would undo that. Reject it \
             (`gitfold reject {}`) and have {} propose again, from the work as now",
            p.layer,
            moved.join(", "),
            p.name,
            p.name,
            p.by,
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
        // Followers move to it at their next turn.
        seal::seal(&store, &layer_state)?;
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
    let _ = writeln!(out, "consent: {}", consent_of(&local, &p));
    let diff = short_diff(&common_dir, &p, &paths)?;
    let mut told = Vec::new();
    for (state, lens) in followers(&common_dir, &p.layer, &p.name)? {
        let branch = state.branch.clone();
        out += &show_layer_again(&store, &p, &state, lens)?;
        if !paths.is_empty() && Read::of(&state, &paths)? == Read::Yes {
            let body = format!(
                "contract {} changed: {}'s proposal {} was accepted into {}; your layer of {} \
                 shows it from your next turn (or `gitfold layer update`).\n{diff}",
                shown.join(", "),
                p.by,
                p.name,
                p.layer,
                p.layer
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
        let _ = writeln!(out, "told {} (they read the files)", told.join(", "));
    }
    out += &retire(&common_dir, &p.name);
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
    let here = super::branch_or_current(args.workspace, repo).map_err(|_| {
        anyhow::anyhow!(
            "reject a proposal in its layer's or its proposer's workspace, or name it with -w"
        )
    })?;
    if here != p.layer && here != p.by {
        return Err(Refusal::NotFound.error(format!(
            "{} is {}'s proposal to change {}: reject it in either workspace",
            p.name, p.by, p.layer
        )));
    }
    let mut out = format!("rejected {} (to change {})\n", p.name, p.layer);
    for (state, lens) in followers(&common_dir, &p.layer, &p.name)? {
        out += &show_layer_again(&store, &p, &state, lens)?;
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

/// Shows `p`'s layer again in the workspace in `state` (with layers
/// `lens`) if it shows `p`, as the layer's work is now; says so.
fn show_layer_again(
    store: &Store,
    p: &Proposal,
    state: &StateDir,
    mut lens: Lens,
) -> Result<String> {
    let Some(i) = lens.find(&p.name) else {
        return Ok(String::new());
    };
    let common_dir = &state.common_dir;
    let on = lens.layers[i].on;
    match lens.find(&p.layer) {
        Some(_) => drop(lens.layers.remove(i)),
        None => {
            lens.layers[i] = super::layer::layer_of(store, common_dir, state, &p.layer)?;
            lens.layers[i].on = on;
        }
    }
    // Stored even if the switch is refused now: the workspace switches to it
    // when it can.
    lens.write(state)?;
    let branch = &state.branch;
    Ok(match super::layer::apply(common_dir, branch) {
        Ok(_) => format!("{branch} shows {} again\n", p.layer),
        Err(e) => format!(
            "{branch} shows {} again once it can switch ({e:#})\n",
            p.layer
        ),
    })
}

/// The proposal's changes to `paths` as changed lines, a few.
fn short_diff(common_dir: &Path, p: &Proposal, paths: &[BString]) -> Result<String> {
    const LINES: usize = 20;
    let (start, work) = (p.start.id.to_string(), p.work.id.to_string());
    let mut args = vec!["diff", "--no-color", "-U0", &start, &work, "--"];
    let paths: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    args.extend(paths.iter().map(String::as_str));
    let out = git::cmd::output(common_dir, &args)?;
    let text = String::from_utf8_lossy(&out);
    let mut lines = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            lines.push(format!("{path}:"));
        } else if (line.starts_with('+') || line.starts_with('-'))
            && !line.starts_with("---")
            && !line.starts_with("+++")
        {
            lines.push(format!("  {line}"));
        }
    }
    let more = lines.len().saturating_sub(LINES);
    lines.truncate(LINES);
    if more > 0 {
        lines.push(format!("  ... ({more} more lines)"));
    }
    Ok(lines.join("\n"))
}

/// Removes proposal `name`: its workspace (its changes are accepted), its
/// branch, its start and its seals no layer names. Says what was done; what
/// failed is said, not an error: the proposal is accepted already.
fn retire(common_dir: &Path, name: &str) -> String {
    let removed = match super::remove::remove_once(common_dir, name, true, false) {
        Ok(_) => Ok(()),
        Err(e) if Refusal::of(&e) == Some(Refusal::NotFound) => Ok(()),
        Err(e) => Err(e),
    };
    if let Err(e) = removed {
        return format!(
            "its workspace wasn't removed ({e:#}): `gitfold remove --force {name}`, then `git \
             branch -D {name}`\n"
        );
    }
    let id = state::workspace_id(name);
    let named: HashSet<ObjectId> = crate::state::lens::named_seals(common_dir);
    let mut done = git::cmd::run(common_dir, &["branch", "-q", "-D", name])
        .and_then(|()| git::cmd::run(common_dir, &["update-ref", "-d", &start_ref(&id)]));
    for (n, seal) in seal::list(common_dir, &id).unwrap_or_default() {
        if done.is_ok() && !named.contains(&seal) {
            let r = format!("{}{n}", seal::ref_prefix(&id));
            done = git::cmd::run(common_dir, &["update-ref", "-d", &r]);
        }
    }
    match done {
        Ok(()) => format!("removed {name} (its workspace and branch)\n"),
        Err(e) => format!("removing branch {name}: {e:#}\n"),
    }
}
