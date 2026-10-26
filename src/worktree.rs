//! Linked-worktree registrations, so git commands run inside the mount work
//! without a checkout: snapshots (detached HEAD, synthesized index) and
//! workspaces (HEAD on the branch, index written once). Also the workspace
//! helpers shared by the daemon and the management commands: `refs/gitfold/<id>`
//! base pins, whether git is busy in a worktree, the `.git/gitfold/.lock` daemon
//! lock, and workspace ids.
//!
//! The synthesized index carries exactly the stat data the filesystem reports
//! (inode, mtime, ctime, uid, gid, size), so `git status` sees every file as
//! clean from stat alone and never has to read file contents.

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

/// Streams an index v4 file. v4 prefix-compresses paths against the previous
/// entry (~half the size for large trees), so entries must arrive in index order.
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

/// Snapshot registrations are named `<SNAPSHOT_PREFIX><first SNAPSHOT_HEX hex
/// digits of the commit>`.
const SNAPSHOT_PREFIX: &str = "gitfold-";
const SNAPSHOT_HEX: usize = 12;

/// Creates `<common>/worktrees/gitfold-<sha>` with a detached HEAD at the commit
/// and the given serialized index. Returns the admin dir; the mount's `.git`
/// file must point at it.
pub fn register(
    common_dir: &Path,
    commit: &Commit,
    index: &[u8],
    gitfile: &Path,
) -> Result<PathBuf> {
    let dir = common_dir.join("worktrees").join(format!(
        "{SNAPSHOT_PREFIX}{}",
        commit.id.to_hex_with_len(SNAPSHOT_HEX)
    ));
    write_admin(&dir, gitfile, index, &format!("{}\n", commit.id))?;
    Ok(dir)
}

/// Admin directory of the workspace with directory name `id`:
/// `<common>/worktrees/gitfold-ws-<id>`.
pub fn ws_admin_dir(common_dir: &Path, id: &str) -> PathBuf {
    common_dir
        .join("worktrees")
        .join(format!("gitfold-ws-{id}"))
}

/// Registers a workspace on `branch` with admin directory `dir` (see
/// `ws_admin_dir`). HEAD and the index belong to git once the worktree exists,
/// so they are only written when it is new or incomplete (`index` is called
/// then); otherwise this just points `gitdir` at the current mount path.
pub fn register_branch(
    dir: &Path,
    branch: &str,
    gitfile: &Path,
    index: impl FnOnce() -> Vec<u8>,
) -> Result<()> {
    if dir.join("HEAD").exists() && dir.join("index").exists() {
        write_atomic(
            &dir.join("gitdir"),
            format!("{}\n", gitfile.display()).as_bytes(),
        )?;
    } else {
        write_admin(
            dir,
            gitfile,
            &index(),
            &format!("ref: refs/heads/{branch}\n"),
        )?;
    }
    Ok(())
}

/// Writes a worktree admin directory. HEAD goes last: git (and
/// `register_branch`) treat a directory with HEAD as a worktree, so a crash
/// earlier leaves something that is rewritten next time instead of a worktree
/// without an index.
fn write_admin(dir: &Path, gitfile: &Path, index: &[u8], head: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    write_atomic(&dir.join("commondir"), b"../..\n")?;
    write_atomic(
        &dir.join("gitdir"),
        format!("{}\n", gitfile.display()).as_bytes(),
    )?;
    // Keeps `git worktree prune` from deleting the entry while unmounted.
    write_atomic(&dir.join("locked"), b"managed by gitfold\n")?;
    write_atomic(&dir.join("index"), index)?;
    write_atomic(&dir.join("HEAD"), head.as_bytes())?;
    Ok(())
}

