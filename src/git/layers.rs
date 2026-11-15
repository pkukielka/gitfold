//! Layers on the git side: the trees that mix other workspaces' work into a
//! workspace's view, and the view commit that keeps them. Pure functions
//! over objects in a repository; nothing here moves a ref. How git sees a
//! union file is `git::filter`'s.
//!
//! Terms (the layers design, §1.1):
//! - B: my base, the branch tip.
//! - A layer: another workspace's work as of one snapshot S (a commit whose tree
//!   is that workspace's view), with the merge base MB it was pinned at. Its
//!   delta is `diff(MB.tree, S.tree)`.
//! - H, the hunk tree: B with every hunk path (a registry, a manifest, as the
//!   caller says: those marked `merge=union`) union across the layers.
//! - M, the view tree: H with each layer's change to every other path
//!   ("whole-file" paths) applied in turn, as a 3-way merge `merge(base =
//!   MB[p], ours = what p is so far, starting from B[p], theirs = S[p])`, so
//!   a layer brings only its own changes, not its stale copy of lines B
//!   changed since; conflict markers kept (or B's entry if a change can't be
//!   merged at all: binary, a deletion against a change, a type change).
//! - V, the view commit: tree M, parents `[B, S...]`; one ref on it keeps M
//!   and the snapshots from `git gc`.
//!
//! Union merging a hunk path folds each layer's change into the file in turn,
//! `merge3(base = MB[p], ours = acc[p], theirs = S[p])`. Two appends at the
//! same place (two agents adding to a registry) conflict in every 3-way
//! merge, so when both sides only add lines the conflict is resolved by
//! keeping both, ours first (`merge=union`, but only there). Any other
//! conflict drops that layer's change to p and is reported. A snapshot whose
//! author added lines among other layers' lines in p records what it saw
//! and made (`Record`): its change is then folded in from what it saw, so
//! its lines keep their place among the others'; while those aren't there
//! (a layer after it brings them, or none does), it waits, and is placed
//! as above at last (`Note::PlacedByBase`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::merge::blob::Resolution;
use gix::merge::blob::builtin_driver::{self, text};
use gix::objs::tree::EntryKind;

/// A tree entry, as far as layers care: what it is and its object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub kind: EntryKind,
    pub oid: ObjectId,
}

/// What a delta does to one path: sets it (adds or replaces) or deletes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Set(Item),
    Delete,
}

impl Change {
    /// What it sets (`None`: a deletion).
    pub fn item(self) -> Option<Item> {
        match self {
            Change::Set(item) => Some(item),
            Change::Delete => None,
        }
    }

    fn from_item(item: Option<Item>) -> Self {
        item.map_or(Change::Delete, Change::Set)
    }
}

/// Writes the tree `base` with `changes` applied in order, touching only the
/// trees along their paths. Directories left empty disappear; setting a path
/// below a file replaces the file with a directory.
pub fn edit_tree<'a>(
    repo: &gix::Repository,
    base: ObjectId,
    changes: impl IntoIterator<Item = (&'a BStr, Change)>,
) -> Result<ObjectId> {
    let mut editor = repo.edit_tree(base)?;
    for (path, change) in changes {
        match change {
            Change::Set(item) => editor.upsert(path, item.kind, item.oid)?,
            Change::Delete => editor.remove(path)?,
        };
    }
    Ok(editor.write()?.detach())
}

/// The changes that turn tree `from` into tree `to`, for files, symlinks and
/// submodules only (a directory is the files in it), in path order. Walks
/// only the subtrees whose ids differ. A path that changes between file and
/// directory is a `Delete` of what was there, then the `Set`s of what is.
pub fn delta(
    repo: &gix::Repository,
    from: ObjectId,
    to: ObjectId,
) -> Result<Vec<(BString, Change)>> {
    let mut out = Vec::new();
    walk(
        repo,
        &mut BString::default(),
        Some(from),
        Some(to),
        &mut out,
    )?;
    Ok(out)
}

