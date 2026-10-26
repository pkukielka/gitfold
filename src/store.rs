//! Content-addressed tables shared by all snapshots and workspaces of a mount.
//!
//! Every git tree is decoded once into a compact table, and every distinct
//! (blob, mode) gets one file id. Snapshots of nearby commits share almost all
//! of both, so an additional snapshot costs only its changed trees plus a small
//! per-directory array (`DirRec`).

use std::cmp::Ordering;
use std::sync::Arc;

use anyhow::Result;
use gix::ObjectId;
use gix::hashtable::hash_table::HashTable;
use gix::objs::tree::EntryKind;
use parking_lot::RwLock;
use rayon::prelude::*;

use crate::git;
use crate::inode;
use crate::worktree::IndexWriter;

/// Tree id of an empty table, used for submodules.
pub const EMPTY_TREE: u32 = 0;

#[derive(Clone, Copy)]
pub struct FileRec {
    pub oid: ObjectId,
    pub size: u64,
    pub kind: EntryKind,
}

impl FileRec {
    /// Permission bits as shown in the mount (and given to copied-up files).
    pub fn perm(&self) -> u16 {
        match self.kind {
            EntryKind::BlobExecutable => 0o755,
            EntryKind::Link => 0o777,
            _ => 0o644,
        }
    }
}

/// One tree entry. 16 bytes; the name lives in the table's name buffer.
#[derive(Clone, Copy)]
pub struct TEntry {
    name_off: u32,
    name_len: u16,
    pub kind: EntryKind,
    /// Child tree id for trees, file id otherwise (submodules: gitlink commit).
    pub id: u32,
    /// For trees and submodules: position among this table's subdirectories.
    pub ord: u32,
}

impl TEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Tree | EntryKind::Commit)
    }
}

pub struct TreeTable {
    /// Sorted in git tree order (directories compare as `name/`), which makes a
    /// depth-first walk produce paths in index order.
    entries: Box<[TEntry]>,
    names: Box<[u8]>,
    /// Index entries below this tree (files, symlinks and submodules).
    index_entries: u32,
    /// Whether the tree object was stored strictly in this order (no entry out
    /// of order or listed twice), as git writes trees. Only then do index entries written from
    /// this table make up that object, as the index's cache tree claims.
    canonical: bool,
}

/// Git's tree entry order: names compare bytewise, with a directory's name
/// followed by an implicit `/`.
pub fn tree_order(a: &[u8], a_dir: bool, b: &[u8], b_dir: bool) -> Ordering {
    let common = a.len().min(b.len());
    a[..common].cmp(&b[..common]).then_with(|| {
        let next = |name: &[u8], dir: bool| name.get(common).copied().or(dir.then_some(b'/'));
        next(a, a_dir).cmp(&next(b, b_dir))
    })
}

impl TreeTable {
    pub fn entries(&self) -> &[TEntry] {
        &self.entries
    }

    pub fn name(&self, e: &TEntry) -> &[u8] {
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }

    pub fn find(&self, name: &[u8]) -> Option<&TEntry> {
        // The caller doesn't know whether `name` is a directory; try both positions.
        [false, true].into_iter().find_map(|dir| {
            let i = self
                .entries
                .binary_search_by(|e| {
                    tree_order(self.name(e), e.kind == EntryKind::Tree, name, dir)
                })
                .ok()?;
            Some(&self.entries[i])
        })
    }
}

/// A directory of one snapshot. Subdirectories of a directory are contiguous:
/// the child for entry `e` is `first_child + e.ord`.
#[derive(Clone, Copy)]
pub struct DirRec {
    pub tree: u32,
    pub first_child: u32,
}

impl DirRec {
    /// Inode of entry `e` of this directory, in snapshot `slot`.
    pub fn child_ino(&self, slot: u32, e: &TEntry) -> u64 {
        if e.is_dir() {
            inode::dir(slot, self.first_child + e.ord)
        } else {
            inode::file(e.id)
        }
    }
}

/// Object ids are uniformly distributed, so their first bytes are a good hash.
fn file_hash(oid: &ObjectId, kind: EntryKind) -> u64 {
    let bytes: [u8; 8] = oid.as_bytes()[..8]
        .try_into()
        .expect("object ids are 20 bytes");
    u64::from_ne_bytes(bytes) ^ kind as u64
}

