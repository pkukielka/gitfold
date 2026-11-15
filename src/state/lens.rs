//! A workspace's layers: other branches' work shown in it, read-only, over
//! its base and under its own changes (the lens, in the code). Each layer is
//! a branch's work as of one commit, its seal: a workspace's work as `seal`
//! makes it, or a branch's or a revision's commits. It shows as the changes
//! it makes since where the two diverged (the merge base, pinned with the
//! layer, so the view doesn't change when this workspace's base moves).
//!
//! A live layer follows its branch (`cli::layer`): a workspace's work as
//! `seal::Driven` says (an agent's at the end of its last turn), or the
//! branch's tip with `--commits` or without a workspace. A frozen one stays
//! as taken, and a revision (a tag, a commit) is fixed.
//!
//! What the workspace shows is then the view tree: its base's tree with each
//! layer's changes on top, and upper over all of it as before. A layer
//! brings only its own changes: at each path it changed, the view shows the
//! 3-way merge of its version at the merge base, what the path is so far
//! (the base's version, with the layers before it applied) and its version,
//! as merging its branch would. Conflict markers are kept (the base's
//! version if a change can't be merged at all), and the conflicts reported
//! (`View::conflicts`). A path a layer changed is
//! mixed: it is that layer's (or those layers'), so it can't be written
//! here; the workspace's own file at that path, if it has one, wins and
//! stays writable.
//!
//! Stored in `lens` in the state directory, rewritten whole: `version 2`,
//! then one line per layer, in the order added: `layer <name> TAB <shows>
//! TAB <updates> TAB <moved> TAB <seal> TAB <merge base>`, with `<shows>`
//! `work`, `commits-only` or `commits` (`Shows`), `<updates>` `live`,
//! `frozen` or `fixed` (`Updates`) and `<moved>` when it last moved, in
//! seconds since the epoch (names hold no control characters). A layer line
//! may be followed by an `instead` line with the same fields: the layer it
//! stands in for, as it was (a proposal shown in its layer's place, see
//! `cli::proposal`), put back when the proposal goes. Read and written under
//! `lock`, so concurrent changes don't undo each other.
//!
//! Version 1 files (from builds before `layer add --frozen/--commits`) are
//! read too: `layer <name> TAB <on|off> TAB <seal> TAB <merge base>`; a
//! layer `on` is live and shows all work, one `off` (hidden) is dropped.

use std::collections::{HashMap, HashSet};
use std::fs;

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::BString;

use super::StateDir;
use crate::fsutil::{not_found, write_atomic};
use crate::git;

/// The file holding a workspace's layers.
pub const LENS: &str = "lens";

/// The file `lock` locks.
const LOCK: &str = "lens.lock";

/// Held while a command reads, changes and stores a workspace's layers
/// (`lock`); released when dropped, also when the process dies.
pub type LayersLock = crate::lock::FileLock;

/// Locks the layers of the workspace in `state` against other commands
/// changing them, waiting for one that does: an exclusive `flock` on a file
/// in its state directory. Taken across each read-modify-write of the
/// stored layers, so two at once (a hook moving live layers while `layer
/// add` runs, a proposal accepted meanwhile) don't lose one's change. The
/// daemon only reads them, but when it moves the live layers of a workspace
/// without an agent (`try_lock`, see `state::follow`).
pub fn lock(state: &StateDir) -> Result<LayersLock> {
    crate::lock::wait(&state.dir.join(LOCK))
}

/// `lock` without waiting: `None` while a command holds it.
pub fn try_lock(state: &StateDir) -> Result<Option<LayersLock>> {
    crate::lock::try_take(&state.dir.join(LOCK))
}

/// What a layer shows of its branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shows {
    /// A workspace's work, uncommitted included (a seal).
    Work,
    /// Only what the branch committed (`--commits`).
    CommitsOnly,
    /// The commits of a branch without a workspace, a tag or a commit.
    Commits,
}

/// How a layer moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Updates {
    /// Follows its branch.
    Live,
    /// As taken (`--frozen`).
    Frozen,
    /// A revision: never moves.
    Fixed,
}

impl Shows {
    fn word(self) -> &'static str {
        match self {
            Shows::Work => "work",
            Shows::CommitsOnly => "commits-only",
            Shows::Commits => "commits",
        }
    }

    fn parse(word: &str) -> Result<Shows> {
        Ok(match word {
            "work" => Shows::Work,
            "commits-only" => Shows::CommitsOnly,
            "commits" => Shows::Commits,
            _ => bail!("{word:?}: not work, commits-only or commits"),
        })
    }
}

