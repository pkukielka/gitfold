//! Inode numbers, computed rather than stored.
//!
//! - files: `FILE_BIT | file_id`. File ids are per distinct (blob, mode), so the
//!   same content has the same inode in every snapshot; the kernel then keeps a
//!   single inode and page-cache copy for all of them (like read-only hardlinks,
//!   but reported with nlink 1 so tools don't treat them as hardlinks).
//! - directories: `(slot + 1) << 32 | dir_index`, unique per snapshot, since the
//!   kernel cannot have one directory under two parents.
//! - the root is 1; each snapshot's virtual `.git` file uses `dir_index = u32::MAX`.
//!
//! The git index stores the low 32 bits, which must match what `stat` reports.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ROOT: u64 = 1;
const FILE_BIT: u64 = 1 << 63;
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
}

pub fn file(id: u32) -> u64 {
    FILE_BIT | id as u64
}

pub fn dir(slot: u32, dir: u32) -> u64 {
    debug_assert_ne!(dir, GITFILE);
    ((slot as u64 + 1) << 32) | dir as u64
}

pub fn gitfile(slot: u32) -> u64 {
    ((slot as u64 + 1) << 32) | GITFILE as u64
}

pub fn decode(ino: u64) -> Option<Ino> {
    if ino == ROOT {
        return Some(Ino::Root);
    }
    if ino & FILE_BIT != 0 {
        return u32::try_from(ino & !FILE_BIT).ok().map(Ino::File);
    }
    let slot = u32::try_from((ino >> 32).checked_sub(1)?).ok()?;
    match ino as u32 {
        GITFILE => Some(Ino::GitFile { slot }),
        dir => Some(Ino::Dir { slot, dir }),
    }
}
