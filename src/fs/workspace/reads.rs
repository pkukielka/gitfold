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
//! Not a file searcher's (`procs::Scanners`): a search reads all it passes.
//! Reads the kernel served from its cache are reported by Claude Code's hook
//! (`note_reads`).
//! A content prewarm would read every file before the agent, leaving its reads
//! unseen: a tracked workspace's `content` walks as `meta`. Off by default: an
//! agent grepping the whole tree reads every file. A fork inherits the log:
//! its changes were built on those reads. Turning tracking off keeps the log.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};

use anyhow::Result;
use log::warn;

use super::Workspace;
use crate::state::reads::{paths, whole};

/// The paths recorded, and the log they are appended to.
pub(super) struct Reads {
    seen: HashSet<Vec<u8>>,
    log: File,
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
        let path = self.state.reads();
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

    /// Records that `local` was read from its start, if tracked and not by
    /// a file searcher (`scan`, asked only for a path not yet recorded).
    pub fn note_read(&self, local: u64, scan: impl FnOnce() -> bool) {
        let mut reads = self.reads.lock();
        let Some(reads) = reads.as_mut() else {
            return;
        };
        let Ok(path) = self.path(local) else {
            return;
        };
        if !reads.seen.contains(&path) && !scan() {
            self.record(reads, path);
        }
    }

    /// Records that the files at `paths` (relative to the workspace root) were
    /// read, if tracked: reads the daemon didn't see, the kernel having
    /// served them from its cache (`gitfold track-reads --claude-hook`).
    /// Returns how many were new.
    pub fn note_reads(&self, paths: impl IntoIterator<Item = Vec<u8>>) -> usize {
        let mut reads = self.reads.lock();
        let Some(reads) = reads.as_mut() else {
            return 0;
        };
        let mut new = 0;
        for path in paths {
            if !reads.seen.contains(&path) && self.record(reads, path) {
                new += 1;
            }
        }
        new
    }

    /// Appends `path` to the log; whether it was.
    fn record(&self, reads: &mut Reads, path: Vec<u8>) -> bool {
        let mut line = path.clone();
        line.push(0);
        if let Err(e) = reads.log.write_all(&line) {
            warn!("recording a read in workspace {}: {e}", self.state.branch);
            return false;
        }
        reads.seen.insert(path)
    }
}
