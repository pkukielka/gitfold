//! Moving a branch that no worktree has checked out onto another commit with
//! `git rebase` itself, in a temporary worktree that checks out almost
//! nothing: a sparse checkout of the top-level files only, with a sparse
//! index, so its cost doesn't grow with the tree (a monorepo's million files
//! aren't written). Git does everything a rebase does: it skips commits
//! whose change is there already (by patch id, for `git rebase <onto>`),
//! drops commits that become empty, keeps authors and runs the hooks; on a
//! conflict it stops, says which paths conflict (`git diff
//! --diff-filter=U`), and is aborted, so the branch stays where it was.
//!
//! The worktree is `<common>/gitfold-moves/gitfold-move-<id>` (not under
//! `<common>/gitfold/`, whose presence means the repository is used with
//! gitfold mounts), made under a lock on `<id>.lock` beside it (`lock`) and
//! removed after. One left by a process that died is cleaned up by the next
//! move of that branch (its rebase aborted: git moves the branch only when
//! the rebase finishes). The sparse settings are given on the command line
//! (`-c`) and the patterns written to the worktree's own
//! `info/sparse-checkout`, so the repository's config isn't changed (`git
//! sparse-checkout` would turn on `extensions.worktreeConfig` for good).
//!
//! Not `git replay`, which needs no worktree: it is experimental, and (git
//! 2.49) keeps commits that become empty and skips none by patch id, so its
//! result isn't what `git rebase` makes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use gix::ObjectId;

use crate::lock::Lock;
use crate::refusal::Refusal;
use crate::state::workspace_id;

/// The sparse checkout: the top-level files, no directory.
const PATTERNS: &str = "/*\n!/*/\n";

/// Where the moves' worktrees are.
fn moves_dir(common_dir: &Path) -> PathBuf {
    common_dir.join("gitfold-moves")
}

/// Whether `path` is the worktree of a move (one that a move left, if it
/// is there when none runs).
pub fn is_scratch(common_dir: &Path, path: &Path) -> bool {
    path.parent() == Some(moves_dir(common_dir).as_path())
}

/// Takes the lock of the moves of `branch` (one at a time), or fails if
/// another holds it.
pub fn lock(common_dir: &Path, branch: &str) -> Result<Lock> {
    let dir = moves_dir(common_dir);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}.lock", workspace_id(branch)));
    crate::lock::try_file(&path)?
        .ok_or_else(|| Refusal::InUse.error(format!("another move of {branch} is running")))
}

/// Takes the lock under which the tree of parents changes (one for the
/// repository, beside the moves' locks), or fails if another holds it.
pub fn tree_lock(common_dir: &Path) -> Result<Lock> {
    let dir = moves_dir(common_dir);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::lock::try_file(&dir.join(".tree.lock"))?.ok_or_else(|| {
        Refusal::InUse.error("another gitfold command is changing the parents of branches; retry")
    })
}

/// What `rebase` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Rebased {
    /// The branch moved to this commit.
    Moved(ObjectId),
    /// A commit conflicted on these paths (as stored); the branch didn't
    /// move.
    Conflict(Vec<Vec<u8>>),
}

/// The temporary worktree of a move.
struct Scratch {
    path: PathBuf,
    common_dir: PathBuf,
}

/// Rebases local branch `branch`, at `tip`, onto `onto`: `git rebase
/// <onto>`, or with `upstream`, `git rebase --onto <onto> <upstream>` (its
/// commits after `upstream`). The caller holds `lock`. Fails if it isn't at
/// `tip` any more, or if it is checked out somewhere (`git worktree add`
/// refuses it).
pub fn rebase(
    common_dir: &Path,
    branch: &str,
    tip: ObjectId,
    onto: ObjectId,
    upstream: Option<ObjectId>,
) -> Result<Rebased> {
    let scratch = Scratch::of(common_dir, branch);
    let rebased = scratch
        .make(branch)
        .and_then(|()| scratch.rebase(branch, tip, onto, upstream));
    let removed = scratch.remove();
    let rebased = rebased?;
    removed?;
    Ok(rebased)
}

/// Removes the worktree a move of `branch` that died left, its rebase
/// aborted first, as git would. (A rebase moves the branch only when it
/// finishes, so dropping the worktree alone also leaves the branch where it
/// was.) The caller holds `lock`.
pub fn clean_up(common_dir: &Path, branch: &str) -> Result<()> {
    Scratch::of(common_dir, branch).clean_up(branch)
}

impl Scratch {
    fn of(common_dir: &Path, branch: &str) -> Scratch {
        Scratch {
            path: moves_dir(common_dir).join(format!("gitfold-move-{}", workspace_id(branch))),
            common_dir: common_dir.to_path_buf(),
        }
    }

