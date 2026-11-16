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
pub fn base_ref(branch: &str) -> String {
    format!("refs/gitfold-base/{branch}")
}

/// The ref naming the commit a move of `branch` under way moves it onto.
fn moving_ref(branch: &str) -> String {
    format!("refs/gitfold-moving/{branch}")
}

/// The commit `spec` names, if it names one.
fn rev(common_dir: &Path, spec: &str) -> Result<Option<ObjectId>> {
    let out = cmd::command(common_dir)
        .args(["rev-parse", "-q", "--verify", "--end-of-options"])
        .arg(format!("{spec}^{{commit}}"))
        .output()?;
    match out.status.success() {
        true => Ok(Some(parse_id(&out.stdout)?)),
        false => Ok(None),
    }
}

fn parse_id(out: &[u8]) -> Result<ObjectId> {
    let hex = String::from_utf8_lossy(out);
    Ok(ObjectId::from_hex(hex.trim().as_bytes())?)
}

/// The commit `spec` (a full ref) names.
pub fn commit(common_dir: &Path, spec: &str) -> Result<ObjectId> {
    rev(common_dir, spec)?.with_context(|| format!("no commit {spec}"))
}

/// The tip of local branch `branch`.
pub fn tip(common_dir: &Path, branch: &str) -> Result<ObjectId> {
    rev(common_dir, &format!("refs/heads/{branch}"))?.with_context(|| format!("no branch {branch}"))
}

/// Whether `a` is an ancestor of `b` (or `b`).
fn is_ancestor(common_dir: &Path, a: ObjectId, b: ObjectId) -> Result<bool> {
    let status = cmd::command(common_dir)
        .args([
            "merge-base",
            "--is-ancestor",
            &a.to_string(),
            &b.to_string(),
        ])
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
    match out.status.success() {
        true => Ok(Some(parse_id(&out.stdout)?)),
        false => Ok(None),
    }
}

/// Points `name` at `at` (with a reflog message).
fn set_ref(common_dir: &Path, name: &str, at: ObjectId) -> Result<()> {
    let at = at.to_string();
    cmd::output(
        common_dir,
        &["update-ref", "-m", "gitfold: parent", name, &at],
    )
    .map(drop)
}

/// Deletes ref `name`, if it exists.
fn drop_ref(common_dir: &Path, name: &str) -> Result<()> {
    match rev(common_dir, name)? {
        Some(_) => cmd::output(common_dir, &["update-ref", "-d", name]).map(drop),
        None => Ok(()),
    }
}

/// Records that the own commits of `branch` start after `at`.
pub fn set_base(common_dir: &Path, branch: &str, at: ObjectId) -> Result<()> {
    set_ref(common_dir, &base_ref(branch), at)
}

/// Forgets where the own commits of `branch` start (it has no parent).
pub fn drop_base(common_dir: &Path, branch: &str) -> Result<()> {
    drop_ref(common_dir, &base_ref(branch))
}

/// Finishes the record of a move of `branch` a crash interrupted (see the
/// module doc).
pub fn recover(common_dir: &Path, branch: &str) -> Result<()> {
    let Some(onto) = rev(common_dir, &moving_ref(branch))? else {
        return Ok(());
    };
    if let Some(tip) = rev(common_dir, &format!("refs/heads/{branch}"))?
        && is_ancestor(common_dir, onto, tip)?
    {
        set_base(common_dir, branch, onto)?;
    }
    drop_ref(common_dir, &moving_ref(branch))
}

