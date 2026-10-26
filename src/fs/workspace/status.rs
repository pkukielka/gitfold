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
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::objs::tree::EntryKind;

use super::markers::{Markers, PathSet};
use super::rebase::{ModeCheck, matches_stored};
use super::{Lower, Workspace, ancestors, hidden, join, not_found, read_base};
use crate::git;
use crate::store::{Store, TreeTable};

pub struct Status {
    pub base: ObjectId,
    pub changed: Vec<Vec<u8>>,
    pub deleted: Vec<Vec<u8>>,
    /// How many untracked upper files git ignores (build output): not
    /// changes, as for `git worktree remove`.
    pub ignored: usize,
}

/// The changes of the workspace stored in `state_dir`, in repository `common_dir`.
pub fn status(store: &Store, common_dir: &Path, state_dir: &Path) -> Result<Status> {
    let base = read_base(state_dir)?;
    let tree = store.load_tree(base.tree)?;
    let (whiteouts, opaque) = Markers::read(&state_dir.join("markers"))?;
    let upper = state_dir.join("upper");
    let mut buf = Vec::new();
    // The worktree's own view of the config (worktree config, `includeIf`).
    let id = state_dir
        .file_name()
        .context("state dir")?
        .to_string_lossy();
    let admin = crate::worktree::ws_admin_dir(common_dir, &id);
    let config_dir = if admin.exists() {
        admin
    } else {
        common_dir.to_path_buf()
    };
    let ignore = gix::ignore::Search::from_git_dir(
        common_dir,
        git::excludes_file(&config_dir)?,
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
    };
    walk.changed_below(b"", Some(tree), Ignoring::Match)?;
    let Walk {
        changed,
        mut deleted,
        ignored,
        ..
    } = walk;
    // Base entries hidden by a marker (one below another marked path is
    // covered by the walk of the outer one).
    let marked = |p: &[u8]| whiteouts.contains(p) || opaque.contains(p);
    for path in whiteouts.iter().chain(&opaque) {
        if !ancestors(path).any(marked)
            && let Some(lower) = Workspace::lower_in(store, tree, path)
        {
            deleted_below(store, &upper, path, lower, &mut deleted)?;
        }
    }
    Ok(Status {
        base: base.id,
        changed,
        deleted: deleted.into_iter().collect(),
        ignored,
    })
}

fn upper_path(upper: &Path, path: &[u8]) -> std::path::PathBuf {
    upper.join(std::ffi::OsStr::from_bytes(path))
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
