//! A view as the mount shows it: the tree git merged (`merge`) as
//! directories and files, a summary of its conflicts at its root, and what
//! changed since the view before it (`Shown::next`), to tell the kernel
//! about.
//!
//! Times, from what git's trees say: a file's time is when its blob (or
//! mode) last changed, a directory's when an entry in it appeared or went
//! (as a file system's directory mtime); the root keeps the time it was
//! first built at; what never changed while mounted shows the fixed
//! `inode::file_time`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::SystemTime;

use gix::objs::tree::EntryKind;
use parking_lot::RwLock;

use super::members::Note;
use super::merge::Conflict;
use crate::inode;
use crate::state::{ancestors, parent};
use crate::store::{EMPTY_TREE, Lower, Store, TEntry};

/// The summary file at a view's root: every conflict and note, one per line
/// (`<kind>\t<branches or ref>\t<path or why>`). A file of the branches with
/// that name doesn't show.
pub const SUMMARY: &[u8] = b".gitfold-conflicts";

/// What a path of a view is.
#[derive(Clone, Debug, PartialEq)]
pub enum Obj {
    Dir,
    /// A file or symlink: its store file id.
    File(u32),
    /// The summary file, with its content.
    Summary(Arc<Vec<u8>>),
}

/// The type of file an `Obj` is, as the kernel sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Dir,
    Symlink,
    File,
}

impl Obj {
    pub fn kind(&self, store: &Store) -> Kind {
        match self {
            Obj::Dir => Kind::Dir,
            Obj::File(id) if store.file(*id).is_some_and(|f| f.kind == EntryKind::Link) => {
                Kind::Symlink
            }
            Obj::File(_) | Obj::Summary(_) => Kind::File,
        }
    }

    fn of(lower: Lower) -> Obj {
        match lower {
            // Submodules are empty directories, as in snapshots.
            Lower::Tree(_) | Lower::Gitlink => Obj::Dir,
            Lower::File(id) => Obj::File(id),
        }
    }
}

/// When a view's paths last changed (those not here: never, they show
/// `inode::file_time`), shared by the views it was built as, so each refresh
/// adds to it rather than copying it.
pub type Times = Arc<RwLock<HashMap<Vec<u8>, SystemTime>>>;

/// A path that differs from the view before.
#[derive(Debug)]
pub struct Changed {
    pub path: Vec<u8>,
    /// It appeared or went: its directory's listing changed.
    pub listed: bool,
    /// It was there and is now of another kind (or gone).
    pub retyped: bool,
}

pub struct Shown {
    /// The merged tree, in the store.
    pub tree: u32,
    summary: Arc<Vec<u8>>,
    times: Times,
}

impl Shown {
    /// The merged `tree` with `conflicts` and `notes`, as shown after `before`
    /// (none: the first): with the paths that differ from it, sorted, given
    /// the time now in `times`.
    pub fn next(
        store: &Store,
        tree: u32,
        conflicts: &[Conflict],
        notes: &[Note],
        before: Option<&Shown>,
        times: &Times,
    ) -> (Shown, Vec<Changed>) {
        let made = SystemTime::now();
        let mut summary = Vec::new();
        for c in conflicts {
            let sides = c.sides.join(", ");
            summary.extend_from_slice(format!("{}\t{sides}\t", c.kind).as_bytes());
            summary.extend_from_slice(&c.path);
            summary.push(b'\n');
        }
        for n in notes {
            summary.extend_from_slice(format!("{}\t{}\t{}\n", n.kind, n.what, n.why).as_bytes());
        }
        let shown = Shown {
            tree,
            summary: Arc::new(summary),
            times: times.clone(),
        };
        let mut times = times.write();
        // The root keeps the time it was first built at: its attributes are
        // also read outside the view (`@committed`'s listing).
        times.entry(Vec::new()).or_insert(made);
        let changed = match before {
            Some(before) => changed(store, before, &shown),
            None => Vec::new(),
        };
        for c in &changed {
            times.insert(c.path.clone(), made);
            let dir = parent(&c.path);
            if c.listed && !dir.is_empty() {
                times.insert(dir.to_vec(), made);
            }
        }
        drop(times);
        (shown, changed)
    }

    /// What `path` is (`""`: the root), if anything.
    pub fn obj(&self, store: &Store, path: &[u8]) -> Option<Obj> {
        if path == SUMMARY {
            return Some(Obj::Summary(self.summary.clone()));
        }
        store.entry_at(self.tree, path).map(Obj::of)
    }

    /// The entries of directory `dir`, sorted by name; `None` if it isn't one.
    pub fn list(&self, store: &Store, dir: &[u8]) -> Option<Vec<(Vec<u8>, Obj)>> {
        let tree = match store.entry_at(self.tree, dir)? {
            Lower::Tree(tree) => tree,
            Lower::Gitlink => EMPTY_TREE,
            Lower::File(_) => return None,
        };
        let table = store.tree(tree);
        let mut out: BTreeMap<Vec<u8>, Obj> = (table.entries().iter())
            .map(|e| (table.name(e).to_vec(), Obj::of(Lower::of(e))))
            .collect();
        if dir.is_empty() {
            out.insert(SUMMARY.to_vec(), Obj::Summary(self.summary.clone()));
        }
        Some(out.into_iter().collect())
    }

