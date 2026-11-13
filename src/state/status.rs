//! What a workspace changes relative to its base, read from its state
//! directory without mounting (for `gitfold list` and `gitfold remove`).
//!
//! A changed path is an upper file or symlink whose content or executable bit
//! differs from the base's file at that path (or that the base doesn't have).
//! A deleted path is a base file or submodule the view no longer shows: hidden
//! by a whiteout or an opaque directory, or below a path upper replaced with a
//! file, and not present in upper as a file. Directories alone are not changes,
//! as for git.
//!
//! Untracked upper files git ignores (by the `.gitignore` files the workspace
//! shows, `info/exclude` or `core.excludesFile`) are counted apart: not changes,
//! as for `git worktree remove`. As for git, an untracked directory holding a
//! repository (`.git`) is one entry, and nothing under a submodule counts as
//! ignored.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::objs::tree::EntryKind;

use super::markers::{Markers, PathSet, hidden};
use super::{ModeCheck, StateDir, matches_stored};
use super::{ancestors, join, upper_path};
use crate::fsutil::not_found;
use crate::git;
use crate::store::{Lower, Store, TreeTable};

pub struct Status {
    pub base: ObjectId,
    pub changed: Vec<Vec<u8>>,
    pub deleted: Vec<Vec<u8>>,
    /// How many untracked upper files git ignores (build output): not
    /// changes, as for `git worktree remove`.
    pub ignored: usize,
    /// The newest modification time in upper (files, directories), if any:
    /// when something was last written, created or removed there.
    pub newest: Option<SystemTime>,
    /// The base's root tree in the store.
    tree: u32,
}

/// What happened to a path, as `git diff --name-status` says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Added,
    Modified,
    Deleted,
}

impl Change {
    pub fn letter(self) -> char {
        match self {
            Change::Added => 'A',
            Change::Modified => 'M',
            Change::Deleted => 'D',
        }
    }
}

/// Lines added and removed at a path, as `git diff --numstat` counts them, or
/// `None` for a binary file (a NUL in its first 8000 bytes, as git decides).
pub type LineCounts = Option<(u32, u32)>;

impl Status {
    /// The lines `change` at `path` added and removed relative to the base,
    /// with upper in `state`: the base's blob against upper's file (a
    /// symlink's target, as git diffs it).
    pub fn line_counts(
        &self,
        store: &Store,
        state: &StateDir,
        change: Change,
        path: &[u8],
    ) -> Result<LineCounts> {
        let before = match store.entry_at(self.tree, path) {
            Some(Lower::File(id)) if change != Change::Added => {
                let rec = store.file(id).context("stored file")?;
                git::read_blob(rec.oid)?
            }
            // A submodule: one line, its commit, as git shows it.
            Some(Lower::Gitlink) => b"Subproject commit\n".to_vec(),
            _ => Vec::new(),
        };
        let upper = upper_path(&state.upper(), path);
        let after = match change {
            Change::Deleted => Vec::new(),
            _ => {
                let meta = fs::symlink_metadata(&upper)?;
                if meta.is_symlink() {
                    fs::read_link(&upper)?.into_os_string().into_vec()
                } else if meta.is_dir() {
                    // An untracked repository: git shows it as one entry.
                    return Ok(None);
                } else {
                    fs::read(&upper)?
                }
            }
        };
        let binary = |d: &[u8]| d[..d.len().min(8000)].contains(&0);
        if binary(&before) || binary(&after) {
            return Ok(None);
        }
        let input = imara_diff::InternedInput::new(
            imara_diff::sources::byte_lines(&before),
            imara_diff::sources::byte_lines(&after),
        );
        let diff = imara_diff::Diff::compute(imara_diff::Algorithm::Myers, &input);
        Ok(Some((diff.count_additions(), diff.count_removals())))
    }

    /// Every changed and deleted path, sorted, with what happened to it:
    /// modified if the base has a file (or submodule) there, else added (a file
    /// where the base has a directory is added; the directory's files are
    /// deleted).
    pub fn changes(&self, store: &Store) -> Vec<(Change, &[u8])> {
        let mut all: Vec<(Change, &[u8])> = self
            .changed
            .iter()
            .map(|p| match store.entry_at(self.tree, p) {
                Some(Lower::File(_) | Lower::Gitlink) => (Change::Modified, p.as_slice()),
                _ => (Change::Added, p.as_slice()),
            })
            .chain(self.deleted.iter().map(|p| (Change::Deleted, p.as_slice())))
            .collect();
        all.sort_by(|a, b| a.1.cmp(b.1));
        all
    }
}

