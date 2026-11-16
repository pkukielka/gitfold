//! Moving a branch onto its parent's commits (layers-design/15-tree-spec.md,
//! sections 2 and 3): `gitfold parent --update` (its parent's new commits)
//! and `--set` (another parent), for a branch that no worktree has checked
//! out. The move is `git rebase` itself (`git::rebase`).
//!
//! - **Its own commits** are those after its base, the commit of the parent
//!   they sit on, kept by `refs/gitfold-base/<branch>` (a ref of its own:
//!   `refs/gitfold/<id>` holds workspaces' base pins). Without one (a branch
//!   given its parent by hand, renamed, or one that isn't an ancestor any
//!   more), git's fork point of the parent (`merge-base --fork-point`, from
//!   the parent's reflog), else their merge base. A branch without a parent
//!   owns what isn't on the root (local or remote-tracking).
//! - **Crash safety:** before the rebase, `refs/gitfold-moving/<branch>`
//!   names the commit it moves onto; after, the base takes it and it goes.
//!   One left behind (`recover`) means the move was interrupted: if the
//!   branch sits on that commit, git finished, and the base is recorded.
//! - **Pushed commits** (on the branch's same-name branch of its push
//!   remote, never main or its upstream) are moved only when asked
//!   (`--yes`); gitfold prints the force-push and never pushes.
//! - **A conflict** moves nothing, and is recorded with the parent's tip it
//!   conflicted with (`settings::set_conflict`) until the parent moves on, or
//!   the branch sits on that tip (rebased by hand).
//! - **Merges** among its own commits aren't moved: rebased by hand.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gix::ObjectId;

use crate::git::{self, cmd, rebase};
use crate::settings::{self, Local};

/// The ref keeping where the own commits of `branch` start.
fn base_ref(branch: &str) -> String {
    format!("refs/gitfold-base/{branch}")
}

/// The ref naming the commit a move of `branch` under way moves it onto.
fn moving_ref(branch: &str) -> String {
    format!("refs/gitfold-moving/{branch}")
}

/// The commit ref `name` points at, if it exists.
fn rev(name: &str) -> Option<ObjectId> {
    git::resolve(name).ok().map(|c| c.id)
}

/// Whether `a` is an ancestor of `b` (or `b`).
fn is_ancestor(common_dir: &Path, a: ObjectId, b: ObjectId) -> Result<bool> {
    let (a, b) = (a.to_string(), b.to_string());
    let status = cmd::command(common_dir)
        .args(["merge-base", "--is-ancestor", &a, &b])
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base --is-ancestor {a} {b} failed ({status})"),
    }
}

/// How many commits `git rev-list` lists for `args`.
fn count(common_dir: &Path, args: &[&str]) -> Result<usize> {
    let out = cmd::output(common_dir, &[&["rev-list", "--count"], args].concat())?;
    Ok(String::from_utf8_lossy(&out).trim().parse()?)
}

/// What `git merge-base` says for `args`, if it finds one.
fn merge_base(common_dir: &Path, args: &[&str]) -> Result<Option<ObjectId>> {
    let out = cmd::command(common_dir)
        .arg("merge-base")
        .args(args)
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let hex = String::from_utf8_lossy(&out.stdout);
    Ok(Some(ObjectId::from_hex(hex.trim().as_bytes())?))
}

/// Points `name` at `at` (with a reflog message).
fn set_ref(common_dir: &Path, name: &str, at: ObjectId) -> Result<()> {
    git::worktree::set_ref(common_dir, name, &at, "gitfold: parent")
}

/// Deletes ref `name`, if it exists.
fn drop_ref(common_dir: &Path, name: &str) -> Result<()> {
    git::worktree::unpin(common_dir, name)
}

/// Takes the lock of the moves of `branch` (see `git::rebase::lock`): held
/// from `recover` to the end of the move.
pub fn lock(common_dir: &Path, branch: &str) -> Result<crate::lock::Lock> {
    rebase::lock(common_dir, branch)
}

/// Records that the own commits of `branch` start after `at`.
pub fn set_base(common_dir: &Path, branch: &str, at: ObjectId) -> Result<()> {
    set_ref(common_dir, &base_ref(branch), at)
}

/// Forgets where the own commits of `branch` start (it has no parent).
pub fn drop_base(common_dir: &Path, branch: &str) -> Result<()> {
    drop_ref(common_dir, &base_ref(branch))
}

/// Forgets a conflict recorded for `branch`, if there is one.
fn clear_conflict(common_dir: &Path, branch: &str) -> Result<()> {
    match Local::read(common_dir).conflict(branch) {
        Some(_) => settings::set_conflict(common_dir, branch, None),
        None => Ok(()),
    }
}

