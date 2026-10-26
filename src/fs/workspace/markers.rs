//! Whiteouts (deleted lower paths) and opaque markers (directories recreated
//! over a whiteout), persisted as an append-only log in `markers`: a change
//! costs one small append. Rewriting the whole set on every change made `rm -r`
//! of a large lower directory quadratic. The log is compacted when opened, and a
//! record cut short by a crash is ignored.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::worktree;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Whiteout,
    Opaque,
}

pub(super) struct Markers {
    pub(super) whiteouts: BTreeSet<Vec<u8>>,
    pub(super) opaque: BTreeSet<Vec<u8>>,
    path: PathBuf,
    log: File,
    /// Records appended since the last compaction.
    appended: usize,
}

/// Record opcodes; each record is `op path NUL` (paths never contain NUL).
const fn op(kind: Kind, present: bool) -> u8 {
    match (kind, present) {
        (Kind::Whiteout, true) => b'W',
        (Kind::Whiteout, false) => b'w',
        (Kind::Opaque, true) => b'O',
        (Kind::Opaque, false) => b'o',
    }
}

impl Markers {
    pub(super) fn open(path: &Path) -> io::Result<Markers> {
        let (mut whiteouts, mut opaque) = (BTreeSet::new(), BTreeSet::new());
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let complete = data.iter().rposition(|b| *b == 0).map_or(0, |i| i + 1);
        for record in data[..complete]
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
        {
            let path = record[1..].to_vec();
            match record[0] {
                b'W' => whiteouts.insert(path),
                b'w' => whiteouts.remove(&path),
                b'O' => opaque.insert(path),
                b'o' => opaque.remove(&path),
                _ => false,
            };
        }
        let log = OpenOptions::new().create(true).append(true).open(path)?;
        let mut markers = Markers {
            whiteouts,
            opaque,
            path: path.to_path_buf(),
            log,
            appended: 0,
        };
        markers.compact()?;
        Ok(markers)
    }

    /// Rewrites the log as the live markers only.
    fn compact(&mut self) -> io::Result<()> {
        let mut data = Vec::new();
        for (kind, set) in [
            (Kind::Whiteout, &self.whiteouts),
            (Kind::Opaque, &self.opaque),
        ] {
            for p in set {
                data.push(op(kind, true));
                data.extend_from_slice(p);
                data.push(0);
            }
        }
        worktree::write_atomic(&self.path, &data)?;
        self.log = OpenOptions::new().append(true).open(&self.path)?;
        self.appended = 0;
        Ok(())
    }

    fn set_of(&mut self, kind: Kind) -> &mut BTreeSet<Vec<u8>> {
        match kind {
            Kind::Whiteout => &mut self.whiteouts,
            Kind::Opaque => &mut self.opaque,
        }
    }

    /// Adds (`present`) or removes a marker; only actual changes are logged.
    /// Returns whether it changed.
    pub(super) fn set(&mut self, kind: Kind, path: &[u8], present: bool) -> io::Result<bool> {
        let set = self.set_of(kind);
        let changed = if present {
            set.insert(path.to_vec())
        } else {
            set.remove(path)
        };
        if changed {
            let mut record = Vec::with_capacity(path.len() + 2);
            record.push(op(kind, present));
            record.extend_from_slice(path);
            record.push(0);
            (&self.log).write_all(&record)?;
            self.appended += 1;
            // `rm -r` of a large directory adds a whiteout per file, then replaces
            // them with one for the directory; don't keep that history around.
            if self.appended > 10_000 + 2 * (self.whiteouts.len() + self.opaque.len()) {
                self.compact()?;
            }
        }
        Ok(changed)
    }

    /// Flushes appended records to stable storage.
    pub(super) fn sync(&self) -> io::Result<()> {
        self.log.sync_data()
    }

    /// Markers strictly below directory `dir`.
    pub(super) fn below(&self, kind: Kind, dir: &[u8]) -> Vec<Vec<u8>> {
        let set = match kind {
            Kind::Whiteout => &self.whiteouts,
            Kind::Opaque => &self.opaque,
        };
        // Paths below `dir` sort between "dir/" and "dir0" ('0' follows '/').
        let start = [dir, b"/"].concat();
        let end = [dir, b"0"].concat();
        set.range(start..end).cloned().collect()
    }
}