/// The changes of the workspace stored in `state`, in repository `common_dir`.
pub fn status(store: &Store, common_dir: &Path, state: &StateDir) -> Result<Status> {
    let base = state.read_base()?;
    let tree = store.load_tree(base.tree)?;
    let (whiteouts, opaque) = Markers::read(&state.markers())?;
    let upper = state.upper();
    let mut buf = Vec::new();
    // The worktree's own view of the config (worktree config, `includeIf`).
    let config_dir = match state.admin.exists() {
        true => &state.admin,
        false => common_dir,
    };
    let ignore = gix::ignore::Search::from_git_dir(
        common_dir,
        git::excludes_file(config_dir)?,
        &mut buf,
        Default::default(),
    )?;
    let mut walk = Walk {
        store,
        upper: &upper,
        whiteouts: &whiteouts,
        opaque: &opaque,
        ignore,
        changed: Vec::new(),
        deleted: BTreeSet::new(),
        ignored: 0,
        newest: None,
    };
    walk.changed_below(b"", Some(tree), Ignoring::Match)?;
    let Walk {
        changed,
        mut deleted,
        ignored,
        newest,
        ..
    } = walk;
    // Base entries hidden by a marker (one below another marked path is
    // covered by the walk of the outer one).
    let marked = |p: &[u8]| whiteouts.contains(p) || opaque.contains(p);
    for path in whiteouts.iter().chain(&opaque) {
        if !ancestors(path).any(marked)
            && let Some(lower) = store.entry_at(tree, path)
        {
            deleted_below(store, &upper, path, lower, &mut deleted)?;
        }
    }
    Ok(Status {
        base: base.id,
        changed,
        deleted: deleted.into_iter().collect(),
        ignored,
        newest,
        tree,
    })
}

/// The walk of upper behind `status`.
struct Walk<'a> {
    store: &'a Store,
    upper: &'a Path,
    whiteouts: &'a PathSet,
    opaque: &'a PathSet,
    /// Ignore patterns: global and `info/exclude`, then the `.gitignore` of
    /// each directory from the root down to the one being walked.
    ignore: gix::ignore::Search,
    changed: Vec<Vec<u8>>,
    deleted: BTreeSet<Vec<u8>>,
    ignored: usize,
    newest: Option<SystemTime>,
}

/// Which untracked files below a directory count as ignored.
#[derive(Clone, Copy, PartialEq)]
enum Ignoring {
    /// Those the ignore patterns match.
    Match,
    /// All: git ignores the directory (patterns below can't re-include).
    All,
    /// None: the directory is a submodule's.
    Never,
}

