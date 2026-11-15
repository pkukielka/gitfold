//! A workspace's layers: other branches' work shown in it, read-only, over
//! its base and under its own changes. Each layer is
//! a branch's work as of one commit, its snapshot: a workspace's work as `snapshot`
//! makes it, or a branch's or a revision's commits. It shows as the changes
//! it makes since where it and the commit the view is built on diverged
//! (their merge base). That is worked out each time a view is built
//! (`view`), against that very commit, never stored: the workspace's base
//! lags behind its branch while layers show (commits made meanwhile), and
//! a merge base taken against another commit would make
//! the lines between the two look like the layer's own changes, which then
//! conflict with the base's.
//!
//! A live layer follows its branch (`cli::layer`): a workspace's work as
//! `snapshot::Driven` says (an agent's at the end of its last turn), or the
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
//! A layer is see-only or editable (`Layer::edit`, `layer add --edit`). An
//! editable layer is one the branch builds on: its commits sit on that
//! layer's commits (`cli::layer::moves`), so what the layer committed is the
//! branch's own; what it hasn't committed yet shows as any layer's work does.
//! A path only editable layers changed is writable (`View::writable`); one a
//! see-only layer changed is read-only.
//!
//! Stored in `layers` in the state directory, rewritten whole: `version 1`,
//! then one line per layer, in the order added: `layer <name> TAB <shows>
//! TAB <updates> TAB <moved> TAB <snapshot> TAB <see|edit>`, with `<shows>`
//! `work`, `commits-only` or `commits` (`Shows`), `<updates>` `live`,
//! `frozen` or `fixed` (`Updates`) and `<moved>` when it last moved, in
//! seconds since the epoch (names hold no control characters). A layer line
//! may be followed by an `instead` line with the same fields: the layer it
//! stands in for, as it was (a proposal shown in its layer's place, see
//! `cli::proposal`), put back when the proposal goes. Read and written under
//! `lock`, so concurrent changes don't undo each other. A branch without a
//! workspace has the same file for its editable layers (its state directory
//! then holds little else), so what a branch builds on is always here.
//!
//! While a move changes what the branch builds on (an editable layer added,
//! removed or made see-only, a landing followed), the layers it will have
//! once moved are kept in `layers.next` (`NEXT`), with the commit its own
//! commits will start from then: they become its layers once its own ref is
//! there (`settle`), and are dropped if the move didn't happen.

use std::collections::{HashMap, HashSet};
use std::fs;

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::BString;

use super::StateDir;
use crate::fsutil::{not_found, write_atomic};
use crate::git;

/// The file holding a workspace's layers.
pub const LAYERS: &str = "layers";

