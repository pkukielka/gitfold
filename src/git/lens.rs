//! Layers on the git side: the trees and commits that mix other workspaces'
//! work into a workspace's view, and the extraction of the commits an agent
//! makes on top of that view back onto its branch. Pure functions over
//! objects in a repository; nothing here moves a ref except `advance`.
//!
//! Terms (the layers design, §1.1):
//! - B: my base, the branch tip.
//! - A layer: another workspace's work as of one seal S (a commit whose tree
//!   is that workspace's view), with the merge base MB it was pinned at. Its
//!   delta is `diff(MB.tree, S.tree)`.
//! - H, the hunk tree: B with every hunk path (a registry, a manifest: the
//!   caller's predicate) blended across the layers.
//! - M, the view tree: H with every other path a layer changed replaced by
//!   the top such layer's entry ("whole-file" paths).
//! - L, the lens commit: tree H, parents `[B, S...]`, an extra header
//!   `gitfold-lens 1`. The worktree's HEAD while hunk paths are mixed in, so
//!   `git diff` shows only my lines of a blended file.
//! - V, the view commit: tree M, parents `[L or B, S...]`; one ref on it
//!   keeps M, L and the seals from `git gc`.
//!
//! Blending a hunk path folds each layer's change into the file in turn,
//! `merge3(base = MB[p], ours = acc[p], theirs = S[p])`. Two appends at the
//! same place (two agents adding to a registry) conflict in every 3-way
//! merge, so when both sides only add lines the conflict is resolved by
//! keeping both, ours first (`merge=union`, but only there). Any other
//! conflict drops that layer's change to p and is reported.
//!
//! Extracting a commit C made on L (or on the previous commit P of a chain)
//! onto the branch tip T is a tree merge `merge(base = P, ours = T, theirs =
//! C)`, except on hunk paths, where P holds the layers' lines and T doesn't.
//! There the line-set rule decides (03-experiments §4):
//! - C may not remove or change a layer's line (one in P but not in T): the
//!   commit depends on that layer and is refused;
//! - otherwise a 3-way merge of P, T and C, which drops the layers' lines;
//! - if that conflicts (my line was appended after a layer's line, which is
//!   then its context): T's lines, less those C removed, then C's new lines.
//!   This assumes a registry's lines are unique and their order doesn't
//!   matter.
//!
//! A plain merge with `merge=union` would put the layers' lines into my
//! commit, and a plain merge without it conflicts on every append. Elsewhere
//! extraction is git's merge, so a commit that edits next to a layer's line
//! in a file whose layer version L holds conflicts too: it depends on that
//! layer. Merge commits made on L are flattened (one parent, the tip).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::merge::blob::Resolution;
use gix::merge::blob::builtin_driver::{self, text};
use gix::objs::tree::EntryKind;

