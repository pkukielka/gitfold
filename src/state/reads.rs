//! The read log of a workspace (`reads` in its state directory): the files read
//! while read tracking is on (`fs::workspace::reads` records them), for
//! `gitfold overlaps`. NUL-terminated paths, once each, appended; a partial
//! last path (a crash, an append in progress) is left out.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Result;

use crate::settings;

pub const READS: &str = "reads";

/// The paths recorded in the workspace stored in `state_dir`, or `None` if
/// reads were never tracked there.
pub fn recorded(state_dir: &Path) -> Result<Option<HashSet<Vec<u8>>>> {
    match fs::read(state_dir.join(READS)) {
        Ok(data) => Ok(Some(paths(whole(&data)))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `data` up to its last whole path: a path being appended, or cut short by a
/// crash, is left out.
pub fn whole(data: &[u8]) -> &[u8] {
    let end = data.iter().rposition(|b| *b == 0).map_or(0, |i| i + 1);
    &data[..end]
}

pub fn paths(data: &[u8]) -> HashSet<Vec<u8>> {
    data.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Whether the workspace of `branch` records reads: its own setting if any,
/// else `default` (the mount's). Read when it opens, and by `gitfold
/// track-reads` while open.
pub fn tracked(common_dir: &Path, branch: &str, default: bool) -> bool {
    settings::branch_bool(common_dir, branch, settings::TRACK_READS).unwrap_or(default)
}
