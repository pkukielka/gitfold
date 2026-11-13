//! Whiteouts (deleted lower paths) and opaque markers (directories recreated
//! over a whiteout), persisted as an append-only log in `markers`: a change
//! costs one small append. Rewriting the whole set on every change made `rm -r`
//! of a large lower directory quadratic. The log is compacted when opened if
//! it holds more than the live markers, and in process once it is mostly
//! history; a record cut short by a crash is ignored.

use crate::fsutil;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub enum Kind {
    Whiteout,
    Opaque,
}

/// A set of paths below the workspace root.
pub type PathSet = BTreeSet<Vec<u8>>;

/// Whether the lower entry at `path` is hidden by a whiteout on it or an
/// ancestor, or by an opaque ancestor.
pub fn hidden(whiteouts: &PathSet, opaque: &PathSet, path: &[u8]) -> bool {
    whiteouts.contains(path)
        || super::ancestors(path).any(|p| whiteouts.contains(p) || opaque.contains(p))
}

pub struct Markers {
    pub whiteouts: PathSet,
    pub opaque: PathSet,
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

/// Appends the record adding (`present`) or removing a marker to `data`.
fn push_record(data: &mut Vec<u8>, kind: Kind, present: bool, path: &[u8]) {
    data.push(op(kind, present));
    data.extend_from_slice(path);
    data.push(0);
}

impl Markers {
    /// Opens the log at `path` for changes, compacted unless it already holds
    /// just the live markers.
    pub fn open(path: &Path) -> io::Result<Markers> {
        let (whiteouts, opaque, compact) = Self::parse(path)?;
        let log = match compact {
            true => Self::rewrite(path, &whiteouts, &opaque)?,
            false => {
                let log = OpenOptions::new().create(true).append(true).open(path)?;
                // Records a daemon appended before it died unsynced (see `sync`)
                // are made durable now, as a compaction would.
                log.sync_data()?;
                log
            }
        };
        Ok(Markers {
            whiteouts,
            opaque,
            path: path.to_path_buf(),
            log,
            appended: 0,
        })
    }

    /// The (whiteouts, opaque markers) recorded in the log at `path`, without
    /// opening it for changes.
    pub fn read(path: &Path) -> io::Result<(PathSet, PathSet)> {
        let (whiteouts, opaque, _) = Self::parse(path)?;
        Ok((whiteouts, opaque))
    }

    /// The markers recorded at `path`, and whether the log holds more than
    /// them (history, or a record cut short).
    fn parse(path: &Path) -> io::Result<(PathSet, PathSet, bool)> {
        let (mut whiteouts, mut opaque) = (BTreeSet::new(), BTreeSet::new());
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut records = 0;
        for record in super::reads::records(&data) {
            records += 1;
            let path = &record[1..];
            match record[0] {
                b'W' => whiteouts.insert(path.to_vec()),
                b'w' => whiteouts.remove(path),
                b'O' => opaque.insert(path.to_vec()),
                b'o' => opaque.remove(path),
                _ => false,
            };
        }
        let torn = super::reads::whole(&data).len() != data.len();
        let history = records != whiteouts.len() + opaque.len();
        Ok((whiteouts, opaque, torn || history))
    }

    /// Rewrites the log as the live markers only.
    fn compact(&mut self) -> io::Result<()> {
        self.log = Self::rewrite(&self.path, &self.whiteouts, &self.opaque)?;
        self.appended = 0;
        Ok(())
    }

    /// Replaces the log at `path` with records of these markers; returns it
    /// opened for appending.
    fn rewrite(path: &Path, whiteouts: &PathSet, opaque: &PathSet) -> io::Result<File> {
        let mut data = Vec::new();
        for (kind, set) in [(Kind::Whiteout, whiteouts), (Kind::Opaque, opaque)] {
            for p in set {
                push_record(&mut data, kind, true, p);
            }
        }
        fsutil::write_atomic(path, &data)?;
        OpenOptions::new().append(true).open(path)
    }

    fn of(&self, kind: Kind) -> &PathSet {
        match kind {
            Kind::Whiteout => &self.whiteouts,
            Kind::Opaque => &self.opaque,
        }
    }

    fn set_of(&mut self, kind: Kind) -> &mut PathSet {
        match kind {
            Kind::Whiteout => &mut self.whiteouts,
            Kind::Opaque => &mut self.opaque,
        }
    }

    /// Adds (`present`) or removes a marker; only actual changes are logged.
    /// Returns whether it changed.
    pub fn set(&mut self, kind: Kind, path: &[u8], present: bool) -> io::Result<bool> {
        let set = self.set_of(kind);
        let changed = if present {
            set.insert(path.to_vec())
        } else {
            set.remove(path)
        };
        if changed {
            let mut record = Vec::with_capacity(path.len() + 2);
            push_record(&mut record, kind, present, path);
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
    pub fn sync(&self) -> io::Result<()> {
        self.log.sync_data()
    }

    /// Removes the markers of `kind` strictly below directory `dir`.
    pub fn clear_below(&mut self, kind: Kind, dir: &[u8]) -> io::Result<()> {
        for p in self.below(kind, dir) {
            self.set(kind, &p, false)?;
        }
        Ok(())
    }

    /// Marks directory `dir` opaque, then drops the whiteouts below it, which
    /// that makes moot (in that order: a crash between leaves them hidden).
    pub fn make_opaque(&mut self, dir: &[u8]) -> io::Result<()> {
        self.set(Kind::Opaque, dir, true)?;
        self.clear_below(Kind::Whiteout, dir)
    }

    /// Markers strictly below directory `dir`.
    pub fn below(&self, kind: Kind, dir: &[u8]) -> Vec<Vec<u8>> {
        let set = self.of(kind);
        // Paths below `dir` sort between "dir/" and "dir0" ('0' follows '/').
        let start = [dir, b"/"].concat();
        let end = [dir, b"0"].concat();
        set.range(start..end).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch markers log for test `name`, removed when dropped.
    struct Log(PathBuf);

    impl Log {
        fn new(name: &str, data: &[u8]) -> Log {
            let dir =
                std::env::temp_dir().join(format!("gitfold-markers-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("markers"), data).unwrap();
            Log(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("markers")
        }
    }

    impl Drop for Log {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Opening rewrites the log only if it holds more than the live markers.
    #[test]
    fn compacted_only_with_history() {
        let live = b"Wa\0Od\0".as_slice();
        let ino = |p: &Path| std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap());
        for (name, data, rewritten) in [
            ("live", live, false),
            ("history", b"Wa\0Wb\0wb\0Od\0".as_slice(), true),
            ("torn", b"Wa\0Od\0Wc".as_slice(), true),
        ] {
            let log = Log::new(name, data);
            let before = ino(&log.path());
            let markers = Markers::open(&log.path()).unwrap();
            assert_eq!(ino(&log.path()) != before, rewritten, "{name}");
            assert_eq!(markers.whiteouts, PathSet::from([b"a".to_vec()]), "{name}");
            assert_eq!(markers.opaque, PathSet::from([b"d".to_vec()]), "{name}");
            assert_eq!(fs::read(log.path()).unwrap(), live, "{name}");
        }
    }
}