struct Trees {
    tables: Vec<Arc<TreeTable>>,
    /// The object id of each table (for the index's cache tree).
    oids: Vec<ObjectId>,
    ids: gix::hashtable::HashMap<ObjectId, u32>,
}

#[derive(Default)]
struct Files {
    recs: Vec<FileRec>,
    /// Indices into `recs`, hashed and compared through `recs`, so each slot is
    /// only 4 bytes (keying by (ObjectId, EntryKind) cost ~35 MB for chromium).
    ids: HashTable<u32>,
}

impl Files {
    fn find(&self, hash: u64, oid: &ObjectId, kind: EntryKind) -> Option<u32> {
        self.ids
            .find(hash, |&id| {
                let f = &self.recs[id as usize];
                f.oid == *oid && f.kind == kind
            })
            .copied()
    }
}

pub struct Store {
    trees: RwLock<Trees>,
    files: RwLock<Files>,
}

pub struct Stats {
    pub trees: usize,
    pub entries: usize,
    pub files: usize,
}

impl Default for Store {
    fn default() -> Self {
        let empty = TreeTable {
            entries: Box::new([]),
            names: Box::new([]),
            index_entries: 0,
            canonical: true,
        };
        Store {
            trees: RwLock::new(Trees {
                tables: vec![Arc::new(empty)],
                oids: vec![ObjectId::empty_tree(gix::hash::Kind::Sha1)],
                // The real empty tree object is the same table; avoids storing it twice.
                ids: [(ObjectId::empty_tree(gix::hash::Kind::Sha1), EMPTY_TREE)]
                    .into_iter()
                    .collect(),
            }),
            files: Default::default(),
        }
    }
}

impl Store {
    pub fn tree(&self, id: u32) -> Arc<TreeTable> {
        self.trees.read().tables[id as usize].clone()
    }

    pub fn file(&self, id: u32) -> Option<FileRec> {
        self.files.read().recs.get(id as usize).copied()
    }

    pub fn stats(&self) -> Stats {
        let trees = self.trees.read();
        Stats {
            trees: trees.tables.len() - 1, // excluding the built-in empty table
            entries: trees.tables.iter().map(|t| t.entries.len()).sum(),
            files: self.files.read().recs.len(),
        }
    }

    /// Returns the id of a tree, decoding it (and, in parallel, any subtrees not
    /// seen before) on first use.
    pub fn load_tree(&self, oid: ObjectId) -> Result<u32> {
        if let Some(&id) = self.trees.read().ids.get(&oid) {
            return Ok(id);
        }
        let mut raw = git::tree_entries(oid)?;
        let order = |a: &git::Entry, b: &git::Entry| {
            tree_order(
                &a.name,
                a.kind == EntryKind::Tree,
                &b.name,
                b.kind == EntryKind::Tree,
            )
        };
        let canonical = raw
            .windows(2)
            .all(|w| order(&w[0], &w[1]) == Ordering::Less);
        // Git already stores trees in this order; sorting keeps malformed trees searchable.
        raw.sort_unstable_by(order);
        let ids: Vec<u32> = raw
            .par_iter()
            .map(|e| match e.kind {
                EntryKind::Tree => self.load_tree(e.oid),
                kind => self.file_id(e.oid, kind),
            })
            .collect::<Result<_>>()?;

        let mut names = Vec::with_capacity(raw.iter().map(|e| e.name.len()).sum());
        let mut entries = Vec::with_capacity(raw.len());
        let mut ord = 0;
        for (e, id) in raw.iter().zip(ids) {
            let entry = TEntry {
                name_off: names.len() as u32,
                name_len: e.name.len() as u16,
                kind: e.kind,
                id,
                ord,
            };
            if entry.is_dir() {
                ord += 1;
            }
            names.extend_from_slice(&e.name);
            entries.push(entry);
        }
        let index_entries = {
            let trees = self.trees.read();
            entries
                .iter()
                .map(|e| match e.kind {
                    EntryKind::Tree => trees.tables[e.id as usize].index_entries,
                    _ => 1,
                })
                .sum()
        };
        let table = TreeTable {
            entries: entries.into(),
            names: names.into(),
            index_entries,
            canonical,
        };

        let mut trees = self.trees.write();
        if let Some(&id) = trees.ids.get(&oid) {
            return Ok(id); // decoded concurrently by another thread
        }
        let id = trees.tables.len() as u32;
        trees.tables.push(Arc::new(table));
        trees.oids.push(oid);
        trees.ids.insert(oid, id);
        Ok(id)
    }