impl Walk<'_> {
    /// Upper files and symlinks below `dir` (whose base tree is `tree`, if any)
    /// that differ from the base's, and base files below upper files that
    /// replaced a base directory, counting ignored untracked files as `ignoring`
    /// says.
    fn changed_below(&mut self, dir: &[u8], tree: Option<u32>, ignoring: Ignoring) -> Result<()> {
        let dir_path = upper_path(self.upper, dir);
        let entries = match fs::read_dir(&dir_path) {
            Ok(entries) => entries,
            Err(e) if not_found(&e) => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir_path.display())),
        };
        self.saw(&fs::metadata(&dir_path)?);
        let table = tree.map(|t| self.store.tree(t));
        let lists = self.ignore.patterns.len();
        if ignoring == Ignoring::Match {
            self.add_gitignore(dir, table.as_deref())?;
        }
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let path = join(dir, name.as_bytes());
            let lower = table
                .as_ref()
                .and_then(|t| t.find(name.as_bytes()))
                .map(Lower::of);
            let meta = entry.metadata()?;
            self.saw(&meta);
            if meta.is_dir() {
                let below = match lower {
                    Some(Lower::Tree(t)) => Some(t),
                    _ => None,
                };
                // Git never looks inside an untracked directory holding a
                // repository: unless ignored, it is one untracked entry.
                if !matches!(lower, Some(Lower::Tree(_) | Lower::Gitlink))
                    && fs::symlink_metadata(entry.path().join(".git")).is_ok()
                {
                    let ignored = ignoring == Ignoring::All
                        || (ignoring == Ignoring::Match && self.is_ignored(&path, true));
                    if ignored {
                        self.ignored += 1;
                    } else {
                        self.changed.push(path);
                    }
                    continue;
                }
                let below_ignoring = match (ignoring, lower) {
                    // A submodule's files are its own; none counts as ignored.
                    (_, Some(Lower::Gitlink)) | (Ignoring::Never, _) => Ignoring::Never,
                    (Ignoring::All, _) => Ignoring::All,
                    (Ignoring::Match, _) if self.is_ignored(&path, true) => Ignoring::All,
                    (Ignoring::Match, _) => Ignoring::Match,
                };
                self.changed_below(&path, below, below_ignoring)?;
                continue;
            }
            let same = match lower {
                Some(Lower::File(id)) => {
                    let rec = self.store.file(id).context("stored file")?;
                    matches_stored(&entry.path(), &meta, &rec, ModeCheck::Git)?
                }
                // A file where the base has a directory: its files are gone.
                Some(dir @ (Lower::Tree(_) | Lower::Gitlink)) => {
                    deleted_below(self.store, self.upper, &path, dir, &mut self.deleted)?;
                    false
                }
                // Untracked: ignored output, or a new file.
                None if ignoring == Ignoring::All
                    || (ignoring == Ignoring::Match && self.is_ignored(&path, false)) =>
                {
                    self.ignored += 1;
                    continue;
                }
                None => false,
            };
            if !same {
                self.changed.push(path);
            }
        }
        self.ignore.patterns.truncate(lists);
        Ok(())
    }

    /// Adds the patterns of `dir/.gitignore` as the workspace shows it: the
    /// upper entry if there is one, else the base's unless deleted (`table`: the
    /// base tree of `dir`). As for git, only a regular file counts: a symlink
    /// (git doesn't follow it), a directory or anything else gives no patterns.
    fn add_gitignore(&mut self, dir: &[u8], table: Option<&TreeTable>) -> Result<()> {
        let path = join(dir, b".gitignore");
        let upper = upper_path(self.upper, &path);
        let bytes = match fs::symlink_metadata(&upper) {
            Ok(meta) if meta.is_file() => Some(fs::read(&upper)?),
            Ok(_) => None,
            Err(e) if not_found(&e) => match table.and_then(|t| t.find(b".gitignore")) {
                Some(e)
                    if matches!(e.kind, EntryKind::Blob | EntryKind::BlobExecutable)
                        && !hidden(self.whiteouts, self.opaque, &path) =>
                {
                    let rec = self.store.file(e.id).context("stored file")?;
                    Some(git::read_blob(rec.oid)?)
                }
                _ => None,
            },
            Err(e) => return Err(e.into()),
        };
        if let Some(bytes) = bytes {
            let source = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&path));
            self.ignore.add_patterns_buffer(
                &bytes,
                source,
                Some(Path::new("")),
                Default::default(),
            )?;
        }
        Ok(())
    }

    /// Keeps the newest modification time seen.
    fn saw(&mut self, meta: &fs::Metadata) {
        if let Ok(t) = meta.modified() {
            self.newest = self.newest.max(Some(t));
        }
    }

    /// Whether git ignores untracked `path`.
    fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        let m = self.ignore.pattern_matching_relative_path(
            path.into(),
            Some(is_dir),
            gix::glob::pattern::Case::Sensitive,
        );
        m.is_some_and(|m| !m.pattern.is_negative())
    }
}

/// Base files and submodules at or below `path` (lower entry `lower`) that
/// upper doesn't have as a file (or, for a submodule, as anything).
fn deleted_below(
    store: &Store,
    upper: &Path,
    path: &[u8],
    lower: Lower,
    out: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    let in_upper = fs::symlink_metadata(upper_path(upper, path));
    match lower {
        Lower::Tree(tree) => {
            let table = store.tree(tree);
            for e in table.entries() {
                let child = join(path, table.name(e));
                deleted_below(store, upper, &child, Lower::of(e), out)?;
            }
        }
        Lower::Gitlink => {
            if in_upper.is_err() {
                out.insert(path.to_vec());
            }
        }
        Lower::File(_) => {
            // A directory there (recreated over the deleted file) doesn't
            // replace it.
            if !in_upper.is_ok_and(|m| !m.is_dir()) {
                out.insert(path.to_vec());
            }
        }
    }
    Ok(())
}
