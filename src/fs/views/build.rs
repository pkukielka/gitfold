//! A view's content: a branch's files with the changes of the branches built
//! on it merged in, path by path.
//!
//! Each member's change is its files against its merge point (where it left
//! its parent), so it brings only its own work. The changes are merged in
//! turn, three-way, into what the view holds so far, starting from the
//! branch's own files: a path only one side changed takes that side's
//! version without reading any content; a path both changed is merged as
//! `git merge-file` does (`merge`), conflicts kept as markers named by
//! branch. Where no markers can show a conflict, a changed file wins over a
//! deletion and otherwise the side merged earlier stays; every conflict is
//! recorded (`Conflict`). Unlike git's merge (accepted 2026-11-16): renames
//! aren't followed, a merge point is one merge base (no virtual base for
//! criss-cross histories), and submodules changed on both sides conflict
//! even when one is a fast-forward of the other.
//!
//! A view is the branch's tree (`View::lower`, a store tree) and the paths
//! where it differs (`View::overlay`): files only, a directory being the files
//! in it. Nothing is written to git.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::ops::Bound;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gix::objs::tree::EntryKind;

use super::merge::{self, ConflictStyle, Merged};
use crate::git;
use crate::state::{ancestors, join};
use crate::store::{EMPTY_TREE, Store, TEntry};

/// What a path holds: a file, a symlink or a submodule.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// One in a git tree (a store file id).
    Stored(u32),
    /// A workspace's uncommitted file or symlink, at `file` in its upper
    /// directory.
    #[allow(dead_code)] // made by `@live`'s views (V3)
    Upper { file: PathBuf, kind: EntryKind },
    /// Content a merge made.
    Merged { data: Arc<Vec<u8>>, kind: EntryKind },
}

impl Entry {
    pub fn kind(&self, store: &Store) -> Result<EntryKind> {
        Ok(match self {
            Entry::Stored(id) => store.file(*id).context("stored file")?.kind,
            Entry::Upper { kind, .. } | Entry::Merged { kind, .. } => *kind,
        })
    }

    /// Its content (a symlink's target).
    pub fn bytes(&self, store: &Store) -> Result<Cow<'_, [u8]>> {
        Ok(match self {
            Entry::Stored(id) => {
                Cow::Owned(git::read_blob(store.file(*id).context("stored file")?.oid)?)
            }
            Entry::Upper { file, kind } if *kind == EntryKind::Link => {
                Cow::Owned(fs::read_link(file)?.into_os_string().into_vec())
            }
            Entry::Upper { file, .. } => {
                Cow::Owned(fs::read(file).with_context(|| format!("reading {}", file.display()))?)
            }
            Entry::Merged { data, .. } => Cow::Borrowed(data.as_slice()),
        })
    }
}

/// Paths set (`Some`) or deleted (`None`) over a tree.
pub type Changes = BTreeMap<Vec<u8>, Option<Entry>>;

/// A branch's files: a tree (its tip, or its workspace's base) with its
/// uncommitted changes over it (none for the committed view).
pub struct Files {
    pub tree: u32,
    pub uncommitted: Changes,
}

/// A branch merged into a view.
pub struct Member {
    pub branch: String,
    /// The tree of its merge point: where it left its parent.
    pub merge_point: u32,
    pub files: Files,
}

/// A view's content: `lower` with `overlay` over it.
pub struct View {
    pub lower: u32,
    /// The paths where the view differs from `lower`.
    pub overlay: Changes,
    pub conflicts: Vec<Conflict>,
}

impl View {
    /// What the view shows at `path`.
    #[cfg(test)]
    pub fn entry(&self, store: &Store, path: &[u8]) -> Option<Entry> {
        shown(store, self.lower, &self.overlay, path)
    }
}

/// What `lower` with `overlay` over it shows at `path`.
fn shown(store: &Store, lower: u32, overlay: &Changes, path: &[u8]) -> Option<Entry> {
    match overlay.get(path) {
        Some(entry) => entry.clone(),
        None => stored(store, lower, path),
    }
}