/// Where the own commits of `branch` (at `tip`) start, on `parent` (a full
/// ref; `None`: on the root, whose names are `roots`). See the module doc.
pub fn own_base(
    common_dir: &Path,
    branch: &str,
    parent: Option<&str>,
    roots: &[String],
    tip: ObjectId,
) -> Result<ObjectId> {
    if let Some(parent) = parent {
        if let Some(base) = rev(common_dir, &base_ref(branch))?
            && is_ancestor(common_dir, base, tip)?
        {
            return Ok(base);
        }
        let tip = tip.to_string();
        for args in [&["--fork-point", parent, &tip][..], &[parent, &tip]] {
            if let Some(base) = merge_base(common_dir, args)? {
                return Ok(base);
            }
        }
    }
    let mut args = vec![tip.to_string()];
    args.extend(
        roots
            .iter()
            .filter(|r| matches!(rev(common_dir, r), Ok(Some(_))))
            .cloned(),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.len() > 1 {
        true => merge_base(common_dir, &args)?,
        false => None,
    }
    .with_context(|| format!("can't tell where the own commits of {branch} start"))
}

/// The full refs of the tree's root `root`: the local branch and origin's.
pub fn root_refs(root: Option<&str>) -> Vec<String> {
    root.map_or(Vec::new(), |root| {
        vec![
            format!("refs/heads/{root}"),
            format!("refs/remotes/origin/{root}"),
        ]
    })
}

/// Own commits of a branch already on its remote.
#[derive(Debug, PartialEq, Eq)]
pub struct Pushed {
    /// How many.
    pub count: usize,
    /// The force-push to run once they are moved.
    pub push: String,
}

/// The own commits of `branch` (after `base`, at `tip`) on its same-name
/// branch of its push remote, as git chooses that remote (`pushRemote`,
/// `remote.pushDefault`, its remote, `origin`), if any.
pub fn pushed(
    common_dir: &Path,
    branch: &str,
    base: ObjectId,
    tip: ObjectId,
) -> Result<Option<Pushed>> {
    let remote = git::with_repo(|repo| {
        let name = repo.branch_remote_name(branch, gix::remote::Direction::Push);
        match name {
            Some(name) => name.as_symbol().map(String::from),
            None => repo
                .remote_default_name(gix::remote::Direction::Push)
                .map(|n| n.to_string()),
        }
    });
    let Some(remote) = remote else {
        return Ok(None);
    };
    let Some(at) = rev(common_dir, &format!("refs/remotes/{remote}/{branch}"))? else {
        return Ok(None);
    };
    let (tip, base, at) = (tip.to_string(), base.to_string(), at.to_string());
    let own = count(common_dir, &[&tip, &format!("^{base}")])?;
    let unpushed = count(common_dir, &[&tip, &format!("^{base}"), &format!("^{at}")])?;
    Ok((own > unpushed).then(|| Pushed {
        count: own - unpushed,
        push: format!("git push --force-with-lease={branch}:{at} {remote} {branch}"),
    }))
}

/// Where `branch` lives, if not on its own: a gitfold workspace, or another
/// git worktree's checkout (its path).
pub enum CheckedOut {
    Workspace,
    Worktree(String),
}

/// Whether `branch` is checked out somewhere (see `CheckedOut`), as `git
/// worktree list` says.
pub fn checked_out(common_dir: &Path, branch: &str) -> Result<Option<CheckedOut>> {
    if crate::state::StateDir::of(common_dir, branch).dir.exists() {
        return Ok(Some(CheckedOut::Workspace));
    }
    let out = cmd::output(common_dir, &["worktree", "list", "--porcelain", "-z"])?;
    let wanted = format!("branch refs/heads/{branch}");
    let mut path = None;
    for field in String::from_utf8_lossy(&out).split('\0') {
        if let Some(p) = field.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if field == wanted {
            let path = path.unwrap_or_default();
            // A move's own worktree, left by one that died: the next move
            // cleans it up.
            if rebase::is_scratch(common_dir, Path::new(&path)) {
                return Ok(None);
            }
            return Ok(Some(CheckedOut::Worktree(path)));
        }
    }
    Ok(None)
}

/// What a move of a branch did, or why it didn't.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    /// It sits on the parent's tip already.
    UpToDate,
    /// Moved: of its `own` commits, `kept` onto the parent's tip (the
    /// others were there already); `push`, the force-push if they had been
    /// pushed.
    Done {
        own: usize,
        kept: usize,
        push: Option<String>,
    },
    /// Not moved: commits are pushed (moved only with `yes`).
    Pushed(Pushed),
    /// Not moved: a merge among its own commits.
    Merge,
    /// Not moved: its commits conflict with the parent's on these paths.
    Conflict(Vec<String>),
}