/// Finishes the record of a move of `branch` a crash interrupted (see the
/// module doc).
pub fn recover(common_dir: &Path, branch: &str) -> Result<()> {
    let Some(onto) = rev(&moving_ref(branch)) else {
        return Ok(());
    };
    if let Some(tip) = rev(&format!("refs/heads/{branch}"))
        && is_ancestor(common_dir, onto, tip)?
    {
        set_base(common_dir, branch, onto)?;
    }
    drop_ref(common_dir, &moving_ref(branch))
}

/// Where the own commits of `branch` (at `tip`) start, on `parent` (a full
/// ref; `None`: on the root, its existing refs `roots`). See the module doc.
fn own_base(
    common_dir: &Path,
    branch: &str,
    parent: Option<&str>,
    roots: &[String],
    tip: ObjectId,
) -> Result<ObjectId> {
    let tip_hex = tip.to_string();
    if let Some(parent) = parent {
        if let Some(base) = rev(&base_ref(branch))
            && is_ancestor(common_dir, base, tip)?
        {
            // Taken more of the parent's commits by hand since (rebased or
            // merged onto it): those aren't its own.
            return Ok(match merge_base(common_dir, &[parent, &tip_hex])? {
                Some(on) if is_ancestor(common_dir, base, on)? => on,
                _ => base,
            });
        }
        for args in [&["--fork-point", parent, &tip_hex][..], &[parent, &tip_hex]] {
            if let Some(base) = merge_base(common_dir, args)? {
                return Ok(base);
            }
        }
    }
    let base = match roots.is_empty() {
        true => None,
        false => {
            let args: Vec<&str> = std::iter::once(tip_hex.as_str())
                .chain(roots.iter().map(String::as_str))
                .collect();
            merge_base(common_dir, &args)?
        }
    };
    base.with_context(|| format!("can't tell where the own commits of {branch} start"))
}

/// Own commits of a branch already on its remote.
#[derive(Debug, PartialEq, Eq)]
pub struct Pushed {
    /// How many.
    pub count: usize,
    /// The force-push to run once they are moved.
    pub push: String,
}

/// Where `branch` lives, if not on its own: a gitfold workspace, or another
/// git worktree's checkout (its path).
pub enum CheckedOut {
    Workspace,
    Worktree(String),
}

/// Whether `branch` is checked out somewhere (see `CheckedOut`): a move's
/// own worktree, left by one that died, doesn't count (the next move cleans
/// it up).
pub fn checked_out(common_dir: &Path, branch: &str) -> Option<CheckedOut> {
    if crate::state::StateDir::of(common_dir, branch).dir.exists() {
        return Some(CheckedOut::Workspace);
    }
    git::worktree::checked_out(common_dir, branch, None)
        .filter(|path| !rebase::is_scratch(common_dir, Path::new(path)))
        .map(CheckedOut::Worktree)
}

/// What a move of a branch did, or why it didn't.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    /// It sits on the parent's tip already.
    UpToDate,
    /// Moved: `kept` of its own commits onto the parent's tip (the others
    /// were there already); `push`, the force-push if they had been pushed.
    Done { kept: usize, push: Option<String> },
    /// Not moved: commits are pushed (moved only with `yes`).
    Pushed(Pushed),
    /// Not moved: a merge among its own commits.
    Merge,
    /// Not moved: its commits conflict with the parent's on these paths.
    Conflict(Vec<String>),
}

/// A move of `branch` from `base` (where its own commits start) onto
/// `onto`.
pub struct Plan {
    pub branch: String,
    pub tip: ObjectId,
    pub base: ObjectId,
    pub onto: ObjectId,
    /// What `onto` is the tip of (a full ref).
    onto_ref: String,
    /// How many own commits it has.
    pub own: usize,
}

impl Plan {
    /// The move of local branch `branch`, built on `from` (a full ref, or
    /// `None`: on the root, with existing refs `roots`), onto the tip of
    /// `onto` (a full ref).
    pub fn new(
        common_dir: &Path,
        branch: &str,
        from: Option<&str>,
        roots: &[String],
        onto: &str,
    ) -> Result<Plan> {
        let tip = git::branch_tip(branch)?.id;
        let base = own_base(common_dir, branch, from, roots, tip)?;
        Ok(Plan {
            branch: branch.to_string(),
            tip,
            base,
            onto: git::resolve(onto)?.id,
            onto_ref: onto.to_string(),
            own: count(common_dir, &[&tip.to_string(), &format!("^{base}")])?,
        })
    }

    /// Whether it sits on `onto` already, with nothing of its old parent's
    /// on top of `onto` (its own commits start on `onto`'s history).
    pub fn up_to_date(&self, common_dir: &Path) -> Result<bool> {
        Ok(is_ancestor(common_dir, self.base, self.onto)?
            && is_ancestor(common_dir, self.onto, self.tip)?)
    }

