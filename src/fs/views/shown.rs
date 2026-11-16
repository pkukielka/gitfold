//! A view as the mount shows it: its content (`build::View`) as directories
//! and files, its conflicts (an attribute on each file, and a summary file at
//! its root), and what changed since the view before it (`Shown::next`), to
//! tell the kernel about.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::SystemTime;

use gix::objs::tree::EntryKind;

use super::build::{self, Entry, View};
use super::members::Note;
use crate::inode;
use crate::state::{ancestors, join};
use crate::store::Store;

/// The summary file at a view's root: every conflict and note, one per line
/// (`<kind>\t<branches or ref>\t<path or why>`).
pub const SUMMARY: &[u8] = b".gitfold-conflicts";

/// The attribute naming a file's conflicts (`<kind>: <branches>`, several
/// separated by `; `).
pub const CONFLICT_XATTR: &[u8] = b"user.gitfold.conflict";

/// What a path of a view is.
#[derive(Clone, Debug, PartialEq)]
pub enum Obj {
    Dir,
    File(Entry),
}

pub struct Shown {
    pub view: View,
    /// Directories that hold only the overlay's files.
    added: BTreeSet<Vec<u8>>,
    /// Directories of `view.lower` whose files are all gone.
    emptied: BTreeSet<Vec<u8>>,
    /// Each conflicting path's attribute.
    conflicts: HashMap<Vec<u8>, String>,
    pub summary: Arc<Vec<u8>>,
    /// When paths last changed (those not here: never, `inode::file_time`).
    times: HashMap<Vec<u8>, SystemTime>,
    /// When this was made: the time of the directories.
    pub made: SystemTime,
}

impl Shown {
    /// `view` as shown, after `before` (none: the first): with the paths
    /// whose files or directories differ from it, sorted.
    pub fn next(
        store: &Store,
        view: View,
        notes: &[Note],
        before: Option<&Shown>,
    ) -> (Shown, Vec<Vec<u8>>) {
        let made = SystemTime::now();
        let mut added = BTreeSet::new();
        for (path, entry) in &view.overlay {
            if entry.is_some() {
                added.extend(ancestors(path).map(<[u8]>::to_vec));
            }
        }
        let emptied = emptied(store, &view, &added);
        let mut conflicts: HashMap<Vec<u8>, String> = HashMap::new();
        let mut summary = Vec::new();
        for c in &view.conflicts {
            let said = format!("{}: {}", c.kind.name(), c.branches.join(", "));
            (conflicts.entry(c.path.clone()))
                .and_modify(|s| *s = format!("{s}; {said}"))
                .or_insert(said);
            summary.extend_from_slice(c.kind.name().as_bytes());
            summary.push(b'\t');
            summary.extend_from_slice(c.branches.join(", ").as_bytes());
            summary.push(b'\t');
            summary.extend_from_slice(&c.path);
            summary.push(b'\n');
        }
        for n in notes {
            summary.extend_from_slice(format!("{}\t{}\t{}\n", n.kind, n.what, n.why).as_bytes());
        }
        let mut shown = Shown {
            view,
            added,
            emptied,
            conflicts,
            summary: Arc::new(summary),
            times: HashMap::new(),
            made,
        };
        let Some(before) = before else {
            for path in shown.view.overlay.keys() {
                shown.times.insert(path.clone(), made);
            }
            return (shown, Vec::new());
        };
        let changed = changed(store, before, &shown);
        shown.times = before.times.clone();
        for path in &changed {
            shown.times.insert(path.clone(), made);
        }
        (shown, changed)
    }

    /// What `path` is (`""`: the root), if anything.
    pub fn obj(&self, store: &Store, path: &[u8]) -> Option<Obj> {
        if path.is_empty() || self.added.contains(path) {
            return Some(Obj::Dir);
        }
        let entry = match self.view.overlay.get(path) {
            Some(entry) => entry.clone()?,
            None => {
                let e = store.tree_entry_at(self.view.lower, path)?;
                if e.kind == EntryKind::Tree {
                    return (!self.emptied.contains(path)).then_some(Obj::Dir);
                }
                Entry::Stored(e.id)
            }
        };
        Some(self.of(store, entry))
    }