fn walk(
    repo: &gix::Repository,
    prefix: &mut BString,
    from: Option<ObjectId>,
    to: Option<ObjectId>,
    out: &mut Vec<(BString, Change)>,
) -> Result<()> {
    if from == to {
        return Ok(());
    }
    let from = level(repo, from)?;
    let to = level(repo, to)?;
    let names: BTreeSet<&BString> = from.keys().chain(to.keys()).collect();
    for name in names {
        let len = prefix.len();
        if len > 0 {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(name);
        let (a, b) = (from.get(name).copied(), to.get(name).copied());
        if a != b {
            let tree = |i: Option<Item>| i.filter(|i| i.kind == EntryKind::Tree).map(|i| i.oid);
            let leaf = |i: Option<Item>| i.filter(|i| i.kind != EntryKind::Tree);
            match (tree(a), tree(b)) {
                (Some(_), Some(_)) => walk(repo, prefix, tree(a), tree(b), out)?,
                _ => {
                    if a.is_some() && b.is_some() && leaf(a).is_some() != leaf(b).is_some() {
                        // File <-> directory: what was there goes first.
                        if leaf(a).is_some() {
                            out.push((prefix.clone(), Change::Delete));
                        } else {
                            walk(repo, prefix, tree(a), None, out)?;
                        }
                        match leaf(b) {
                            Some(item) => out.push((prefix.clone(), Change::Set(item))),
                            None => walk(repo, prefix, None, tree(b), out)?,
                        }
                    } else if tree(a).is_some() || tree(b).is_some() {
                        walk(repo, prefix, tree(a), tree(b), out)?;
                    } else {
                        out.push((prefix.clone(), Change::from_item(b)));
                    }
                }
            }
        }
        prefix.truncate(len);
    }
    Ok(())
}

/// One tree level by name; empty for `None`.
fn level(repo: &gix::Repository, tree: Option<ObjectId>) -> Result<BTreeMap<BString, Item>> {
    let Some(tree) = tree else {
        return Ok(BTreeMap::new());
    };
    let tree = repo.find_tree(tree)?;
    let decoded = tree.decode()?;
    Ok(decoded
        .entries
        .iter()
        .map(|e| {
            let item = Item {
                kind: e.mode.kind(),
                oid: e.oid.to_owned(),
            };
            (e.filename.to_owned(), item)
        })
        .collect())
}

/// The entry at `path` (`/`-separated) in tree `tree`, if any.
pub fn lookup(repo: &gix::Repository, tree: ObjectId, path: &BStr) -> Result<Option<Item>> {
    let tree = repo.find_tree(tree)?;
    let entry = tree.lookup_entry(path.split(|b| *b == b'/'))?;
    Ok(entry.map(|e| Item {
        kind: e.mode().kind(),
        oid: e.object_id(),
    }))
}

/// The tree of `commit`.
pub fn commit_tree(repo: &gix::Repository, commit: ObjectId) -> Result<ObjectId> {
    let commit = repo
        .find_commit(commit)
        .with_context(|| format!("commit {commit}"))?;
    Ok(commit.tree_id()?.detach())
}

/// One layer of a stack: a snapshot and the merge base it is pinned to, and the
/// name conflict markers give it.
#[derive(Clone, Debug)]
pub struct Pin {
    pub name: String,
    pub snapshot: ObjectId,
    pub merge_base: ObjectId,
}

/// What `build_view` made.
#[derive(Debug)]
pub struct Built {
    /// H: B's tree with the hunk paths union-merged (B's tree if none is).
    pub hunk_tree: ObjectId,
    /// M: H with the whole-file paths of the layers.
    pub view_tree: ObjectId,
    /// Whole-file paths where M differs from H (or a layer's change doesn't
    /// merge), with the layers (indexes into the stack) that change them.
    pub mixed: BTreeMap<BString, Vec<usize>>,
    /// Hunk paths where H differs from B, with the layers whose changes are
    /// union-merged in.
    pub union: BTreeMap<BString, Vec<usize>>,
    /// What the switch should tell the agent.
    pub notes: Vec<Note>,
}

/// Something about a view the agent should hear.
#[derive(Debug, PartialEq, Eq)]
pub enum Note {
    /// The layer's change to this hunk path conflicts with what is below it
    /// (B or the layers under it), so it is not union-merged in.
    NotInUnion { layer: usize, path: BString },
    /// The layer's snapshot records where its lines in this hunk path sat
    /// among the lines its author saw (`Record`), but those lines aren't
    /// here (that layer isn't shown, or changed them since): its change is
    /// placed by the base's lines instead, as without a record.
    PlacedByBase { layer: usize, path: BString },
    /// The layer's added lines in this hunk path sit between lines its
    /// author saw from another branch, next to none of the base's: the file
    /// may not be a list. `others`: the layers here whose lines those are
    /// (none: a branch not shown here).
    Among {
        layer: usize,
        path: BString,
        others: Vec<usize>,
    },
    /// A layer's change to this whole-file path conflicts: with B's change
    /// since the layer's merge base (`base`; `layers` is that one layer), or
    /// with other layers' changes (`layers` are those layers). M has the
    /// merge with git's conflict markers (`markers`), or, where that can't
    /// be (binary, a deletion against a change, a type change), B's entry.
    Conflict {
        layers: Vec<usize>,
        base: bool,
        path: BString,
        markers: bool,
    },
}

/// How layers' changes to a whole-file path merged.
enum Merged {
    Clean(Option<Item>),
    /// With conflict markers.
    Markers(Item),
    /// Not at all (see `Note::Conflict`).
    Unmergeable,
}

/// Builds the view of base commit `base` with `layers` (in the order
/// added), union merging hunk paths and, for the others, applying the change of
/// each layer that changes them in turn, starting from B's entry: the 3-way
/// merge of the layer's merge base's entry, what is there so far and the
/// layer's, so it brings only its own changes; conflicts reported. The
/// paths a layer sets that B has and no layer deletes (a union needs B's
/// version, and a deletion is whole) are given to `hunk_paths`, once, which
/// returns those that are hunk paths. Conflict markers name B `below`, as
/// users know it (`main`, or what the branch builds on), then each layer.
pub fn build_view(
    repo: &gix::Repository,
    base: ObjectId,
    below: &str,
    layers: &[Pin],
    hunk_paths: &dyn Fn(&[BString]) -> Result<HashSet<BString>>,
) -> Result<Built> {
    let base_tree = commit_tree(repo, base)?;
    let mut layer_bases = Vec::with_capacity(layers.len());
    let mut deltas = Vec::with_capacity(layers.len());
    for layer in layers {
        let mb = commit_tree(repo, layer.merge_base)?;
        deltas.push(delta(repo, mb, commit_tree(repo, layer.snapshot)?)?);
        layer_bases.push(mb);
    }
    let mut candidates = BTreeSet::new();
    let mut deleted = HashSet::new();
    for (path, change) in deltas.iter().flatten() {
        match change {
            Change::Delete => {
                deleted.insert(path);
            }
            Change::Set(_) => {
                if lookup(repo, base_tree, path.as_ref())?.is_some() {
                    candidates.insert(path.clone());
                }
            }
        }
    }
    candidates.retain(|p| !deleted.contains(p));
    let is_hunk = match candidates.is_empty() {
        true => HashSet::new(),
        false => hunk_paths(&candidates.into_iter().collect::<Vec<_>>())?,
    };
    // What each layer's author saw in its union files, if recorded.
    let records = match is_hunk.is_empty() {
        true => Vec::new(),
        false => (layers.iter())
            .map(|l| record_of(repo, l.snapshot))
            .collect::<Result<Vec<_>>>()?,
    };
    let mut notes = Vec::new();
    let mut hunks: BTreeMap<BString, Option<Item>> = BTreeMap::new();
    let mut union: BTreeMap<BString, Vec<usize>> = BTreeMap::new();
    // Whole-file changes by path, in layer order; the paths in the order
    // first changed (a file's deletion before the files of the directory
    // replacing it).
    let mut whole: HashMap<BString, Vec<(usize, Change)>> = HashMap::new();
    let mut order: Vec<BString> = Vec::new();
    // Hunk paths' changes by path, in layer order.
    let mut hunk_changes: BTreeMap<BString, Vec<(usize, Change)>> = BTreeMap::new();
    for (ix, delta) in deltas.into_iter().enumerate() {
        for (path, change) in delta {
            if is_hunk.contains(&path) {
                hunk_changes.entry(path).or_default().push((ix, change));
                continue;
            }
            let changes = whole.entry(path.clone()).or_default();
            if changes.is_empty() {
                order.push(path);
            }
            changes.push((ix, change));
        }
    }
    for (path, mut pending) in hunk_changes {
        let mut ours = lookup(repo, base_tree, path.as_ref())?;
        let mut folded = Vec::new();
        // A recorded change whose context isn't there yet waits for the
        // others (the lines it sat among may come with a later layer);
        // once a round folds none in, those waiting are placed by the
        // base's lines.
        let mut by_base = false;
        while !pending.is_empty() {
            let round = pending.len();
            let mut waiting = Vec::new();
            for (ix, change) in std::mem::take(&mut pending) {
                let ancestor = lookup(repo, layer_bases[ix], path.as_ref())?;
                let record = records.get(ix).and_then(Option::as_ref);
                let in_context = match record {
                    Some(record) if !by_base => {
                        union_recorded(repo, record, ancestor, ours, path.as_ref())?
                    }
                    _ => None,
                };
                let merged = match in_context {
                    Some(None) => {
                        waiting.push((ix, change));
                        continue;
                    }
                    Some(Some(merged)) => Some(merged),
                    // No record of this path (an older snapshot), or what
                    // its lines sat among isn't here.
                    None => {
                        let merged = union_merge(repo, ancestor, ours, change.item())?;
                        if by_base && merged.is_some() {
                            let path = path.clone();
                            notes.push(Note::PlacedByBase { layer: ix, path });
                        }
                        merged
                    }
                };
                match merged {
                    Some(merged) => {
                        ours = merged;
                        folded.push(ix);
                    }
                    None => {
                        let path = path.clone();
                        notes.push(Note::NotInUnion { layer: ix, path });
                    }
                }
            }
            by_base = waiting.len() == round;
            pending = waiting;
        }
        for &ix in &folded {
            if let Some(record) = records.get(ix).and_then(Option::as_ref)
                && let Some(around) = among_others_in(repo, record, path.as_ref())?
            {
                let others = whose_lines(repo, layers, &layer_bases, ix, path.as_ref(), &around)?;
                let path = path.clone();
                notes.push(Note::Among {
                    layer: ix,
                    path,
                    others,
                });
            }
        }
        if !folded.is_empty() {
            folded.sort_unstable();
            hunks.insert(path.clone(), ours);
            union.insert(path, folded);
        }
    }
    let hunk_tree = edit_tree(
        repo,
        base_tree,
        hunks
            .iter()
            .map(|(p, item)| (p.as_ref(), Change::from_item(*item))),
    )?;
    // A union can come back to B's file (a layer reverting another's line).
    let mut unchanged = Vec::new();
    for (path, item) in &hunks {
        if lookup(repo, base_tree, path.as_ref())? == *item {
            unchanged.push(path.clone());
        }
    }
    for path in unchanged {
        union.remove(&path);
    }
    let mut edits = Vec::with_capacity(order.len());
    let mut mixed = BTreeMap::new();
    // What each whole-file path shows, as decided.
    let mut shown: HashMap<&BStr, Option<Item>> = HashMap::new();
    for path in &order {
        let changes = &whole[path];
        let ixs: Vec<usize> = changes.iter().map(|(ix, _)| *ix).collect();
        let in_base = lookup(repo, base_tree, path.as_ref())?;
        let mut sides = Vec::with_capacity(changes.len());
        for &(ix, change) in changes {
            sides.push(Side {
                layer: ix,
                name: &layers[ix].name,
                ancestor: lookup(repo, layer_bases[ix], path.as_ref())?,
                theirs: change.item(),
            });
        }
        let (merged, conflicted) = apply_layers(repo, in_base, below, &sides)?;
        let mut found = Vec::new();
        for &step in &conflicted {
            let one = conflicting(repo, in_base, &sides, step)?;
            if !found.contains(&one) {
                found.push(one);
            }
        }
        let (mut entry, markers) = match merged {
            Merged::Clean(item) => (item, true),
            Merged::Markers(item) => (Some(item), true),
            Merged::Unmergeable => (in_base, false),
        };
        // A new path below a file that stays (the base's, or one whose
        // layers' changes couldn't be merged) can't show without hiding it.
        if entry.is_some()
            && in_base.is_none()
            && let Some(file) = file_above(repo, base_tree, &shown, path.as_ref())?
        {
            entry = None;
            if !shown.contains_key(file.as_bstr()) {
                found = vec![(ixs.clone(), true)];
            }
        }
        shown.insert(path.as_ref(), entry);
        if entry != in_base {
            edits.push((path.as_ref(), Change::from_item(entry)));
        } else if found.is_empty() {
            // Its layers' changes are all on the base already.
            continue;
        }
        for (layers, base) in found {
            notes.push(Note::Conflict {
                layers,
                base,
                path: path.clone(),
                markers: markers && entry != in_base,
            });
        }
        // A directory the base made of it stays the base's.
        if !entry.is_some_and(|e| e.kind == EntryKind::Tree) {
            mixed.insert(path.clone(), ixs);
        }
    }
    let view_tree = edit_tree(repo, hunk_tree, edits)?;
    Ok(Built {
        hunk_tree,
        view_tree,
        mixed,
        union,
        notes,
    })
}

/// One layer's change to a whole-file path: its entry at the layer's merge
/// base and at its snapshot.
#[derive(Clone, Copy)]
struct Side<'a> {
    layer: usize,
    name: &'a str,
    ancestor: Option<Item>,
    theirs: Option<Item>,
}