    fn file_id(&self, oid: ObjectId, kind: EntryKind) -> Result<u32> {
        let hash = file_hash(&oid, kind);
        if let Some(id) = self.files.read().find(hash, &oid, kind) {
            return Ok(id);
        }
        // Gitlink commits usually are not in this repository's object database.
        let size = if kind == EntryKind::Commit {
            0
        } else {
            git::object_size(oid)?
        };
        let mut files = self.files.write();
        if let Some(id) = files.find(hash, &oid, kind) {
            return Ok(id); // added concurrently by another thread
        }
        let Files { recs, ids } = &mut *files;
        let id = recs.len() as u32;
        recs.push(FileRec { oid, size, kind });
        ids.insert_unique(hash, id, |&id| {
            let f = &recs[id as usize];
            file_hash(&f.oid, f.kind)
        });
        Ok(id)
    }

    /// Lays out the directories of a snapshot rooted at `root` (breadth first).
    pub fn dir_table(&self, root: u32) -> Box<[DirRec]> {
        let trees = self.trees.read();
        let mut dirs = vec![DirRec {
            tree: root,
            first_child: 0,
        }];
        let mut i = 0;
        while i < dirs.len() {
            dirs[i].first_child = dirs.len() as u32;
            for e in trees.tables[dirs[i].tree as usize].entries() {
                if e.is_dir() {
                    // Submodules are shown as empty directories.
                    let tree = if e.kind == EntryKind::Tree {
                        e.id
                    } else {
                        EMPTY_TREE
                    };
                    dirs.push(DirRec {
                        tree,
                        first_child: 0,
                    });
                }
            }
            i += 1;
        }
        dirs.into()
    }

    /// Writes the git index entries of a tree, in index order (tables are in tree
    /// order, so a depth-first walk yields it), and its cache tree (the `TREE`
    /// extension: entry count and tree id of every directory, which lets `git
    /// status` skip comparing unchanged directories with HEAD entry by entry).
    /// `ino(path, dir, entry)` gives the low 32 bits of the inode `stat` reports
    /// for each entry.
    pub fn write_index(
        &self,
        dirs: &[DirRec],
        w: &mut IndexWriter,
        ino: impl Fn(&[u8], &DirRec, &TEntry) -> u32,
    ) {
        let trees = self.trees.read();
        let files = self.files.read();
        let walk = IndexWalk {
            tables: &trees.tables,
            oids: &trees.oids,
            files: &files.recs,
            dirs,
            ino,
        };
        walk.dir(0, b"", &mut Vec::new(), w);
    }
}

struct IndexWalk<'a, F> {
    tables: &'a [Arc<TreeTable>],
    oids: &'a [ObjectId],
    files: &'a [FileRec],
    dirs: &'a [DirRec],
    ino: F,
}

impl<F: Fn(&[u8], &DirRec, &TEntry) -> u32> IndexWalk<'_, F> {
    fn dir(&self, dir: u32, name: &[u8], path: &mut Vec<u8>, w: &mut IndexWriter) {
        let rec = self.dirs[dir as usize];
        let table = &self.tables[rec.tree as usize];
        // A cache tree node comes before its subdirectories' (empty ones
        // included, with 0 entries, as git's own `read-tree` writes them).
        let subtrees = table
            .entries()
            .iter()
            .filter(|e| e.kind == EntryKind::Tree)
            .count();
        let valid = table
            .canonical
            .then(|| (table.index_entries, &self.oids[rec.tree as usize]));
        w.push_tree(name, valid, subtrees as u32);
        for e in table.entries() {
            let len = path.len();
            path.extend_from_slice(table.name(e));
            if e.kind == EntryKind::Tree {
                path.push(b'/');
                self.dir(rec.first_child + e.ord, table.name(e), path, w);
            } else {
                // Submodules are index entries but empty directories in the mount.
                let f = &self.files[e.id as usize];
                w.push(path, e.kind, &f.oid, f.size, (self.ino)(path, &rec, e));
            }
            path.truncate(len);
        }
    }
}