/// Where branches' changes conflict.
#[derive(Debug, PartialEq)]
pub struct Conflict {
    pub path: Vec<u8>,
    pub kind: ConflictKind,
    /// The branches whose changes meet there: those merged first, then the
    /// one that conflicted.
    pub branches: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConflictKind {
    /// Shown with git's markers in the file.
    Markers,
    /// Changed on one side, deleted on the other: the changed file shows.
    ModifiedDeleted,
    /// Changed on both sides, one of them binary: the earlier side shows.
    Binary,
    /// Changed on both sides, not both into regular files (a symlink, a
    /// submodule): the earlier side shows.
    Kind,
    /// The executable bit changed differently on both sides: the content is
    /// merged (and may hold markers too), with the earlier side's bit.
    Mode,
    /// A file where the other side has files below it (or below where it
    /// has a file): the earlier side shows.
    FileDirectory,
}

impl ConflictKind {
    /// As the view says it (its summary, the files' attribute).
    pub fn name(self) -> &'static str {
        match self {
            ConflictKind::Markers => "markers",
            ConflictKind::ModifiedDeleted => "modified-deleted",
            ConflictKind::Binary => "binary",
            ConflictKind::Kind => "kind",
            ConflictKind::Mode => "mode",
            ConflictKind::FileDirectory => "file-directory",
        }
    }
}

/// The view of `branch`, its `files`, with `members` merged in order (each
/// after the one it is built on); conflict markers in `style`.
pub fn build(
    store: &Store,
    branch: &str,
    files: Files,
    members: &[Member],
    style: ConflictStyle,
) -> Result<View> {
    let lower = files.tree;
    let mut acc = Acc {
        store,
        branch,
        style,
        lower,
        overlay: files.uncommitted,
        who: HashMap::new(),
        conflicts: Vec::new(),
    };
    for member in members {
        acc.add(member)?;
    }
    let Acc {
        mut overlay,
        conflicts,
        ..
    } = acc;
    // A change can come back to the branch's own file.
    overlay.retain(|path, entry| match entry {
        Some(Entry::Stored(_)) | None => *entry != stored(store, lower, path),
        Some(_) => true,
    });
    Ok(View {
        lower,
        overlay,
        conflicts,
    })
}

/// A view being built.
struct Acc<'a> {
    store: &'a Store,
    branch: &'a str,
    style: ConflictStyle,
    lower: u32,
    overlay: Changes,
    /// The branches whose changes a path holds, the view's branch first; a
    /// path missing holds only the view's branch.
    who: HashMap<Vec<u8>, Vec<String>>,
    conflicts: Vec<Conflict>,
}

