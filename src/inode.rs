//! Inode numbers, computed rather than stored.
//!
//! - snapshot files: `FILE_BIT | file_id`. File ids are per distinct (blob,
//!   mode), so the same content has the same inode in every snapshot; the kernel
//!   then keeps a single inode and page-cache copy for all of them (like
//!   read-only hardlinks, but reported with nlink 1 so tools don't treat them as
//!   hardlinks).
//! - snapshot directories: `(slot + 1) << 32 | dir_index`, unique per snapshot,
//!   since the kernel cannot have one directory under two parents.
//! - the root is 1; each snapshot's virtual `.git` file uses `dir_index = u32::MAX`.
//! - workspace entries: `WS_BIT | ws_slot << 44 | local`. `local` is derived
//!   from the path (see `path_local`) so it is stable across remounts; 0 is the
//!   workspace (or namespace) directory itself. Workspace entries can't
//!   use the shared file inodes: a write must keep the inode the kernel holds.
//!
//! The git index stores the low 32 bits, which must match what `stat` reports.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ROOT: u64 = 1;
const FILE_BIT: u64 = 1 << 63;
const WS_BIT: u64 = 1 << 62;
const WS_LOCAL_BITS: u32 = 44;
/// Mask of a workspace inode's `local` part; the all-ones `local` is the
/// workspace's virtual `.git` file (`workspace::GITFILE_LOCAL`).
pub const WS_LOCAL_MASK: u64 = (1 << WS_LOCAL_BITS) - 1;
const GITFILE: u32 = u32::MAX;

/// mtime/ctime of everything in a snapshot. Shared file inodes need one value
/// regardless of which commit they are seen through. 2020-01-01T00:00:00Z.
pub const FILE_TIME_SECS: u64 = 1_577_836_800;

pub fn file_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(FILE_TIME_SECS)
}

#[derive(Clone, Copy)]
pub enum Ino {
    Root,
    Dir { slot: u32, dir: u32 },
    GitFile { slot: u32 },
    File(u32),
    Ws { slot: u32, local: u64 },
}

pub fn file(id: u32) -> u64 {
    FILE_BIT | id as u64
}

pub fn dir(slot: u32, dir: u32) -> u64 {
    debug_assert_ne!(dir, GITFILE);
    debug_assert!(slot < (1 << 29), "snapshot slots must stay below WS_BIT");
    ((slot as u64 + 1) << 32) | dir as u64
}

pub fn gitfile(slot: u32) -> u64 {
    ((slot as u64 + 1) << 32) | GITFILE as u64
}

pub fn ws(slot: u32, local: u64) -> u64 {
    debug_assert!(local <= WS_LOCAL_MASK && slot < (1 << 18));
    WS_BIT | (slot as u64) << WS_LOCAL_BITS | local
}

/// Preferred local inode part for a workspace path (relative, no leading `/`):
/// FNV-1a of the path, never 0. Deterministic, so the inode low 32 bits written
/// into the workspace's git index at creation match `stat` after any remount.
pub fn path_local(path: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in path {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h & WS_LOCAL_MASK).max(1)
}

pub fn decode(ino: u64) -> Option<Ino> {
    if ino == ROOT {
        return Some(Ino::Root);
    }
    if ino & FILE_BIT != 0 {
        return u32::try_from(ino & !FILE_BIT).ok().map(Ino::File);
    }
    if ino & WS_BIT != 0 {
        let slot = u32::try_from((ino & !WS_BIT) >> WS_LOCAL_BITS).ok()?;
        return Some(Ino::Ws {
            slot,
            local: ino & WS_LOCAL_MASK,
        });
    }
    let slot = u32::try_from((ino >> 32).checked_sub(1)?).ok()?;
    match ino as u32 {
        GITFILE => Some(Ino::GitFile { slot }),
        dir => Some(Ino::Dir { slot, dir }),
    }
}

/// A file's ctime as reported to the kernel (and recorded by git in its index).
pub fn ctime(meta: &std::fs::Metadata) -> SystemTime {
    use std::os::unix::fs::MetadataExt;
    UNIX_EPOCH + Duration::new(meta.ctime().max(0) as u64, meta.ctime_nsec() as u32)
}
