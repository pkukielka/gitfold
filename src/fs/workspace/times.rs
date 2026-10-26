//! Times shown for files dropped from upper: when a rebase or the unchanged-file
//! cleanup drops an upper file because the base has the same content (see
//! `rebase`), the file keeps showing the upper file's mtime and ctime, so the
//! stat data git recorded in the index still matches and `git status` stays fast
//! and quiet.
//!
//! Persisted in `times` (rewritten whole, atomically, when changed): one
//! record per path, `path NUL` followed by mtime and ctime as big-endian
//! (i64 seconds, u32 nanoseconds).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::worktree;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub mtime: SystemTime,
    pub ctime: SystemTime,
}

const TIME_LEN: usize = 12;

fn encode(t: SystemTime, out: &mut Vec<u8>) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    out.extend_from_slice(&(d.as_secs() as i64).to_be_bytes());
    out.extend_from_slice(&d.subsec_nanos().to_be_bytes());
}

fn decode(b: &[u8]) -> SystemTime {
    let secs = i64::from_be_bytes(b[..8].try_into().unwrap()).max(0) as u64;
    let nanos = u32::from_be_bytes(b[8..TIME_LEN].try_into().unwrap()).min(999_999_999);
    UNIX_EPOCH + Duration::new(secs, nanos)
}

pub(super) struct Times {
    map: BTreeMap<Vec<u8>, Stamp>,
    path: PathBuf,
    /// Changed since the last `save`.
    dirty: bool,
}

impl Times {
    pub(super) fn open(path: &Path) -> io::Result<Times> {
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut map = BTreeMap::new();
        let mut rest = &data[..];
        while let Some(nul) = rest.iter().position(|b| *b == 0) {
            let Some(times) = rest.get(nul + 1..nul + 1 + 2 * TIME_LEN) else {
                break;
            };
            let stamp = Stamp {
                mtime: decode(&times[..TIME_LEN]),
                ctime: decode(&times[TIME_LEN..]),
            };
            map.insert(rest[..nul].to_vec(), stamp);
            rest = &rest[nul + 1 + 2 * TIME_LEN..];
        }
        Ok(Times {
            map,
            path: path.to_path_buf(),
            dirty: false,
        })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub(super) fn get(&self, path: &[u8]) -> Option<Stamp> {
        self.map.get(path).copied()
    }

    pub(super) fn set(&mut self, path: &[u8], stamp: Option<Stamp>) {
        let old = match stamp {
            Some(stamp) => self.map.insert(path.to_vec(), stamp),
            None => self.map.remove(path),
        };
        self.dirty |= old != stamp;
    }

    /// Writes the stamps out if they changed.
    pub(super) fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut data = Vec::new();
        for (path, stamp) in &self.map {
            data.extend_from_slice(path);
            data.push(0);
            encode(stamp.mtime, &mut data);
            encode(stamp.ctime, &mut data);
        }
        worktree::write_atomic(&self.path, &data)?;
        self.dirty = false;
        Ok(())
    }
}