    /// When `path` last changed.
    pub fn time(&self, path: &[u8]) -> SystemTime {
        (self.times.read().get(path).copied()).unwrap_or_else(inode::file_time)
    }
}

/// The paths whose files or directories (or the summary) differ between `a`
/// and `b`, sorted.
fn changed(store: &Store, a: &Shown, b: &Shown) -> Vec<Changed> {
    let mut paths = BTreeSet::new();
    delta(store, a.tree, b.tree, &mut Vec::new(), &mut paths);
    // Directories that appear or go with their files.
    let dirs: Vec<Vec<u8>> = (paths.iter())
        .flat_map(|p| ancestors(p))
        .map(<[u8]>::to_vec)
        .collect();
    paths.extend(dirs);
    paths.insert(SUMMARY.to_vec());
    (paths.into_iter())
        .filter_map(|path| {
            let (was, is) = (a.obj(store, &path), b.obj(store, &path));
            if was == is {
                return None;
            }
            let kind = |o: &Option<Obj>| o.as_ref().map(|o| o.kind(store));
            Some(Changed {
                listed: was.is_none() || is.is_none(),
                retyped: was.is_some() && kind(&was) != kind(&is),
                path,
            })
        })
        .collect()
}

/// The paths of files, symlinks and submodules that differ between trees
/// `from` and `to` below `dir` (a directory and a file at one path: the
/// directory's files and the file). Walks only subtrees that differ.
fn delta(store: &Store, from: u32, to: u32, dir: &mut Vec<u8>, out: &mut BTreeSet<Vec<u8>>) {
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
            out.insert(dir.clone());
        }
        dir.truncate(len);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_repo::{commit, tip};
    use super::*;

    fn tree(store: &Store, c: &str) -> u32 {
        store.load_tree(tip(c).tree).unwrap()
    }

    fn names(list: Option<Vec<(Vec<u8>, Obj)>>) -> Vec<String> {
        (list.unwrap().into_iter())
            .map(|(n, o)| {
                let n = String::from_utf8(n).unwrap();
                match o {
                    Obj::Dir => format!("{n}/"),
                    _ => n,
                }
            })
            .collect()
    }

    #[test]
    fn listings_changes_and_times() {
        let store = Store::default();
        let times = Times::default();
        let one = commit(
            None,
            &[("d/x", Some("x")), ("f", Some("f")), ("g/z", Some("z"))],
        );
        let (s, changed) = Shown::next(&store, tree(&store, &one), &[], &[], None, &times);
        assert!(changed.is_empty());
        assert_eq!(
            names(s.list(&store, b"")),
            [".gitfold-conflicts", "d/", "f", "g/"]
        );
        assert_eq!(s.time(b"f"), inode::file_time());

        // d/x changed in place, g/z gone (and g), n/k new, f now a directory.
        let two = commit(
            Some(&one),
            &[
                ("d/x", Some("x2")),
                ("g/z", None),
                ("n/k", Some("k")),
                ("f", None),
                ("f/y", Some("y")),
            ],
        );
        let (s2, changed) = Shown::next(&store, tree(&store, &two), &[], &[], Some(&s), &times);
        let said: Vec<(String, bool, bool)> = (changed.iter())
            .map(|c| {
                (
                    String::from_utf8(c.path.clone()).unwrap(),
                    c.listed,
                    c.retyped,
                )
            })
            .collect();
        let expected = [
            ("d/x", false, false),
            ("f", false, true),
            ("f/y", true, false),
            ("g", true, true),
            ("g/z", true, true),
            ("n", true, false),
            ("n/k", true, false),
        ]
        .map(|(p, l, r)| (p.to_string(), l, r));
        assert_eq!(said, expected);
        assert!(s2.time(b"d/x") > inode::file_time(), "its blob changed");
        assert_eq!(s2.time(b"d"), inode::file_time(), "d's listing didn't");
        assert!(s2.time(b"n") > inode::file_time());
        assert_eq!(s2.time(b""), s.time(b""), "the root keeps its time");
        assert_eq!(names(s2.list(&store, b"f")), ["y"]);
    }

    #[test]
    fn the_summary() {
        let store = Store::default();
        let c = commit(None, &[("a", Some("1"))]);
        let conflict = Conflict {
            kind: "CONFLICT (contents)".into(),
            sides: ["api+ui".into(), "web".into()],
            path: b"a".to_vec(),
        };
        let note = Note {
            kind: "skipped",
            what: "css".into(),
            why: "no branch css".into(),
        };
        let times = Times::default();
        let (s, _) = Shown::next(&store, tree(&store, &c), &[conflict], &[note], None, &times);
        assert_eq!(
            String::from_utf8(s.summary.to_vec()).unwrap(),
            "CONFLICT (contents)\tapi+ui, web\ta\nskipped\tcss\tno branch css\n"
        );
    }
}
