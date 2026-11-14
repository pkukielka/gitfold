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

/// Entry flags: the path length (`NAME_MASK` if longer), and that the
/// extended flags word follows (index v3 and later).
pub(super) const NAME_MASK: u16 = 0x0fff;
pub(super) const EXTENDED: u16 = 0x4000;
/// Extended flags: skip-worktree.
pub(super) const SKIP_WORKTREE: u16 = 0x4000;

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
    /// `skip_worktree`: git takes the entry as it is and never looks at the
    /// file (`git ls-files -v` shows `S`).
    pub fn push(
        &mut self,
        path: &[u8],
        kind: EntryKind,
        oid: &ObjectId,
        size: u64,
        ino: u32,
        skip_worktree: bool,
    ) {
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
        let len = path.len().min(NAME_MASK.into()) as u16;
        if skip_worktree {
            self.out.extend_from_slice(&(EXTENDED | len).to_be_bytes());
            self.out.extend_from_slice(&SKIP_WORKTREE.to_be_bytes());
        } else {
            self.out.extend_from_slice(&len.to_be_bytes());
        }
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
pub(super) fn push_varint(out: &mut Vec<u8>, mut value: u64) {
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, UNIX_EPOCH};

    /// An empty directory for test `name`.
    pub fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gitfold-index-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs git in `dir` (no user or system config) with `input` on its
    /// stdin, returning what it printed; panics if it fails.
    pub fn git_in(dir: &Path, args: &[&str], input: &[u8]) -> String {
        let mut child = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn git(dir: &Path, args: &[&str]) -> String {
        git_in(dir, args, b"")
    }

    /// Sets the mtime of `path` well in the past, so that its index entry is
    /// never racily clean (git would smudge it when writing the index).
    pub fn age(path: &Path) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000))
            .unwrap();
    }

    /// A repository in `dir` with `files` (path, content) committed.
    pub fn repo(dir: &Path, files: &[(&str, &str)]) {
        git(dir, &["init", "-q"]);
        for (path, content) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            age(&path);
        }
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", "init"]);
    }

    /// The id of the blob at `path` in the index of the repository in `dir`.
    pub fn staged(dir: &Path, path: &str) -> ObjectId {
        let hex = git(dir, &["rev-parse", &format!(":{path}")]);
        ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
    }

    /// An entry pushed with skip-worktree is what `git update-index
    /// --skip-worktree` writes: the same flags, extended word and path bytes.
    #[test]
    fn skip_worktree_entry_as_git_writes_it() {
        let dir = scratch("skip");
        repo(&dir, &[("a.txt", "a\n"), ("d/b.txt", "b\n")]);
        git(&dir, &["update-index", "--index-version", "4"]);
        git(&dir, &["update-index", "--skip-worktree", "a.txt"]);
        let theirs = fs::read(dir.join(".git/index")).unwrap();

        let mut w = IndexWriter::new(StatInfo { uid: 0, gid: 0 }, 0);
        w.push(b"a.txt", EntryKind::Blob, &staged(&dir, "a.txt"), 2, 1, true);
        w.push(b"d/b.txt", EntryKind::Blob, &staged(&dir, "d/b.txt"), 2, 2, false);
        let ours = w.finish();
        // The first entry after its stat data and id: flags 0x4005, extended
        // word 0x4000, prefix length 0, "a.txt", NUL.
        let tail = 12 + 60..12 + 60 + 2 + 2 + 1 + 6;
        assert_eq!(ours[tail.clone()], theirs[tail.clone()]);
        assert_eq!(ours[tail][..4], [0x40, 0x05, 0x40, 0x00]);

        fs::write(dir.join(".git/index"), &ours).unwrap();
        assert_eq!(git(&dir, &["ls-files", "-v"]), "S a.txt\nH d/b.txt\n");
        assert_eq!(git(&dir, &["status", "--porcelain"]), "");
        fs::remove_dir_all(dir).unwrap();
    }
}
