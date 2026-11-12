//! The git index gitfold writes for a snapshot or a new workspace: exactly the
//! stat data the filesystem reports (inode, mtime, ctime, uid, gid, size), so
//! `git status` sees every file as clean from stat alone and never has to read
//! file contents, and a cache tree (`TREE` extension), so it compares unchanged
//! directories with HEAD by tree id instead of entry by entry.

use std::io::Write;

use gix::ObjectId;
use gix::objs::tree::EntryKind;
use sha1::{Digest, Sha1};

use crate::inode;

#[derive(Clone, Copy)]
pub struct StatInfo {
    pub uid: u32,
    pub gid: u32,
}

/// Streams an index v4 file, with a cache tree. v4 prefix-compresses paths
/// against the previous entry (~half the size for large trees), so entries
/// must arrive in index order.
pub struct IndexWriter {
    out: Vec<u8>,
    /// The body of the `TREE` extension (cache tree).
    tree: Vec<u8>,
    prev: Vec<u8>,
    count: u32,
    stat: StatInfo,
}

impl IndexWriter {
    /// `capacity`: expected size in bytes, e.g. the previous index of the repo.
    pub fn new(stat: StatInfo, capacity: usize) -> Self {
        let mut out = Vec::with_capacity(capacity.max(1 << 20));
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&4u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // entry count, patched in finish()
        IndexWriter {
            out,
            tree: Vec::new(),
            prev: Vec::new(),
            count: 0,
            stat,
        }
    }

    /// `ino` is the low 32 bits of the inode `stat` reports for the path.
    pub fn push(&mut self, path: &[u8], kind: EntryKind, oid: &ObjectId, size: u64, ino: u32) {
        debug_assert!(self.prev.as_slice() < path, "index entries must be sorted");
        debug_assert_ne!(kind, EntryKind::Tree, "trees are not index entries");
        // `EntryKind` discriminants are the git modes.
        let mode = kind as u32;
        let secs = inode::FILE_TIME_SECS as u32;
        let fields = [
            secs,
            0,
            secs,
            0,
            0,
            ino,
            mode,
            self.stat.uid,
            self.stat.gid,
            size as u32,
        ];
        for v in fields {
            self.out.extend_from_slice(&v.to_be_bytes());
        }
        self.out.extend_from_slice(oid.as_bytes());
        let flags = path.len().min(0xfff) as u16;
        self.out.extend_from_slice(&flags.to_be_bytes());
        // Path: bytes to drop from the end of the previous path, then the new suffix.
        let common = self
            .prev
            .iter()
            .zip(path)
            .take_while(|(a, b)| a == b)
            .count();
        push_varint(&mut self.out, (self.prev.len() - common) as u64);
        self.out.extend_from_slice(&path[common..]);
        self.out.push(0);
        self.prev.clear();
        self.prev.extend_from_slice(path);
        self.count += 1;
    }

    /// Adds a cache tree node, in pre-order (a directory before its
    /// subdirectories; their order doesn't matter to git): directory `name`
    /// (empty for the root) with `valid` = (index entries below it, its tree
    /// id), or `None` to have git work them out, and `subtrees` nodes to
    /// follow for its subdirectories.
    pub fn push_tree(&mut self, name: &[u8], valid: Option<(u32, &ObjectId)>, subtrees: u32) {
        self.tree.extend_from_slice(name);
        self.tree.push(0);
        let count = valid.map_or(-1, |(n, _)| i64::from(n));
        // Writing to a Vec can't fail.
        let _ = writeln!(self.tree, "{count} {subtrees}");
        if let Some((_, oid)) = valid {
            self.tree.extend_from_slice(oid.as_bytes());
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.out[8..12].copy_from_slice(&self.count.to_be_bytes());
        if !self.tree.is_empty() {
            self.out.extend_from_slice(b"TREE");
            self.out
                .extend_from_slice(&(self.tree.len() as u32).to_be_bytes());
            self.out.extend_from_slice(&self.tree);
        }
        let digest = Sha1::digest(&self.out);
        self.out.extend_from_slice(&digest);
        self.out
    }

    pub fn len(&self) -> u32 {
        self.count
    }
}

/// Git's offset varint (`varint.c`), used by index v4 path compression.
fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    let mut buf = [0u8; 16];
    let mut pos = buf.len() - 1;
    buf[pos] = (value & 127) as u8;
    value >>= 7;
    while value != 0 {
        value -= 1;
        pos -= 1;
        buf[pos] = 128 | (value & 127) as u8;
        value >>= 7;
    }
    out.extend_from_slice(&buf[pos..]);
}