/// The file `lock` locks.
const LOCK: &str = "layers.lock";

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
    /// A workspace's work, uncommitted included (a snapshot).
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
    pub snapshot: ObjectId,
    /// Editable (`layer add --edit`): the branch builds on it.
    pub edit: bool,
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
            self.snapshot,
            if self.edit { "edit" } else { "see" },
        )
    }

    /// A layer from its stored fields (see `fields`), of `line`.
    fn from_fields(line: &str, fields: &str) -> Result<Layer> {
        let fields: Vec<&str> = fields.split('\t').collect();
        let [branch, shows, updates, moved, snapshot, edit] = fields[..] else {
            bail!("bad line {line:?}");
        };
        Ok(Layer {
            branch: branch.to_string(),
            shows: Shows::parse(shows)?,
            updates: Updates::parse(updates)?,
            moved: moved.parse()?,
            snapshot: ObjectId::from_hex(snapshot.as_bytes())?,
            edit: match edit {
                "edit" => true,
                "see" => false,
                _ => bail!("bad line {line:?}"),
            },
            instead: None,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layers {
    /// In the order added.
    pub layers: Vec<Layer>,
}

impl Layers {
    /// The layers stored for the workspace in `state` (none if it has no file).
    pub fn read(state: &StateDir) -> Result<Layers> {
        let path = state.dir.join(LAYERS);
        Layers::parse(&stored_bytes(state)?).with_context(|| format!("reading {}", path.display()))
    }

    /// A layers file's content (empty: no layers).
    pub fn parse(data: &[u8]) -> Result<Layers> {
        if data.is_empty() {
            return Ok(Layers::default());
        }
        let text = std::str::from_utf8(data)?;
        let mut lines = text.lines();
        if lines.next() != Some("version 1") {
            bail!("not a layers file this gitfold knows");
        }
        let mut layers: Vec<Layer> = Vec::new();
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
        Ok(Layers { layers })
    }

    /// Its content as stored (empty: no layers).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut text = String::new();
        if !self.layers.is_empty() {
            text += "version 1\n";
        }
        for l in &self.layers {
            text += &format!("layer {}\n", l.fields());
            if let Some(instead) = &l.instead {
                text += &format!("instead {}\n", instead.fields());
            }
        }
        text.into_bytes()
    }

    /// Stores them (no file when there are none).
    pub fn write(&self, state: &StateDir) -> Result<()> {
        store_bytes(state, &self.to_bytes())
    }

    /// The editable layers, in the order added: what the branch builds on.
    pub fn editable(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter().filter(|l| l.edit)
    }

    /// The names of the editable layers.
    pub fn editable_names(&self) -> Vec<String> {
        self.editable().map(|l| l.branch.clone()).collect()
    }

    /// Whether there are layers.
    pub fn is_on(&self) -> bool {
        !self.layers.is_empty()
    }

    /// The subject of the layers commit of workspace `branch` showing them.
    pub fn subject(&self, branch: &str) -> String {
        let names: Vec<&str> = self.layers.iter().map(|l| l.branch.as_str()).collect();
        git::layers::layers_subject(branch, &names)
    }

    pub fn find(&self, branch: &str) -> Option<usize> {
        self.layers.iter().position(|l| l.branch == branch)
    }
}

/// Every workspace's layers stored in the repository at `common_dir`, and
/// with `applied`, those applied too; files that can't be read are left out.
fn every_layers(common_dir: &std::path::Path, applied: bool) -> impl Iterator<Item = Layers> {
    let dirs = fs::read_dir(super::state_root(common_dir))
        .into_iter()
        .flatten();
    dirs.flatten().flat_map(move |dir| {
        let stored = fs::read(dir.path().join(LAYERS)).ok();
        let stored = stored.and_then(|data| Layers::parse(&data).ok());
        let applied = (applied
            .then(|| fs::read(dir.path().join(SHOWN_LAYERS)).ok())
            .flatten())
        .and_then(|data| ShownLayers::parse(&data).ok())
        .and_then(|a| a.layers().ok());
        stored.into_iter().chain(applied)
    })
}

/// Every branch's layers stored in the repository at `common_dir` (a
/// workspace's, or a branch's without one: its editable layers), by branch;
/// files that can't be read are left out.
pub fn every_branch(common_dir: &std::path::Path) -> Vec<(String, Layers)> {
    let dirs = fs::read_dir(super::state_root(common_dir))
        .into_iter()
        .flatten();
    let mut out: Vec<(String, Layers)> = (dirs.flatten())
        .filter_map(|dir| {
            let id = dir.file_name().to_string_lossy().into_owned();
            if id.starts_with('.') {
                return None;
            }
            let layers = Layers::parse(&fs::read(dir.path().join(LAYERS)).ok()?).ok()?;
            let branch =
                super::read_branch(&dir.path()).unwrap_or_else(|| super::workspace_branch(&id));
            Some((branch, layers))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Gives branch `to` (a fork of `from`, just made) the editable layers of
/// `from`, and its own ref if that is in `to`'s history: it is built on the
/// same.
pub fn copy_editable(common_dir: &std::path::Path, from: &str, to: &str) -> Result<()> {
    use git::stack;
    let source = StateDir::of(common_dir, from);
    let editable = Layers {
        layers: Layers::read(&source)?
            .layers
            .into_iter()
            .filter(|l| l.edit)
            .collect(),
    };
    if editable.layers.is_empty() {
        return Ok(());
    }
    let target = StateDir::of(common_dir, to);
    fs::create_dir_all(&target.dir)?;
    let mut layers = Layers::read(&target)?;
    layers.layers.retain(|l| editable.find(&l.branch).is_none());
    layers.layers.extend(editable.layers);
    layers.write(&target)?;
    let (Some(base), Some(tip)) = (
        stack::rev(common_dir, &stack::own_ref(from))?,
        stack::rev(common_dir, &format!("refs/heads/{to}"))?,
    ) else {
        return Ok(());
    };
    if stack::is_ancestor(common_dir, base, tip)? {
        stack::set_base(common_dir, to, base)?;
    }
    Ok(())
}

/// Whether any workspace of the repository at `common_dir` has `branch`'s
/// work as a layer.
pub fn shown_anywhere(common_dir: &std::path::Path, branch: &str) -> bool {
    every_layers(common_dir, false).any(|layers| layers.find(branch).is_some())
}

/// Every snapshot a workspace's layers name, stored or applied, and those of the
/// layers proposals stand in for (put back later): their refs are kept
/// (`snapshot`).
pub fn named_snapshots(common_dir: &std::path::Path) -> HashSet<ObjectId> {
    every_layers(common_dir, true)
        .flat_map(|layers| layers.layers.into_iter())
        .flat_map(|l| std::iter::once(l.snapshot).chain(l.instead.map(|i| i.snapshot)))
        .collect()
}

/// A path where a layer's change conflicts with the base's or with other
/// layers' (see `view`).
#[derive(Clone, Debug)]
pub struct Conflict {
    pub path: BString,
    /// The layers (indexes in `Layers::layers`): the one whose change
    /// conflicts with the base's, or those whose changes conflict.
    pub layers: Vec<usize>,
    /// With the base's change.
    pub base: bool,
    /// It shows git's conflict markers; else the base's version.
    pub markers: bool,
}

impl Conflict {
    /// In words, with the names of `layers`' layers: `<path>: <layer>'s change
    /// conflicts with your base` or `<path> is changed by both <a> and <b>`,
    /// and when it can't be merged, that it shows the base's version.
    pub fn describe(&self, layers: &Layers) -> String {
        let names = || self.layers.iter().map(|&i| layers.layers[i].branch.clone());
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
    /// H: the base's tree with only the union files union-merged (see `union`).
    pub hunk_tree: ObjectId,
    /// The paths the layers' whole files replace, delete or merge, with the
    /// layers (their indexes in `Layers::layers`) whose changes show there:
    /// read-only.
    pub mixed: HashMap<Vec<u8>, Vec<usize>>,
    /// The paths of `mixed` only editable layers changed: writable.
    pub writable: HashSet<Vec<u8>>,
    /// The union files (see `union`), with the layers union-merged in: the
    /// workspace writes them, its own lines on top of theirs.
    pub union: HashMap<Vec<u8>, Vec<usize>>,
    /// The snapshots of the layers union-merged in (L's parents after the base).
    pub hunk_snapshots: Vec<ObjectId>,
    /// The paths where a layer's change conflicts with the base's or
    /// another layer's.
    pub conflicts: Vec<Conflict>,
    /// What the agent should hear about it.
    pub notes: Vec<String>,
    /// The notes the listing shows too, each with its `Note:` or
    /// `Warning:`: union lines placed by the base's lines, or sitting
    /// among another branch's.
    pub warnings: Vec<String>,
}

/// The view of `layers` over base commit `base`, or `None` if it has no
/// layers. Each layer's changes are its snapshot's since it and `base` diverged
/// (`merge_base`, see the module doc).
///
/// A file is union-merged, rather than shown whole, if it is `merge=union` (in
/// the base's `.gitattributes` or `info/attributes`), the base has it and no
/// layer deletes it: every layer's change to it is merged into the base's
/// version, lines both sides add kept both (registries, manifests: files
/// several agents append to).
pub fn view(
    common_dir: &std::path::Path,
    base: &ObjectId,
    layers: &Layers,
) -> Result<Option<View>> {
    if !layers.is_on() {
        return Ok(None);
    }
    let stack = (layers.layers.iter())
        .map(|l| {
            Ok(git::layers::Pin {
                name: l.branch.clone(),
                snapshot: l.snapshot,
                merge_base: merge_base(base, l)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let hunk_paths = |paths: &[BString]| -> Result<HashSet<BString>> {
        let rev = base.to_string();
        let values = git::attr::values(common_dir, &rev, "merge", paths)?;
        Ok((values.into_iter())
            .filter(|(_, value)| value == "union")
            .map(|(path, _)| path)
            .collect())
    };
    let view = git::with_repo(|repo| git::layers::build_view(repo, *base, &stack, &hunk_paths))?;
    let name = |ix: usize| layers.layers[ix].branch.clone();
    let mut conflicts = Vec::new();
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    let hunk_layers = view.hunk_layers();
    for note in view.notes {
        notes.push(match note {
            git::layers::Note::NotInUnion { layer, path } => format!(
                "{path}: {}'s change conflicts with what is below it, so it isn't union-merged in",
                name(layer)
            ),
            git::layers::Note::PlacedByBase { layer, path } => {
                let note = placed_by_base(&name(layer), &path);
                warnings.push(format!("Note: {note}"));
                note
            }
            git::layers::Note::Among {
                layer,
                path,
                others,
            } => {
                let others = match others.is_empty() {
                    true => "another branch's".to_string(),
                    false => format!("{}'s", and_list(others.into_iter().map(name))),
                };
                let note = among(&format!("{}'s", name(layer)), &path, &others);
                warnings.push(format!("Warning: {note}"));
                note
            }
            git::layers::Note::Conflict {
                layers: involved,
                base,
                path,
                markers,
            } => {
                let conflict = Conflict {
                    path,
                    layers: involved,
                    base,
                    markers,
                };
                let note = format!(
                    "Conflict: {}{}",
                    conflict.describe(layers),
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
    let editable = |ixs: &Vec<usize>| ixs.iter().all(|&i| layers.layers[i].edit);
    Ok(Some(View {
        tree: view.view_tree,
        hunk_tree: view.hunk_tree,
        writable: (view.mixed.iter())
            .filter(|(_, ixs)| editable(ixs))
            .map(|(p, _)| p.to_vec())
            .collect(),
        mixed: (view.mixed.iter())
            .map(|(p, ixs)| (p.to_vec(), ixs.clone()))
            .collect(),
        union: (view.union.iter())
            .map(|(p, ixs)| (p.to_vec(), ixs.clone()))
            .collect(),
        hunk_snapshots: (hunk_layers.into_iter())
            .map(|ix| layers.layers[ix].snapshot)
            .collect(),
        conflicts,
        notes,
        warnings,
    }))
}

/// The warnings (`View::warnings`) for the own lines of the workspace in
/// `state` (on base commit `base`, showing `layers` as `view`) in its union
/// files: those added between two lines of its layers', next to none of the
/// base's.
pub fn own_among(
    state: &StateDir,
    base: &git::Commit,
    layers: &Layers,
    view: &View,
) -> Result<Vec<String>> {
    let content = |tree: ObjectId, path: &[u8]| -> Result<Vec<u8>> {
        let item = git::with_repo(|repo| git::layers::lookup(repo, tree, path.into()))?;
        item.map_or(Ok(Vec::new()), |i| git::read_blob(i.oid))
    };
    let mut out = Vec::new();
    let mut paths: Vec<&Vec<u8>> = view.union.keys().collect();
    paths.sort();
    for path in paths {
        let Ok(made) = fs::read(super::upper_path(&state.upper(), path)) else {
            continue;
        };
        let (at_base, seen) = (content(base.tree, path)?, content(view.hunk_tree, path)?);
        let around = git::layers::among_others(&at_base, &seen, &made);
        if around.is_empty() {
            continue;
        }
        let mut whose = Vec::new();
        for &ix in &view.union[path] {
            let layer = &layers.layers[ix];
            let theirs = content(git::resolve(&layer.snapshot.to_string())?.tree, path)?;
            let mb = git::resolve(&merge_base(&base.id, layer)?.to_string())?;
            let added = git::layers::added_lines(&content(mb.tree, path)?, &theirs);
            let key = git::layers::key;
            if (added.iter()).any(|l| around.iter().any(|a| key(a) == key(l))) {
                whose.push(layer.branch.clone());
            }
        }
        let others = match whose.is_empty() {
            true => "the layers'".to_string(),
            false => format!("{}'s", and_list(whose)),
        };
        let path = BString::from(path.as_slice());
        out.push(format!("Warning: {}", among("your", &path, &others)));
    }
    Ok(out)
}

/// The note for a layer `layer` whose lines in union file `path` are
/// placed by the base's lines (`git::layers::Note::PlacedByBase`).
fn placed_by_base(layer: &str, path: &BString) -> String {
    format!(
        "{layer}'s lines in {path} are placed by your base's lines: the lines {layer} added them \
         among aren't in the layers shown here"
    )
}

/// The warning for lines of `whose` (`B's`, `your`) in union file `path`
/// that sit among lines of `others` (`A's`): `path` may not be a list.
pub fn among(whose: &str, path: &BString, others: &str) -> String {
    format!(
        "{whose} lines in {path} sit among {others}: this file may not be a list (it's marked \
         merge=union)"
    )
}

/// Where `layer` and base commit `base` diverged: its changes are its
/// snapshot's against this. Without a commit in common they can't be told
/// apart, and no view can be built.
pub fn merge_base(base: &ObjectId, layer: &Layer) -> Result<ObjectId> {
    git::merge_base_if_any(*base, layer.snapshot)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{} shares no history with the base {base}, so its changes can't be told apart",
            layer.branch
        )
    })
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
/// (`ShownLayers`).
pub const SHOWN_LAYERS: &str = "layers-shown";

/// What was last applied of a workspace's layers, as opposed to what is
/// stored (`Layers`): the layers file then, and what git was told. The view
/// and git always describe the same layers: a workspace opens with these,
/// and switches to the stored layers when they differ (a change made while
/// it was closed, or a switch that failed half-way). Stored in `layers-shown`:
/// `version 1`, `head <oid or ->`, `next <oid or ->` (the layers commit HEAD is
/// being moved to, adopted if it got there), a line `layers <bytes>` giving the length of
/// the layers file that follows, then NUL-terminated records: `S<path>` a
/// path git was told to skip (skip-worktree), `B<path>` a union path whose
/// index entry git was given the union for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShownLayers {
    /// The layers file's content when applied (empty: none).
    pub layers: Vec<u8>,
    /// The layers commit HEAD was detached at.
    pub head: Option<ObjectId>,
    /// The layers commit HEAD was being moved to (a switch or an extraction
    /// writes it first): if HEAD is there, it got there.
    pub next: Option<ObjectId>,
    pub skipped: std::collections::BTreeSet<Vec<u8>>,
    pub union: std::collections::BTreeSet<Vec<u8>>,
}

impl ShownLayers {
    pub fn read(state: &StateDir) -> Result<ShownLayers> {
        let path = state.dir.join(SHOWN_LAYERS);
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if not_found(&e) => return Ok(ShownLayers::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        ShownLayers::parse(&data).with_context(|| format!("reading {}", path.display()))
    }

    /// A `layers-shown` file's content.
    pub fn parse(data: &[u8]) -> Result<ShownLayers> {
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
        rest = rest.strip_prefix(b"layers ").ok_or_else(bad)?;
        let nl = rest.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
        let len: usize = std::str::from_utf8(&rest[..nl])?.parse()?;
        rest = &rest[nl + 1..];
        anyhow::ensure!(rest.len() >= len, bad());
        let mut applied = ShownLayers {
            layers: rest[..len].to_vec(),
            head,
            next,
            ..Default::default()
        };
        for record in rest[len..].split(|&b| b == 0).filter(|r| !r.is_empty()) {
            match record.split_first() {
                Some((b'S', path)) => applied.skipped.insert(path.to_vec()),
                Some((b'B', path)) => applied.union.insert(path.to_vec()),
                _ => return Err(bad()),
            };
        }
        Ok(applied)
    }

    pub fn write(&self, state: &StateDir) -> Result<()> {
        let show = |o: Option<ObjectId>| o.map_or("-".to_string(), |o| o.to_string());
        let (head, next) = (show(self.head), show(self.next));
        let mut data = format!(
            "version 1\nhead {head}\nnext {next}\nlayers {}\n",
            self.layers.len()
        )
        .into_bytes();
        data.extend_from_slice(&self.layers);
        for (tag, set) in [(b'S', &self.skipped), (b'B', &self.union)] {
            for path in set {
                data.push(tag);
                data.extend_from_slice(path);
                data.push(0);
            }
        }
        Ok(write_atomic(&state.dir.join(SHOWN_LAYERS), &data)?)
    }

    /// The layers as applied.
    pub fn layers(&self) -> Result<Layers> {
        Layers::parse(&self.layers)
    }
}

/// The file holding the layers a branch will have once a move under way is
/// done (see the module doc): `onto <commit>`, the commit its own commits
/// start from then, followed by the layers file's content.
pub const NEXT: &str = "layers.next";

/// The layers of a move under way (`NEXT`).
#[derive(Clone, Debug, PartialEq)]
pub struct Next {
    /// Where the branch's own commits start once moved (its own ref then).
    pub onto: ObjectId,
    /// The layers file's content then.
    pub layers: Vec<u8>,
}

/// Records `layers` as what the branch of `state` will have once its own
/// ref is at `onto`, durably; `lock` held.
pub fn write_next(state: &StateDir, onto: ObjectId, layers: &Layers) -> Result<()> {
    fs::create_dir_all(&state.dir)?;
    let mut data = format!("onto {onto}\n").into_bytes();
    data.extend_from_slice(&layers.to_bytes());
    write_atomic(&state.dir.join(NEXT), &data)?;
    fs::File::open(&state.dir)?.sync_all()?;
    Ok(())
}

/// The layers of a move under way for the branch of `state`, if any.
pub fn read_next(state: &StateDir) -> Result<Option<Next>> {
    let data = match fs::read(state.dir.join(NEXT)) {
        Ok(data) => data,
        Err(e) if not_found(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let nl = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
    let onto = (data[..nl].strip_prefix(b"onto "))
        .and_then(|hex| ObjectId::from_hex(hex).ok())
        .with_context(|| format!("reading {}", state.dir.join(NEXT).display()))?;
    let layers = data.get(nl + 1..).unwrap_or_default().to_vec();
    Ok(Some(Next { onto, layers }))
}

/// Forgets the layers of a move under way (it didn't happen).
pub fn drop_next(state: &StateDir) -> Result<()> {
    match fs::remove_file(state.dir.join(NEXT)) {
        Err(e) if !not_found(&e) => Err(e.into()),
        _ => Ok(()),
    }
}

/// Whether the move `next` records got there: the branch's own ref is at
/// its `onto`.
fn arrived(state: &StateDir, next: &Next) -> Result<bool> {
    let own = git::stack::rev(&state.common_dir, &git::stack::own_ref(&state.branch))?;
    Ok(own == Some(next.onto))
}

/// Settles a move under way that changed what the branch of `state` builds
/// on: its own ref got there, so its layers are the move's (and with no
/// editable layer left, its own ref goes); or it didn't, and no workspace's
/// move of it may still finish (`state::moves`), so they are forgotten.
/// `lock` held.
pub fn settle(state: &StateDir) -> Result<()> {
    let Some(next) = read_next(state)? else {
        return Ok(());
    };
    if arrived(state, &next)? {
        store_bytes(state, &next.layers)?;
        if Layers::parse(&next.layers)?.editable().next().is_none() {
            git::stack::drop_base(&state.common_dir, &state.branch)?;
        }
        return drop_next(state);
    }
    if super::moves::pending(state) {
        return Ok(());
    }
    drop_next(state)
}

/// The layers of the workspace in `state` to show, as stored: those of a
/// move that got there but isn't settled yet, else its layers file. A cheap
/// read but while a move is under way.
pub fn shown_bytes(state: &StateDir) -> Result<Vec<u8>> {
    if let Some(next) = read_next(state)?
        && arrived(state, &next)?
    {
        return Ok(next.layers);
    }
    stored_bytes(state)
}

/// What a move of the branch of `state` shows over its new tip: the layers
/// of a move under way that changes what it builds on, else its layers.
pub fn next_or_stored(state: &StateDir) -> Result<Vec<u8>> {
    match read_next(state)? {
        Some(next) => Ok(next.layers),
        None => stored_bytes(state),
    }
}

/// `shown_bytes`, parsed, with the bytes.
pub fn to_show(state: &StateDir) -> Result<(Layers, Vec<u8>)> {
    let bytes = shown_bytes(state)?;
    Ok((Layers::parse(&bytes)?, bytes))
}

/// The content of the layers file of the workspace in `state` (empty if none).
pub fn stored_bytes(state: &StateDir) -> Result<Vec<u8>> {
    match fs::read(state.dir.join(LAYERS)) {
        Ok(data) => Ok(data),
        Err(e) if not_found(&e) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Stores `data` as the layers file of the workspace in `state` (empty: no
/// file). Also puts back what `stored_bytes` read, after the daemon refused
/// to apply a change.
pub fn store_bytes(state: &StateDir, data: &[u8]) -> Result<()> {
    let path = state.dir.join(LAYERS);
    if data.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if !not_found(&e) => Err(e.into()),
            _ => Ok(()),
        };
    }
    Ok(write_atomic(&path, data)?)
}

/// Stores `layers` for the workspace in `state`, then has it shown (`show`:
/// the daemon's switch, asked or run); says what `show` did. If `show`
/// fails, the layers stored before are put back, so what is stored is what
/// shows. Called under `lock`.
pub fn store_and_show(
    state: &StateDir,
    layers: &Layers,
    show: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let before = stored_bytes(state)?;
    layers.write(state)?;
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
    fn a_proposal_keeps_the_layer_it_stands_in_for() {
        let text = format!(
            "version 1\nlayer b-prop/c\twork\tlive\t5\t{A}\tsee\n\
             instead b\tcommits-only\tfrozen\t3\t{B}\tsee\nlayer d\tcommits\tfixed\t4\t{A}\tedit\n"
        );
        let layers = Layers::parse(text.as_bytes()).unwrap();
        let was = layers.layers[0].instead.as_deref().unwrap();
        assert_eq!(
            (was.branch.as_str(), was.shows, was.updates, was.moved),
            ("b", Shows::CommitsOnly, Updates::Frozen, 3)
        );
        assert!(layers.layers[1].instead.is_none());
        assert_eq!(layers.editable_names(), ["d"]);
        let dir = std::env::temp_dir().join(format!("gitfold-layers-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = StateDir {
            dir: dir.clone(),
            ..StateDir::of(&dir, "x")
        };
        layers.write(&state).unwrap();
        assert_eq!(fs::read_to_string(dir.join(LAYERS)).unwrap(), text);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_this_version_is_read() {
        assert!(Layers::parse(b"version 3\n").is_err());
        let five = format!("version 1\nlayer d\tcommits\tfixed\t4\t{A}\n");
        assert!(Layers::parse(five.as_bytes()).is_err(), "no see|edit field");
        let bad = format!("version 1\nlayer d\tcommits\tfixed\t4\t{A}\tmaybe\n");
        assert!(Layers::parse(bad.as_bytes()).is_err());
    }
}
