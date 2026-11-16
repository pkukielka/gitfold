//! Moving a branch that no worktree has checked out onto another commit with
//! `git rebase` itself, in a temporary worktree that checks out almost
//! nothing: a sparse checkout of the top-level files only, with a sparse
//! index, so its cost doesn't grow with the tree (a monorepo's million files
//! aren't written). Git does everything a rebase does: it skips commits
//! whose change is there already (by patch id), drops commits that become
//! empty, keeps authors and runs the hooks; on a conflict it stops, says
//! which paths conflict (`git diff --diff-filter=U`), and is aborted, so the
//! branch stays where it was.
//!
//! The worktree is `<common>/gitfold/.moves/gitfold-move-<id>`, made under a
//! lock on `<id>.lock` beside it and removed after. One left by a process
//! that died is cleaned up by the next move of that branch (its rebase
//! aborted: git moves the branch only when the rebase finishes). The sparse
//! settings are given on the command line (`-c`) and the patterns written to
//! the worktree's own `info/sparse-checkout`, so the repository's config
//! isn't changed (`git sparse-checkout` would turn on
//! `extensions.worktreeConfig` for good).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use gix::ObjectId;

use crate::lock::Lock;
use crate::state::{state_root, workspace_id};

/// The sparse checkout: the top-level files, no directory.
const PATTERNS: &str = "/*\n!/*/\n";

/// Where the moves' worktrees are.
fn moves_dir(common_dir: &Path) -> PathBuf {
    state_root(common_dir).join(".moves")
}

/// Whether `path` is the worktree of a move (one that a move left, if it
/// is there when none runs).
pub fn is_scratch(common_dir: &Path, path: &Path) -> bool {
    path.parent() == Some(moves_dir(common_dir).as_path())
}

/// What `rebase` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Rebased {
    /// The branch moved to this commit.
    Moved(ObjectId),
    /// A commit conflicted on these paths; the branch didn't move.
    Conflict(Vec<String>),
}

/// The temporary worktree of a move of `branch`, and its lock.
struct Scratch {
    path: PathBuf,
    common_dir: PathBuf,
    _lock: Lock,
}

/// Rebases local branch `branch`, at `tip`, onto `onto`: its commits after
/// `upstream` (`git rebase --onto <onto> <upstream>`). Fails if another move
/// of it is running, if it isn't at `tip` any more, or if it is checked out
/// somewhere (`git worktree add` refuses it).
pub fn rebase(
    common_dir: &Path,
    branch: &str,
    tip: ObjectId,
    onto: ObjectId,
    upstream: ObjectId,
) -> Result<Rebased> {
    let scratch = Scratch::make(common_dir, branch)?;
    let rebased = scratch.rebase(branch, tip, onto, upstream);
    let removed = scratch.remove();
    let rebased = rebased?;
    removed?;
    Ok(rebased)
}