    /// A file's entry as shown: submodules are empty directories, as in
    /// snapshots.
    fn of(&self, store: &Store, entry: Entry) -> Obj {
        match entry.kind(store) {
            Ok(EntryKind::Commit) => Obj::Dir,
            _ => Obj::File(entry),
        }
    }

    /// The entries of directory `dir`, sorted by name; `None` if it isn't one.
    pub fn list(&self, store: &Store, dir: &[u8]) -> Option<Vec<(Vec<u8>, Obj)>> {
        if self.obj(store, dir)? != Obj::Dir {
            return None;
        }
        let mut out: BTreeMap<Vec<u8>, Obj> = BTreeMap::new();
        let lower = match dir.is_empty() {
            true => Some(self.view.lower),
            false => (store.tree_entry_at(self.view.lower, dir))
                .filter(|e| e.kind == EntryKind::Tree)
                .map(|e| e.id),
        };
        if let Some(tree) = lower.filter(|_| !self.emptied.contains(dir)) {
            let table = store.tree(tree);
            for e in table.entries() {
                let name = table.name(e);
                let path = join(dir, name);
                if self.view.overlay.contains_key(&path) || self.added.contains(&path) {
                    continue; // below
                }
                let obj = match e.kind {
                    EntryKind::Tree if self.emptied.contains(&path) => continue,
                    EntryKind::Tree => Obj::Dir,
                    _ => self.of(store, Entry::Stored(e.id)),
                };
                out.insert(name.to_vec(), obj);
            }
        }
        let prefix = match dir.is_empty() {
            true => Vec::new(),
            false => [dir, b"/"].concat(),
        };
        let direct = |p: &[u8]| p.starts_with(&prefix) && !p[prefix.len()..].contains(&b'/');
        for (path, entry) in (self.view.overlay.range(prefix.clone()..))
            .take_while(|(p, _)| p.starts_with(&prefix))
            .filter(|(p, _)| direct(p))
        {
            if let Some(entry) = entry {
                out.insert(path[prefix.len()..].to_vec(), self.of(store, entry.clone()));
            }
        }
        for path in (self.added.range(prefix.clone()..))
            .take_while(|p| p.starts_with(&prefix))
            .filter(|p| direct(p))
        {
            out.insert(path[prefix.len()..].to_vec(), Obj::Dir);
        }
        Some(out.into_iter().collect())
    }

    /// The conflict attribute of `path`, if it has one.
    pub fn conflict(&self, path: &[u8]) -> Option<&str> {
        self.conflicts.get(path).map(String::as_str)
    }

    /// When `path` last changed.
    pub fn time(&self, path: &[u8]) -> SystemTime {
        self.times
            .get(path)
            .copied()
            .unwrap_or_else(inode::file_time)
    }
}

/// The directories of `view.lower` with files deleted below them that have
/// none left (none `added` holds).
fn emptied(store: &Store, view: &View, added: &BTreeSet<Vec<u8>>) -> BTreeSet<Vec<u8>> {
    let mut dirs: Vec<&[u8]> = (view.overlay.iter())
        .filter(|(_, e)| e.is_none())
        .flat_map(|(p, _)| ancestors(p))
        .collect();
    // Deepest first: a directory is empty once its subdirectories are.
    dirs.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    dirs.dedup();
    let mut out = BTreeSet::new();
    for dir in dirs {
        if added.contains(dir) {
            continue;
        }
        let Some(e) = store.tree_entry_at(view.lower, dir) else {
            continue;
        };
        if e.kind != EntryKind::Tree {
            continue;
        }
        let table = store.tree(e.id);
        let any = table.entries().iter().any(|e| {
            let path = join(dir, table.name(e));
            match e.kind {
                EntryKind::Tree => !out.contains(&path),
                _ => view.overlay.get(&path) != Some(&None),
            }
        });
        if !any {
            out.insert(dir.to_vec());
        }
    }
    out
}