impl Acc<'_> {
    /// Merges `member`'s change in.
    fn add(&mut self, member: &Member) -> Result<()> {
        let files = &member.files;
        let (from, to) = (member.merge_point, files.tree);
        let mut changes = BTreeMap::new();
        delta(self.store, from, to, &mut Vec::new(), &mut changes);
        for (path, entry) in &files.uncommitted {
            let before = match changes.remove(path) {
                Some((before, _)) => before,
                None => stored(self.store, from, path),
            };
            if before != *entry {
                changes.insert(path.clone(), (before, entry.clone()));
            }
        }
        // Deletions first: a directory replaced by a file is gone before the
        // file comes (each part in path order).
        let (deleted, set): (Vec<_>, Vec<_>) =
            (changes.into_iter()).partition(|(_, (_, after))| after.is_none());
        for (path, (before, after)) in deleted.into_iter().chain(set) {
            self.merge(&member.branch, path, before, after)?;
        }
        Ok(())
    }

    /// Merges `name`'s change of `path`, from `before` to `after`.
    fn merge(
        &mut self,
        name: &str,
        path: Vec<u8>,
        before: Option<Entry>,
        after: Option<Entry>,
    ) -> Result<()> {
        let ours = self.entry(&path);
        if ours == after {
            return Ok(());
        }
        if ours == before {
            self.take(name, path, after);
            return Ok(());
        }
        match (ours, after) {
            (None, Some(after)) => {
                let branches = self.involved(&path, name);
                if self.take(name, path.clone(), Some(after)) {
                    self.record(path, ConflictKind::ModifiedDeleted, branches);
                }
            }
            (Some(_), None) => {
                let branches = self.involved(&path, name);
                self.record(path, ConflictKind::ModifiedDeleted, branches);
            }
            (Some(ours), Some(after)) => self.content(name, path, before, ours, after)?,
            (None, None) => unreachable!("ours == after"),
        }
        Ok(())
    }

    /// Merges the content of `path`, changed on both sides.
    fn content(
        &mut self,
        name: &str,
        path: Vec<u8>,
        before: Option<Entry>,
        ours: Entry,
        after: Entry,
    ) -> Result<()> {
        let store = self.store;
        let before_kind = before.as_ref().map(|e| e.kind(store)).transpose()?;
        let branches = self.involved(&path, name);
        let merged = merged_kind(before_kind, ours.kind(store)?, after.kind(store)?);
        let Some((kind, modes_conflict)) = merged else {
            self.record(path, ConflictKind::Kind, branches);
            return Ok(());
        };
        // As git: a base that wasn't a regular file merges as none (two-way).
        let base = match (&before, before_kind) {
            (Some(e), Some(k)) if is_blob(k) => e.bytes(store)?,
            _ => Cow::Borrowed(&[][..]),
        };
        let ours_label = branches[..branches.len() - 1].join(" + ");
        let (mine, theirs) = (ours.bytes(store)?, after.bytes(store)?);
        let (data, markers) =
            match merge::merge3(&base, &mine, &theirs, &ours_label, name, self.style) {
                Merged::Binary => {
                    self.record(path, ConflictKind::Binary, branches);
                    return Ok(());
                }
                Merged::Markers(data) => (data, true),
                Merged::Clean(data) => (data, false),
            };
        if modes_conflict {
            self.record(path.clone(), ConflictKind::Mode, branches.clone());
        }
        if markers {
            self.record(path.clone(), ConflictKind::Markers, branches);
        }
        let entry = Entry::Merged {
            data: Arc::new(data),
            kind,
        };
        self.take(name, path, Some(entry));
        Ok(())
    }

    /// Sets `path` to `entry` from `name`, unless a file would come where
    /// nothing is and sit above or below other files: that is recorded
    /// instead. Whether it was set.
    fn take(&mut self, name: &str, path: Vec<u8>, entry: Option<Entry>) -> bool {
        if entry.is_some()
            && self.entry(&path).is_none()
            && let Some(clash) = self.clash(&path)
        {
            // The branches of what is there, then this one.
            let branches = self.involved(&clash, name);
            self.record(path, ConflictKind::FileDirectory, branches);
            return false;
        }
        self.overlay.insert(path.clone(), entry);
        let branch = self.branch;
        (self.who.entry(path))
            .or_insert_with(|| vec![branch.to_string()])
            .push(name.to_string());
        true
    }

    /// A file the view holds that a file at `path` would sit below, or one
    /// it would sit above, if any.
    fn clash(&self, path: &[u8]) -> Option<Vec<u8>> {
        // One walk down `lower` along `path`.
        let last = |p: &[u8]| -> usize { p.iter().rposition(|b| *b == b'/').map_or(0, |i| i + 1) };
        let mut tree = Some(self.lower);
        for above in ancestors(path).chain([path]) {
            let lower = tree.and_then(|t| self.store.tree(t).find(&above[last(above)..]).copied());
            tree = lower.filter(|e| e.kind == EntryKind::Tree).map(|e| e.id);
            if above.len() == path.len() {
                break;
            }
            let file = match self.overlay.get(above) {
                Some(entry) => entry.is_some(),
                None => lower.is_some_and(|e| e.kind != EntryKind::Tree),
            };
            if file {
                return Some(above.to_vec());
            }
        }
        let prefix = [path, b"/"].concat();
        let below = (Bound::Included(&prefix[..]), Bound::Unbounded);
        let overlaid = (self.overlay.range::<[u8], _>(below))
            .take_while(|(p, _)| p.starts_with(&prefix))
            .find(|(_, e)| e.is_some());
        if let Some((p, _)) = overlaid {
            return Some(p.clone());
        }
        self.shown_below(path, tree?)
    }

    /// A file of `lower`'s tree `tree` at `dir` that isn't deleted, if any.
    fn shown_below(&self, dir: &[u8], tree: u32) -> Option<Vec<u8>> {
        let table = self.store.tree(tree);
        table.entries().iter().find_map(|e| {
            let path = join(dir, table.name(e));
            match e.kind {
                EntryKind::Tree => self.shown_below(&path, e.id),
                _ => (self.overlay.get(&path) != Some(&None)).then_some(path),
            }
        })
    }

    /// What the view holds at `path` so far.
    fn entry(&self, path: &[u8]) -> Option<Entry> {
        shown(self.store, self.lower, &self.overlay, path)
    }

    /// The branches whose changes meet at `path` when `name`'s comes.
    fn involved(&self, path: &[u8], name: &str) -> Vec<String> {
        let mut branches =
            (self.who.get(path).cloned()).unwrap_or_else(|| vec![self.branch.into()]);
        branches.push(name.to_string());
        branches
    }

    fn record(&mut self, path: Vec<u8>, kind: ConflictKind, branches: Vec<String>) {
        self.conflicts.push(Conflict {
            path,
            kind,
            branches,
        });
    }
}