impl Scratch {
    /// Locks the moves of `branch`, cleans up a worktree a dead move left,
    /// and adds a new one on the branch, nothing checked out.
    fn make(common_dir: &Path, branch: &str) -> Result<Scratch> {
        let dir = moves_dir(common_dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let id = workspace_id(branch);
        let Some(lock) = crate::lock::try_file(&dir.join(format!("{id}.lock")))? else {
            bail!("another move of {branch} is running");
        };
        let scratch = Scratch {
            path: dir.join(format!("gitfold-move-{id}")),
            common_dir: common_dir.to_path_buf(),
            _lock: lock,
        };
        let admins = scratch.registrations()?;
        if scratch.path.exists() || !admins.is_empty() {
            log::warn!("{branch}: cleaning up the worktree of an interrupted move");
            // Its rebase aborted first, as git would. (A rebase moves the
            // branch only when it finishes, so dropping the worktree alone
            // also leaves the branch where it was.)
            if admins.iter().any(|a| a.join("rebase-merge").exists())
                && let Err(e) = scratch.git(&["rebase", "--abort"])
            {
                log::warn!("{branch}: aborting the interrupted move: {e:#}");
            }
            scratch.remove()?;
        }
        let path = scratch.path.to_str().context("a path that isn't UTF-8")?;
        super::cmd::output(
            common_dir,
            &["worktree", "add", "-q", "--no-checkout", path, branch],
        )?;
        fs::create_dir_all(scratch.admin()?.join("info"))?;
        fs::write(scratch.admin()?.join("info/sparse-checkout"), PATTERNS)?;
        scratch.git(&["read-tree", "-mu", "HEAD"])?;
        Ok(scratch)
    }

    /// The worktree's admin directory (`.git/worktrees/<name>`), as git says.
    fn admin(&self) -> Result<PathBuf> {
        let out = self.git(&["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
    }

    /// git in the worktree, sparse; fails if git does.
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = self.command().args(args).output()?;
        if !out.status.success() {
            bail!(
                "git {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.path)
            .args(["-c", "core.sparseCheckout=true"])
            .args(["-c", "core.sparseCheckoutCone=true"])
            .args(["-c", "index.sparse=true"])
            .stdin(Stdio::null());
        command
    }

    fn rebase(
        &self,
        branch: &str,
        tip: ObjectId,
        onto: ObjectId,
        upstream: ObjectId,
    ) -> Result<Rebased> {
        let head = self.git(&["rev-parse", "HEAD"])?;
        if ObjectId::from_hex(String::from_utf8_lossy(&head).trim().as_bytes()).ok() != Some(tip) {
            bail!("{branch} moved meanwhile");
        }
        let (onto, upstream) = (onto.to_string(), upstream.to_string());
        // Only this branch moves, and only its own commits (whatever the
        // user's rebase.* settings say); an editor is never needed.
        let out = self
            .command()
            .env("GIT_EDITOR", ":")
            .args(["rebase", "-q", "--no-update-refs", "--no-autosquash"])
            .args([
                "--no-autostash",
                "--empty=drop",
                "--no-reapply-cherry-picks",
            ])
            .args(["--onto", &onto, &upstream])
            .output()?;
        if out.status.success() {
            let tip = crate::git::branch_tip(branch)?;
            return Ok(Rebased::Moved(tip.id));
        }
        // Stopped at a conflict: the paths, then the branch back.
        let paths = self.git(&["diff", "--name-only", "--diff-filter=U", "-z"]);
        let stopped = self.admin()?.join("rebase-merge").exists();
        if stopped {
            self.git(&["rebase", "--abort"])?;
        }
        match paths {
            Ok(paths) if stopped && !paths.is_empty() => Ok(Rebased::Conflict(
                (paths.split(|b| *b == 0))
                    .filter(|p| !p.is_empty())
                    .map(|p| String::from_utf8_lossy(p).into_owned())
                    .collect(),
            )),
            _ => bail!(
                "git rebase --onto {onto} {upstream} on {branch} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    /// The registrations (`.git/worktrees/<name>`) of the worktree's path.
    fn registrations(&self) -> Result<Vec<PathBuf>> {
        let gitfile = self.path.join(".git");
        let Ok(entries) = fs::read_dir(self.common_dir.join("worktrees")) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in entries {
            let admin = entry?.path();
            let gitdir = fs::read_to_string(admin.join("gitdir")).unwrap_or_default();
            if Path::new(gitdir.trim()) == gitfile {
                out.push(admin);
            }
        }
        Ok(out)
    }

    /// Removes the worktree and its registration: by git, else (half made,
    /// or its directory gone) by hand. Never `git worktree prune`, which
    /// would also drop the registrations of workspaces not mounted now.
    fn remove(&self) -> Result<()> {
        let path = self.path.to_str().context("a path that isn't UTF-8")?;
        let args = ["worktree", "remove", "--force", path];
        if super::cmd::output(&self.common_dir, &args).is_err() {
            if self.path.exists() {
                fs::remove_dir_all(&self.path)?;
            }
            for admin in self.registrations()? {
                fs::remove_dir_all(&admin)?;
            }
        }
        Ok(())
    }
}
