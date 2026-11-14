//! A workspace's layers: other workspaces' work shown in it, read-only, over
//! its base and under its own changes (a lens). Each layer is another
//! workspace's work as of one seal (`seal`), as the changes it makes since
//! where the two diverged (the merge base, pinned with the layer, so the view
//! doesn't change when this workspace's base moves).
//!
//! What the workspace shows is then the view tree: its base's tree with each
//! shown layer's changes on top, the higher layer winning a path (whole files;
//! deletions too), and upper over all of it as before. A path a shown layer
//! changed is mixed: it is that layer's, so it can't be written here; the
//! workspace's own file at that path, if it has one, wins and stays writable.
//!
//! Stored in `lens` in the state directory, rewritten whole: `version 1`, then
//! one line per layer, bottom first: `layer <branch> TAB <on|off> TAB <seal>
//! TAB <merge base>` (branch names hold no control characters).

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

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub branch: String,
    /// Shown (the eye icon).
    pub on: bool,
    pub seal: ObjectId,
    /// Where the two workspaces' histories diverged: the layer's changes are
    /// its seal's against this.
    pub merge_base: ObjectId,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lens {
    /// Bottom first.
    pub layers: Vec<Layer>,
}

impl Lens {
    /// The layers stored for the workspace in `state` (none if it has no file).
    pub fn read(state: &StateDir) -> Result<Lens> {
        let path = state.dir.join(LENS);
        Lens::parse(&stored_bytes(state)?).with_context(|| format!("reading {}", path.display()))
    }

    /// A layers file's content (empty: no layers).
    pub fn parse(data: &[u8]) -> Result<Lens> {
        if data.is_empty() {
            return Ok(Lens::default());
        }
        let text = std::str::from_utf8(data)?;
        let mut lines = text.lines();
        if lines.next() != Some("version 1") {
            bail!("not a layers file this gitfold knows");
        }
        let mut layers = Vec::new();
        for line in lines {
            let fields: Vec<&str> = line
                .strip_prefix("layer ")
                .unwrap_or("")
                .split('\t')
                .collect();
            let [branch, on, seal, merge_base] = fields[..] else {
                bail!("bad line {line:?}");
            };
            layers.push(Layer {
                branch: branch.to_string(),
                on: on == "on",
                seal: ObjectId::from_hex(seal.as_bytes())?,
                merge_base: ObjectId::from_hex(merge_base.as_bytes())?,
            });
        }
        Ok(Lens { layers })
    }

    /// Stores them (no file when there are none).
    pub fn write(&self, state: &StateDir) -> Result<()> {
        let mut text = String::new();
        if !self.layers.is_empty() {
            text += "version 1\n";
        }
        for l in &self.layers {
            let on = if l.on { "on" } else { "off" };
            text += &format!("layer {}\t{on}\t{}\t{}\n", l.branch, l.seal, l.merge_base);
        }
        store_bytes(state, text.as_bytes())
    }

    /// Whether any layer is shown.
    pub fn is_on(&self) -> bool {
        self.layers.iter().any(|l| l.on)
    }

    /// The layers shown, bottom first.
    pub fn shown(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter().filter(|l| l.on)
    }

    /// The subject of the lens commit of workspace `branch` showing them.
    pub fn subject(&self, branch: &str) -> String {
        let names: Vec<&str> = self.shown().map(|l| l.branch.as_str()).collect();
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

/// Every seal a workspace's layers name, stored or applied: their refs are
/// kept (`seal`).
pub fn named_seals(common_dir: &std::path::Path) -> HashSet<ObjectId> {
    every_lens(common_dir, true)
        .flat_map(|lens| lens.layers.into_iter().map(|l| l.seal))
        .collect()
}

/// What a workspace with layers shows below its own changes.
pub struct View {
    /// M: the base's tree with the shown layers' changes.
    pub tree: ObjectId,
    /// H: the base's tree with only the blended files blended (see `blend`).
    pub hunk_tree: ObjectId,
    /// The paths the layers' whole files replace or delete, with the layer
    /// (its index in `Lens::layers`) whose version shows: read-only.
    pub mixed: HashMap<Vec<u8>, usize>,
    /// The blended files (see `blend`), with the layers blended in: the
    /// workspace writes them, its own lines on top of theirs.
    pub blended: HashMap<Vec<u8>, Vec<usize>>,
    /// The seals of the layers blended in (L's parents after the base).
    pub hunk_seals: Vec<ObjectId>,
    /// What the agent should hear about it.
    pub notes: Vec<String>,
}

/// The view of `lens` over base commit `base`, or `None` if no layer is
/// shown.
///
/// A file is blended, rather than shown whole, if the attribute
/// `gitfold-blend` is `hunk` for it (in the base's `.gitattributes` or
/// `info/attributes`), the base has it and no shown layer deletes it: every
/// layer's change to it is merged into the base's version, lines both sides
/// add kept both (registries, manifests: files several agents append to).
pub fn view(common_dir: &std::path::Path, base: &ObjectId, lens: &Lens) -> Result<Option<View>> {
    if !lens.is_on() {
        return Ok(None);
    }
    let shown: Vec<usize> = (0..lens.layers.len())
        .filter(|&i| lens.layers[i].on)
        .collect();
    let stack: Vec<git::lens::Layer> = (shown.iter())
        .map(|&i| git::lens::Layer {
            seal: lens.layers[i].seal,
            merge_base: lens.layers[i].merge_base,
        })
        .collect();
    let hunk_paths = |paths: &[BString]| -> Result<HashSet<BString>> {
        let rev = base.to_string();
        let values = git::attr::values(common_dir, &rev, "gitfold-blend", paths)?;
        Ok((values.into_iter())
            .filter(|(_, value)| value == "hunk")
            .map(|(path, _)| path)
            .collect())
    };
    let view = git::with_repo(|repo| git::lens::build_view(repo, *base, &stack, &hunk_paths))?;
    let layer = |ix: usize| shown[ix];
    let name = |ix: usize| lens.layers[shown[ix]].branch.clone();
    let notes = (view.notes.iter())
        .map(|n| match n {
            git::lens::Note::NotBlended { layer, path } => format!(
                "{}: {}'s change conflicts with what is below it, so it isn't blended in",
                path,
                name(*layer)
            ),
            git::lens::Note::HidesBase { layer, path } => format!(
                "{}: {}'s version hides a change on your base since it diverged",
                path,
                name(*layer)
            ),
        })
        .collect();
    Ok(Some(View {
        tree: view.view_tree,
        hunk_tree: view.hunk_tree,
        mixed: (view.mixed.iter())
            .map(|(p, &ix)| (p.to_vec(), layer(ix)))
            .collect(),
        blended: (view.blended.iter())
            .map(|(p, ixs)| (p.to_vec(), ixs.iter().map(|&ix| layer(ix)).collect()))
            .collect(),
        hunk_seals: (view.hunk_layers().into_iter())
            .map(|ix| lens.layers[shown[ix]].seal)
            .collect(),
        notes,
    }))
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
