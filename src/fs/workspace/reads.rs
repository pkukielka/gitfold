//! Read tracking: the files an agent read in a workspace, for `gitfold overlaps`,
//! which lists those changed since elsewhere: work built on what someone else
//! has changed since.
//!
//! On for the workspaces of a mount with `--track-reads`, and for branches
//! with `branch.<branch>.gitfoldTrackReads` set (`gitfold track-reads`), which
//! wins either way. The reads are what Claude Code's hook reports
//! (`note_read`, a `record-reads` control request), recorded once per path in
//! an append-only `reads` log (NUL-terminated paths) in the state directory,
//! which `gitfold overlaps` reads mounted or not. The filesystem records none
//! (ARCHITECTURE, "Overlaps"). A fork inherits the log: its changes were built
//! on those reads. Turning tracking off keeps the log.

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

    /// Records that the file at `path` (relative to the workspace root) was
    /// read, if tracked and not recorded already: as Claude Code's hook
    /// reports it (`gitfold track-reads --claude-hook`).
    pub fn note_read(&self, path: Vec<u8>) {
        let mut reads = self.reads.lock();
        let Some(reads) = reads.as_mut() else {
            return;
        };
        if reads.seen.contains(&path) {
            return;
        }
        let mut line = path.clone();
        line.push(0);
        match reads.log.write_all(&line) {
            Ok(()) => drop(reads.seen.insert(path)),
            Err(e) => warn!("recording a read in workspace {}: {e}", self.state.branch),
        }
    }
}