    /// Its own commits on its same-name branch of its push remote, as git
    /// chooses that remote (`pushRemote`, `remote.pushDefault`, its remote,
    /// `origin`), if any; not when that branch is what it moves onto (a
    /// branch built on its own remote branch).
    pub fn pushed(&self, common_dir: &Path) -> Result<Option<Pushed>> {
        let branch = &self.branch;
        let remote = git::with_repo(|repo| {
            match repo.branch_remote_name(branch.as_str(), gix::remote::Direction::Push) {
                Some(name) => name.as_symbol().map(String::from),
                None => {
                    (repo.remote_default_name(gix::remote::Direction::Push)).map(|n| n.to_string())
                }
            }
        });
        let Some(remote) = remote else {
            return Ok(None);
        };
        let theirs = format!("refs/remotes/{remote}/{branch}");
        if theirs == self.onto_ref {
            return Ok(None);
        }
        let Some(at) = rev(&theirs) else {
            return Ok(None);
        };
        let (tip, base) = (self.tip.to_string(), format!("^{}", self.base));
        let unpushed = count(common_dir, &[&tip, &base, &format!("^{at}")])?;
        Ok((self.own > unpushed).then(|| Pushed {
            count: self.own - unpushed,
            push: format!("git push --force-with-lease={branch}:{at} {remote} {branch}"),
        }))
    }

    /// Why it can't move as it stands, if it can't: a merge among its own
    /// commits, or (unless `yes`) commits already pushed.
    pub fn blocked(&self, common_dir: &Path, yes: bool) -> Result<Option<Moved>> {
        let range = format!("{}..{}", self.base, self.tip);
        if count(common_dir, &["--merges", &range])? > 0 {
            return Ok(Some(Moved::Merge));
        }
        match yes {
            true => Ok(None),
            false => Ok(self.pushed(common_dir)?.map(Moved::Pushed)),
        }
    }

    /// Moves it (see the module doc), unless `blocked`: its base recorded, a
    /// conflict cleared, or with `record` recorded (following its parent;
    /// not for a move to another parent). `yes`: pushed commits move too.
    /// The caller holds `lock`.
    pub fn run(&self, common_dir: &Path, yes: bool, record: bool) -> Result<Moved> {
        let branch = &self.branch;
        if self.up_to_date(common_dir)? {
            // Sits on it already (rebased by hand, say).
            set_base(common_dir, branch, self.onto)?;
            clear_conflict(common_dir, branch)?;
            return Ok(Moved::UpToDate);
        }
        if let Some(blocked) = self.blocked(common_dir, yes)? {
            return Ok(blocked);
        }
        let pushed = self.pushed(common_dir)?;
        // On its parent's history: `git rebase <onto>`, which also skips
        // commits the parent has by patch id. Off it (a parent rewritten,
        // another parent): only its own commits, `--onto <onto> <base>`.
        let upstream = match is_ancestor(common_dir, self.base, self.onto)? {
            true => None,
            false => Some(self.base),
        };
        set_ref(common_dir, &moving_ref(branch), self.onto)?;
        let rebased = rebase::rebase(common_dir, branch, self.tip, self.onto, upstream);
        // Its base is recorded before the record of the move goes.
        if let Ok(rebase::Rebased::Moved(_)) = rebased {
            set_base(common_dir, branch, self.onto)?;
        }
        drop_ref(common_dir, &moving_ref(branch))?;
        Ok(match rebased? {
            rebase::Rebased::Moved(tip) => {
                clear_conflict(common_dir, branch)?;
                let kept = count(common_dir, &[&tip.to_string(), &format!("^{}", self.onto)])?;
                Moved::Done {
                    kept,
                    push: pushed.map(|p| p.push),
                }
            }
            rebase::Rebased::Conflict(paths) => {
                if record {
                    let at = self.onto.to_string();
                    settings::set_conflict(common_dir, branch, Some((&at, &paths)))?;
                }
                Moved::Conflict(paths)
            }
        })
    }

    /// The conflict recorded for it, if it still holds: recorded against
    /// `onto`, which it doesn't sit on.
    pub fn conflict(&self, common_dir: &Path, local: &Local) -> Result<Option<Vec<String>>> {
        let Some((at, paths)) = local.conflict(&self.branch) else {
            return Ok(None);
        };
        if ObjectId::from_hex(at.as_bytes()).ok() != Some(self.onto)
            || is_ancestor(common_dir, self.onto, self.tip)?
        {
            return Ok(None);
        }
        Ok(Some(paths))
    }
}