fn is_blob(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::Blob | EntryKind::BlobExecutable)
}

/// The kind a content merge of `ours` and `theirs` (changed from `before`)
/// gets, as git's merge decides it, and whether the executable bits
/// conflict; `None` if they can't be merged: only two regular files can. A
/// change of the executable bit on one side wins; changed differently on
/// both, ours stays.
fn merged_kind(
    before: Option<EntryKind>,
    ours: EntryKind,
    theirs: EntryKind,
) -> Option<(EntryKind, bool)> {
    if !is_blob(ours) || !is_blob(theirs) {
        return None;
    }
    Some(if ours == theirs || before == Some(ours) {
        (theirs, false)
    } else {
        (ours, before != Some(theirs))
    })
}

/// What a change does to one path: from, to.
pub(super) type Change = (Option<Entry>, Option<Entry>);

/// The files, symlinks and submodules that differ between trees `from` and
/// `to` below `dir` (a directory and a file at one path: the directory's
/// files and the file), with what each is in both. Walks only subtrees that
/// differ.
pub(super) fn delta(
    store: &Store,
    from: u32,
    to: u32,
    dir: &mut Vec<u8>,
    out: &mut BTreeMap<Vec<u8>, Change>,
) {
    if from == to {
        return;
    }
    let (a, b) = (store.tree(from), store.tree(to));
    let mut names: BTreeMap<&[u8], (Option<TEntry>, Option<TEntry>)> = BTreeMap::new();
    for e in a.entries() {
        names.entry(a.name(e)).or_default().0 = Some(*e);
    }
    for e in b.entries() {
        names.entry(b.name(e)).or_default().1 = Some(*e);
    }
    let subtree = |e: Option<TEntry>| e.filter(|e| e.kind == EntryKind::Tree).map(|e| e.id);
    // File ids differ by kind too.
    let leaf = |e: Option<TEntry>| (e.filter(|e| e.kind != EntryKind::Tree)).map(|e| e.id);
    for (name, (x, y)) in names {
        let len = dir.len();
        if len > 0 {
            dir.push(b'/');
        }
        dir.extend_from_slice(name);
        let (a, b) = (subtree(x), subtree(y));
        delta(
            store,
            a.unwrap_or(EMPTY_TREE),
            b.unwrap_or(EMPTY_TREE),
            dir,
            out,
        );
        if leaf(x) != leaf(y) {
            let entry = |id: Option<u32>| id.map(Entry::Stored);
            out.insert(dir.clone(), (entry(leaf(x)), entry(leaf(y))));
        }
        dir.truncate(len);
    }
}

