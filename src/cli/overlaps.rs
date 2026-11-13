//! `gitfold overlaps`: what a workspace touched that changed elsewhere since.
//!
//! A workspace's writes are its changes against a target: commits on its
//! branch since the two diverged, and its uncommitted changes. Its reads are
//! the files recorded while tracked (`workspace::reads`). Either overlaps a target where
//! the target changed the same path, or a directory above it, or (for a
//! directory replaced by a file) a path below it. A target is a revision, its
//! changes being its commits since it diverged from the workspace's base; a
//! branch with a workspace adds that workspace's uncommitted changes, so a
//! target workspace gives the paths two workspaces both changed.
//!
//! Everything is read from stored state and git, mounted or not.

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::Path;

use anyhow::{Result, bail};
use gix::ObjectId;

use super::write_path;
use crate::git;
use crate::refusal::Refusal;
use crate::state::reads;
use crate::state::retire::{base_note, check_name};
use crate::state::status;
use crate::state::{StateDir, ancestors};
use crate::store::Store;

/// Which lines to print.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kinds {
    /// Reads if recorded, and writes.
    Both,
    Reads,
    Writes,
}

/// The exit code when there are overlaps.
pub const FOUND: i32 = 6;

/// One target and the paths it changed.
struct Target {
    name: String,
    changed: HashSet<Vec<u8>>,
    /// Every directory above a changed path.
    dirs: HashSet<Vec<u8>>,
}

impl Target {
    fn new(name: String, paths: impl IntoIterator<Item = Vec<u8>>) -> Self {
        let changed: HashSet<Vec<u8>> = paths.into_iter().collect();
        let dirs = changed
            .iter()
            .flat_map(|p| ancestors(p))
            .map(<[u8]>::to_vec)
            .collect();
        Target {
            name,
            changed,
            dirs,
        }
    }

    /// Whether this target changed `path`, a directory above it, or (`path`
    /// being a directory then) a path below it.
    fn touches(&self, path: &[u8]) -> bool {
        self.changed.contains(path)
            || self.dirs.contains(path)
            || ancestors(path).any(|d| self.changed.contains(d))
    }
}

/// Whether git itself reads `path` in every directory it walks (`git status`
/// reads each `.gitignore`): recorded, but not what the agent built on.
fn git_reads(path: &[u8]) -> bool {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    name == b".gitignore" || name == b".gitattributes"
}