/// A planned move of `branch` from `base` (where its own commits start)
/// onto `onto`.
pub struct Plan {
    pub branch: String,
    pub tip: ObjectId,
    pub base: ObjectId,
    pub onto: ObjectId,
}

impl Plan {
    /// How many own commits it moves.
    pub fn own(&self, common_dir: &Path) -> Result<usize> {
        count(
            common_dir,
            &[&self.tip.to_string(), &format!("^{}", self.base)],
        )
    }

    /// Whether it sits on `onto` already, with nothing of its old parent's
    /// on top of `onto` (its own commits start on `onto`'s history).
    pub fn up_to_date(&self, common_dir: &Path) -> Result<bool> {
        Ok(is_ancestor(common_dir, self.base, self.onto)?
            && is_ancestor(common_dir, self.onto, self.tip)?)
    }

    /// Moves it (see the module doc): its base recorded, a conflict
    /// cleared, or with `record` recorded (following its parent; not for a
    /// move to another parent). `yes`: pushed commits move too.
    pub fn run(&self, common_dir: &Path, yes: bool, record: bool) -> Result<Moved> {
        let branch = &self.branch;
        if self.up_to_date(common_dir)? {
            // Sits on it already (rebased by hand, say).
            set_base(common_dir, branch, self.onto)?;
            settings::set_conflict(common_dir, branch, None)?;
            return Ok(Moved::UpToDate);
        }
        let range = format!("{}..{}", self.base, self.tip);
        if count(common_dir, &["--merges", &range])? > 0 {
            return Ok(Moved::Merge);
        }
        let pushed = pushed(common_dir, branch, self.base, self.tip)?;
        if let Some(pushed) = pushed.as_ref().filter(|_| !yes) {
            return Ok(Moved::Pushed(Pushed {
                count: pushed.count,
                push: pushed.push.clone(),
            }));
        }
        let own = self.own(common_dir)?;
        set_ref(common_dir, &moving_ref(branch), self.onto)?;
        let rebased = rebase::rebase(common_dir, branch, self.tip, self.onto, self.base);
        let moved = match rebased {
            Ok(rebase::Rebased::Moved(tip)) => {
                set_base(common_dir, branch, self.onto)?;
                settings::set_conflict(common_dir, branch, None)?;
                let kept = count(common_dir, &[&tip.to_string(), &format!("^{}", self.onto)])?;
                Moved::Done {
                    own,
                    kept,
                    push: pushed.map(|p| p.push),
                }
            }
            Ok(rebase::Rebased::Conflict(paths)) => {
                if record {
                    let at = self.onto.to_string();
                    settings::set_conflict(common_dir, branch, Some((&at, &paths)))?;
                }
                Moved::Conflict(paths)
            }
            Err(e) => {
                drop_ref(common_dir, &moving_ref(branch))?;
                return Err(e);
            }
        };
        drop_ref(common_dir, &moving_ref(branch))?;
        Ok(moved)
    }
}

/// The conflict recorded for `branch` (at `tip`) on its parent at
/// `parent_tip`, if it still holds: recorded against that tip, which the
/// branch doesn't sit on.
pub fn conflict(
    common_dir: &Path,
    local: &Local,
    branch: &str,
    parent_tip: ObjectId,
    tip: ObjectId,
) -> Result<Option<Vec<String>>> {
    let Some((at, paths)) = local.conflict(branch) else {
        return Ok(None);
    };
    if ObjectId::from_hex(at.as_bytes()).ok() != Some(parent_tip)
        || is_ancestor(common_dir, parent_tip, tip)?
    {
        return Ok(None);
    }
    Ok(Some(paths))
}
