//! Read tracking: the files an agent read in a workspace, for `gitfold overlaps`,
//! which lists those changed since elsewhere: work built on what someone else
//! has changed since.
//!
//! On for the workspaces of a mount with `--track-reads`, and for branches
//! with `branch.<branch>.gitfoldTrackReads` set (`gitfold track-reads`), which
//! wins either way. The kernel keeps file contents cached, so the daemon sees
//! the first read of each file in a workspace; that is what is recorded (reads
//! at offset 0), once per path, in an append-only `reads` log (NUL-terminated
//! paths) in the state directory, which `gitfold overlaps` reads mounted or not.
//! A content prewarm would read every file before the agent, leaving its reads
//! unseen: a tracked workspace's `content` walks as `meta`. Off by default: an
//! agent grepping the whole tree reads every file. A fork inherits the log:
//! its changes were built on those reads. Turning tracking off keeps the log.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::Result;
use log::warn;

use super::Workspace;

pub const READS: &str = "reads";

/// The git config key, below `branch.<branch>`, of a branch's own setting.
pub const BRANCH_KEY: &str = "gitfoldTrackReads";

/// The paths recorded, and the log they are appended to.
pub(super) struct Reads {
    seen: HashSet<Vec<u8>>,
    log: File,
}

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
fn whole(data: &[u8]) -> &[u8] {
    let end = data.iter().rposition(|b| *b == 0).map_or(0, |i| i + 1);
    &data[..end]
}

fn paths(data: &[u8]) -> HashSet<Vec<u8>> {
    data.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Whether the workspace of `branch` records reads: its own setting if any,
/// else `default` (the mount's). Read when it opens, and by `gitfold
/// track-reads` while open.
pub fn tracked(common_dir: &Path, branch: &str, default: bool) -> bool {
    // The repository's own config file, read now (the daemon's repository
    // handle has the config from mount time); where `git config` writes it.
    let path = common_dir.join("config");
    let Ok(config) = gix::config::File::from_path_no_includes(path, gix::config::Source::Local)
    else {
        return default;
    };
    match config.boolean_by("branch", Some(branch.as_bytes().into()), BRANCH_KEY) {
        Ok(Some(on)) => on,
        Ok(None) => default,
        Err(_) => {
            warn!("branch.{branch}.{BRANCH_KEY} is not a boolean");
            default
        }
    }
}

impl Workspace {
    /// Starts or stops recording what is read in this workspace (see the
    /// module doc), on top of what was recorded before.
    pub fn track_reads(&self, on: bool) -> Result<()> {
        let mut reads = self.reads.lock();
        if !on {
            *reads = None;
            return Ok(());
        }
        if reads.is_some() {
            return Ok(());
        }
        let path = self.state_dir.join(READS);
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        // A path cut short by a crash: dropped, or the next would join it.
        let whole = whole(&data);
        if whole.len() < data.len() {
            log.set_len(whole.len() as u64)?;
        }
        *reads = Some(Reads {
            seen: paths(whole),
            log,
        });
        Ok(())
    }

    /// Records that `local` was read from its start, if tracked.
    pub fn note_read(&self, local: u64) {
        let mut reads = self.reads.lock();
        let Some(reads) = reads.as_mut() else {
            return;
        };
        let Ok(path) = self.path(local) else {
            return;
        };
        if reads.seen.contains(&path) {
            return;
        }
        let mut line = path.clone();
        line.push(0);
        if let Err(e) = reads.log.write_all(&line) {
            warn!("recording a read in workspace {}: {e}", self.branch);
            return;
        }
        reads.seen.insert(path);
    }
}