/// The paths whose files or directories differ between `a` and `b`, sorted.
fn changed(store: &Store, a: &Shown, b: &Shown) -> Vec<Vec<u8>> {
    let mut paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    paths.extend(a.view.overlay.keys().cloned());
    paths.extend(b.view.overlay.keys().cloned());
    paths.extend(a.added.iter().chain(&b.added).cloned());
    paths.extend(a.emptied.iter().chain(&b.emptied).cloned());
    let mut lower = BTreeMap::new();
    build::delta(
        store,
        a.view.lower,
        b.view.lower,
        &mut Vec::new(),
        &mut lower,
    );
    paths.extend(lower.into_keys());
    // Directories that appear or go with their files.
    let all: Vec<Vec<u8>> = paths.iter().cloned().collect();
    for path in &all {
        paths.extend(ancestors(path).map(<[u8]>::to_vec));
    }
    (paths.into_iter())
        .filter(|p| a.obj(store, p) != b.obj(store, p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::build::{Changes, Conflict, ConflictKind};
    use super::super::test_repo::{commit, tip};
    use super::*;

    fn view(store: &Store, lower: &str, overlay: &[(&str, Option<u32>)]) -> View {
        View {
            lower: store.load_tree(tip(lower).tree).unwrap(),
            overlay: (overlay.iter())
                .map(|(p, e)| (p.as_bytes().to_vec(), e.map(Entry::Stored)))
                .collect::<Changes>(),
            conflicts: Vec::new(),
        }
    }

    fn names(list: Option<Vec<(Vec<u8>, Obj)>>) -> Vec<String> {
        (list.unwrap().into_iter())
            .map(|(n, o)| {
                let n = String::from_utf8(n).unwrap();
                match o {
                    Obj::Dir => format!("{n}/"),
                    Obj::File(_) => n,
                }
            })
            .collect()
    }

    #[test]
    fn directories_and_listings() {
        let store = Store::default();
        let c = commit(
            None,
            &[
                ("d/x", Some("x")),
                ("d/e/y", Some("y")),
                ("f", Some("f")),
                ("g/z", Some("z")),
            ],
        );
        let lower = store.load_tree(tip(&c).tree).unwrap();
        let f = match store.tree_entry_at(lower, b"f") {
            Some(e) => e.id,
            None => panic!(),
        };
        // d's files all deleted; g replaced by a file; a new directory n.
        let v = view(
            &store,
            &c,
            &[
                ("d/x", None),
                ("d/e/y", None),
                ("g/z", None),
                ("g", Some(f)),
                ("n/m/k", Some(f)),
            ],
        );
        let (s, changed) = Shown::next(&store, v, &[], None);
        assert!(changed.is_empty());
        assert_eq!(names(s.list(&store, b"")), ["f", "g", "n/"]);
        assert_eq!(s.obj(&store, b"d"), None);
        assert_eq!(s.obj(&store, b"d/e"), None);
        assert_eq!(names(s.list(&store, b"n")), ["m/"]);
        assert_eq!(names(s.list(&store, b"n/m")), ["k"]);
        assert_eq!(s.obj(&store, b"g/z"), None);
        assert!(s.list(&store, b"g").is_none(), "a file");

        // Next: d/x back, n gone.
        let v = view(
            &store,
            &c,
            &[("d/e/y", None), ("g/z", None), ("g", Some(f))],
        );
        let (s2, changed) = Shown::next(&store, v, &[], Some(&s));
        let changed: Vec<String> = (changed.into_iter())
            .map(|p| String::from_utf8(p).unwrap())
            .collect();
        assert_eq!(changed, ["d", "d/x", "n", "n/m", "n/m/k"]);
        assert_eq!(names(s2.list(&store, b"d")), ["x"]);
        assert!(s2.time(b"d/x") > inode::file_time());
    }

    #[test]
    fn summary_and_attributes() {
        let store = Store::default();
        let c = commit(None, &[("a", Some("1"))]);
        let mut v = view(&store, &c, &[]);
        let both = vec!["api".to_string(), "ui".to_string()];
        for kind in [ConflictKind::Mode, ConflictKind::Markers] {
            v.conflicts.push(Conflict {
                path: b"a".to_vec(),
                kind,
                branches: both.clone(),
            });
        }
        let note = Note {
            kind: "skipped",
            what: "web".into(),
            why: "no branch web".into(),
        };
        let (s, _) = Shown::next(&store, v, &[note], None);
        assert_eq!(s.conflict(b"a"), Some("mode: api, ui; markers: api, ui"));
        assert_eq!(
            String::from_utf8(s.summary.to_vec()).unwrap(),
            "mode\tapi, ui\ta\nmarkers\tapi, ui\ta\nskipped\tweb\tno branch web\n"
        );
    }
}