/// The extra header that marks a lens commit (and its value), so that hooks
/// and the daemon recognise one without a list of ids.
pub const LENS_HEADER: &str = "gitfold-lens";
const LENS_HEADER_VALUE: &str = "1";

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
    fn item(self) -> Option<Item> {
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

fn commit_tree(repo: &gix::Repository, commit: ObjectId) -> Result<ObjectId> {
    let commit = repo
        .find_commit(commit)
        .with_context(|| format!("commit {commit}"))?;
    Ok(commit.tree_id()?.detach())
}

/// One layer of a stack: a seal and the merge base it is pinned to.
#[derive(Clone, Copy, Debug)]
pub struct Layer {
    pub seal: ObjectId,
    pub merge_base: ObjectId,
}

/// What `build_view` made.
#[derive(Debug)]
pub struct View {
    /// H: B's tree with the hunk paths blended (B's tree if none is).
    pub hunk_tree: ObjectId,
    /// M: H with the whole-file paths of the layers.
    pub view_tree: ObjectId,
    /// Whole-file paths where M differs from H, with the layer (an index into
    /// the stack) whose entry M has.
    pub mixed: BTreeMap<BString, usize>,
    /// Hunk paths where H differs from B, with the layers whose changes are
    /// blended in.
    pub blended: BTreeMap<BString, Vec<usize>>,
    /// What the switch should tell the agent.
    pub notes: Vec<Note>,
}

impl View {
    /// The layers whose changes are in H: the seals that are L's parents.
    pub fn hunk_layers(&self) -> Vec<usize> {
        let set: BTreeSet<usize> = self.blended.values().flatten().copied().collect();
        set.into_iter().collect()
    }
}

/// Something about a view the agent should hear.
#[derive(Debug, PartialEq, Eq)]
pub enum Note {
    /// The layer's change to this hunk path conflicts with what is below it
    /// (B or the layers under it), so it is not blended in.
    NotBlended { layer: usize, path: BString },
    /// B changed this whole-file path since the layer's merge base, and the
    /// layer's version, shown in M, hides that change.
    HidesBase { layer: usize, path: BString },
}

/// Builds the view of base commit `base` with `layers` (bottom to top),
/// blending hunk paths and taking the top layer's entry for the others
/// (§2.1 steps 1-3). The paths a layer sets that B has and no layer deletes
/// (a blend needs B's version, and a deletion is whole) are given to
/// `hunk_paths`, once, which returns those that are hunk paths.
pub fn build_view(
    repo: &gix::Repository,
    base: ObjectId,
    layers: &[Layer],
    hunk_paths: &dyn Fn(&[BString]) -> Result<HashSet<BString>>,
) -> Result<View> {
    let base_tree = commit_tree(repo, base)?;
    let mut layer_bases = Vec::with_capacity(layers.len());
    let mut deltas = Vec::with_capacity(layers.len());
    for layer in layers {
        let mb = commit_tree(repo, layer.merge_base)?;
        deltas.push(delta(repo, mb, commit_tree(repo, layer.seal)?)?);
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
    let mut notes = Vec::new();
    let mut hunks: BTreeMap<BString, Option<Item>> = BTreeMap::new();
    let mut blended: BTreeMap<BString, Vec<usize>> = BTreeMap::new();
    // Whole-file changes in layer order, so later (higher) layers win.
    let mut whole: Vec<(BString, Change, usize)> = Vec::new();
    for (ix, delta) in deltas.into_iter().enumerate() {
        let mb = layer_bases[ix];
        for (path, change) in delta {
            if !is_hunk.contains(&path) {
                whole.push((path, change, ix));
                continue;
            }
            let ours = match hunks.get(&path) {
                Some(item) => *item,
                None => lookup(repo, base_tree, path.as_ref())?,
            };
            let ancestor = lookup(repo, mb, path.as_ref())?;
            match blend(repo, ancestor, ours, change.item())? {
                Some(merged) => {
                    hunks.insert(path.clone(), merged);
                    blended.entry(path).or_default().push(ix);
                }
                None => notes.push(Note::NotBlended { layer: ix, path }),
            }
        }
    }
    let hunk_tree = edit_tree(
        repo,
        base_tree,
        hunks
            .iter()
            .map(|(p, item)| (p.as_ref(), Change::from_item(*item))),
    )?;
    // A blend can come back to B's file (a layer reverting another's line).
    let mut unchanged = Vec::new();
    for (path, item) in &hunks {
        if lookup(repo, base_tree, path.as_ref())? == *item {
            unchanged.push(path.clone());
        }
    }
    for path in unchanged {
        blended.remove(&path);
    }
    let view_tree = edit_tree(
        repo,
        hunk_tree,
        whole.iter().map(|(p, change, _)| (p.as_ref(), *change)),
    )?;
    let mut top: BTreeMap<&BString, (Change, usize)> = BTreeMap::new();
    for (path, change, ix) in &whole {
        top.insert(path, (*change, *ix));
    }
    let mut mixed = BTreeMap::new();
    for (path, (change, ix)) in top {
        let in_base = lookup(repo, base_tree, path.as_ref())?;
        if change.item() == in_base {
            continue;
        }
        if lookup(repo, layer_bases[ix], path.as_ref())? != in_base {
            notes.push(Note::HidesBase {
                layer: ix,
                path: path.clone(),
            });
        }
        mixed.insert(path.clone(), ix);
    }
    Ok(View {
        hunk_tree,
        view_tree,
        mixed,
        blended,
        notes,
    })
}

/// Folds `theirs` (a layer's entry, changed from `ancestor`) into `ours`:
/// the merged entry (`None`: deleted), or `None` on a conflict.
fn blend(
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

fn keep_conflicts() -> text::Conflict {
    text::Conflict::Keep {
        style: text::ConflictStyle::Merge,
        marker_size: text::Conflict::DEFAULT_MARKER_SIZE
            .try_into()
            .expect("non-zero"),
    }
}

/// A 3-way line merge, as `git merge-file`; `None` if conflicts remain.
fn merge3(base: &[u8], ours: &[u8], theirs: &[u8], conflict: text::Conflict) -> Option<Vec<u8>> {
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

// The line-set rule's helpers, shared by blending, extraction, carrying own
// lines over a switch (`fs::workspace::lens`) and sealing them
// (`state::seal`). Lines are compared whole but for the `\n`: two registry
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

/// The line-set rule for carrying `ours`' lines from blend `old` onto blend
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

/// Appends `line` to `out`, ending the line before it if it had no newline.
fn push_line(out: &mut Vec<u8>, line: &[u8]) {
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(line);
}

/// Writes lens commit L: tree `hunk_tree`, parents `base` then `seals`, the
/// `gitfold-lens` header and `subject` (see `lens_subject`).
pub fn lens_commit(
    repo: &gix::Repository,
    hunk_tree: ObjectId,
    base: ObjectId,
    seals: &[ObjectId],
    subject: &str,
) -> Result<ObjectId> {
    let header = (LENS_HEADER.into(), LENS_HEADER_VALUE.into());
    write_commit(repo, hunk_tree, base, seals, subject, vec![header])
}

/// Writes view commit V: tree `view_tree`, parents `first` (L, or B when no
/// hunk path is blended) then `seals`.
pub fn view_commit(
    repo: &gix::Repository,
    view_tree: ObjectId,
    first: ObjectId,
    seals: &[ObjectId],
    subject: &str,
) -> Result<ObjectId> {
    write_commit(repo, view_tree, first, seals, subject, Vec::new())
}

/// L's subject, as `git log -1` shows it in the workspace.
pub fn lens_subject(branch: &str, layers: &[&str]) -> String {
    format!(
        "gitfold lens: {branch} + {} (commits go to {branch}; never push this commit)",
        layers.join(" + ")
    )
}

/// Whether `commit` is a lens commit (has the `gitfold-lens` header).
#[cfg(test)]
pub fn is_lens(repo: &gix::Repository, commit: ObjectId) -> Result<bool> {
    let commit = repo.find_commit(commit)?;
    let decoded = commit.decode()?;
    Ok(decoded.extra_headers().find(LENS_HEADER).is_some())
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

/// The commits on the first-parent chain from `head` down to `lens`,
/// oldest first; `None` if `lens` isn't on it within `limit` commits. Empty
/// when `head` is `lens`.
pub fn chain(
    repo: &gix::Repository,
    lens: ObjectId,
    head: ObjectId,
    limit: usize,
) -> Result<Option<Vec<ObjectId>>> {
    let mut out = Vec::new();
    let mut at = head;
    while at != lens {
        if out.len() == limit {
            return Ok(None);
        }
        out.push(at);
        let commit = repo.find_commit(at)?;
        match commit.parent_ids().next() {
            Some(parent) => at = parent.detach(),
            None => return Ok(None),
        }
    }
    out.reverse();
    Ok(Some(out))
}

/// What `extract` did.
#[derive(Debug)]
pub struct Extraction {
    /// The branch tip after it: the last extracted commit, or the tip given.
    pub tip: ObjectId,
    /// The agent's commits extracted, each with its copy on the branch.
    pub extracted: Vec<(ObjectId, ObjectId)>,
    /// L': the tree of the agent's last commit, parents the new tip and the
    /// seals. Only when every commit was extracted.
    pub lens: Option<ObjectId>,
    /// The first commit that couldn't be: it and the later ones stay where
    /// they are, to retry from the last extracted one.
    pub blocked: Option<Blocked>,
}

/// A commit that can't go onto the branch alone.
#[derive(Debug)]
pub struct Blocked {
    pub commit: ObjectId,
    pub conflicts: Vec<Conflict>,
}

/// One path of a blocked commit.
#[derive(Debug, PartialEq, Eq)]
pub struct Conflict {
    pub path: BString,
    /// The layers whose content is at the path (from the caller's `owners`).
    pub layers: Vec<usize>,
    /// For a hunk path, the layers' lines the commit removed or changed;
    /// empty for a conflicting merge.
    pub lines: Vec<BString>,
}

/// What extraction needs to know about the lens it extracts from.
pub struct Lens<'a> {
    /// L's parents after the base, for L'.
    pub seals: &'a [ObjectId],
    /// The subject of L' (see `lens_subject`).
    pub subject: &'a str,
    pub is_hunk: &'a dyn Fn(&BStr) -> bool,
    /// The layers whose content is at a path, for conflicts.
    pub owners: &'a dyn Fn(&BStr) -> Vec<usize>,
}

/// Extracts the agent's `commits` (a first-parent chain on `prev`, which is
/// L or the last commit extracted before) onto branch tip `tip` (§3.5): each
/// becomes a commit on the tip with the same author, committer and message
/// and only the agent's changes. Moves no ref: see `advance`.
pub fn extract(
    repo: &gix::Repository,
    lens: &Lens,
    prev: ObjectId,
    tip: ObjectId,
    commits: &[ObjectId],
) -> Result<Extraction> {
    let mut done = Extraction {
        tip,
        extracted: Vec::new(),
        lens: None,
        blocked: None,
    };
    let mut prev = prev;
    for &commit in commits {
        match extract_one(repo, lens, prev, done.tip, commit)? {
            Ok(copy) => {
                done.extracted.push((commit, copy));
                done.tip = copy;
                prev = commit;
            }
            Err(conflicts) => {
                done.blocked = Some(Blocked { commit, conflicts });
                return Ok(done);
            }
        }
    }
    if let Some(&last) = commits.last() {
        let tree = commit_tree(repo, last)?;
        done.lens = Some(lens_commit(repo, tree, done.tip, lens.seals, lens.subject)?);
    }
    Ok(done)
}

/// One commit of `extract`: its copy on `tip`, or the conflicts.
fn extract_one(
    repo: &gix::Repository,
    lens: &Lens,
    prev: ObjectId,
    tip: ObjectId,
    commit: ObjectId,
) -> Result<std::result::Result<ObjectId, Vec<Conflict>>> {
    let (prev_tree, tip_tree) = (commit_tree(repo, prev)?, commit_tree(repo, tip)?);
    let commit_tree_id = commit_tree(repo, commit)?;
    // Hunk paths where the layers' lines are (prev differs from the tip) or
    // that the commit changed.
    let mut paths = BTreeSet::new();
    for (path, _) in
        delta(repo, tip_tree, prev_tree)?
            .into_iter()
            .chain(delta(repo, prev_tree, commit_tree_id)?)
    {
        if (lens.is_hunk)(path.as_ref()) {
            paths.insert(path);
        }
    }
    let (mut base_edits, mut ours_edits, mut conflicts) = (Vec::new(), Vec::new(), Vec::new());
    for path in &paths {
        let at = |tree| lookup(repo, tree, path.as_ref());
        let (t, p, c) = (at(tip_tree)?, at(prev_tree)?, at(commit_tree_id)?);
        match extract_blend(repo, t, p, c)? {
            Ok(item) => {
                base_edits.push((path.as_ref(), Change::from_item(t)));
                ours_edits.push((path.as_ref(), Change::from_item(item)));
            }
            Err(lines) => conflicts.push(Conflict {
                path: path.clone(),
                layers: (lens.owners)(path.as_ref()),
                lines,
            }),
        }
    }
    if !conflicts.is_empty() {
        return Ok(Err(conflicts));
    }
    // With the hunk paths at the tip's version in the base, the merge takes
    // the commit's (line-set) version of them as is.
    let base = edit_tree(repo, prev_tree, base_edits)?;
    let theirs = edit_tree(repo, commit_tree_id, ours_edits)?;
    let tree = match merge_trees(repo, base, tip_tree, theirs)? {
        Ok(tree) => tree,
        Err(paths) => {
            return Ok(Err(paths
                .into_iter()
                .map(|path| Conflict {
                    layers: (lens.owners)(path.as_ref()),
                    path,
                    lines: Vec::new(),
                })
                .collect()));
        }
    };
    let mut copy: gix::objs::Commit = repo.find_commit(commit)?.decode()?.try_into()?;
    copy.tree = tree;
    copy.parents = [tip].into_iter().collect();
    // A signature would not match the new commit; a merge's tags are gone;
    // an amended lens commit's mark mustn't reach the branch.
    copy.extra_headers.retain(|(k, _)| {
        !matches!(k.as_bytes(), b"gpgsig" | b"gpgsig-sha256" | b"mergetag")
            && k.as_bytes() != LENS_HEADER.as_bytes()
    });
    Ok(Ok(repo.write_object(&copy)?.detach()))
}

/// The line-set rule for one hunk path (see the module comment): the
/// entry the commit's change makes of the tip's `t`, given the commit's `c`
/// and its parent's `p` (which has the layers' lines), or the layers' lines
/// the commit removed or changed.
fn extract_blend(
    repo: &gix::Repository,
    t: Option<Item>,
    p: Option<Item>,
    c: Option<Item>,
) -> Result<std::result::Result<Option<Item>, Vec<BString>>> {
    if c == p {
        return Ok(Ok(t));
    }
    if p == t {
        return Ok(Ok(c));
    }
    let blob = |i: Option<Item>| {
        i.is_none_or(|i| matches!(i.kind, EntryKind::Blob | EntryKind::BlobExecutable))
    };
    let (td, pd, cd) = (
        read_blob(repo, t)?,
        read_blob(repo, p)?,
        read_blob(repo, c)?,
    );
    if !blob(t) || !blob(p) || !blob(c) || [&td, &pd, &cd].iter().any(|d| is_binary(d)) {
        // Not lines: the commit changed a layer's version of the path.
        return Ok(Err(Vec::new()));
    }
    let tip_lines: HashSet<&[u8]> = lines(&td).map(key).collect();
    let prev_lines: HashSet<&[u8]> = lines(&pd).map(key).collect();
    let commit_lines: HashSet<&[u8]> = lines(&cd).map(key).collect();
    let removed: HashSet<&[u8]> = prev_lines.difference(&commit_lines).copied().collect();
    let mut theirs = Vec::new();
    for line in lines(&pd).map(key) {
        if removed.contains(line)
            && !tip_lines.contains(line)
            && !theirs.iter().any(|l: &BString| l == line)
        {
            theirs.push(line.into());
        }
    }
    if !theirs.is_empty() {
        return Ok(Err(theirs));
    }
    let Some(c) = c else {
        // Deleted, with no layer's line in it: delete it on the tip too.
        return Ok(Ok(None));
    };
    let merged = match merge3(&pd, &td, &cd, keep_conflicts()) {
        Some(merged) => merged,
        None => {
            let mut out = Vec::new();
            for line in lines(&td).filter(|l| !removed.contains(key(l))) {
                push_line(&mut out, line);
            }
            let mut added = HashSet::new();
            for line in lines(&cd) {
                let k = key(line);
                if !prev_lines.contains(k) && !tip_lines.contains(k) && added.insert(k) {
                    push_line(&mut out, line);
                }
            }
            out
        }
    };
    let oid = repo.write_blob(&merged)?.detach();
    Ok(Ok(Some(Item { kind: c.kind, oid })))
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

/// Moves the worktree with admin directory `admin` after an extraction, in
/// one `git update-ref` transaction: branch `branch` from `tip.0` to `tip.1`
/// (unless equal) and the detached HEAD from `head.0` (the agent's last
/// commit) to `head.1` (L'). Either both move or, if either was moved
/// meanwhile, neither does and this fails.
pub fn advance(
    admin: &Path,
    branch: &str,
    tip: (ObjectId, ObjectId),
    head: (ObjectId, ObjectId),
) -> Result<()> {
    let mut script = String::new();
    if tip.0 != tip.1 {
        script += &format!("update refs/heads/{branch} {} {}\n", tip.1, tip.0);
    }
    script += &format!("update HEAD {} {}\n", head.1, head.0);
    let args = [
        "update-ref",
        "--no-deref",
        "-m",
        "gitfold: extract",
        "--stdin",
    ];
    super::cmd::output_with_input(admin, &args, script.into_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Output};

    /// A scratch repository, driven with the git CLI in an isolated
    /// configuration.
    struct Repo {
        dir: PathBuf,
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    impl Repo {
        fn new(name: &str) -> Repo {
            let dir =
                std::env::temp_dir().join(format!("gitfold-lens-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("repo")).unwrap();
            let repo = Repo { dir };
            repo.git(&["init", "-q", "-b", "main"]);
            repo
        }

        fn work(&self) -> PathBuf {
            self.dir.join("repo")
        }

        fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.dir)
                .env("LC_ALL", "C")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Agent")
                .env("GIT_AUTHOR_EMAIL", "agent@example.com")
                .env("GIT_COMMITTER_NAME", "Committer")
                .env("GIT_COMMITTER_EMAIL", "committer@example.com")
                .output()
                .unwrap()
        }

        /// Runs git in `dir`, which must succeed; its output, trimmed.
        fn git_in(&self, dir: &Path, args: &[&str]) -> String {
            let out = self.run_in(dir, args);
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout)
                .unwrap()
                .trim_end()
                .to_string()
        }

        fn git(&self, args: &[&str]) -> String {
            self.git_in(&self.work(), args)
        }

        fn id(&self, spec: &str) -> ObjectId {
            ObjectId::from_hex(self.git(&["rev-parse", spec]).as_bytes()).unwrap()
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.work().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        /// Commits everything on the current branch; the commit.
        fn commit(&self, message: &str) -> ObjectId {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
            self.id("HEAD")
        }

        fn open(&self) -> gix::Repository {
            gix::open_opts(self.work(), gix::open::Options::isolated()).unwrap()
        }

        /// `git cat-file -p <object>:<path>`, trimmed.
        fn show(&self, object: ObjectId, path: &str) -> String {
            self.git(&["cat-file", "-p", &format!("{object}:{path}")])
        }

        /// `git diff --name-status` between two trees or commits, renames
        /// not followed.
        fn diff(&self, from: ObjectId, to: ObjectId) -> String {
            let (from, to) = (from.to_string(), to.to_string());
            self.git(&["diff", "--no-renames", "--name-status", &from, &to])
        }

        /// `git merge-tree --write-tree` of three trees or commits: the tree,
        /// or `None` on conflicts.
        fn merge_tree(&self, base: ObjectId, ours: ObjectId, theirs: ObjectId) -> Option<ObjectId> {
            let base = format!("--merge-base={base}");
            let (ours, theirs) = (ours.to_string(), theirs.to_string());
            let args = ["merge-tree", "--write-tree", &base, &ours, &theirs];
            let out = self.run_in(&self.work(), &args);
            let first = String::from_utf8(out.stdout).unwrap();
            let first = first.lines().next().unwrap_or_default().to_string();
            match out.status.code() {
                Some(0) => Some(ObjectId::from_hex(first.as_bytes()).unwrap()),
                Some(1) => None,
                _ => panic!("merge-tree: {}", String::from_utf8_lossy(&out.stderr)),
            }
        }
    }

    fn lines(n: usize, prefix: &str) -> String {
        (1..=n).map(|i| format!("{prefix}{i}\n")).collect()
    }

    /// The scenario of 03-experiments §1: a base, and branches `contracts`,
    /// `a` and `b` with one commit each on it (the seals). Returns the base.
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

    fn stack(r: &Repo, base: ObjectId, names: &[&str]) -> Vec<Layer> {
        let layer = |name: &&str| Layer {
            seal: r.id(name),
            merge_base: base,
        };
        names.iter().map(layer).collect()
    }

    fn registry(path: &BStr) -> bool {
        path == "registry.txt"
    }

    /// The layers whose content is at `path` in M (`Lens::owners`).
    fn owners_of(view: &View, path: &BStr) -> Vec<usize> {
        match view.mixed.get(path) {
            Some(layer) => vec![*layer],
            None => view.blended.get(path).cloned().unwrap_or_default(),
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
        let view = build_view(&repo, base, &stack(&r, base, &names), &registries).unwrap();
        assert_eq!(view.notes, []);
        let base_tree = tree_of(&r, base);
        assert_eq!(r.diff(base_tree, view.hunk_tree), "M\tregistry.txt");
        assert_eq!(
            r.show(view.hunk_tree, "registry.txt"),
            "entry_base1\nentry_base2\nentry_base3\nentry_a"
        );
        assert_eq!(view.blended.get(BStr::new("registry.txt")), Some(&vec![1]));
        assert_eq!(view.blended.len(), 1);
        assert_eq!(view.hunk_layers(), [1]);
        let mixed: Vec<_> = view.mixed.iter().map(|(p, l)| format!("{p}:{l}")).collect();
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

        // L and V, as git sees them.
        let seals: Vec<_> = names.iter().map(|n| r.id(n)).collect();
        let subject = lens_subject("main", &names);
        let l = lens_commit(&repo, view.hunk_tree, base, &seals[1..2], &subject).unwrap();
        let v = view_commit(&repo, view.view_tree, l, &seals, "gitfold view").unwrap();
        assert!(is_lens(&repo, l).unwrap());
        assert!(!is_lens(&repo, v).unwrap());
        assert!(!is_lens(&repo, base).unwrap());
        let l_hex = l.to_string();
        assert_eq!(
            r.git(&["log", "-1", "--format=%P%n%s", &l_hex]),
            format!("{base} {}\n{subject}", seals[1])
        );
        assert!(
            r.git(&["cat-file", "-p", &l_hex])
                .contains("\ngitfold-lens 1\n")
        );
        r.git(&["update-ref", "refs/gitfold-lens/test", &v.to_string()]);
        r.git(&["fsck", "--strict", "--no-dangling"]);
    }

    /// The base moved on since the layers' merge base: a whole-file layer
    /// hides the base's newer change (reported), a blended path keeps both.
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
        let view = build_view(&repo, new, &stack(&r, old, &["a"]), &registries).unwrap();
        assert_eq!(
            view.notes,
            [Note::HidesBase {
                layer: 0,
                path: "src/util.c".into()
            }]
        );
        assert!(!r.show(view.view_tree, "src/util.c").contains("UP"));
        assert_eq!(
            r.show(view.hunk_tree, "registry.txt"),
            "entry_base1\nentry_base2\nentry_base3\nentry_up\nentry_a"
        );
    }

    /// Two layers append to the registry: a plain 3-way merge conflicts
    /// (git's too), the blend keeps both. Two layers changing the same line
    /// differently: the upper one's change is dropped and reported.
    #[test]
    fn registry_blends() {
        let r = Repo::new("blend");
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

        let view = build_view(&repo, base, &stack(&r, base, &["x", "y"]), &registries).unwrap();
        assert_eq!(view.notes, []);
        assert_eq!(
            r.show(view.view_tree, "registry.txt"),
            "entry_base1\nentry_base2\nentry_base3\nentry_x\nentry_y"
        );
        assert_eq!(
            view.blended.get(BStr::new("registry.txt")),
            Some(&vec![0, 1])
        );

        let view =
            build_view(&repo, base, &stack(&r, base, &["p", "x", "q"]), &registries).unwrap();
        assert_eq!(
            view.notes,
            [Note::NotBlended {
                layer: 2,
                path: "registry.txt".into()
            }]
        );
        assert_eq!(
            r.show(view.view_tree, "registry.txt"),
            "entry_base1\nentry_base2 P\nentry_base3\nentry_x"
        );
        assert_eq!(
            view.blended.get(BStr::new("registry.txt")),
            Some(&vec![0, 1])
        );
    }

    /// The lensed worktree as gitfold would serve it: HEAD detached at L,
    /// the files of M, skip-worktree on the layers' paths L has and the
    /// layers' new paths excluded. Returns the worktree's directory.
    fn lensed_worktree(r: &Repo, l: ObjectId, view: &View) -> PathBuf {
        let wt = r.dir.join("mine");
        let wt_str = wt.to_str().unwrap();
        r.git(&["worktree", "add", "-q", "--detach", wt_str, &l.to_string()]);
        let mut excludes = String::new();
        let lt = tree_of(r, l);
        let repo = r.open();
        for path in view.mixed.keys() {
            let file = wt.join(path.to_str().unwrap());
            if file.is_file() || file.is_symlink() {
                fs::remove_file(&file).unwrap();
            }
        }
        for path in view.mixed.keys() {
            let p = path.to_str().unwrap();
            let file = wt.join(p);
            if lookup(&repo, lt, path.as_ref()).unwrap().is_some() {
                r.git_in(&wt, &["update-index", "--skip-worktree", p]);
            } else {
                excludes += &format!("/{p}\n");
            }
            let Some(item) = lookup(&repo, view.view_tree, path.as_ref()).unwrap() else {
                continue;
            };
            if item.kind == EntryKind::Tree {
                // A file replaced by a directory: its files are mixed paths too.
                continue;
            }
            let data = repo.find_blob(item.oid).unwrap().take_data();
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            if item.kind == EntryKind::Link {
                std::os::unix::fs::symlink(data.to_str().unwrap(), &file).unwrap();
            } else {
                fs::write(&file, data).unwrap();
                if item.kind == EntryKind::BlobExecutable {
                    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
        }
        fs::write(r.work().join(".git/info/exclude"), excludes).unwrap();
        wt
    }

    /// 03-experiments §4: commits made on L are extracted onto the branch
    /// with only my lines, git's merge agrees, and after `advance` HEAD is L'
    /// and `git status` is clean.
    #[test]
    fn extraction() {
        let r = Repo::new("extract");
        let base = scenario(&r);
        r.git(&["branch", "mine", "main"]);
        let repo = r.open();
        let names = ["contracts", "a", "b"];
        let view = build_view(&repo, base, &stack(&r, base, &names), &registries).unwrap();
        let seals: Vec<_> = names.iter().map(|n| r.id(n)).collect();
        let subject = lens_subject("mine", &names);
        let l = lens_commit(&repo, view.hunk_tree, base, &seals, &subject).unwrap();
        let wt = lensed_worktree(&r, l, &view);
        assert_eq!(
            r.git_in(&wt, &["status", "--porcelain"]),
            "",
            "lensed and clean"
        );

        let edit = |path: &str, from: &str, to: &str| {
            let text = fs::read_to_string(wt.join(path)).unwrap();
            fs::write(wt.join(path), text.replace(from, to)).unwrap();
        };
        edit("README.md", "readme", "readme MINE");
        edit("src/main.c", "main5\n", "main5 MINE\n");
        edit("registry.txt", "entry_a\n", "entry_a\nentry_mine\n");
        fs::write(wt.join("src/mine.c"), "mine\n").unwrap();
        r.git_in(&wt, &["add", "-A"]);
        r.git_in(&wt, &["commit", "-q", "-m", "mine1\n\nbody"]);
        edit("src/main.c", "main1\n", "main1 MINE\n");
        r.git_in(&wt, &["commit", "-q", "-am", "mine2"]);
        let head = ObjectId::from_hex(r.git_in(&wt, &["rev-parse", "HEAD"]).as_bytes()).unwrap();

        let commits = chain(&repo, l, head, 10).unwrap().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(chain(&repo, base, l, 10).unwrap(), Some(vec![l]));
        assert_eq!(chain(&repo, seals[0], head, 10).unwrap(), None);
        let owners = |p: &BStr| owners_of(&view, p);
        let lens = Lens {
            seals: &seals,
            subject: &subject,
            is_hunk: &registry,
            owners: &owners,
        };
        let out = extract(&repo, &lens, l, base, &commits).unwrap();
        assert!(out.blocked.is_none(), "{:?}", out.blocked);
        let [(c1, c1x), (c2, c2x)] = out.extracted[..] else {
            panic!("{out:?}")
        };
        assert_eq!((c1, c2, out.tip), (commits[0], commits[1], c2x));

        // Only my changes, my registry line without A's.
        assert_eq!(
            r.diff(base, c2x),
            "M\tREADME.md\nM\tregistry.txt\nM\tsrc/main.c\nA\tsrc/mine.c"
        );
        assert_eq!(
            r.show(c1x, "registry.txt"),
            "entry_base1\nentry_base2\nentry_base3\nentry_mine"
        );
        let format = "--format=%an <%ae> %ad|%cn <%ce> %cd|%B|%P";
        let log = |c: ObjectId| r.git(&["log", "-1", format, &c.to_string()]);
        let (orig, copy) = (log(c1), log(c1x));
        assert_eq!(
            orig.rsplit_once('|').unwrap().0,
            copy.rsplit_once('|').unwrap().0
        );
        assert!(copy.ends_with(&format!("|{base}")));
        assert!(log(c2x).ends_with(&format!("|{c1x}")));
        // The second commit, which touches no hunk path, as git merges it.
        assert_eq!(Some(tree_of(&r, c2x)), r.merge_tree(c1, c1x, c2));

        // L': the agent's tree, on the new tip and the seals.
        let l2 = out.lens.unwrap();
        assert_eq!(tree_of(&r, l2), tree_of(&r, head));
        let parents: Vec<_> = std::iter::once(c2x).chain(seals.iter().copied()).collect();
        let parents: Vec<_> = parents.iter().map(ObjectId::to_string).collect();
        assert_eq!(
            r.git(&["log", "-1", "--format=%P", &l2.to_string()]),
            parents.join(" ")
        );
        assert!(is_lens(&repo, l2).unwrap());

        let admin = r.work().join(".git/worktrees/mine");
        advance(&admin, "mine", (base, c2x), (head, l2)).unwrap();
        assert_eq!(r.id("mine"), c2x);
        assert_eq!(r.git_in(&wt, &["rev-parse", "HEAD"]), l2.to_string());
        assert_eq!(
            r.git_in(&wt, &["status", "--porcelain"]),
            "",
            "extraction is invisible"
        );
        // Moved meanwhile: neither ref moves.
        assert!(advance(&admin, "mine", (base, c1x), (l2, head)).is_err());
        assert!(advance(&admin, "mine", (c2x, c1x), (head, l)).is_err());
        assert_eq!(r.id("mine"), c2x);
        assert_eq!(r.git_in(&wt, &["rev-parse", "HEAD"]), l2.to_string());
    }

    /// 03-experiments §4 (iii)-(v): a commit that changes a layer's registry
    /// line, or edits next to a layer's line in a file whose layer version L
    /// holds (promoted), is refused with the path and the layer; an edit
    /// away from the layer's line extracts with only my line.
    #[test]
    fn dependent_changes_are_refused() {
        let r = Repo::new("depends");
        let base = scenario(&r);
        let repo = r.open();
        let names = ["contracts", "a"];
        let view = build_view(&repo, base, &stack(&r, base, &names), &registries).unwrap();
        let seals: Vec<_> = names.iter().map(|n| r.id(n)).collect();
        // L with A's util.c promoted into it.
        let util = lookup(&repo, tree_of(&r, seals[1]), "src/util.c".into());
        let promoted = [(BStr::new("src/util.c"), Change::Set(util.unwrap().unwrap()))];
        let hunk_tree = edit_tree(&repo, view.hunk_tree, promoted).unwrap();
        let l = lens_commit(&repo, hunk_tree, base, &seals, "lens").unwrap();
        let owners = |p: &BStr| owners_of(&view, p);
        let lens = Lens {
            seals: &seals,
            subject: "lens",
            is_hunk: &registry,
            owners: &owners,
        };
        // A commit on L changing `path` with `edit`.
        let commit_on_l = |path: &str, edit: &dyn Fn(&str) -> String| {
            let old = r.show(l, path) + "\n";
            let item = Item {
                kind: EntryKind::Blob,
                oid: repo.write_blob(edit(&old)).unwrap().detach(),
            };
            let tree = edit_tree(&repo, hunk_tree, [(BStr::new(path), Change::Set(item))]);
            write_commit(&repo, tree.unwrap(), l, &[], "mine", Vec::new()).unwrap()
        };

        let changed = commit_on_l("registry.txt", &|t| t.replace("entry_a", "entry_a_v2"));
        let out = extract(&repo, &lens, l, base, &[changed]).unwrap();
        assert!(out.extracted.is_empty() && out.lens.is_none() && out.tip == base);
        let blocked = out.blocked.unwrap();
        assert_eq!(blocked.commit, changed);
        assert_eq!(
            blocked.conflicts,
            [Conflict {
                path: "registry.txt".into(),
                layers: vec![1],
                lines: vec!["entry_a".into()],
            }]
        );

        // A registry line away from A's keeps its place.
        let inserted = commit_on_l("registry.txt", &|t| {
            t.replace("entry_base1\n", "entry_base1\nentry_mine\n")
        });
        let out = extract(&repo, &lens, l, base, &[inserted]).unwrap();
        assert!(out.blocked.is_none());
        assert_eq!(
            r.show(out.tip, "registry.txt"),
            "entry_base1\nentry_mine\nentry_base2\nentry_base3"
        );

        let adjacent = commit_on_l("src/util.c", &|t| t.replace("util4", "util4 MINE"));
        let out = extract(&repo, &lens, l, base, &[adjacent]).unwrap();
        assert_eq!(
            out.blocked.unwrap().conflicts,
            [Conflict {
                path: "src/util.c".into(),
                layers: vec![1],
                lines: Vec::new(),
            }]
        );
        assert_eq!(r.merge_tree(l, base, adjacent), None, "git conflicts too");

        let away = commit_on_l("src/util.c", &|t| t.replace("util8", "util8 MINE"));
        let out = extract(&repo, &lens, l, base, &[away]).unwrap();
        assert!(out.blocked.is_none());
        let expected = lines(8, "util").replace("util8", "util8 MINE");
        assert_eq!(r.show(out.tip, "src/util.c"), expected.trim_end());
        assert_eq!(r.merge_tree(l, base, away), Some(tree_of(&r, out.tip)));
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
