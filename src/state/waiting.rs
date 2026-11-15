//! Waiting edits: a workspace's changes to an editable layer's uncommitted
//! lines. Its branch doesn't have those lines yet, so it can't commit
//! changes to them: they are kept in the file, which shows them on top of
//! the layer's lines, and out of git, whose filter strips them with the
//! layer's lines (`git::filter::Edited`). Once the layer commits those
//! lines and the branch moves onto that commit, the file is no longer the
//! layer's, and they are ordinary uncommitted changes. A switch carries
//! them onto the layer's newer work (a 3-way merge; a conflict keeps git's
//! markers, listed). A file only the layers have (they added it) is hidden
//! from git until a layer's commit brings it: every change to it waits.
//!
//! They are stored with the workspace's own files (upper, where every write
//! lands durably) and, in its snapshots, in the record of what it saw and
//! made (`git::layers::Record`). Worked out here from those, as the
//! listing, the turn's hook and the commands that take a layer away need
//! them.

use std::fs;

use anyhow::Result;
use gix::ObjectId;

use super::StateDir;
use super::layers::{Layers, View, and_list};
use crate::git;
use crate::git::filter::{Edited, has_markers};

/// The waiting edits in one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    pub path: Vec<u8>,
    /// How many (hunks).
    pub edits: usize,
    /// The editable layers whose lines they change.
    pub layers: Vec<String>,
    /// They conflict with the layers' newer work: the file shows git's
    /// conflict markers.
    pub conflict: bool,
}

impl Waiting {
    /// `2 waiting edits in src/x.rs: they change api's uncommitted lines;
    /// they become yours to commit once api commits them`.
    pub fn describe(&self) -> String {
        let path = String::from_utf8_lossy(&self.path);
        let who = and_list(self.layers.iter().cloned());
        let theirs = and_list(self.layers.iter().map(|l| format!("{l}'s")));
        let (they, change, becomes) = match self.edits {
            1 => ("it", "changes", "becomes"),
            _ => ("they", "change", "become"),
        };
        format!(
            "{} in {path}: {they} {change} {theirs} uncommitted lines; {they} {becomes} yours to \
             commit once {who} {} them",
            crate::cli::plural(self.edits, "waiting edit", "waiting edits"),
            if self.layers.len() == 1 {
                "commits"
            } else {
                "commit"
            }
        )
    }

    /// The conflict, in words: `src/x.rs: your waiting edits conflict with
    /// api's newer work (it shows git's conflict markers)`.
    pub fn describe_conflict(&self) -> String {
        format!(
            "{}: your waiting edits conflict with {} newer work (it shows git's conflict \
             markers; edit them out)",
            String::from_utf8_lossy(&self.path),
            and_list(self.layers.iter().map(|l| format!("{l}'s")))
        )
    }
}

/// The waiting edits of the workspace in `state`, on base tree `base`,
/// showing `layers` as `view`: in each file editable layers changed, by
/// its upper copy.
pub fn of(state: &StateDir, base: ObjectId, layers: &Layers, view: &View) -> Result<Vec<Waiting>> {
    let content = |tree: ObjectId, path: &[u8]| -> Result<Vec<u8>> {
        let item = git::with_repo(|repo| git::layers::lookup(repo, tree, path.into()))?;
        item.map_or(Ok(Vec::new()), |i| git::read_blob(i.oid))
    };
    let mut paths: Vec<&Vec<u8>> = view.edited.iter().chain(&view.edited_new).collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let file = super::upper_path(&state.upper(), path);
        if !fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let work = fs::read(&file)?;
        let shown = content(view.tree, path)?;
        let (edits, kept) = match view.edited.contains(path) {
            true => {
                let edited = Edited {
                    base: content(base, path)?,
                    shown: shown.clone(),
                };
                let split = edited.split(&work);
                (split.waiting, split.kept)
            }
            false => (hunks(&shown, &work), work),
        };
        if edits == 0 {
            continue;
        }
        let owners = view.mixed.get(path).cloned().unwrap_or_default();
        out.push(Waiting {
            path: path.clone(),
            edits,
            layers: (owners.iter())
                .map(|&i| layers.layers[i].branch.clone())
                .collect(),
            conflict: has_markers(&kept) && !has_markers(&shown),
        });
    }
    Ok(out)
}

/// The waiting edits of the workspace in `state` as it shows its layers
/// now (those applied); none if it isn't open or shows none.
pub fn of_workspace(state: &StateDir) -> Result<Vec<Waiting>> {
    if !state.opened() {
        return Ok(Vec::new());
    }
    let layers = super::layers::ShownLayers::read(state)?.layers()?;
    if layers.editable().next().is_none() {
        return Ok(Vec::new());
    }
    git::init(&state.common_dir)?;
    let base = state.read_base()?;
    match super::layers::view(&state.common_dir, &base.id, &layers)? {
        Some(view) => of(state, base.tree, &layers, &view),
        None => Ok(Vec::new()),
    }
}

/// How many hunks turn `from` into `to` (at least one if they differ).
fn hunks(from: &[u8], to: &[u8]) -> usize {
    use imara_diff::{Algorithm, Diff, InternedInput, sources::byte_lines};
    if from == to {
        return 0;
    }
    let input = InternedInput::new(byte_lines(from), byte_lines(to));
    Diff::compute(Algorithm::Histogram, &input)
        .hunks()
        .count()
        .max(1)
}