impl Updates {
    fn word(self) -> &'static str {
        match self {
            Updates::Live => "live",
            Updates::Frozen => "frozen",
            Updates::Fixed => "fixed",
        }
    }

    fn parse(word: &str) -> Result<Updates> {
        Ok(match word {
            "live" => Updates::Live,
            "frozen" => Updates::Frozen,
            "fixed" => Updates::Fixed,
            _ => bail!("{word:?}: not live, frozen or fixed"),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// The branch (or revision) as named.
    pub branch: String,
    pub shows: Shows,
    pub updates: Updates,
    /// When it last moved (taken, or followed to newer work), in seconds
    /// since the epoch.
    pub moved: i64,
    pub seal: ObjectId,
    /// Where the two workspaces' histories diverged: the layer's changes are
    /// its seal's against this.
    pub merge_base: ObjectId,
    /// For a proposal shown in its layer's place: that layer as it was, put
    /// back (its way of updating kept) when the proposal goes.
    pub instead: Option<Box<Layer>>,
}

impl Layer {
    /// Its fields as stored (see the module doc), from the name on.
    fn fields(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.branch,
            self.shows.word(),
            self.updates.word(),
            self.moved,
            self.seal,
            self.merge_base
        )
    }

    /// A layer from its stored fields (see `fields`), of `line`.
    fn from_fields(line: &str, fields: &str) -> Result<Layer> {
        let fields: Vec<&str> = fields.split('\t').collect();
        let [branch, shows, updates, moved, seal, merge_base] = fields[..] else {
            bail!("bad line {line:?}");
        };
        Ok(Layer {
            branch: branch.to_string(),
            shows: Shows::parse(shows)?,
            updates: Updates::parse(updates)?,
            moved: moved.parse()?,
            seal: ObjectId::from_hex(seal.as_bytes())?,
            merge_base: ObjectId::from_hex(merge_base.as_bytes())?,
            instead: None,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lens {
    /// In the order added.
    pub layers: Vec<Layer>,
}

impl Lens {
    /// The layers stored for the workspace in `state` (none if it has no file).
    pub fn read(state: &StateDir) -> Result<Lens> {
        let path = state.dir.join(LENS);
        let mut lens = Lens::parse(&stored_bytes(state)?)
            .with_context(|| format!("reading {}", path.display()))?;
        // A version 1 file doesn't say when its layers moved: when it was
        // last written, then.
        if lens.layers.iter().any(|l| l.moved == 0)
            && let Ok(at) = fs::metadata(&path).and_then(|m| m.modified())
        {
            let secs = crate::fsutil::epoch_secs(at);
            for l in lens.layers.iter_mut().filter(|l| l.moved == 0) {
                l.moved = secs;
            }
        }
        Ok(lens)
    }

    /// A layers file's content (empty: no layers).
    pub fn parse(data: &[u8]) -> Result<Lens> {
        if data.is_empty() {
            return Ok(Lens::default());
        }
        let text = std::str::from_utf8(data)?;
        let mut lines = text.lines();
        let version = lines.next();
        let mut layers: Vec<Layer> = Vec::new();
        if version == Some("version 1") {
            for line in lines {
                let fields: Vec<&str> = (line.strip_prefix("layer ").unwrap_or(""))
                    .split('\t')
                    .collect();
                let [branch, on, seal, merge_base] = fields[..] else {
                    bail!("bad line {line:?}");
                };
                // Hidden: there are no hidden layers now.
                if on != "on" {
                    continue;
                }
                layers.push(Layer {
                    branch: branch.to_string(),
                    shows: Shows::Work,
                    updates: Updates::Live,
                    moved: 0,
                    seal: ObjectId::from_hex(seal.as_bytes())?,
                    merge_base: ObjectId::from_hex(merge_base.as_bytes())?,
                    instead: None,
                });
            }
            return Ok(Lens { layers });
        }
        if version != Some("version 2") {
            bail!("not a layers file this gitfold knows");
        }
        for line in lines {
            if let Some(fields) = line.strip_prefix("instead ") {
                let Some(last) = layers.last_mut() else {
                    bail!("bad line {line:?}");
                };
                last.instead = Some(Box::new(Layer::from_fields(line, fields)?));
                continue;
            }
            let fields = line.strip_prefix("layer ").unwrap_or("");
            layers.push(Layer::from_fields(line, fields)?);
        }
        Ok(Lens { layers })
    }

    /// Stores them (no file when there are none).
    pub fn write(&self, state: &StateDir) -> Result<()> {
        let mut text = String::new();
        if !self.layers.is_empty() {
            text += "version 2\n";
        }
        for l in &self.layers {
            text += &format!("layer {}\n", l.fields());
            if let Some(instead) = &l.instead {
                text += &format!("instead {}\n", instead.fields());
            }
        }
        store_bytes(state, text.as_bytes())
    }

    /// Whether there are layers.
    pub fn is_on(&self) -> bool {
        !self.layers.is_empty()
    }

    /// The subject of the layers commit of workspace `branch` showing them.
    pub fn subject(&self, branch: &str) -> String {
        let names: Vec<&str> = self.layers.iter().map(|l| l.branch.as_str()).collect();
        git::lens::lens_subject(branch, &names)
    }

    pub fn find(&self, branch: &str) -> Option<usize> {
        self.layers.iter().position(|l| l.branch == branch)
    }
}

/// Every workspace's layers stored in the repository at `common_dir`, and
/// with `applied`, those applied too; files that can't be read are left out.
fn every_lens(common_dir: &std::path::Path, applied: bool) -> impl Iterator<Item = Lens> {
    let dirs = fs::read_dir(super::state_root(common_dir))
        .into_iter()
        .flatten();
    dirs.flatten().flat_map(move |dir| {
        let stored = fs::read(dir.path().join(LENS)).ok();
        let stored = stored.and_then(|data| Lens::parse(&data).ok());
        let applied = (applied
            .then(|| fs::read(dir.path().join(APPLIED)).ok())
            .flatten())
        .and_then(|data| Applied::parse(&data).ok())
        .and_then(|a| a.layers().ok());
        stored.into_iter().chain(applied)
    })
}

/// Whether any workspace of the repository at `common_dir` has `branch`'s
/// work as a layer.
pub fn shown_anywhere(common_dir: &std::path::Path, branch: &str) -> bool {
    every_lens(common_dir, false).any(|lens| lens.find(branch).is_some())
}

/// Every seal a workspace's layers name, stored or applied, and those of the
/// layers proposals stand in for (put back later): their refs are kept
/// (`seal`).
pub fn named_seals(common_dir: &std::path::Path) -> HashSet<ObjectId> {
    every_lens(common_dir, true)
        .flat_map(|lens| lens.layers.into_iter())
        .flat_map(|l| std::iter::once(l.seal).chain(l.instead.map(|i| i.seal)))
        .collect()
}

/// A path where a layer's change conflicts with the base's or with other
/// layers' (see `view`).
#[derive(Clone, Debug)]
pub struct Conflict {
    pub path: BString,
    /// The layers (indexes in `Lens::layers`): the one whose change
    /// conflicts with the base's, or those whose changes conflict.
    pub layers: Vec<usize>,
    /// With the base's change.
    pub base: bool,
    /// It shows git's conflict markers; else the base's version.
    pub markers: bool,
}

impl Conflict {
    /// In words, with the names of `lens`' layers: `<path>: <layer>'s change
    /// conflicts with your base` or `<path> is changed by both <a> and <b>`,
    /// and when it can't be merged, that it shows the base's version.
    pub fn describe(&self, lens: &Lens) -> String {
        let names = || self.layers.iter().map(|&i| lens.layers[i].branch.clone());
        let mut out = match self.base {
            true => format!(
                "{}: {}'s change conflicts with your base",
                self.path,
                and_list(names())
            ),
            false => format!(
                "{} is changed by {}{}",
                self.path,
                if self.layers.len() == 2 { "both " } else { "" },
                and_list(names())
            ),
        };
        if !self.markers {
            out += " (the changes can't be merged: it shows your base's version)";
        }
        out
    }
}

/// What a workspace with layers shows below its own changes.
pub struct View {
    /// M: the base's tree with the layers' changes.
    pub tree: ObjectId,
    /// H: the base's tree with only the blended files blended (see `blend`).
    pub hunk_tree: ObjectId,
    /// The paths the layers' whole files replace, delete or merge, with the
    /// layers (their indexes in `Lens::layers`) whose changes show there:
    /// read-only.
    pub mixed: HashMap<Vec<u8>, Vec<usize>>,
    /// The blended files (see `blend`), with the layers blended in: the
    /// workspace writes them, its own lines on top of theirs.
    pub blended: HashMap<Vec<u8>, Vec<usize>>,
    /// The seals of the layers blended in (L's parents after the base).
    pub hunk_seals: Vec<ObjectId>,
    /// The paths where a layer's change conflicts with the base's or
    /// another layer's.
    pub conflicts: Vec<Conflict>,
    /// What the agent should hear about it.
    pub notes: Vec<String>,
}

/// The view of `lens` over base commit `base`, or `None` if it has no
/// layers.
///
/// A file is blended, rather than shown whole, if it is `merge=union` (in
/// the base's `.gitattributes` or `info/attributes`), the base has it and no
/// layer deletes it: every layer's change to it is merged into the base's
/// version, lines both sides add kept both (registries, manifests: files
/// several agents append to).
pub fn view(common_dir: &std::path::Path, base: &ObjectId, lens: &Lens) -> Result<Option<View>> {
    if !lens.is_on() {
        return Ok(None);
    }
    let stack: Vec<git::lens::Pin> = (lens.layers.iter())
        .map(|l| git::lens::Pin {
            name: l.branch.clone(),
            seal: l.seal,
            merge_base: l.merge_base,
        })
        .collect();
    let hunk_paths = |paths: &[BString]| -> Result<HashSet<BString>> {
        let rev = base.to_string();
        let values = git::attr::values(common_dir, &rev, "merge", paths)?;
        Ok((values.into_iter())
            .filter(|(_, value)| value == "union")
            .map(|(path, _)| path)
            .collect())
    };
    let view = git::with_repo(|repo| git::lens::build_view(repo, *base, &stack, &hunk_paths))?;
    let name = |ix: usize| lens.layers[ix].branch.clone();
    let mut conflicts = Vec::new();
    let mut notes = Vec::new();
    let hunk_layers = view.hunk_layers();
    for note in view.notes {
        notes.push(match note {
            git::lens::Note::NotBlended { layer, path } => format!(
                "{path}: {}'s change conflicts with what is below it, so it isn't blended in",
                name(layer)
            ),
            git::lens::Note::Conflict {
                layers,
                base,
                path,
                markers,
            } => {
                let conflict = Conflict {
                    path,
                    layers,
                    base,
                    markers,
                };
                let note = format!(
                    "Conflict: {}{}",
                    conflict.describe(lens),
                    match markers {
                        true => " (it shows git's conflict markers)",
                        false => "",
                    }
                );
                conflicts.push(conflict);
                note
            }
        });
    }
    Ok(Some(View {
        tree: view.view_tree,
        hunk_tree: view.hunk_tree,
        mixed: (view.mixed.iter())
            .map(|(p, ixs)| (p.to_vec(), ixs.clone()))
            .collect(),
        blended: (view.blended.iter())
            .map(|(p, ixs)| (p.to_vec(), ixs.clone()))
            .collect(),
        hunk_seals: (hunk_layers.into_iter())
            .map(|ix| lens.layers[ix].seal)
            .collect(),
        conflicts,
        notes,
    }))
}

/// `a`, `a and b`, `a, b and c`.
pub fn and_list(names: impl IntoIterator<Item = String>) -> String {
    let mut names: Vec<String> = names.into_iter().collect();
    match names.pop() {
        None => String::new(),
        Some(last) if names.is_empty() => last,
        Some(last) => format!("{} and {last}", names.join(", ")),
    }
}

/// The file recording what the daemon last applied of a workspace's layers
/// (`Applied`).
pub const APPLIED: &str = "lens-applied";

/// What was last applied of a workspace's layers, as opposed to what is
/// stored (`Lens`): the layers file then, and what git was told. The view
/// and git always describe the same layers: a workspace opens with these,
/// and switches to the stored layers when they differ (a change made while
/// it was closed, or a switch that failed half-way). Stored in `lens-applied`:
/// `version 1`, `head <oid or ->`, `next <oid or ->` (the lens commit HEAD is
/// being moved to, adopted if it got there), a line `lens <bytes>` giving the length of
/// the layers file that follows, then NUL-terminated records: `S<path>` a
/// path git was told to skip (skip-worktree), `B<path>` a blended path whose
/// index entry git was given the blend for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Applied {
    /// The layers file's content when applied (empty: none).
    pub lens: Vec<u8>,
    /// The lens commit HEAD was detached at.
    pub head: Option<ObjectId>,
    /// The lens commit HEAD was being moved to (a switch or an extraction
    /// writes it first): if HEAD is there, it got there.
    pub next: Option<ObjectId>,
    pub skipped: std::collections::BTreeSet<Vec<u8>>,
    pub blended: std::collections::BTreeSet<Vec<u8>>,
}

impl Applied {
    pub fn read(state: &StateDir) -> Result<Applied> {
        let path = state.dir.join(APPLIED);
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if not_found(&e) => return Ok(Applied::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Applied::parse(&data).with_context(|| format!("reading {}", path.display()))
    }

    /// A `lens-applied` file's content.
    pub fn parse(data: &[u8]) -> Result<Applied> {
        let bad = || anyhow::anyhow!("not a file this gitfold knows");
        let mut rest = data.strip_prefix(b"version 1\n").ok_or_else(bad)?;
        let mut oid = |key: &[u8]| -> Result<Option<ObjectId>> {
            rest = rest.strip_prefix(key).ok_or_else(bad)?;
            let nl = rest.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
            let oid = match &rest[..nl] {
                b"-" => None,
                hex => Some(ObjectId::from_hex(hex)?),
            };
            rest = &rest[nl + 1..];
            Ok(oid)
        };
        let head = oid(b"head ")?;
        let next = oid(b"next ")?;
        rest = rest.strip_prefix(b"lens ").ok_or_else(bad)?;
        let nl = rest.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
        let len: usize = std::str::from_utf8(&rest[..nl])?.parse()?;
        rest = &rest[nl + 1..];
        anyhow::ensure!(rest.len() >= len, bad());
        let mut applied = Applied {
            lens: rest[..len].to_vec(),
            head,
            next,
            ..Default::default()
        };
        for record in rest[len..].split(|&b| b == 0).filter(|r| !r.is_empty()) {
            match record.split_first() {
                Some((b'S', path)) => applied.skipped.insert(path.to_vec()),
                Some((b'B', path)) => applied.blended.insert(path.to_vec()),
                _ => return Err(bad()),
            };
        }
        Ok(applied)
    }

    pub fn write(&self, state: &StateDir) -> Result<()> {
        let show = |o: Option<ObjectId>| o.map_or("-".to_string(), |o| o.to_string());
        let (head, next) = (show(self.head), show(self.next));
        let mut data = format!(
            "version 1\nhead {head}\nnext {next}\nlens {}\n",
            self.lens.len()
        )
        .into_bytes();
        data.extend_from_slice(&self.lens);
        for (tag, set) in [(b'S', &self.skipped), (b'B', &self.blended)] {
            for path in set {
                data.push(tag);
                data.extend_from_slice(path);
                data.push(0);
            }
        }
        Ok(write_atomic(&state.dir.join(APPLIED), &data)?)
    }

    /// The layers as applied.
    pub fn layers(&self) -> Result<Lens> {
        Lens::parse(&self.lens)
    }
}

/// The file whose presence means the layers of a workspace are taken off
/// for a move of its branch onto its base (`cli::stack`): the layers file
/// stays as it is (what is stored is the truth), but none show until
/// `put_back`.
pub const OFF: &str = "lens-off";

/// Whether the layers of the workspace in `state` are taken off for a move.
pub fn off(state: &StateDir) -> bool {
    fs::symlink_metadata(state.dir.join(OFF)).is_ok()
}

/// Takes the layers of the workspace in `state` off for a move (see `OFF`),
/// durably; `lock` held.
pub fn take_off(state: &StateDir) -> Result<()> {
    write_atomic(
        &state.dir.join(OFF),
        b"taken off for a move onto the base\n",
    )?;
    fs::File::open(&state.dir)?.sync_all()?;
    Ok(())
}

/// Puts the layers of the workspace in `state` back after a move (or one
/// interrupted, finished or undone since): each layer's merge base is
/// pinned again against where the branch is now (what the workspace's base
/// becomes), and the layers show again (the daemon switches to them).
/// Whether they were off; `lock` held, `git::init` done.
pub fn put_back(state: &StateDir) -> Result<bool> {
    if !off(state) {
        return Ok(false);
    }
    let tip = git::branch_tip(&state.branch)?.id;
    let mut lens = Lens::read(state)?;
    let before = lens.clone();
    let repin = |layer: &mut Layer| match git::merge_base(tip, layer.seal) {
        Ok(base) => layer.merge_base = base,
        Err(e) => log::warn!("{}: layer {}: {e:#}", state.branch, layer.branch),
    };
    for layer in &mut lens.layers {
        repin(layer);
        if let Some(instead) = &mut layer.instead {
            repin(instead);
        }
    }
    if lens != before {
        lens.write(state)?;
    }
    match fs::remove_file(state.dir.join(OFF)) {
        Err(e) if !not_found(&e) => return Err(e.into()),
        _ => {}
    }
    fs::File::open(&state.dir)?.sync_all()?;
    Ok(true)
}

/// The layers of the workspace in `state` to show, and their bytes as
/// stored: those stored, none while taken off for a move (`OFF`).
pub fn to_show(state: &StateDir) -> Result<(Lens, Vec<u8>)> {
    match off(state) {
        true => Ok((Lens::default(), Vec::new())),
        false => Ok((Lens::read(state)?, stored_bytes(state)?)),
    }
}

/// `to_show`'s bytes alone (a cheap read).
pub fn shown_bytes(state: &StateDir) -> Result<Vec<u8>> {
    match off(state) {
        true => Ok(Vec::new()),
        false => stored_bytes(state),
    }
}

/// `put_back` for the daemon, after a move it didn't make was interrupted:
/// only if no command holds the layers' lock (one moving it holds it
/// throughout). Whether they were put back.
pub fn put_back_if_left(state: &StateDir) -> Result<bool> {
    if !off(state) {
        return Ok(false);
    }
    match try_lock(state)? {
        Some(_lock) => put_back(state),
        None => Ok(false),
    }
}

/// The content of the layers file of the workspace in `state` (empty if none).
pub fn stored_bytes(state: &StateDir) -> Result<Vec<u8>> {
    match fs::read(state.dir.join(LENS)) {
        Ok(data) => Ok(data),
        Err(e) if not_found(&e) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Stores `data` as the layers file of the workspace in `state` (empty: no
/// file). Also puts back what `stored_bytes` read, after the daemon refused
/// to apply a change.
pub fn store_bytes(state: &StateDir, data: &[u8]) -> Result<()> {
    let path = state.dir.join(LENS);
    if data.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if !not_found(&e) => Err(e.into()),
            _ => Ok(()),
        };
    }
    Ok(write_atomic(&path, data)?)
}

/// Stores `lens` for the workspace in `state`, then has it shown (`show`:
/// the daemon's switch, asked or run); says what `show` did. If `show`
/// fails, the layers stored before are put back, so what is stored is what
/// shows. Called under `lock`.
pub fn store_and_show(
    state: &StateDir,
    lens: &Lens,
    show: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let before = stored_bytes(state)?;
    lens.write(state)?;
    // Taken off for a move that was interrupted: back, pinned again.
    put_back(state)?;
    show().inspect_err(|_| {
        if let Err(e) = store_bytes(state, &before) {
            log::warn!("putting the layers back: {e:#}");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn version_1_files_are_read() {
        let v1 = format!("version 1\nlayer on1\ton\t{A}\t{B}\nlayer off1\toff\t{B}\t{A}\n");
        let lens = Lens::parse(v1.as_bytes()).unwrap();
        // On: live, all work; off (hidden): dropped.
        assert_eq!(lens.layers.len(), 1);
        let l = &lens.layers[0];
        assert_eq!(
            (l.branch.as_str(), l.shows, l.updates, l.seal.to_string()),
            ("on1", Shows::Work, Updates::Live, A.to_string())
        );
        assert!(l.instead.is_none());
        assert!(Lens::parse(b"version 9\n").is_err());
    }

    #[test]
    fn a_proposal_keeps_the_layer_it_stands_in_for() {
        let text = format!(
            "version 2\nlayer b-prop/c\twork\tlive\t5\t{A}\t{B}\n\
             instead b\tcommits-only\tfrozen\t3\t{B}\t{A}\nlayer d\tcommits\tfixed\t4\t{A}\t{A}\n"
        );
        let lens = Lens::parse(text.as_bytes()).unwrap();
        let was = lens.layers[0].instead.as_deref().unwrap();
        assert_eq!(
            (was.branch.as_str(), was.shows, was.updates, was.moved),
            ("b", Shows::CommitsOnly, Updates::Frozen, 3)
        );
        assert!(lens.layers[1].instead.is_none());
        let dir = std::env::temp_dir().join(format!("gitfold-lens-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = StateDir {
            dir: dir.clone(),
            ..StateDir::of(&dir, "x")
        };
        lens.write(&state).unwrap();
        assert_eq!(fs::read_to_string(dir.join(LENS)).unwrap(), text);
        fs::remove_dir_all(&dir).unwrap();
    }
}