/// Applies the layers' changes `sides` (in order) to one whole-file path,
/// starting from `base` (the base's entry): each the 3-way merge of its
/// merge base's entry, what is there so far and its own, as `git
/// merge-file` does. With the steps (indexes into `sides`) that conflicted;
/// stops at one that can't be merged at all.
fn apply_layers(
    repo: &gix::Repository,
    base: Option<Item>,
    below: &str,
    sides: &[Side],
) -> Result<(Merged, Vec<usize>)> {
    let (mut acc, mut label) = (base, below.to_string());
    let mut markers = false;
    let mut conflicted = Vec::new();
    for (step, side) in sides.iter().enumerate() {
        match merge_entries(repo, side.ancestor, acc, side.theirs, &label, side.name)? {
            Merged::Clean(item) => acc = item,
            Merged::Markers(item) => {
                (acc, markers) = (Some(item), true);
                conflicted.push(step);
            }
            Merged::Unmergeable => {
                conflicted.push(step);
                return Ok((Merged::Unmergeable, conflicted));
            }
        }
        label = format!("{label} + {}", side.name);
    }
    let merged = match (markers, acc) {
        (true, Some(item)) => Merged::Markers(item),
        _ => Merged::Clean(acc),
    };
    Ok((merged, conflicted))
}

/// Who the change of `sides[step]`, applied after those before it
/// (`apply_layers`), conflicts with: `(its layer, true)`, the base, if it
/// alone conflicts with the base's entry `base`; else `(layers, false)`,
/// it and each layer before it whose change alone conflicts with it (all
/// before it if none alone does).
fn conflicting(
    repo: &gix::Repository,
    base: Option<Item>,
    sides: &[Side],
    step: usize,
) -> Result<(Vec<usize>, bool)> {
    let side = sides[step];
    if !apply_layers(repo, base, "", &[side])?.1.is_empty() {
        return Ok((vec![side.layer], true));
    }
    let mut with = Vec::new();
    for &before in &sides[..step] {
        if apply_layers(repo, base, "", &[before, side])?
            .1
            .contains(&1)
        {
            with.push(before.layer);
        }
    }
    if with.is_empty() {
        with = sides[..step].iter().map(|s| s.layer).collect();
    }
    with.push(side.layer);
    Ok((with, false))
}

/// The file (or symlink, submodule) above `path`, as shown so far (`shown`,
/// else as in tree `tree`), if there is one.
fn file_above(
    repo: &gix::Repository,
    tree: ObjectId,
    shown: &HashMap<&BStr, Option<Item>>,
    path: &BStr,
) -> Result<Option<BString>> {
    let mut at = 0;
    while let Some(slash) = path[at..].find_byte(b'/') {
        let above = path[..at + slash].as_bstr();
        let item = match shown.get(above) {
            Some(item) => *item,
            None => lookup(repo, tree, above)?,
        };
        match item {
            None => return Ok(None),
            Some(item) if item.kind != EntryKind::Tree => return Ok(Some(above.into())),
            Some(_) => at += slash + 1,
        }
    }
    Ok(None)
}

/// The 3-way merge of a path's entries `ours` and `theirs`, each changed
/// from `ancestor`, as `git merge-file` does: conflict markers named
/// `ours_label` and `theirs_label`.
fn merge_entries(
    repo: &gix::Repository,
    ancestor: Option<Item>,
    ours: Option<Item>,
    theirs: Option<Item>,
    ours_label: &str,
    theirs_label: &str,
) -> Result<Merged> {
    if let Some(merged) = trivial_merge(ancestor, ours, theirs) {
        return Ok(Merged::Clean(merged));
    }
    let (Some(ours), Some(theirs)) = (ours, theirs) else {
        // Deleted on one side, changed on the other.
        return Ok(Merged::Unmergeable);
    };
    let Some(kind) = merged_kind(ancestor, ours, theirs) else {
        return Ok(Merged::Unmergeable);
    };
    let ancestor_data = read_blob(repo, ancestor)?;
    let (ours_data, theirs_data) = (read_blob(repo, Some(ours))?, read_blob(repo, Some(theirs))?);
    if [&ancestor_data, &ours_data, &theirs_data]
        .iter()
        .any(|d| is_binary(d))
    {
        return Ok(Merged::Unmergeable);
    }
    let labels = text::Labels {
        ancestor: Some("before".into()),
        current: Some(ours_label.into()),
        other: Some(theirs_label.into()),
    };
    let mut input = gix::diff::blob::InternedInput::default();
    let mut out = Vec::new();
    let options = text::Options {
        conflict: keep_conflicts(),
        ..Default::default()
    };
    let resolution = builtin_driver::text(
        &mut out,
        &mut input,
        labels,
        &ours_data,
        &ancestor_data,
        &theirs_data,
        options,
    );
    let item = Item {
        kind,
        oid: repo.write_blob(&out)?.detach(),
    };
    Ok(match resolution {
        Resolution::Conflict => Merged::Markers(item),
        Resolution::Complete | Resolution::CompleteWithAutoResolvedConflict => {
            Merged::Clean(Some(item))
        }
    })
}

/// Whether entry `ours` has the change from `ancestor` to `theirs` already:
/// merging it in leaves `ours` as it is (`ours` is `theirs`, or changed it
/// further on other lines).
pub fn has_change(
    repo: &gix::Repository,
    ancestor: Option<Item>,
    ours: Option<Item>,
    theirs: Option<Item>,
) -> Result<bool> {
    let merged = merge_entries(repo, ancestor, ours, theirs, "", "")?;
    Ok(matches!(merged, Merged::Clean(item) if item == ours))
}

/// Folds `theirs` (a layer's entry, changed from `ancestor`) into `ours`:
/// the merged entry (`None`: deleted), or `None` on a conflict.
fn union_merge(
    repo: &gix::Repository,
    ancestor: Option<Item>,
    ours: Option<Item>,
    theirs: Option<Item>,
) -> Result<Option<Option<Item>>> {
    if let Some(trivial) = trivial_merge(ancestor, ours, theirs) {
        return Ok(Some(trivial));
    }
    let (Some(ours), Some(theirs)) = (ours, theirs) else {
        // Deleted on one side, changed on the other.
        return Ok(None);
    };
    let Some(kind) = merged_kind(ancestor, ours, theirs) else {
        return Ok(None);
    };
    let base = read_blob(repo, ancestor)?;
    let (ours_data, theirs_data) = (read_blob(repo, Some(ours))?, read_blob(repo, Some(theirs))?);
    if [&base, &ours_data, &theirs_data]
        .iter()
        .any(|d| is_binary(d))
    {
        return Ok(None);
    }
    let merged = match merge3(&base, &ours_data, &theirs_data, keep_conflicts()) {
        Some(merged) => merged,
        None if only_adds(&base, &ours_data) && only_adds(&base, &theirs_data) => {
            match merge3(
                &base,
                &ours_data,
                &theirs_data,
                text::Conflict::ResolveWithUnion,
            ) {
                Some(merged) => merged,
                None => return Ok(None),
            }
        }
        None => return Ok(None),
    };
    let oid = repo.write_blob(&merged)?.detach();
    Ok(Some(Some(Item { kind, oid })))
}

/// The extra header that marks a snapshot's record of its union files
/// (`Record`); its value is the commit the author's view was built on.
pub const SEEN_HEADER: &str = "gitfold-seen";

/// What the author of a snapshot saw in its union files and made of them:
/// for each union file it added lines to, the union it saw (the base's
/// version with the other layers' lines) and its own copy (its lines among
/// them). The snapshot's tree holds only its own lines, against its base, so
/// layers bring their own work, never another's through them; the record
/// keeps where those lines sat among the others'.
///
/// Kept as the snapshot's second parent: a commit `made` (tree: the files as
/// the author left them; header `gitfold-seen <base>`) whose only parent
/// is a root commit `seen` (tree: the same files as the author saw them),
/// so `git diff <snapshot>^2^ <snapshot>^2` is the author's change in context. Both
/// are made with a fixed identity and date (the same record is the same
/// commits, and an unchanged workspace's snapshot stays the same), and the
/// root commit's message names the branch, base and snapshot tree, so two
/// snapshots never share it. No branch's commit descends from them: they never
/// become a merge base, and `merge-base(view base, snapshot)` is as without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// The commit the author's view was built on (its base).
    pub base: ObjectId,
    /// The tree of the files as the author saw them.
    pub seen: ObjectId,
    /// The tree of the same files as the author left them.
    pub made: ObjectId,
}

/// Writes the record of a snapshot of `branch` on `base` whose tree is
/// `tree`: trees `seen` and `made` (see `Record`). Returns the commit to
/// make the snapshot's second parent.
pub fn write_record(
    repo: &gix::Repository,
    branch: &str,
    base: ObjectId,
    tree: ObjectId,
    seen: ObjectId,
    made: ObjectId,
) -> Result<ObjectId> {
    let me = gix::actor::Signature {
        name: "gitfold".into(),
        email: "gitfold@localhost".into(),
        // 2020-01-01T00:00:00Z, as snapshots (`state::snapshot::IDENTITY`).
        time: gix::date::Time {
            seconds: 1_577_836_800,
            offset: 0,
        },
    };
    let commit = |tree, parents, message: String, extra_headers| gix::objs::Commit {
        tree,
        parents,
        author: me.clone(),
        committer: me.clone(),
        encoding: None,
        message: message.into(),
        extra_headers,
    };
    let root = commit(
        seen,
        Default::default(),
        format!(
            "gitfold snapshot: the union files {branch} saw\n\nOn {base}, for its snapshot \
             of tree {tree}.\n"
        ),
        Vec::new(),
    );
    let root = repo.write_object(&root)?.detach();
    let header = (SEEN_HEADER.into(), base.to_string().into());
    let made = commit(
        made,
        std::iter::once(root).collect(),
        format!("gitfold snapshot: {branch}'s lines in them, where it put them\n"),
        vec![header],
    );
    Ok(repo.write_object(&made)?.detach())
}