/// `gitfold overlaps`: prints, for a workspace, `R` (read) and `W` (written)
/// lines naming each target that changed the path since; returns the exit
/// code, `FOUND` if there were any.
pub fn overlaps(repo: &Path, args: OverlapsArgs) -> Result<i32> {
    let kinds = args.kinds();
    let OverlapsArgs {
        branch,
        against,
        against_workspaces,
        nul,
        ..
    } = args;
    let branch = &super::branch_or_current(branch, repo)?;
    check_name(branch)?;
    if branch.starts_with('@') {
        bail!("{branch} is a snapshot: it reads nothing and changes nothing");
    }
    let common_dir = git::init(repo)?;
    git::require_branch(branch)?;
    let state = StateDir::of(&common_dir, branch);
    let opened = state.opened();
    let base = match opened {
        true => state.read_base()?.id,
        false => git::branch_tip(branch)?.id,
    };
    let note = base_note(branch, &base);
    if opened && !note.is_empty() {
        eprintln!("note: {branch}: {note}; its changes are against the stored base");
    }
    let store = Store::default();
    let uncommitted = match opened {
        true => {
            let s = status::status(&store, &common_dir, &state)?;
            s.changed.into_iter().chain(s.deleted).collect()
        }
        false => Vec::new(),
    };
    let read = match opened {
        true => reads::recorded(&state)?,
        false => None,
    };
    match (&read, kinds) {
        (None, Kinds::Reads) => {
            let message = format!(
                "no reads recorded for {branch}: `gitfold track-reads {branch} on`, or mount \
                 with --track-reads"
            );
            return Err(Refusal::NotFound.error(message));
        }
        (None, Kinds::Both) => eprintln!(
            "note: reads aren't tracked in {branch} (`gitfold track-reads {branch} on`): \
             writes only"
        ),
        _ => {}
    }

    let mut targets = Vec::new();
    let mut revs = against;
    if revs.is_empty() && !against_workspaces {
        revs.push(default_target(&common_dir)?);
    }
    for rev in revs {
        anyhow::ensure!(rev != *branch, "{branch} can't be compared with itself");
        targets.push(rev_target(&common_dir, &store, &base, rev)?);
    }
    if against_workspaces {
        for w in crate::state::list(&common_dir)? {
            if w.branch == *branch || !git::branch_exists(&w.branch) {
                continue;
            }
            if targets.iter().any(|t| t.name == w.branch) {
                continue;
            }
            match rev_target(&common_dir, &store, &base, w.branch.clone()) {
                Ok(t) => targets.push(t),
                Err(e) => eprintln!("skipping workspace {}: {e:#}", w.branch),
            }
        }
    }

    let mut lines = BTreeSet::new();
    for t in &targets {
        if kinds != Kinds::Reads {
            // The branch's own commits since it diverged from this target.
            let committed = diff(&common_dir, &t.name, &base.to_string())?;
            for path in committed.into_iter().chain(uncommitted.iter().cloned()) {
                if t.touches(&path) {
                    lines.insert((path, 'W', t.name.clone()));
                }
            }
        }
        if kinds != Kinds::Writes {
            for path in read.iter().flatten() {
                // A path also written says more (git's own reads of a file the
                // agent wrote, for one).
                let written = || lines.contains(&(path.clone(), 'W', t.name.clone()));
                if t.touches(path) && !git_reads(path) && !(kinds == Kinds::Both && written()) {
                    lines.insert((path.clone(), 'R', t.name.clone()));
                }
            }
        }
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for (path, kind, target) in &lines {
        write!(out, "{kind}\t{target}\t")?;
        write_path(&mut out, path, nul)?;
    }
    out.flush()?;
    Ok(if lines.is_empty() { 0 } else { FOUND })
}

/// The remote's default branch (`origin/HEAD`, normally `origin/main`), else
/// `main`. Not the branch's upstream: after `git push -u` that is its own
/// remote branch, which its work is ahead of.
fn default_target(common_dir: &Path) -> Result<String> {
    let out = git::cmd::command(common_dir)
        .args(["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .output()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !name.is_empty() && name != "origin/HEAD" {
        return Ok(name);
    }
    match git::resolve("refs/heads/main") {
        Ok(_) => Ok("main".to_string()),
        Err(_) => bail!("no origin/HEAD and no branch main: name a target with --against"),
    }
}

/// Target `rev`: what it changed since it diverged from `base`, and, for a
/// branch with a workspace, that workspace's uncommitted changes.
fn rev_target(common_dir: &Path, store: &Store, base: &ObjectId, rev: String) -> Result<Target> {
    // A branch, not a tag of the same name (git prefers the tag).
    let branch = git::branch_exists(&rev);
    let spec = match branch {
        true => format!("refs/heads/{rev}"),
        false => rev.clone(),
    };
    let mut changed = diff(common_dir, &base.to_string(), &spec)?;
    let state = StateDir::of(common_dir, &rev);
    if branch && state.opened() {
        let s = status::status(store, common_dir, &state)?;
        changed.extend(s.changed.into_iter().chain(s.deleted));
    }
    Ok(Target::new(rev, changed))
}

/// The paths `to` changed since it diverged from `from` (`git diff
/// from...to`), both sides of a rename.
fn diff(common_dir: &Path, from: &str, to: &str) -> Result<Vec<Vec<u8>>> {
    let range = format!("{from}...{to}");
    let args = ["diff", "--name-only", "-z", "--no-renames", &range, "--"];
    Ok(git::cmd::output(common_dir, &args)?
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// The arguments of `gitfold overlaps`.
#[derive(clap::Args)]
pub struct OverlapsArgs {
    /// The workspace's branch (default: the workspace you are in).
    pub branch: Option<String>,
    /// What to compare with (repeatable; default: origin/HEAD, the remote's
    /// default branch, else main).
    #[arg(long, value_name = "REV")]
    pub against: Vec<String>,
    /// Compare with every other workspace (instead of the default target;
    /// with --against, as well).
    #[arg(long)]
    pub against_workspaces: bool,
    /// Only files read.
    #[arg(long, conflicts_with = "writes")]
    pub reads: bool,
    /// Only paths changed.
    #[arg(long)]
    pub writes: bool,
    /// End each line with NUL instead of a newline, paths unquoted.
    #[arg(short = 'z')]
    pub nul: bool,
}

impl OverlapsArgs {
    /// Which lines `--reads` and `--writes` ask for.
    fn kinds(&self) -> Kinds {
        match (self.reads, self.writes) {
            (true, _) => Kinds::Reads,
            (_, true) => Kinds::Writes,
            _ => Kinds::Both,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(paths: &[&str]) -> Target {
        Target::new("t".into(), paths.iter().map(|p| p.as_bytes().to_vec()))
    }

    #[test]
    fn touches_paths_and_directories_both_ways() {
        let t = target(&["a/b/c.txt", "d"]);
        assert!(t.touches(b"a/b/c.txt"), "the same path");
        assert!(
            t.touches(b"a/b"),
            "a directory above a changed path (replaced by a file)"
        );
        assert!(
            t.touches(b"d/x"),
            "below a path changed (a directory replaced by a file)"
        );
        assert!(!t.touches(b"a/bc"), "a sibling with a common prefix");
        assert!(!t.touches(b"dx"));
    }

    #[test]
    fn git_reads_are_ignore_and_attribute_files() {
        assert!(git_reads(b".gitignore") && git_reads(b"x/y/.gitattributes"));
        assert!(!git_reads(b"x/.gitignore.bak") && !git_reads(b"gitignore"));
    }
}