/// The file, symlink or submodule of tree `tree` at `path`.
fn stored(store: &Store, tree: u32, path: &[u8]) -> Option<Entry> {
    (store.tree_entry_at(tree, path))
        .filter(|e| e.kind != EntryKind::Tree)
        .map(|e| Entry::Stored(e.id))
}

#[cfg(test)]
mod tests {
    use super::super::members::{Bases, committed, members, refs};
    use super::super::test_repo::{branch, commit, scratch, tip};
    use super::*;
    use crate::tree::Tree;

    /// The view of `n` with `children` (branch, parent) built on it, as committed.
    fn view(store: &Store, n: &str, children: &[(&str, &str)]) -> View {
        let stored = (children.iter())
            .map(|(c, p)| (c.to_string(), format!("refs/heads/{p}")))
            .collect();
        let tree = Tree::of(stored, Some("main".into()), |_| true);
        let mut bases = Bases::default();
        let members = members(&tree, n, None, |_| true);
        let r = refs(n, None, &members, &mut bases).unwrap();
        assert!(r.notes.is_empty());
        let (files, members) = committed(store, &r, &mut bases).unwrap();
        build(store, n, files, &members, ConflictStyle::Merge).unwrap()
    }

    /// What `view` shows at `path`, as text.
    fn show(store: &Store, view: &View, path: &str) -> Option<String> {
        let entry = view.entry(store, path.as_bytes())?;
        Some(String::from_utf8(entry.bytes(store).unwrap().into_owned()).unwrap())
    }

    fn conflict(path: &str, kind: ConflictKind, branches: &[&str]) -> Conflict {
        Conflict {
            path: path.into(),
            kind,
            branches: branches.iter().map(|b| b.to_string()).collect(),
        }
    }

    #[test]
    fn siblings_merge_cleanly() {
        let base = commit(
            None,
            &[("a", Some("1\n2\n3\n4\n5\n")), ("d/x", Some("x\n"))],
        );
        let api = branch("t1-api", &base);
        branch(
            "t1-ui",
            &commit(Some(&api), &[("a", Some("1\n2\n3\n4\nUI\n"))]),
        );
        let web = commit(
            Some(&api),
            &[("a", Some("WEB\n2\n3\n4\n5\n")), ("new", Some("n\n"))],
        );
        branch("t1-web", &web);
        let store = Store::default();
        let v = view(
            &store,
            "t1-api",
            &[("t1-ui", "t1-api"), ("t1-web", "t1-api")],
        );
        assert_eq!(show(&store, &v, "a").unwrap(), "WEB\n2\n3\n4\nUI\n");
        assert_eq!(show(&store, &v, "new").unwrap(), "n\n");
        assert_eq!(show(&store, &v, "d/x").unwrap(), "x\n");
        assert!(v.conflicts.is_empty());
        assert_eq!(v.overlay.len(), 2, "only what differs from api");
    }

    #[test]
    fn siblings_conflict_with_markers() {
        let api = branch("t2-api", &commit(None, &[("a", Some("1\n2\n3\n"))]));
        branch("t2-ui", &commit(Some(&api), &[("a", Some("1\nUI\n3\n"))]));
        branch("t2-web", &commit(Some(&api), &[("a", Some("1\nWEB\n3\n"))]));
        let store = Store::default();
        let v = view(
            &store,
            "t2-api",
            &[("t2-ui", "t2-api"), ("t2-web", "t2-api")],
        );
        assert_eq!(
            show(&store, &v, "a").unwrap(),
            "1\n<<<<<<< t2-api + t2-ui\nUI\n=======\nWEB\n>>>>>>> t2-web\n3\n"
        );
        let expected = conflict("a", ConflictKind::Markers, &["t2-api", "t2-ui", "t2-web"]);
        assert_eq!(v.conflicts, [expected]);
    }