/// The record of snapshot `snapshot` (see `Record`), if it has one: older snapshots,
/// branches' commits and revisions have none.
pub fn record_of(repo: &gix::Repository, snapshot: ObjectId) -> Result<Option<Record>> {
    let commit = repo.find_commit(snapshot)?;
    let parents: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
    let [_, made] = parents[..] else {
        return Ok(None);
    };
    let made = repo.find_commit(made)?;
    let decoded = made.decode()?;
    let Some(base) = decoded.extra_headers().find(SEEN_HEADER) else {
        return Ok(None);
    };
    let Ok(base) = ObjectId::from_hex(base) else {
        return Ok(None);
    };
    let Some(seen) = decoded.parents().next() else {
        return Ok(None);
    };
    Ok(Some(Record {
        base,
        seen: commit_tree(repo, seen)?,
        made: decoded.tree(),
    }))
}

/// Folds a layer's change to hunk path `path` into `ours` by its record:
/// first its commits up to its author's base (from `ancestor`, its merge
/// base's entry), then its author's change from what it saw to what it
/// made, so its lines keep their place among other layers' lines.
/// `None` if the record doesn't have the path; `Some(None)` if it doesn't
/// merge (what its lines sat among isn't in `ours`).
fn union_recorded(
    repo: &gix::Repository,
    record: &Record,
    ancestor: Option<Item>,
    ours: Option<Item>,
    path: &BStr,
) -> Result<Option<Option<Option<Item>>>> {
    let (Some(seen), Some(made)) = (
        lookup(repo, record.seen, path)?,
        lookup(repo, record.made, path)?,
    ) else {
        return Ok(None);
    };
    let at_base = lookup(repo, commit_tree(repo, record.base)?, path)?;
    let Some(on_base) = union_merge(repo, ancestor, ours, at_base)? else {
        return Ok(Some(None));
    };
    Ok(Some(union_merge(repo, Some(seen), on_base, Some(made))?))
}

/// The lines the author's added lines in hunk path `path` sit between, if
/// any sit between two lines of another branch's (`among_others`), by
/// `record`.
fn among_others_in(
    repo: &gix::Repository,
    record: &Record,
    path: &BStr,
) -> Result<Option<Vec<Vec<u8>>>> {
    let (Some(seen), Some(made)) = (
        lookup(repo, record.seen, path)?,
        lookup(repo, record.made, path)?,
    ) else {
        return Ok(None);
    };
    let at_base = lookup(repo, commit_tree(repo, record.base)?, path)?;
    let (base, seen, made) = (
        read_blob(repo, at_base)?,
        read_blob(repo, Some(seen))?,
        read_blob(repo, Some(made))?,
    );
    let around = among_others(&base, &seen, &made);
    Ok((!around.is_empty()).then(|| around.iter().map(|l| l.to_vec()).collect()))
}

/// The lines that lines `made` added to `seen` sit between, where both
/// lines around an addition are lines `seen` has and `base` doesn't
/// (another branch's), next to none of the base's: a sign the file isn't a
/// list (a method added inside another branch's class). Empty if none
/// does, or for anything but text.
pub(crate) fn among_others<'a>(base: &[u8], seen: &'a [u8], made: &[u8]) -> Vec<&'a [u8]> {
    use imara_diff::{Algorithm, Diff, InternedInput, sources::byte_lines};
    if [base, seen, made].iter().any(|d| is_binary(d)) {
        return Vec::new();
    }
    let seen_lines: Vec<&[u8]> = lines(seen).collect();
    let mut others = vec![false; seen_lines.len()];
    let input = InternedInput::new(byte_lines(base), byte_lines(seen));
    for hunk in Diff::compute(Algorithm::Histogram, &input).hunks() {
        for i in hunk.after {
            others[i as usize] = true;
        }
    }
    let input = InternedInput::new(byte_lines(seen), byte_lines(made));
    let mut out = Vec::new();
    for hunk in Diff::compute(Algorithm::Histogram, &input).hunks() {
        let at = hunk.before.start as usize;
        if !hunk.before.is_empty() || at == 0 || at >= seen_lines.len() {
            continue;
        }
        if others[at - 1] && others[at] {
            out.extend([seen_lines[at - 1], seen_lines[at]]);
        }
    }
    out
}

/// The layers of `layers` but `ix` (with their merge bases' trees,
/// `bases`) that added any of `around` to `path`.
fn whose_lines(
    repo: &gix::Repository,
    layers: &[Pin],
    bases: &[ObjectId],
    ix: usize,
    path: &BStr,
    around: &[Vec<u8>],
) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for (j, layer) in layers.iter().enumerate() {
        if j == ix {
            continue;
        }
        let theirs = lookup(repo, commit_tree(repo, layer.snapshot)?, path)?;
        let ancestor = lookup(repo, bases[j], path)?;
        if theirs == ancestor {
            continue;
        }
        let (theirs, ancestor) = (read_blob(repo, theirs)?, read_blob(repo, ancestor)?);
        let added = added_lines(&ancestor, &theirs);
        if (added.iter()).any(|l| around.iter().any(|a| key(a) == key(l))) {
            out.push(j);
        }
    }
    Ok(out)
}

/// The result of a 3-way merge of entries that needs no content merge: one
/// side unchanged, or both changed alike.
fn trivial_merge(
    ancestor: Option<Item>,
    ours: Option<Item>,
    theirs: Option<Item>,
) -> Option<Option<Item>> {
    if theirs == ancestor || ours == theirs {
        Some(ours)
    } else if ours == ancestor {
        Some(theirs)
    } else {
        None
    }
}

/// The kind of a content merge of two regular files: a mode change on one
/// side wins, as in git; anything but regular files can't be merged.
fn merged_kind(ancestor: Option<Item>, ours: Item, theirs: Item) -> Option<EntryKind> {
    let blob = |k: EntryKind| matches!(k, EntryKind::Blob | EntryKind::BlobExecutable);
    if !blob(ours.kind) || !blob(theirs.kind) || ancestor.is_some_and(|a| !blob(a.kind)) {
        return None;
    }
    match ancestor {
        Some(a) if theirs.kind != a.kind => Some(theirs.kind),
        Some(_) => Some(ours.kind),
        None if ours.kind == theirs.kind => Some(ours.kind),
        None => None,
    }
}

fn read_blob(repo: &gix::Repository, item: Option<Item>) -> Result<Vec<u8>> {
    match item {
        Some(item) => Ok(repo.find_blob(item.oid)?.take_data()),
        None => Ok(Vec::new()),
    }
}

/// As git decides it: a NUL byte in the first 8000.
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

pub(crate) fn keep_conflicts() -> text::Conflict {
    text::Conflict::Keep {
        style: text::ConflictStyle::Merge,
        marker_size: text::Conflict::DEFAULT_MARKER_SIZE
            .try_into()
            .expect("non-zero"),
    }
}

/// A 3-way line merge, as `git merge-file`; `None` if conflicts remain.
pub(crate) fn merge3(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    conflict: text::Conflict,
) -> Option<Vec<u8>> {
    let mut input = gix::diff::blob::InternedInput::default();
    let mut out = Vec::new();
    let options = text::Options {
        conflict,
        ..Default::default()
    };
    let labels = text::Labels::default();
    match builtin_driver::text(&mut out, &mut input, labels, ours, base, theirs, options) {
        Resolution::Conflict => None,
        Resolution::Complete | Resolution::CompleteWithAutoResolvedConflict => Some(out),
    }
}

/// A 3-way line merge, as `git merge-file`, conflicts kept with git's
/// markers named `ours_label` and `theirs_label`: the merge, and whether
/// conflicts remain.
pub(crate) fn merge3_marked(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    ours_label: &str,
    theirs_label: &str,
) -> (Vec<u8>, bool) {
    let mut input = gix::diff::blob::InternedInput::default();
    let mut out = Vec::new();
    let options = text::Options {
        conflict: keep_conflicts(),
        ..Default::default()
    };
    let labels = text::Labels {
        ancestor: Some("before".into()),
        current: Some(ours_label.into()),
        other: Some(theirs_label.into()),
    };
    let resolution =
        builtin_driver::text(&mut out, &mut input, labels, ours, base, theirs, options);
    (out, matches!(resolution, Resolution::Conflict))
}

// The line-set rule's helpers, shared by union merging, extraction, carrying own
// lines over a switch (`fs::workspace::layers`) and snapshotting them
// (`state::snapshot`). Lines are compared whole but for the `\n`: two registry
// lines differing in a `\r` are two lines.

/// The lines of `data`, each with its newline (the last may have none).
pub(crate) fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split_inclusive(|b| *b == b'\n')
}