    fn clean_up(&self, branch: &str) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        log::warn!("{branch}: cleaning up the worktree of an interrupted move");
        // Only in a worktree that is one (with its `.git` file): git would
        // otherwise find the repository's own worktree above it.
        if self.path.join(".git").is_file()
            && self.admin().is_ok_and(|a| a.join("rebase-merge").exists())
            && let Err(e) = self.git(&["rebase", "--abort"])
        {
            log::warn!("{branch}: aborting the interrupted move: {e:#}");
        }
        self.remove()
    }

    /// Adds the worktree on the branch, nothing checked out but the
    /// top-level files. A registration of it whose directory is gone (a
    /// removal that failed half-way) is taken over (`-f`, only then: it also
    /// lifts git's refusal of a branch checked out elsewhere).
    fn make(&self, branch: &str) -> Result<()> {
        let path = self.path.to_str().context("a path that isn't UTF-8")?;
        let args = ["worktree", "add", "-q", "--no-checkout", path, branch];
        if let Err(e) = self.git_here(&args) {
            let ours = super::worktree::checked_out(&self.common_dir, branch, None)
                .is_some_and(|at| Path::new(&at) == self.path);
            if !ours {
                return Err(e);
            }
            let forced = ["worktree", "add", "-q", "-f", "--no-checkout", path, branch];
            self.git_here(&forced)?;
        }
        let info = self.admin()?.join("info");
        fs::create_dir_all(&info)?;
        fs::write(info.join("sparse-checkout"), PATTERNS)?;
        self.git(&["read-tree", "-mu", "HEAD"]).map(drop)
    }

    /// The worktree's admin directory (`.git/worktrees/<name>`), as git says.
    fn admin(&self) -> Result<PathBuf> {
        let out = self.git(&["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
    }

    /// git in the worktree, sparse; fails if git does.
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        super::cmd::output_of(&mut self.command()?, args, None)
    }

    /// git on the repository (`git worktree add`, `remove`): not the
    /// caller's worktree either (see `cmd::in_other_worktree`).
    fn git_here(&self, args: &[&str]) -> Result<Vec<u8>> {
        super::cmd::output_of(
            &mut super::cmd::command_in_other(&self.common_dir)?,
            args,
            None,
        )
    }

    /// git in the worktree, as `cmd::in_other_worktree` runs git in another
    /// worktree than the caller's.
    fn command(&self) -> Result<Command> {
        let mut command = Command::new("git");
        super::cmd::in_other_worktree(&mut command)?;
        command
            .arg("-C")
            .arg(&self.path)
            .args(["-c", "core.sparseCheckout=true"])
            .args(["-c", "core.sparseCheckoutCone=true"])
            .args(["-c", "index.sparse=true"])
            // A conflict here is reported, not resolved, and records no
            // resolution of its own.
            .args(["-c", "rerere.enabled=false"])
            .stdin(Stdio::null());
        Ok(command)
    }

    fn rebase(
        &self,
        branch: &str,
        tip: ObjectId,
        onto: ObjectId,
        upstream: Option<ObjectId>,
    ) -> Result<Rebased> {
        // The worktree is on the branch: its tip is what it has.
        if crate::git::branch_tip(branch)?.id != tip {
            bail!("{branch} moved meanwhile");
        }
        let onto = onto.to_string();
        let upstream = upstream.map(|u| u.to_string());
        let range: Vec<&str> = match &upstream {
            Some(upstream) => vec!["--onto", &onto, upstream],
            None => vec![&onto],
        };
        // Only this branch moves, and only its own commits (whatever the
        // user's rebase.* settings say); an editor is never needed.
        let out = self
            .command()?
            .env("GIT_EDITOR", ":")
            .args(["rebase", "-q", "--no-update-refs", "--no-autosquash"])
            .args([
                "--no-autostash",
                "--empty=drop",
                "--no-reapply-cherry-picks",
            ])
            .args(["--no-fork-point"])
            .args(&range)
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
                    .map(<[u8]>::to_vec)
                    .collect(),
            )),
            _ => bail!(
                "git rebase {} on {branch} failed ({}): {}",
                range.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    /// Removes the worktree and its registration, by git; if git can't (half
    /// made), its directory, leaving a registration `make` takes over.
    /// Never `git worktree prune`, which would also drop the registrations of
    /// workspaces not mounted now.
    fn remove(&self) -> Result<()> {
        let path = self.path.to_str().context("a path that isn't UTF-8")?;
        let args = ["worktree", "remove", "--force", path];
        if self.git_here(&args).is_err() && self.path.exists() {
            fs::remove_dir_all(&self.path)?;
        }
        Ok(())
    }
}