    #[test]
    fn each_brings_only_its_own_work() {
        // ui left api at `api0`, api moved on; css left ui, ui moved on.
        let api0 = commit(None, &[("a", Some("1\n2\n3\n")), ("u", Some("u\n"))]);
        let ui0 = commit(Some(&api0), &[("a", Some("1\n2\nUI\n"))]);
        let css = commit(Some(&ui0), &[("c", Some("css\n"))]);
        branch("t3-css", &css);
        branch("t3-ui", &commit(Some(&ui0), &[("u", Some("u2\n"))]));
        branch(
            "t3-api",
            &commit(Some(&api0), &[("a", Some("API\n2\n3\n"))]),
        );
        let store = Store::default();
        let v = view(
            &store,
            "t3-api",
            &[("t3-ui", "t3-api"), ("t3-css", "t3-ui")],
        );
        assert_eq!(show(&store, &v, "a").unwrap(), "API\n2\nUI\n");
        assert_eq!(
            show(&store, &v, "u").unwrap(),
            "u2\n",
            "css's old copy of u isn't its change"
        );
        assert_eq!(show(&store, &v, "c").unwrap(), "css\n");
        assert!(v.conflicts.is_empty(), "{:?}", v.conflicts);
    }

    #[test]
    fn conflicts_without_markers() {
        let files = [
            ("gone", Some("1\n")),
            ("kept", Some("1\n")),
            ("bin", Some("b\0in")),
            ("link", Some("1\n")),
        ];
        let api = branch("t4-api", &commit(None, &files));
        let ui = [
            ("gone", None),
            ("kept", Some("ui\n")),
            ("bin", Some("ui\0")),
            ("link", Some("link:target")),
            ("d", Some("file\n")),
        ];
        branch("t4-ui", &commit(Some(&api), &ui));
        let web = [
            ("gone", Some("web\n")),
            ("kept", None),
            ("bin", Some("web\0")),
            ("link", Some("web\n")),
            ("d/x", Some("below\n")),
        ];
        branch("t4-web", &commit(Some(&api), &web));
        let store = Store::default();
        let v = view(
            &store,
            "t4-api",
            &[("t4-ui", "t4-api"), ("t4-web", "t4-api")],
        );
        assert_eq!(
            show(&store, &v, "gone").unwrap(),
            "web\n",
            "changed wins over deleted"
        );
        assert_eq!(show(&store, &v, "kept").unwrap(), "ui\n");
        assert_eq!(show(&store, &v, "bin").unwrap(), "ui\0", "the earlier side");
        assert_eq!(show(&store, &v, "link").unwrap(), "target");
        assert_eq!(show(&store, &v, "d").unwrap(), "file\n");
        assert_eq!(show(&store, &v, "d/x"), None);
        let both = ["t4-api", "t4-ui", "t4-web"];
        let mut got = v.conflicts;
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            got,
            [
                conflict("bin", ConflictKind::Binary, &both),
                conflict("d/x", ConflictKind::FileDirectory, &both),
                conflict("gone", ConflictKind::ModifiedDeleted, &both),
                conflict("kept", ConflictKind::ModifiedDeleted, &both),
                conflict("link", ConflictKind::Kind, &both),
            ]
        );
    }

    #[test]
    fn modes_and_kinds_as_git_merges_them() {
        let api = branch(
            "t7-api",
            &commit(None, &[("l", Some("link:1\n2\n3\n4\n5\n"))]),
        );
        let ui = [
            ("l", Some("UI\n2\n3\n4\n5\n")),
            ("x", Some("exec:a\nb\n")),
            ("m", Some("exec:m\n")),
        ];
        branch("t7-ui", &commit(Some(&api), &ui));
        let web = [
            ("l", Some("1\n2\n3\n4\nWEB\n")),
            ("x", Some("a\nb\n")),
            ("m", Some("m\nweb\n")),
        ];
        branch("t7-web", &commit(Some(&api), &web));
        let store = Store::default();
        let v = view(
            &store,
            "t7-api",
            &[("t7-ui", "t7-api"), ("t7-web", "t7-api")],
        );
        // A symlink made a file on both sides: merged without a base, as git.
        assert!(
            show(&store, &v, "l")
                .unwrap()
                .starts_with("<<<<<<< t7-api + t7-ui\n")
        );
        // Added on both sides with different executable bits: the content
        // merged, the earlier side's bit kept, a conflict recorded.
        assert_eq!(show(&store, &v, "x").unwrap(), "a\nb\n");
        let kind = |p: &str| v.entry(&store, p.as_bytes()).unwrap().kind(&store).unwrap();
        assert_eq!(kind("x"), EntryKind::BlobExecutable);
        assert_eq!(kind("m"), EntryKind::BlobExecutable);
        let both = ["t7-api", "t7-ui", "t7-web"];
        let mut got = v.conflicts;
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            got,
            [
                conflict("l", ConflictKind::Markers, &both),
                conflict("m", ConflictKind::Mode, &both),
                conflict("m", ConflictKind::Markers, &both),
                conflict("x", ConflictKind::Mode, &both),
            ]
        );
    }

    #[test]
    fn directory_replaced_by_a_file() {
        let api = branch(
            "t5-api",
            &commit(None, &[("d/x", Some("x\n")), ("d/y", Some("y\n"))]),
        );
        branch(
            "t5-ui",
            &commit(
                Some(&api),
                &[("d/x", None), ("d/y", None), ("d", Some("f\n"))],
            ),
        );
        let store = Store::default();
        let v = view(&store, "t5-api", &[("t5-ui", "t5-api")]);
        assert_eq!(show(&store, &v, "d").unwrap(), "f\n");
        assert_eq!(show(&store, &v, "d/x"), None);
        assert!(v.conflicts.is_empty(), "{:?}", v.conflicts);
    }

    #[test]
    fn uncommitted_changes() {
        let api = branch(
            "t6-api",
            &commit(None, &[("a", Some("1\n2\n3\n")), ("b", Some("b\n"))]),
        );
        let ui = commit(Some(&api), &[("a", Some("1\n2\nUI\n"))]);
        let store = Store::default();
        let dir = scratch("t6");
        let upper = |name: &str, data: &str| {
            let file = dir.join(name);
            fs::write(&file, data).unwrap();
            Some(Entry::Upper {
                file,
                kind: EntryKind::Blob,
            })
        };
        let tree = |c: &str| store.load_tree(tip(c).tree).unwrap();
        let files = Files {
            tree: tree(&api),
            uncommitted: [
                (b"a".to_vec(), upper("a", "API\n2\n3\n")),
                (b"b".to_vec(), None),
            ]
            .into_iter()
            .collect(),
        };
        let members = [Member {
            branch: "t6-ui".into(),
            merge_point: tree(&api),
            files: Files {
                tree: tree(&ui),
                uncommitted: [(b"new".to_vec(), upper("new", "n\n"))]
                    .into_iter()
                    .collect(),
            },
        }];
        let v = build(&store, "t6-api", files, &members, ConflictStyle::Merge).unwrap();
        assert_eq!(show(&store, &v, "a").unwrap(), "API\n2\nUI\n");
        assert_eq!(show(&store, &v, "b"), None);
        assert_eq!(show(&store, &v, "new").unwrap(), "n\n");
    }

    #[test]
    fn delta_walks_only_what_differs() {
        let one = commit(
            None,
            &[("a/b/c", Some("1")), ("a/d", Some("2")), ("e", Some("3"))],
        );
        let two = commit(
            Some(&one),
            &[("a/b/c", Some("x")), ("e", None), ("e/f", Some("4"))],
        );
        let store = Store::default();
        let (one, two) = (
            store.load_tree(tip(&one).tree).unwrap(),
            store.load_tree(tip(&two).tree).unwrap(),
        );
        let mut out = BTreeMap::new();
        delta(&store, one, two, &mut Vec::new(), &mut out);
        let paths: Vec<&[u8]> = out.keys().map(Vec::as_slice).collect();
        assert_eq!(paths, [&b"a/b/c"[..], b"e", b"e/f"]);
        assert_eq!(out[&b"e"[..]].1, None, "e is a directory now");
        assert_eq!(out[&b"e/f"[..]].0, None);
        assert_eq!(out[&b"a/b/c"[..]].0, stored(&store, one, b"a/b/c"));
    }
}