/// Replaces `path` with `data` atomically (via `<path>.gitfold-tmp`).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("gitfold-tmp");
    let mut file = fs::File::create(&tmp)?;
    std::io::Write::write_all(&mut file, data)?;
    // Synced before the rename, so a power loss leaves the old file or the new
    // one, never an empty one.
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Keeps `commit` reachable for `git gc` with ref `name` (a workspace's base is
/// not otherwise referenced once its branch moves on). Goes through `git
/// update-ref`, which handles every ref backend (files, packed, reftable).
pub fn pin(common_dir: &Path, name: &str, commit: &gix::ObjectId) -> Result<()> {
    let commit = commit.to_string();
    git(
        common_dir,
        &["update-ref", "-m", "gitfold: workspace base", name, &commit],
    )
}

/// Deletes ref `name` (a workspace's base pin), if it exists.
pub fn unpin(common_dir: &Path, name: &str) -> Result<()> {
    if crate::git::resolve(name).is_err() {
        return Ok(());
    }
    git(common_dir, &["update-ref", "-d", name])
}

/// The ref keeping workspace `id`'s base from `git gc`.
pub fn pin_ref(id: &str) -> String {
    format!("refs/gitfold/{id}")
}

fn git(common_dir: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(args)
        .status()?;
    anyhow::ensure!(status.success(), "git {} failed ({status})", args.join(" "));
    Ok(())
}

/// Files in a worktree's admin directory that mean a git operation is under
/// way, or stopped half-way waiting for the user (conflicts, `rebase -i`,
/// `bisect`).
const BUSY: [&str; 9] = [
    "index.lock",
    "HEAD.lock",
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "sequencer",
];

/// Why git is not idle in the worktree with admin directory `admin` on
/// `branch`, if it isn't: HEAD elsewhere (detached for a rebase or bisect, or
/// switched away), an operation running or stopped half-way, or a ref locked.
pub fn git_busy(common_dir: &Path, admin: &Path, branch: &str) -> Option<String> {
    let head = fs::read_to_string(admin.join("HEAD")).unwrap_or_default();
    if head.trim() != format!("ref: refs/heads/{branch}") {
        return Some(format!("its HEAD is not on {branch} ({})", head.trim()));
    }
    let exists = |p: PathBuf| fs::symlink_metadata(p).is_ok();
    if let Some(f) = BUSY.iter().find(|f| exists(admin.join(f))) {
        return Some(format!("a git operation is in progress ({f})"));
    }
    let ref_lock = format!("refs/heads/{branch}.lock");
    (exists(common_dir.join(ref_lock)) || exists(common_dir.join("packed-refs.lock")))
        .then(|| "a ref is locked".to_string())
}

/// `<common>/gitfold`: the workspaces' state directories, by id.
pub fn state_root(common_dir: &Path) -> PathBuf {
    common_dir.join("gitfold")
}

/// Takes the lock on `<common>/gitfold/.lock` that a mounted daemon holds for
/// its life and `remove`/`prune` hold while working (exclusively, so one
/// repository has at most one daemon, and none while those run), without
/// waiting: `None` if it is taken. Released when the returned file is closed,
/// also when the process dies.
pub fn lock(common_dir: &Path) -> Result<Option<fs::File>> {
    let root = state_root(common_dir);
    fs::create_dir_all(&root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(".lock"))?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(e.into());
    }
    Ok(Some(file))
}

/// A workspace's id, naming its state and registration directories: the branch
/// with `%` and `/` escaped.
pub fn workspace_id(branch: &str) -> String {
    branch.replace('%', "%25").replace('/', "%2F")
}

/// The branch of workspace id `id` (inverse of `workspace_id`).
pub fn workspace_branch(id: &str) -> String {
    id.replace("%2F", "/").replace("%25", "%")
}

/// Whether `name` (in `<common>/worktrees/`) is a snapshot registration
/// (`gitfold-<sha12>`, see `register`).
pub fn is_snapshot_registration(name: &str) -> bool {
    name.strip_prefix(SNAPSHOT_PREFIX)
        .is_some_and(|hex| hex.len() == SNAPSHOT_HEX && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}
