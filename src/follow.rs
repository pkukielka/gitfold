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
//! - **Crash safety:** before the rebase, one record (`gitfoldMoving`, see
//!   `settings::set_moving`) holds the whole outcome: where the branch was,
//!   the commit it moves onto, and for `--set` the new parent and
//!   following. After, they are written (`finish`) and it goes. One left
//!   behind (`recover`, before any `gitfold parent` reads the tree) means
//!   the move was interrupted: if git moved the branch from where it was
//!   onto that commit, the outcome is written then; else nothing moved, and
//!   it goes.
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

/// What `git merge-base` says for `args`, if it finds one (exit 1: none;
/// any other failure is an error). Git itself, for `--fork-point` and
/// several commits (`fs::views` has a gix one for two, in the daemon, which
/// avoids git processes).
fn merge_base(common_dir: &Path, args: &[&str]) -> Result<Option<ObjectId>> {
    let out = cmd::command(common_dir)
        .arg("merge-base")
        .args(args)
        .output()?;
    match out.status.code() {
        Some(0) => {
            let hex = String::from_utf8_lossy(&out.stdout);
            Ok(Some(ObjectId::from_hex(hex.trim().as_bytes())?))
        }
        Some(1) => Ok(None),
        _ => bail!(
            "git merge-base {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Points `name` at `at` (with a reflog message).
fn set_ref(common_dir: &Path, name: &str, at: ObjectId) -> Result<()> {
    git::worktree::set_ref(common_dir, name, &at, "gitfold: parent")
}

/// Deletes ref `name`, if it exists.
fn drop_ref(common_dir: &Path, name: &str) -> Result<()> {
    git::worktree::unpin(common_dir, name)
}

/// Takes the lock of the moves of `branch` (see `git::rebase::lock`), held
/// to the end of the move, and cleans up what a move that died left: its
/// worktree (its rebase aborted, which may put the branch back), then the
/// record of where it was moving (`recover`).
pub fn lock(common_dir: &Path, branch: &str) -> Result<crate::lock::Lock> {
    let lock = rebase::lock(common_dir, branch)?;
    rebase::clean_up(common_dir, branch)?;
    recover(common_dir, branch)?;
    Ok(lock)
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

/// The outcome of a move, recorded before it (see the module doc).
pub struct Outcome {
    /// The branch's tip before: still there, nothing moved (git moves a
    /// branch only when its rebase finishes).
    pub from: ObjectId,
    /// What it moves onto: its base after.
    pub onto: ObjectId,
    /// For `--set`: the new parent (a full ref) and whether it follows it.
    pub parent: Option<(String, bool)>,
}

impl Outcome {
    fn encode(&self) -> String {
        let (from, onto) = (self.from, self.onto);
        match &self.parent {
            None => format!("{from} {onto}"),
            Some((parent, follow)) => format!("{from} {onto} {follow} {parent}"),
        }
    }

    fn decode(text: &str) -> Option<Outcome> {
        let mut words = text.split(' ');
        let mut id = || ObjectId::from_hex(words.next()?.as_bytes()).ok();
        let (from, onto) = (id()?, id()?);
        let parent = match (words.next(), words.next()) {
            (Some(follow), Some(parent)) => Some((parent.to_string(), follow.parse().ok()?)),
            _ => None,
        };
        Some(Outcome { from, onto, parent })
    }

    /// Writes it for `branch`, which git moved (or, with commits pushed,
    /// that is built on a new parent without the move): its base, its
    /// parent, then the record goes.
    pub fn finish(&self, common_dir: &Path, branch: &str) -> Result<()> {
        set_base(common_dir, branch, self.onto)?;
        if let Some((parent, follow)) = &self.parent {
            settings::set_parent(common_dir, branch, parent)?;
            settings::set_follow(common_dir, branch, *follow)?;
        }
        settings::set_moving(common_dir, branch, None)
    }
}

/// Finishes or forgets the outcome of a move of `branch` a crash
/// interrupted (see the module doc): finished if git moved the branch from
/// where it was onto the target, else forgotten.
fn recover(common_dir: &Path, branch: &str) -> Result<()> {
    let Some(text) = Local::read_checked(common_dir)?.moving(branch) else {
        return Ok(());
    };
    let tip = git::branch_tip(branch)?.id;
    match Outcome::decode(&text) {
        Some(outcome) if tip != outcome.from && is_ancestor(common_dir, outcome.onto, tip)? => {
            outcome.finish(common_dir, branch)
        }
        _ => settings::set_moving(common_dir, branch, None),
    }
}

/// Finishes the record of a move of `branch` a crash interrupted, unless a
/// move of it is running (then that one sees to it): for commands that
/// read the tree before they act.
pub fn settle(common_dir: &Path, branch: &str) -> Result<()> {
    if Local::read_checked(common_dir)?.moving(branch).is_none() {
        return Ok(());
    }
    match rebase::lock(common_dir, branch) {
        Ok(_lock) => {
            rebase::clean_up(common_dir, branch)?;
            recover(common_dir, branch)
        }
        Err(_) => Ok(()),
    }
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
        // A parent gone (T3) has nothing to ask git about.
        let exists = rev(parent).is_some();
        if let Some(base) = rev(&base_ref(branch))
            && is_ancestor(common_dir, base, tip)?
        {
            // Taken more of the parent's commits by hand since (rebased or
            // merged onto it): those aren't its own.
            let on = match exists {
                true => merge_base(common_dir, &[parent, &tip_hex])?,
                false => None,
            };
            return Ok(match on {
                Some(on) if is_ancestor(common_dir, base, on)? => on,
                _ => base,
            });
        }
        if exists {
            for args in [&["--fork-point", parent, &tip_hex][..], &[parent, &tip_hex]] {
                if let Some(base) = merge_base(common_dir, args)? {
                    return Ok(base);
                }
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
    /// Whether its base is on `onto`'s history (the parent only gained
    /// commits since).
    on_history: bool,
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
        let onto_id = git::resolve(onto)?.id;
        let on_history = is_ancestor(common_dir, base, onto_id)?;
        let mut plan = Plan {
            branch: branch.to_string(),
            tip,
            base,
            onto: onto_id,
            onto_ref: onto.to_string(),
            own: 0,
            on_history,
        };
        let [tip, from] = plan.own_range();
        plan.own = count(common_dir, &[&tip, &from])?;
        Ok(plan)
    }

    /// `rev-list` arguments for the commits the move replays: on its
    /// parent's history those `git rebase <onto>` takes (not in `onto`),
    /// else those after its base.
    fn own_range(&self) -> [String; 2] {
        let from = if self.on_history {
            self.onto
        } else {
            self.base
        };
        [self.tip.to_string(), format!("^{from}")]
    }

    /// Whether it sits on `onto` already, with nothing of its old parent's
    /// on top of `onto` (its own commits start on `onto`'s history).
    pub fn up_to_date(&self, common_dir: &Path) -> Result<bool> {
        Ok(self.on_history && is_ancestor(common_dir, self.onto, self.tip)?)
    }

    /// The `git rebase` that moves it, as run in its own worktree: on its
    /// parent's history `git rebase <onto>` (which also skips commits the
    /// parent has by patch id), else only its own commits, `--onto <onto>
    /// <base>` (a parent rewritten, another parent). `onto` named as given.
    pub fn by_hand(&self, onto: &str) -> String {
        let branch = &self.branch;
        match self.on_history {
            true => format!("git rebase {onto} {branch}"),
            false => format!("git rebase --onto {onto} {} {branch}", self.base),
        }
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
                // As git: origin.
                None => Some("origin".to_string()),
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
        let [tip, from] = self.own_range();
        let unpushed = count(common_dir, &[&tip, &from, &format!("^{at}")])?;
        Ok((self.own > unpushed).then(|| Pushed {
            count: self.own - unpushed,
            push: format!("git push --force-with-lease={branch}:{at} {remote} {branch}"),
        }))
    }

    /// Moves it (see the module doc): its base recorded and, for `--set`,
    /// its new `parent` (a full ref) and following; a conflict cleared, or
    /// recorded when following its parent (not for `--set`). Not with a
    /// merge among its own commits, nor commits already pushed unless
    /// `yes`. `moving` is called once it is sure to try. The caller holds
    /// `lock`.
    pub fn run(
        &self,
        common_dir: &Path,
        yes: bool,
        parent: Option<(String, bool)>,
        moving: impl FnOnce(),
    ) -> Result<Moved> {
        let branch = &self.branch;
        let record = parent.is_none();
        let outcome = Outcome {
            from: self.tip,
            onto: self.onto,
            parent,
        };
        if self.up_to_date(common_dir)? {
            // Sits on it already (rebased by hand, say).
            outcome.finish(common_dir, branch)?;
            clear_conflict(common_dir, branch)?;
            return Ok(Moved::UpToDate);
        }
        let [tip, from] = self.own_range();
        if count(common_dir, &["--merges", &tip, &from])? > 0 {
            return Ok(Moved::Merge);
        }
        let pushed = self.pushed(common_dir)?;
        if !yes && let Some(pushed) = pushed {
            return Ok(Moved::Pushed(pushed));
        }
        moving();
        let upstream = (!self.on_history).then_some(self.base);
        settings::set_moving(common_dir, branch, Some(&outcome.encode()))?;
        // On an error (git may have moved it) the record stays, for
        // `recover`.
        let rebased = rebase::rebase(common_dir, branch, self.tip, self.onto, upstream)?;
        match rebased {
            rebase::Rebased::Moved(_) => outcome.finish(common_dir, branch)?,
            rebase::Rebased::Conflict(_) => settings::set_moving(common_dir, branch, None)?,
        }
        Ok(match rebased {
            rebase::Rebased::Moved(tip) => {
                clear_conflict(common_dir, branch)?;
                let kept = count(common_dir, &[&tip.to_string(), &format!("^{}", self.onto)])?;
                Moved::Done {
                    kept,
                    push: pushed.map(|p| p.push),
                }
            }
            rebase::Rebased::Conflict(paths) => {
                // As git quotes paths: a newline in one can't split it.
                let paths: Vec<String> = (paths.iter())
                    .map(|p| String::from_utf8_lossy(&crate::cli::quote(p)).into_owned())
                    .collect();
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
