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

use std::collections::HashSet;
use std::fs;

use anyhow::Result;
use gix::ObjectId;

use super::StateDir;
use super::layers::{Layer, Layers, View, and_list};
use crate::git;
use crate::git::filter::{Edited, has_markers};
use crate::git::guard::{norm, significant};

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
    /// The file is deleted (it has the layers' uncommitted work: git sees
    /// the deletion, and the guard refuses committing it).
    pub deleted: bool,
}

impl Waiting {
    /// `2 waiting edits in src/x.rs: they change api's uncommitted lines;
    /// they become yours to commit once api commits them`.
    pub fn describe(&self) -> String {
        let path = String::from_utf8_lossy(&self.path);
        let who = and_list(self.layers.iter().cloned());
        let theirs = and_list(self.layers.iter().map(|l| format!("{l}'s")));
        let commits = match self.layers.len() {
            1 => "commits",
            _ => "commit",
        };
        if self.deleted {
            return format!(
                "{path} is deleted here, but it has {theirs} uncommitted work: git can't commit \
                 its deletion until {who} {commits} that work (`git checkout -- {path}` brings it \
                 back)"
            );
        }
        let (they, change, becomes) = match self.edits {
            1 => ("it", "changes", "becomes"),
            _ => ("they", "change", "become"),
        };
        format!(
            "{} in {path}: {they} {change} {theirs} uncommitted lines; {they} {becomes} yours to \
             commit once {who} {commits} them",
            crate::cli::plural(self.edits, "waiting edit", "waiting edits"),
        )
    }

    /// The conflict, in words: `src/x.rs: your changes conflict with api's
    /// newer work (...)`.
    pub fn describe_conflict(&self) -> String {
        format!(
            "{}: your changes conflict with {} newer work: it shows git's conflict markers, and \
             git sees none of your changes to it until you edit them out",
            String::from_utf8_lossy(&self.path),
            and_list(self.layers.iter().map(|l| format!("{l}'s")))
        )
    }
}

/// The waiting edits of the workspace in `state`, on base tree `base`,
/// showing `layers` as `view`: in each file editable layers changed, by
/// its upper copy, each named after the layers whose uncommitted lines it
/// changes (those it touches, by content; all of the file's if it can't
/// tell); and the deletions of such files.
pub fn of(state: &StateDir, base: ObjectId, layers: &Layers, view: &View) -> Result<Vec<Waiting>> {
    let content = |tree: ObjectId, path: &[u8]| -> Result<Vec<u8>> {
        let item = git::with_repo(|repo| git::layers::lookup(repo, tree, path.into()))?;
        item.map_or(Ok(Vec::new()), |i| git::read_blob(i.oid))
    };
    // git's own version is its branch's (`Edited::on_tip`).
    let tip = git::branch_tip(&state.branch).map_or(base, |c| c.tree);
    let mut paths: Vec<&Vec<u8>> = view.edited.iter().chain(&view.edited_new).collect();
    paths.sort();
    let (whiteouts, _) = super::markers::Markers::read(&state.markers())?;
    let mut out = Vec::new();
    for path in paths {
        let owners = view.mixed.get(path).cloned().unwrap_or_default();
        let names = |ixs: &[usize]| -> Vec<String> {
            ixs.iter()
                .map(|&i| layers.layers[i].branch.clone())
                .collect()
        };
        let file = super::upper_path(&state.upper(), path);
        if !fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
            if whiteouts.contains(path.as_slice()) {
                out.push(Waiting {
                    path: path.clone(),
                    edits: 1,
                    layers: names(&owners),
                    conflict: false,
                    deleted: true,
                });
            }
            continue;
        }
        let work = fs::read(&file)?;
        let mut shown = content(view.tree, path)?;
        let (edits, kept, touched) = match view.edited.contains(path) {
            true => {
                let edited = Edited::on_tip(content(base, path)?, shown, content(tip, path)?);
                let split = edited.split(&work);
                shown = edited.shown;
                (split.waiting, split.kept, Some(split.touched))
            }
            false => (hunks(&shown, &work), work, None),
        };
        if edits == 0 {
            continue;
        }
        // Whose lines: the owners' uncommitted lines among those touched.
        let shown_lines: Vec<&[u8]> = git::layers::lines(&shown).map(git::layers::key).collect();
        let touched: Vec<&[u8]> = (touched.into_iter().flatten())
            .filter_map(|i| shown_lines.get(i).copied())
            .filter(|l| significant(l))
            .collect();
        let mut whose = Vec::new();
        for &ix in &owners {
            let theirs = uncommitted_lines(&layers.layers[ix], path)?;
            if touched.iter().any(|l| theirs.contains(&norm(l))) {
                whose.push(ix);
            }
        }
        if whose.is_empty() {
            whose = owners;
        }
        out.push(Waiting {
            path: path.clone(),
            edits,
            layers: names(&whose),
            conflict: has_markers(&kept) && !has_markers(&shown),
            deleted: false,
        });
    }
    Ok(out)
}

/// The lines (compared as the guard does) `layer`'s snapshot has at `path`
/// that the commit it is on hasn't: its uncommitted lines there.
fn uncommitted_lines(layer: &Layer, path: &[u8]) -> Result<HashSet<Vec<u8>>> {
    git::with_repo(|repo| -> Result<HashSet<Vec<u8>>> {
        let commit = repo.find_commit(layer.snapshot)?;
        let at = |tree: ObjectId| -> Result<Vec<u8>> {
            match git::layers::lookup(repo, tree, path.into())? {
                Some(item) => Ok(repo.find_blob(item.oid)?.take_data()),
                None => Ok(Vec::new()),
            }
        };
        let snapshot = at(commit.tree_id()?.detach())?;
        let committed = match commit.parent_ids().next() {
            Some(p) => at(git::layers::commit_tree(repo, p.detach())?)?,
            None => Vec::new(),
        };
        let had: HashSet<Vec<u8>> = (git::layers::lines(&committed))
            .map(|l| norm(git::layers::key(l)))
            .collect();
        Ok((git::layers::lines(&snapshot))
            .map(|l| norm(git::layers::key(l)))
            .filter(|l| !had.contains(l))
            .collect())
    })
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