/// A line without its newline, to compare lines by.
pub(crate) fn key(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// Whether `side` is `base` with lines added and none removed or changed.
pub(crate) fn only_adds(base: &[u8], side: &[u8]) -> bool {
    let mut base = lines(base).map(key).peekable();
    for line in lines(side) {
        if base.peek() == Some(&key(line)) {
            base.next();
        }
    }
    base.peek().is_none()
}

/// The lines of `side` that `base` doesn't have, a repeated line counted
/// once per time it is there: what `side` added, if it only added.
pub(crate) fn added_lines<'a>(base: &[u8], side: &'a [u8]) -> Vec<&'a [u8]> {
    let mut left: HashMap<&[u8], usize> = HashMap::new();
    for line in lines(base) {
        *left.entry(key(line)).or_default() += 1;
    }
    lines(side)
        .filter(|line| match left.get_mut(key(line)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// `data` without `remove`'s lines, each taken out once per time it is
/// there.
pub(crate) fn without_lines(data: &[u8], remove: &[&[u8]]) -> Vec<u8> {
    let mut left: HashMap<&[u8], usize> = HashMap::new();
    for line in remove {
        *left.entry(key(line)).or_default() += 1;
    }
    let mut out = Vec::new();
    for line in lines(data) {
        match left.get_mut(key(line)) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.extend_from_slice(line),
        }
    }
    out
}

/// The line-set rule for carrying `ours`' lines from union `old` onto union
/// `new` where their merge conflicts, if `ours` only added lines (next to a
/// layer's, which are then their context): `new`'s lines, then the lines
/// `ours` added, in its order, each ending in a newline. `None` if it
/// changed or removed any line (that conflict is real), or for anything but
/// text.
pub(crate) fn line_set(old: &[u8], ours: &[u8], new: &[u8]) -> Option<Vec<u8>> {
    if [old, ours, new].iter().any(|d| is_binary(d)) || !only_adds(old, ours) {
        return None;
    }
    let mut out = Vec::new();
    for line in lines(new).chain(added_lines(old, ours)) {
        out.extend_from_slice(line);
        if !line.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    Some(out)
}

/// Writes view commit V: tree `view_tree`, parents `first` (B) then
/// `snapshots`.
pub fn view_commit(
    repo: &gix::Repository,
    view_tree: ObjectId,
    first: ObjectId,
    snapshots: &[ObjectId],
    subject: &str,
) -> Result<ObjectId> {
    write_commit(repo, view_tree, first, snapshots, subject, Vec::new())
}

/// V's subject.
pub fn layers_subject(branch: &str, layers: &[&str]) -> String {
    format!(
        "gitfold view: {branch} + {} (keeps the layers' snapshots; never push this commit)",
        layers.join(" + ")
    )
}

fn write_commit(
    repo: &gix::Repository,
    tree: ObjectId,
    first: ObjectId,
    rest: &[ObjectId],
    message: &str,
    extra_headers: Vec<(BString, BString)>,
) -> Result<ObjectId> {
    let me = gix::actor::Signature {
        name: "gitfold".into(),
        email: "gitfold@localhost".into(),
        time: gix::date::Time::now_local_or_utc(),
    };
    let commit = gix::objs::Commit {
        tree,
        parents: std::iter::once(first).chain(rest.iter().copied()).collect(),
        author: me.clone(),
        committer: me,
        encoding: None,
        message: format!("{message}\n").into(),
        extra_headers,
    };
    Ok(repo.write_object(&commit)?.detach())
}

/// A copy of `commit` with tree `tree` and the one parent `parent`: the same
/// author, committer, message and other headers (`git rebase` with the
/// committer kept). A signature would not match the new commit and is
/// dropped; so are a merge's tags.
pub fn copy_commit(
    repo: &gix::Repository,
    commit: ObjectId,
    tree: ObjectId,
    parent: ObjectId,
) -> Result<ObjectId> {
    let mut copy: gix::objs::Commit = repo.find_commit(commit)?.decode()?.try_into()?;
    copy.tree = tree;
    copy.parents = [parent].into_iter().collect();
    copy.extra_headers
        .retain(|(k, _)| !matches!(k.as_bytes(), b"gpgsig" | b"gpgsig-sha256" | b"mergetag"));
    Ok(repo.write_object(&copy)?.detach())
}

/// `merge_trees(base, ours, theirs)` as git's ort merges them (renames
/// followed): the tree, or the paths with conflicts git would leave.
pub fn merge_trees(
    repo: &gix::Repository,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<std::result::Result<ObjectId, Vec<BString>>> {
    let options = repo.tree_merge_options()?;
    let labels = text::Labels::default();
    let mut outcome = repo.merge_trees(base, ours, theirs, labels, options)?;
    let how = gix::merge::tree::TreatAsUnresolved::git();
    if outcome.has_unresolved_conflicts(how) {
        let paths: BTreeSet<BString> = outcome
            .conflicts
            .iter()
            .filter(|c| c.is_unresolved(how))
            .flat_map(|c| [c.ours.location().to_owned(), c.theirs.location().to_owned()])
            .collect();
        return Ok(Err(paths.into_iter().collect()));
    }
    Ok(Ok(outcome.tree.write()?.detach()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_repo::Repo;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn lines(n: usize, prefix: &str) -> String {
        (1..=n).map(|i| format!("{prefix}{i}\n")).collect()
    }

    /// The scenario of 03-experiments §1: a base, and branches `contracts`,
    /// `a` and `b` with one commit each on it (the snapshots). Returns the base.
    fn scenario(r: &Repo) -> ObjectId {
        r.write("README.md", "readme\n");
        r.write("src/core.c", &lines(5, "core"));
        r.write("src/util.c", &lines(8, "util"));
        r.write("src/main.c", &lines(6, "main"));
        r.write("src/old.c", "old\n");
        r.write("docs/guide.md", &lines(6, "guide"));
        r.write("lib/legacy.c", "legacy\n");
        r.write("api/contract.h", "contract v1\n");
        r.write("api/current.h", "current\n");
        r.write("bin/tool", "tool\n");
        r.write("conf", "conf\n");
        r.write("registry.txt", &lines(3, "entry_base"));
        let base = r.commit("base");

        r.git(&["checkout", "-q", "-b", "contracts"]);
        r.write("api/contract.h", "contract v2\n");
        r.write("api/types.h", "types\n");
        fs::remove_file(r.work().join("api/current.h")).unwrap();
        std::os::unix::fs::symlink("contract.h", r.work().join("api/current.h")).unwrap();
        r.commit("contracts");

        r.git(&["checkout", "-q", "-b", "a", "main"]);
        r.write(
            "src/util.c",
            &lines(8, "util").replace("util3\n", "util3 A\n"),
        );
        r.write("src/feature_a.c", "feature a\n");
        r.git(&["rm", "-q", "src/old.c"]);
        r.git(&["mv", "docs/guide.md", "docs/manual.md"]);
        r.write("registry.txt", &(lines(3, "entry_base") + "entry_a\n"));
        let tool = r.work().join("bin/tool");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        r.git(&["rm", "-q", "conf"]);
        r.write("conf/main.conf", "conf as a directory\n");
        r.commit("a");

        r.git(&["checkout", "-q", "-b", "b", "main"]);
        r.write(
            "src/core.c",
            &lines(5, "core").replace("core2\n", "core2 B\n"),
        );
        r.write("src/feature_b.c", "feature b\n");
        r.git(&["rm", "-q", "lib/legacy.c"]);
        r.commit("b");

        r.git(&["checkout", "-q", "main"]);
        base
    }

    fn stack(r: &Repo, base: ObjectId, names: &[&str]) -> Vec<Pin> {
        let layer = |name: &&str| Pin {
            name: name.to_string(),
            snapshot: r.id(name),
            merge_base: base,
        };
        names.iter().map(layer).collect()
    }

    fn registry(path: &BStr) -> bool {
        path == "registry.txt"
    }

    /// The layers whose content is at `path` in M (`Extractor::owners`).
    fn owners_of(view: &Built, path: &BStr) -> Vec<usize> {
        match view.mixed.get(path) {
            Some(layers) => layers.clone(),
            None => view.union.get(path).cloned().unwrap_or_default(),
        }
    }

    fn registries(paths: &[BString]) -> Result<HashSet<BString>> {
        Ok(paths
            .iter()
            .filter(|p| registry(p.as_ref()))
            .cloned()
            .collect())
    }

    fn tree_of(r: &Repo, commit: ObjectId) -> ObjectId {
        r.id(&format!("{commit}^{{tree}}"))
    }

    /// Edits written by `edit_tree` come back from `delta`, including a file
    /// turned into a directory and a directory emptied.
    #[test]
    fn edit_tree_and_delta() {
        let r = Repo::new("edit");
        let base = scenario(&r);
        let repo = r.open();
        let blob = Item {
            kind: EntryKind::Blob,
            oid: repo.write_blob("new\n").unwrap().detach(),
        };
        let link = Item {
            kind: EntryKind::Link,
            oid: blob.oid,
        };
        let base_tree = tree_of(&r, base);
        let changes = [
            ("README.md", Change::Set(link)),
            ("conf", Change::Delete),
            ("conf/inner", Change::Set(blob)),
            ("docs/guide.md", Change::Delete),
            ("src/old.c", Change::Delete),
            ("x/y/z.txt", Change::Set(blob)),
        ];
        let changes = changes.map(|(p, c)| (BStr::new(p), c));
        let tree = edit_tree(&repo, base_tree, changes).unwrap();
        assert_eq!(
            r.diff(base_tree, tree),
            "T\tREADME.md\nD\tconf\nA\tconf/inner\nD\tdocs/guide.md\nD\tsrc/old.c\nA\tx/y/z.txt"
        );
        let top = r.git(&["ls-tree", &tree.to_string()]);
        assert!(!top.contains("docs"), "empty directory gone: {top}");
        let expected: Vec<_> = changes
            .iter()
            .map(|(p, c)| (BString::from(*p), *c))
            .collect();
        assert_eq!(delta(&repo, base_tree, tree).unwrap(), expected);
        // And back: the directory is deleted file by file before the file.
        let back = delta(&repo, tree, base_tree).unwrap();
        let conf: Vec<_> = back
            .iter()
            .filter(|(p, _)| p.starts_with(b"conf"))
            .collect();
        assert_eq!(conf.len(), 2);
        assert_eq!(conf[0], &("conf/inner".into(), Change::Delete));
        assert_eq!(conf[1].0, "conf");
        let undo = back.iter().map(|(p, c)| (p.as_ref(), *c));
        assert_eq!(edit_tree(&repo, tree, undo).unwrap(), base_tree);
    }

    /// The view of three layers with new files, deletions, a rename, mode
    /// and type changes, and one appended registry line: M is what git's
    /// own merge of the three branches gives, H differs from B only in the
    /// registry, and every mixed path names its layer.
    #[test]
    fn view_of_three_layers() {
        let r = Repo::new("view");
        let base = scenario(&r);
        let repo = r.open();
        let names = ["contracts", "a", "b"];
        let view = build_view(&repo, base, "main", &stack(&r, base, &names), &registries).unwrap();
        assert_eq!(view.notes, []);
        let base_tree = tree_of(&r, base);
        assert_eq!(r.diff(base_tree, view.hunk_tree), "M\tregistry.txt");
        assert_eq!(
            r.show(view.hunk_tree, "registry.txt"),
            "entry_base1\nentry_base2\nentry_base3\nentry_a"
        );
        assert_eq!(view.union.get(BStr::new("registry.txt")), Some(&vec![1]));
        assert_eq!(view.union.len(), 1);
        let mixed: Vec<_> = (view.mixed.iter())
            .map(|(p, l)| format!("{p}:{}", l[0]))
            .collect();
        assert_eq!(
            mixed.join(" "),
            "api/contract.h:0 api/current.h:0 api/types.h:0 bin/tool:1 conf:1 conf/main.conf:1 \
             docs/guide.md:1 docs/manual.md:1 lib/legacy.c:2 src/core.c:2 src/feature_a.c:1 \
             src/feature_b.c:2 src/old.c:1 src/util.c:1"
        );
        assert_eq!(owners_of(&view, "src/util.c".into()), [1]);
        assert_eq!(owners_of(&view, "registry.txt".into()), [1]);
        assert_eq!(owners_of(&view, "README.md".into()), Vec::<usize>::new());

        // Git merges the three branches into the same tree.
        let ab = r.merge_tree(base, r.id("contracts"), r.id("a")).unwrap();
        let abc = r.merge_tree(base_tree, ab, r.id("b")).unwrap();
        assert_eq!(view.view_tree, abc);
        let ls = r.git(&["ls-tree", "-r", &view.view_tree.to_string()]);
        assert!(ls.contains("120000 blob"), "{ls}");
        assert!(ls.contains("100755 blob"), "{ls}");

        // V, as git sees it.
        let snapshots: Vec<_> = names.iter().map(|n| r.id(n)).collect();
        let subject = layers_subject("main", &names);
        let v = view_commit(&repo, view.view_tree, base, &snapshots, &subject).unwrap();
        assert_eq!(
            r.git(&["log", "-1", "--format=%P%n%s", &v.to_string()]),
            format!(
                "{base} {} {} {}\n{subject}",
                snapshots[0], snapshots[1], snapshots[2]
            )
        );
        r.git(&["update-ref", "refs/gitfold-layers/test", &v.to_string()]);
        r.git(&["fsck", "--strict", "--no-dangling"]);
    }

    /// The base moved on since the layers' merge base: a whole-file path
    /// shows the base's newer change and the layer's own, as does a union
    /// path.
    #[test]
    fn view_on_a_newer_base() {
        let r = Repo::new("newer");
        let old = scenario(&r);
        r.write(
            "src/util.c",
            &lines(8, "util").replace("util8\n", "util8 UP\n"),
        );
        r.write("registry.txt", &(lines(3, "entry_base") + "entry_up\n"));
        let new = r.commit("upstream");
        let repo = r.open();
        let view = build_view(&repo, new, "main", &stack(&r, old, &["a"]), &registries).unwrap();
        assert_eq!(view.notes, []);
        assert_eq!(
            r.show(view.view_tree, "src/util.c"),
            lines(8, "util")
                .replace("util3\n", "util3 A\n")
                .replace("util8\n", "util8 UP")
        );
        assert_eq!(view.mixed.get(BStr::new("src/util.c")), Some(&vec![0]));
        assert_eq!(
            r.show(view.hunk_tree, "registry.txt"),
            "entry_base1\nentry_base2\nentry_base3\nentry_up\nentry_a"
        );
    }

    /// The same with two layers changing the path, each pinned at a
    /// different merge base, neither the base: every change is kept.
    #[test]
    fn view_on_a_newer_base_merged() {
        let r = Repo::new("newer-merged");
        let old = scenario(&r);
        let util = lines(8, "util");
        r.git(&["checkout", "-q", "-b", "low", "main"]);
        r.write("src/util.c", &util.replace("util1\n", "util1 LOW\n"));
        r.commit("low");
        r.git(&["checkout", "-q", "main"]);
        r.write("src/util.c", &util.replace("util4\n", "util4 UP\n"));
        let mid = r.commit("upstream");
        r.git(&["checkout", "-q", "-b", "high", "main"]);
        r.write(
            "src/util.c",
            &util
                .replace("util4\n", "util4 UP\n")
                .replace("util8\n", "util8 HIGH\n"),
        );
        r.commit("high");
        r.git(&["checkout", "-q", "main"]);
        r.write(
            "src/util.c",
            &util
                .replace("util4\n", "util4 UP\n")
                .replace("util6\n", "util6 NEW\n"),
        );
        let new = r.commit("newer");
        let repo = r.open();
        let mut layers = stack(&r, old, &["low"]);
        layers.extend(stack(&r, mid, &["high"]));
        let expected = util
            .replace("util1\n", "util1 LOW\n")
            .replace("util4\n", "util4 UP\n")
            .replace("util6\n", "util6 NEW\n")
            .replace("util8\n", "util8 HIGH");
        let view = build_view(&repo, new, "main", &layers, &registries).unwrap();
        assert_eq!(view.notes, []);
        assert_eq!(r.show(view.view_tree, "src/util.c"), expected);
        assert_eq!(view.mixed.get(BStr::new("src/util.c")), Some(&vec![0, 1]));
        // In the other order too.
        layers.reverse();
        let view = build_view(&repo, new, "main", &layers, &registries).unwrap();
        assert_eq!(view.notes, []);
        assert_eq!(r.show(view.view_tree, "src/util.c"), expected);
    }

    /// A layer brings only its own changes to a whole file: what the base
    /// changed since stays; a change of the same lines conflicts with the
    /// base (markers, reported); deletions, a mode change and binary files
    /// merge as git would, and where they can't, the base's version shows.
    #[test]
    fn own_changes_only() {
        let r = Repo::new("own");
        let text = lines(60, "line");
        r.write("x.rs", &text);
        r.write("same.rs", &text);
        r.write("gone.rs", "gone\n");
        r.write("kept.rs", "kept\n");
        r.write("edited.rs", "edited\n");
        r.write("tool.sh", &text);
        r.write("blob.bin", "bin\0one\n");
        r.write("blob2.bin", "bin\0two\n");
        let old = r.commit("old");
        r.git(&["checkout", "-q", "-b", "b", "main"]);
        r.write("x.rs", &text.replace("line50\n", "line50 B\n"));
        r.write("same.rs", &text.replace("line10\n", "line10 B\n"));
        r.git(&["rm", "-q", "gone.rs", "kept.rs"]);
        r.write("edited.rs", "edited by B\n");
        let tool = r.work().join("tool.sh");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        r.write("blob.bin", "bin\0one B\n");
        r.write("blob2.bin", "bin\0two B\n");
        r.commit("b");
        r.git(&["checkout", "-q", "main"]);
        r.write("x.rs", &text.replace("line10\n", "line10 FIX\n"));
        r.write("same.rs", &text.replace("line10\n", "line10 FIX\n"));
        r.write("kept.rs", "kept, changed\n");
        r.git(&["rm", "-q", "edited.rs"]);
        r.write("tool.sh", &text.replace("line1\n", "line1 FIX\n"));
        r.write("blob.bin", "bin\0one FIX\n");
        let new = r.commit("new");
        let repo = r.open();
        let view = build_view(&repo, new, "main", &stack(&r, old, &["b"]), &registries).unwrap();

        // Different lines: both.
        assert_eq!(
            r.show(view.view_tree, "x.rs"),
            text.replace("line10\n", "line10 FIX\n")
                .replace("line50\n", "line50 B\n")
                .trim_end()
        );
        // The same line: git's conflict markers, the base's side first.
        let same = r.show(view.view_tree, "same.rs");
        assert!(
            same.contains("<<<<<<< main\nline10 FIX\n=======\nline10 B\n>>>>>>> b\n"),
            "{same}"
        );
        // Deleted, unchanged on the base: gone. Deleted, changed on the
        // base, or changed, deleted on the base: the base's.
        let at = |p: &str| lookup(&repo, view.view_tree, p.into()).unwrap();
        assert_eq!(at("gone.rs"), None);
        assert_eq!(r.show(view.view_tree, "kept.rs"), "kept, changed");
        assert_eq!(at("edited.rs"), None);
        // A mode change with a content change: both.
        assert_eq!(at("tool.sh").unwrap().kind, EntryKind::BlobExecutable);
        assert!(r.show(view.view_tree, "tool.sh").starts_with("line1 FIX\n"));
        // Binary: the base's where both changed it, else the layer's.
        assert_eq!(
            at("blob.bin"),
            lookup(&repo, tree_of(&r, new), "blob.bin".into()).unwrap()
        );
        assert_eq!(r.show(view.view_tree, "blob2.bin"), "bin\0two B");

        let conflict = |path: &str, markers| Note::Conflict {
            layers: vec![0],
            base: true,
            path: path.into(),
            markers,
        };
        assert_eq!(
            view.notes,
            [
                conflict("blob.bin", false),
                conflict("edited.rs", false),
                conflict("kept.rs", false),
                conflict("same.rs", true),
            ]
        );
        // Every path the layer changed is its own, read-only.
        let mixed: Vec<_> = view.mixed.keys().map(|p| p.to_string()).collect();
        assert_eq!(
            mixed,
            [
                "blob.bin",
                "blob2.bin",
                "edited.rs",
                "gone.rs",
                "kept.rs",
                "same.rs",
                "tool.sh",
                "x.rs"
            ]
        );
    }

    /// Files and directories: a layer's change to a file the base made a
    /// directory of, or a new file below a file the base has, shows the
    /// base's tree, reported; a file the layer made a directory of, which
    /// the base didn't change, is the layer's directory.
    #[test]
    fn files_and_directories() {
        let r = Repo::new("dirs");
        r.write("d", "file\n");
        r.write("e", "file\n");
        let old = r.commit("old");
        r.git(&["checkout", "-q", "-b", "b", "main"]);
        r.write("d", "file B\n");
        r.git(&["rm", "-q", "e"]);
        r.write("e/inner", "inner B\n");
        r.write("f/inner", "inner B\n");
        r.commit("b");
        r.git(&["checkout", "-q", "main"]);
        r.git(&["rm", "-q", "d"]);
        r.write("d/inner", "base's\n");
        r.write("f", "base's file\n");
        let new = r.commit("new");
        let repo = r.open();
        let view = build_view(&repo, new, "main", &stack(&r, old, &["b"]), &registries).unwrap();
        assert_eq!(r.show(view.view_tree, "d/inner"), "base's");
        assert_eq!(r.show(view.view_tree, "e/inner"), "inner B");
        assert_eq!(r.show(view.view_tree, "f"), "base's file");
        let conflict = |path: &str| Note::Conflict {
            layers: vec![0],
            base: true,
            path: path.into(),
            markers: false,
        };
        assert_eq!(view.notes, [conflict("d"), conflict("f/inner")]);
        let mixed: Vec<_> = view.mixed.keys().map(|p| p.to_string()).collect();
        assert_eq!(mixed, ["e", "e/inner", "f/inner"]);
    }

    /// A layer's change the base already has: the path shows the base's,
    /// isn't the layer's and isn't reported.
    #[test]
    fn change_already_on_the_base() {
        let r = Repo::new("already");
        r.write("x.rs", &lines(5, "x"));
        let old = r.commit("old");
        r.git(&["checkout", "-q", "-b", "b", "main"]);
        r.write("x.rs", &lines(5, "x").replace("x2\n", "x2 B\n"));
        r.commit("b");
        r.git(&["checkout", "-q", "main"]);
        r.write(
            "x.rs",
            &lines(5, "x")
                .replace("x2\n", "x2 B\n")
                .replace("x4\n", "x4 UP\n"),
        );
        let new = r.commit("new");
        let repo = r.open();
        let view = build_view(&repo, new, "main", &stack(&r, old, &["b"]), &registries).unwrap();
        assert_eq!(view.notes, []);
        assert!(view.mixed.is_empty());
        assert_eq!(view.view_tree, tree_of(&r, new));
    }

    /// Two layers append to the registry: a plain 3-way merge conflicts
    /// (git's too), the union keeps both. Two layers changing the same line
    /// differently: the upper one's change is dropped and reported.
    #[test]
    fn registry_unions() {
        let r = Repo::new("union");
        let base = scenario(&r);
        for (name, line) in [("x", "entry_x"), ("y", "entry_y")] {
            r.git(&["checkout", "-q", "-b", name, "main"]);
            r.write("registry.txt", &(lines(3, "entry_base") + line + "\n"));
            r.commit(name);
        }
        for (name, line) in [("p", "entry_base2 P"), ("q", "entry_base2 Q")] {
            r.git(&["checkout", "-q", "-b", name, "main"]);
            r.write(
                "registry.txt",
                &lines(3, "entry_base").replace("entry_base2", line),
            );
            r.commit(name);
        }
        r.git(&["checkout", "-q", "main"]);
        assert_eq!(
            r.merge_tree(base, r.id("x"), r.id("y")),
            None,
            "git conflicts"
        );
        let repo = r.open();

        let view = build_view(
            &repo,
            base,
            "main",
            &stack(&r, base, &["x", "y"]),
            &registries,
        )
        .unwrap();
        assert_eq!(view.notes, []);
        assert_eq!(
            r.show(view.view_tree, "registry.txt"),
            "entry_base1\nentry_base2\nentry_base3\nentry_x\nentry_y"
        );
        assert_eq!(view.union.get(BStr::new("registry.txt")), Some(&vec![0, 1]));

        let view = build_view(
            &repo,
            base,
            "main",
            &stack(&r, base, &["p", "x", "q"]),
            &registries,
        )
        .unwrap();
        assert_eq!(
            view.notes,
            [Note::NotInUnion {
                layer: 2,
                path: "registry.txt".into()
            }]
        );
        assert_eq!(
            r.show(view.view_tree, "registry.txt"),
            "entry_base1\nentry_base2 P\nentry_base3\nentry_x"
        );
        assert_eq!(view.union.get(BStr::new("registry.txt")), Some(&vec![0, 1]));
    }

    /// A snapshot of `b` whose author saw `a`'s registry lines and put its own
    /// among them: its tree holds only its own line, its record what it saw
    /// and made. Returns (base, a, b's snapshot).
    fn recorded_scenario(r: &Repo, made: &str) -> (ObjectId, ObjectId, ObjectId) {
        r.write("registry.txt", "alpha\nzulu\n");
        let base = r.commit("base");
        r.git(&["checkout", "-q", "-b", "a"]);
        r.write("registry.txt", "alpha\nbravo\ndelta\nzulu\n");
        let a = r.commit("a");
        r.git(&["checkout", "-q", "-b", "b", "main"]);
        let repo = r.open();
        let blob = |data: &str| Item {
            kind: EntryKind::Blob,
            oid: repo.write_blob(data).unwrap().detach(),
        };
        let path = BStr::new("registry.txt");
        let base_tree = tree_of(r, base);
        let own = super::without_lines(made.as_bytes(), &[b"bravo\n", b"delta\n"]);
        let own = Item {
            kind: EntryKind::Blob,
            oid: repo.write_blob(own).unwrap().detach(),
        };
        let tree = edit_tree(&repo, base_tree, [(path, Change::Set(own))]).unwrap();
        let empty = repo
            .write_object(gix::objs::Tree::empty())
            .unwrap()
            .detach();
        let side = |data: &str| edit_tree(&repo, empty, [(path, Change::Set(blob(data)))]);
        let (seen, made_tree) = (
            side(&r.show(a, "registry.txt")).unwrap(),
            side(made).unwrap(),
        );
        let record = write_record(&repo, "b", base, tree, seen, made_tree).unwrap();
        let (tree, base_hex, record_hex) = (tree.to_string(), base.to_string(), record.to_string());
        let snapshot = r.git(&[
            "commit-tree",
            &tree,
            "-p",
            &base_hex,
            "-p",
            &record_hex,
            "-m",
            "snapshot",
        ]);
        r.git(&["checkout", "-q", "main"]);
        (base, a, ObjectId::from_hex(snapshot.as_bytes()).unwrap())
    }

    fn pins(base: ObjectId, layers: &[(&str, ObjectId)]) -> Vec<Pin> {
        (layers.iter())
            .map(|(name, snapshot)| Pin {
                name: name.to_string(),
                snapshot: *snapshot,
                merge_base: base,
            })
            .collect()
    }

    /// A layer whose snapshot records what its author saw keeps its lines'
    /// place among another layer's, in either order; without that layer
    /// they are placed by the base's lines, noted. Lines added between two
    /// of another branch's are warned about.
    #[test]
    fn recorded_unions_keep_their_place() {
        let r = Repo::new("recorded");
        let made = "alpha\nbravo\ncharlie\ndelta\nzulu\n";
        let (base, a, b) = recorded_scenario(&r, made);
        let repo = r.open();
        let record = record_of(&repo, b).unwrap().unwrap();
        assert_eq!(record.base, base);
        assert_eq!(r.show(record.made, "registry.txt"), made.trim_end());
        assert_eq!(record_of(&repo, a).unwrap(), None, "a branch's commit");
        assert_eq!(
            r.git(&["merge-base", &a.to_string(), &b.to_string()]),
            base.to_string(),
            "the record is no merge base"
        );
        assert_eq!(
            r.show(b, "registry.txt"),
            "alpha\ncharlie\nzulu",
            "own lines"
        );

        let among = |layer, others| Note::Among {
            layer,
            path: "registry.txt".into(),
            others,
        };
        for order in [[("a", a), ("b", b)], [("b", b), ("a", a)]] {
            let view = build_view(&repo, base, "main", &pins(base, &order), &registries).unwrap();
            assert_eq!(r.show(view.view_tree, "registry.txt"), made.trim_end());
            assert_eq!(view.union.get(BStr::new("registry.txt")), Some(&vec![0, 1]));
            let b_ix = order.iter().position(|(n, _)| *n == "b").unwrap();
            assert_eq!(view.notes, [among(b_ix, vec![1 - b_ix])]);
        }

        let view = build_view(&repo, base, "main", &pins(base, &[("b", b)]), &registries).unwrap();
        assert_eq!(
            r.show(view.view_tree, "registry.txt"),
            "alpha\ncharlie\nzulu"
        );
        let placed = Note::PlacedByBase {
            layer: 0,
            path: "registry.txt".into(),
        };
        assert_eq!(view.notes, [placed, among(0, vec![])]);
    }

    /// Lines added next to a base's line, or at the end, aren't among
    /// another branch's; between two of them they are.
    #[test]
    fn lines_among_others() {
        let base = b"head\ntail\n";
        let seen = b"head\na1\na2\ntail\n";
        let among = |made: &str| among_others(base, seen, made.as_bytes()).len();
        assert_eq!(among("head\na1\nb\na2\ntail\n"), 2);
        assert_eq!(among("head\nb\na1\na2\ntail\n"), 0, "next to the base's");
        assert_eq!(among("head\na1\na2\nb\ntail\n"), 0, "next to the base's");
        assert_eq!(among("head\na1\na2\ntail\nb\n"), 0, "at the end");
        assert_eq!(among_others(b"", b"a1\na2\n", b"a1\na2\nb\n").len(), 0);
        assert_eq!(among_others(b"", b"a1\na2\n", b"a1\nb\na2\n").len(), 2);
    }

    /// Several layers changing one whole-file path: changes to different
    /// lines merge; to the same line, the merge keeps git's conflict markers,
    /// named after the layers; a deletion against a change shows B's
    /// version. Each is reported and belongs to every layer involved.
    #[test]
    fn whole_files_merged() {
        let r = Repo::new("merged");
        let base = scenario(&r);
        let util = lines(8, "util");
        let layers = [
            ("top", util.replace("util1\n", "util1 TOP\n"), None),
            ("bottom", util.replace("util8\n", "util8 BOTTOM\n"), None),
            ("other", util.replace("util8\n", "util8 OTHER\n"), None),
            ("gone", util.clone(), Some("src/main.c")),
            ("edit", util.clone(), None),
        ];
        for (name, content, delete) in &layers {
            r.git(&["checkout", "-q", "-b", name, "main"]);
            r.write("src/util.c", content);
            match delete {
                Some(path) => drop(r.git(&["rm", "-q", path])),
                None => r.write("src/main.c", &lines(6, "main").replace("main2", "main2 E")),
            }
            r.commit(name);
        }
        r.git(&["checkout", "-q", "main"]);
        let repo = r.open();

        let view = build_view(
            &repo,
            base,
            "main",
            &stack(&r, base, &["top", "bottom"]),
            &registries,
        )
        .unwrap();
        assert_eq!(
            r.show(view.view_tree, "src/util.c"),
            util.replace("util1\n", "util1 TOP\n")
                .replace("util8\n", "util8 BOTTOM")
        );
        assert_eq!(view.mixed.get(BStr::new("src/util.c")), Some(&vec![0, 1]));
        assert_eq!(view.notes, []);

        let names = ["top", "bottom", "other"];
        let view = build_view(&repo, base, "main", &stack(&r, base, &names), &registries).unwrap();
        let shown = r.show(view.view_tree, "src/util.c");
        assert!(shown.contains("<<<<<<< main + top + bottom\n"), "{shown}");
        assert!(
            shown.contains("util8 BOTTOM\n=======\nutil8 OTHER\n"),
            "{shown}"
        );
        assert!(shown.ends_with(">>>>>>> other"), "{shown}");
        // Named: the two whose changes conflict, not top's, which merges.
        assert_eq!(
            view.notes,
            [Note::Conflict {
                layers: vec![1, 2],
                path: "src/util.c".into(),
                base: false,
                markers: true
            }]
        );
        // The path is still all three's.
        assert_eq!(
            view.mixed.get(BStr::new("src/util.c")),
            Some(&vec![0, 1, 2])
        );

        let view = build_view(
            &repo,
            base,
            "main",
            &stack(&r, base, &["gone", "edit"]),
            &registries,
        )
        .unwrap();
        assert_eq!(
            r.show(view.view_tree, "src/main.c"),
            lines(6, "main").trim_end()
        );
        assert_eq!(view.mixed.get(BStr::new("src/main.c")), Some(&vec![0, 1]));
        assert_eq!(
            view.notes,
            [Note::Conflict {
                layers: vec![0, 1],
                path: "src/main.c".into(),
                base: false,
                markers: false
            }]
        );
    }

    /// The line-set helpers: repeats counted, `\r` part of a line, own lines
    /// carried over only where they were added.
    #[test]
    fn line_sets() {
        let (old, ours) = (&b"a\nb\nb\n"[..], &b"a\nb\nmine\nb\nb\nlast"[..]);
        assert!(only_adds(old, ours));
        assert_eq!(added_lines(old, ours), [&b"mine\n"[..], b"b\n", b"last"]);
        assert_eq!(without_lines(b"a\r\na\nb\n", &[b"a"]), b"a\r\nb\n");
        assert_eq!(
            line_set(old, ours, b"a\nb\nb\nnew").unwrap(),
            b"a\nb\nb\nnew\nmine\nb\nlast\n"
        );
        assert_eq!(line_set(old, b"a\nmine\n", b"a\n"), None, "removed a line");
        assert_eq!(
            line_set(old, b"a\r\nb\nb\n", b"a\n"),
            None,
            "changed a line"
        );
        assert_eq!(line_set(b"\0", b"\0x", b""), None, "not text");
    }

    /// gix's tree merge against `git merge-tree`, with a rename on one side
    /// and an edit of the renamed file on the other, and with unrelated
    /// changes on each.
    #[test]
    fn merges_match_git() {
        let r = Repo::new("merge");
        let base = scenario(&r);
        r.git(&["checkout", "-q", "-b", "edit", "main"]);
        r.write(
            "docs/guide.md",
            &lines(6, "guide").replace("guide5", "guide5 EDIT"),
        );
        r.write(
            "src/core.c",
            &lines(5, "core").replace("core2", "core2 EDIT"),
        );
        r.commit("edit");
        r.git(&["checkout", "-q", "main"]);
        let repo = r.open();
        let base_tree = tree_of(&r, base);
        for (ours, theirs) in [
            ("a", "edit"),
            ("edit", "a"),
            ("contracts", "edit"),
            ("a", "b"),
        ] {
            let (ours, theirs) = (tree_of(&r, r.id(ours)), tree_of(&r, r.id(theirs)));
            let merged = merge_trees(&repo, base_tree, ours, theirs).unwrap().ok();
            assert_eq!(merged, r.merge_tree(base_tree, ours, theirs));
        }
        let (a, edit) = (tree_of(&r, r.id("a")), tree_of(&r, r.id("edit")));
        let merged = merge_trees(&repo, base_tree, a, edit).unwrap().unwrap();
        assert!(
            r.show(merged, "docs/manual.md").contains("guide5 EDIT"),
            "rename followed"
        );
        // b and edit change the same line of core.c.
        let b = tree_of(&r, r.id("b"));
        let conflict = merge_trees(&repo, base_tree, b, edit).unwrap();
        assert_eq!(conflict, Err(vec![BString::from("src/core.c")]));
        assert_eq!(r.merge_tree(base_tree, b, edit), None);
    }
}
