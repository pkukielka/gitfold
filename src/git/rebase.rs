//! A branch's commits rebased onto another commit with `git rebase` itself,
//! on a detached HEAD at the branch's tip in a temporary worktree that
//! checks out almost nothing: a sparse checkout of the top-level files only,
//! with a sparse index, so its cost doesn't grow with the tree (a monorepo's
//! million files aren't written). The branch and where it is checked out
//! don't change: the caller moves them to the result (`follow`). Git does
//! everything a rebase does: it skips commits whose change is there already
//! (by patch id, for `git rebase <onto>`), drops commits that become empty,
//! keeps authors and runs the hooks; on a conflict it stops, says which
//! paths conflict (`git diff --diff-filter=U`), and is aborted.
//!
//! The worktree is `<common>/gitfold-moves/gitfold-move-<id>` (not under
//! `<common>/gitfold/`, whose presence means the repository is used with
//! gitfold mounts), made under a lock on `<id>.lock` beside it (`lock`) and
//! removed after. One left by a process that died is removed by the next
//! move of that branch. The sparse settings are given on the command line
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
/// repository, beside the moves' locks). It is held only to check and
/// write settings, so it is waited for, up to `TREE_WAIT`.
pub fn tree_lock(common_dir: &Path) -> Result<Lock> {
    let dir = moves_dir(common_dir);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let started = std::time::Instant::now();
    // Tests shorten the wait (`GITFOLD_TEST_TREE_WAIT`, seconds).
    let wait = std::env::var("GITFOLD_TEST_TREE_WAIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(TREE_WAIT, std::time::Duration::from_secs);
    loop {
        if let Some(lock) = crate::lock::try_file(&dir.join(".tree.lock"))? {
            return Ok(lock);
        }
        if started.elapsed() > wait {
            return Err(Refusal::InUse
                .error("another gitfold command is changing the parents of branches; retry"));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// How long `tree_lock` waits for another command's.
const TREE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What `rebase` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Rebased {
    /// Its commits, rebased: the new tip.
    Done(ObjectId),
    /// A commit conflicted on these paths (as stored); the branch didn't
    /// move.
    Conflict(Vec<Vec<u8>>),
}

/// The temporary worktree of a move.
struct Scratch {
    path: PathBuf,
    common_dir: PathBuf,
}

/// What `git rebase <onto>` (or with `upstream`, `git rebase --onto <onto>
/// <upstream>`: the commits after `upstream`) makes of the commits of
/// branch `branch` at `tip`: rebased on a detached HEAD at `tip`, so
/// neither the branch nor where it is checked out changes. The caller
/// holds `lock`.
pub fn rebase(
    common_dir: &Path,
    branch: &str,
    tip: ObjectId,
    onto: ObjectId,
    upstream: Option<ObjectId>,
) -> Result<Rebased> {
    let scratch = Scratch::of(common_dir, branch);
    let rebased = scratch
        .make(tip)
        .and_then(|()| scratch.rebase(onto, upstream));
    let removed = scratch.remove();
    let rebased = rebased?;
    removed?;
    Ok(rebased)
}

/// Removes the worktree a move of `branch` that died left. (It is on a
/// detached HEAD: nothing of the branch is in it.) The caller holds `lock`.
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
        self.remove()
    }

    /// Adds the worktree, detached at `tip`, nothing checked out but the
    /// top-level files. `-f`: a registration of it whose directory is gone
    /// (a removal that failed half-way) is taken over (detached, it lifts
    /// nothing else).
    fn make(&self, tip: ObjectId) -> Result<()> {
        let path = self.path.to_str().context("a path that isn't UTF-8")?;
        let tip = tip.to_string();
        let args = ["worktree", "add", "-q", "-f", "--no-checkout", "--detach"];
        self.git_here(&[&args[..], &[path, &tip]].concat())?;
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
        let mut command = super::cmd::in_workspace(&self.path)?;
        command
            .args(["-c", "core.sparseCheckout=true"])
            .args(["-c", "core.sparseCheckoutCone=true"])
            .args(["-c", "index.sparse=true"])
            // Unlike a person's rebase, nobody is here to check a recorded
            // resolution (rerere) before it is taken: a conflict stops the
            // move and is reported, and the rebase aborted records none.
            .args(["-c", "rerere.enabled=false"])
            .stdin(Stdio::null());
        Ok(command)
    }

    fn rebase(&self, onto: ObjectId, upstream: Option<ObjectId>) -> Result<Rebased> {
        let onto = onto.to_string();
        let upstream = upstream.map(|u| u.to_string());
        let range: Vec<&str> = match &upstream {
            Some(upstream) => vec!["--onto", &onto, upstream],
            None => vec![&onto],
        };
        // Only the detached HEAD moves, and only its own commits (whatever
        // the user's rebase.* settings say); an editor is never needed.
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
            let head = self.git(&["rev-parse", "HEAD"])?;
            let head = String::from_utf8_lossy(&head);
            return Ok(Rebased::Done(ObjectId::from_hex(head.trim().as_bytes())?));
        }
        // Stopped at a conflict: the paths, then the rebase gone.
        let paths = self.git(&["diff", "--name-only", "--diff-filter=U", "-z"]);
        let stopped = self.admin()?.join("rebase-merge").exists();
        if stopped {
            self.git(&["rebase", "--abort"])?;
        }
        match paths {
            Ok(paths) if stopped && !paths.is_empty() => {
                Ok(Rebased::Conflict(super::cmd::nul_paths(&paths)))
            }
            _ => bail!(
                "git rebase {} failed ({}): {}",
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
