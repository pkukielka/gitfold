//! Registers a mounted snapshot as a linked git worktree and synthesizes its
//! index, so git commands run inside the mount work without a checkout.
//!
//! The index carries exactly the stat data the filesystem reports (inode,
//! mtime, ctime, uid, gid, size), so `git status` sees every file as clean
//! from stat alone and never has to read file contents.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use gix::ObjectId;
use gix::objs::tree::EntryKind;
use sha1::{Digest, Sha1};

use crate::git::Commit;
use crate::inode;

#[derive(Clone, Copy)]
pub struct StatInfo {
    pub uid: u32,
    pub gid: u32,
}

/// Streams an index v4 file. v4 stores each path as a suffix of the previous
/// one (~2.5x smaller for large trees), so entries must arrive in index order.
pub struct IndexWriter {
    out: Vec<u8>,
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

    pub fn finish(mut self) -> Vec<u8> {
        self.out[8..12].copy_from_slice(&self.count.to_be_bytes());
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

/// Creates `<common>/worktrees/gitfold-<sha>` with a detached HEAD at the commit
/// and the given serialized index. Returns the admin dir; the mount's `.git`
/// file must point at it.
pub fn register(
    common_dir: &Path,
    commit: &Commit,
    index: &[u8],
    gitfile: &Path,
) -> Result<PathBuf> {
    let dir = common_dir
        .join("worktrees")
        .join(format!("gitfold-{}", commit.id.to_hex_with_len(12)));
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("HEAD"), format!("{}\n", commit.id))?;
    fs::write(dir.join("commondir"), "../..\n")?;
    fs::write(dir.join("gitdir"), format!("{}\n", gitfile.display()))?;
    // Keeps `git worktree prune` from deleting the entry while unmounted.
    fs::write(dir.join("locked"), "managed by gitfold\n")?;
    let tmp = dir.join("index.gitfold-tmp");
    fs::write(&tmp, index)?;
    fs::rename(&tmp, dir.join("index"))?;
    Ok(dir)
}
