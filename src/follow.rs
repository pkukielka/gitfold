//! Moving a branch onto its parent's commits (layers-design/15-tree-spec.md,
//! sections 2 and 3): `gitfold parent --update` (its parent's new commits)
//! and `--set` (another parent). Its commits are rebased by `git rebase`
//! itself, on a detached HEAD (`git::rebase`); then the branch goes there:
//! its ref (`update-ref`), or in its workspace, through the mount, by git
//! there with its uncommitted work carried along (`move_workspace`).
//!
//! - **Its own commits** are those after its base, the commit of the parent
//!   they sit on, kept by `refs/gitfold-base/<branch>` (a ref of its own:
//!   `refs/gitfold/<id>` holds workspaces' base pins). Without one (a branch
//!   given its parent by hand, renamed, or one that isn't an ancestor any
//!   more), git's fork point of the parent (`merge-base --fork-point`, from
//!   the parent's reflog), else their merge base. A branch without a parent
//!   owns what isn't on the root (local or remote-tracking).
//! - **Crash safety:** before the rebase, a record (`gitfoldMoving`, see
//!   `settings::set_moving`) holds where the branch was and the commit it
//!   moves onto; after, that commit is its base and the record goes. One
//!   left behind (`recover`, by the next move of the branch) means the move
//!   was interrupted: if git moved the branch from where it was onto that
//!   commit, the base is recorded then; else nothing moved, and it goes.
//!   `--set` writes the new parent before it moves (see `cli::tree`): a
//!   move cut short leaves a branch whose parent's commits are yet to be
//!   taken, as any following does.
//! - **The tree** changes under one lock (`tree_lock`): checking a new
//!   parent (no cycle) and writing it, so two `--set`s at once can't make a
//!   cycle.
//! - **Pushed commits** (on the branch's same-name branch of its push
//!   remote, never main or its upstream) are moved only when asked
//!   (`--yes`); gitfold prints the force-push and never pushes.
//! - **A conflict** moves nothing, and is recorded with the parent's tip it
//!   conflicted with (`settings::set_conflict`) until the parent moves on, or
//!   the branch sits on that tip (rebased by hand).
//! - **Merges** among its own commits aren't moved: rebased by hand.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gix::ObjectId;

use crate::git::{self, cmd, rebase};
use crate::refusal::Refusal;
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
    git::worktree::set_ref(common_dir, name, &at, None, "gitfold: parent")
}

/// Deletes ref `name`, if it exists.
fn drop_ref(common_dir: &Path, name: &str) -> Result<()> {
    git::worktree::unpin(common_dir, name)
}

/// Takes the lock of the moves of `branch` (see `git::rebase::lock`), held
/// to the end of the move, and cleans up what a move that died left: its
/// worktree, then the record of where it was moving (`recover`).
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

/// Takes the lock under which the tree changes (see the module doc), or
/// fails if another command holds it.
pub fn tree_lock(common_dir: &Path) -> Result<crate::lock::Lock> {
    rebase::tree_lock(common_dir)
}

/// A move under way, recorded before it (see the module doc).
struct Moving {
    /// The branch's tip before: still there, nothing moved (the branch
    /// moves in one step, by `update-ref` or by git's rebase in its
    /// workspace, when that finishes).
    from: ObjectId,
    /// What it moves onto: its base after.
    onto: ObjectId,
}

impl Moving {
    fn encode(&self) -> String {
        format!("{} {}", self.from, self.onto)
    }

    fn decode(text: &str) -> Option<Moving> {
        let mut words = text
            .split(' ')
            .map(|w| ObjectId::from_hex(w.as_bytes()).ok());
        match (words.next()??, words.next()??, words.next()) {
            (from, onto, None) => Some(Moving { from, onto }),
            _ => None,
        }
    }

    /// Records it done for `branch`, which git moved: its base, then the
    /// record goes.
    fn finish(&self, common_dir: &Path, branch: &str) -> Result<()> {
        set_base(common_dir, branch, self.onto)?;
        settings::set_moving(common_dir, branch, None)
    }
}

/// Finishes or forgets the record of a move of `branch` a crash
/// interrupted (see the module doc): finished if git moved the branch from
/// where it was onto the target, else forgotten. One it can't read is
/// reported, and stays.
fn recover(common_dir: &Path, branch: &str) -> Result<()> {
    let Some(text) = Local::read_checked(common_dir)?.moving(branch) else {
        return Ok(());
    };
    let Some(moving) = Moving::decode(&text) else {
        bail!(
            "branch.{branch}.gitfoldMoving holds {text:?}, not the record of a move (two \
             commits); remove it once {branch} is as you want it"
        );
    };
    let tip = git::branch_tip(branch)?.id;
    match tip != moving.from && is_ancestor(common_dir, moving.onto, tip)? {
        true => moving.finish(common_dir, branch),
        false => settings::set_moving(common_dir, branch, None),
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

/// How moving the branch to its rebased commits went.
enum Applied {
    /// Moved to this tip; the paths where its uncommitted work, put back,
    /// conflicted all the same (git keeps the work in the stash).
    Done(ObjectId, Vec<Vec<u8>>),
    /// Not moved: its uncommitted work conflicts with the move on these
    /// paths (untracked files included).
    Work(Vec<Vec<u8>>),
}

/// Moves the checkout of workspace `dir` (through the mount) of `branch`,
/// at `tip`, to `new`, the uncommitted work carried along, all by git: if
/// git was used there since (HEAD elsewhere, the branch moved), refused,
/// to retry; untracked files where `new` adds files, or uncommitted work
/// (as `git stash create` takes it, nothing changed) that conflicts with
/// the move (`git merge-tree` of it onto `new`, from `tip`): nothing moves,
/// the paths are said; else `git rebase --autostash --onto <new> <tip>`,
/// which replays nothing (the branch was at `tip`), so the branch and its
/// files go to `new`, and puts the work back (staged changes come back
/// unstaged, as with git's autostash). If git stops half-way anyway (a
/// hook, a race), its state stays as git leaves it: a rebase in progress,
/// the work kept in the stash.
fn move_workspace(
    common_dir: &Path,
    dir: &Path,
    branch: &str,
    tip: ObjectId,
    new: ObjectId,
) -> Result<Applied> {
    let git = |args: &[&str]| cmd::output_of(&mut cmd::in_workspace(dir)?, args, None);
    let head = git(&["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
    let on_branch = String::from_utf8_lossy(&head).trim() == format!("refs/heads/{branch}");
    if !on_branch || git::branch_tip(branch)?.id != tip {
        let why = format!(
            "{branch} moved in its workspace meanwhile; again: `gitfold parent {branch} --update`"
        );
        return Err(Refusal::InUse.error(why));
    }
    // A conflict git left unresolved there (a move's work put back, say).
    let unmerged = cmd::nul_paths(&git(&["diff", "-z", "--name-only", "--diff-filter=U"])?);
    if !unmerged.is_empty() {
        return Ok(Applied::Work(unmerged));
    }
    let (tip, new_hex) = (tip.to_string(), new.to_string());
    // Untracked files where the move adds files (renames included: the
    // plumbing diff pairs none): git would refuse them.
    let diff = ["diff-tree", "-r", "-z", "--name-only", "--diff-filter=A"];
    let added = cmd::nul_paths(&cmd::output(
        common_dir,
        &[&diff[..], &[&tip, &new_hex]].concat(),
    )?);
    // In batches: a large import adds more paths than one command line holds.
    for batch in added.chunks(1000) {
        let mut ls = cmd::in_workspace(dir)?;
        let args = [
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
        ];
        ls.args(args)
            .arg("--")
            .args(batch.iter().map(|p| OsStr::from_bytes(p)));
        // (The paths given already, as bytes.)
        let in_the_way = cmd::nul_paths(&cmd::output_of(&mut ls, &[], None)?);
        if !in_the_way.is_empty() {
            return Ok(Applied::Work(in_the_way));
        }
    }
    let work = String::from_utf8_lossy(&git(&["stash", "create"])?)
        .trim()
        .to_string();
    if !work.is_empty() {
        let mut check = cmd::command(common_dir);
        let args = ["merge-tree", "--write-tree", "-z", "--name-only"];
        let out = (check.args(args))
            .args(["--merge-base", &tip, &new_hex, &work])
            .output()?;
        match out.status.code() {
            Some(0) => {}
            // The tree, then the conflicted paths, then an empty field.
            Some(1) => {
                let paths = out.stdout.splitn(2, |b| *b == 0).nth(1).unwrap_or_default();
                return Ok(Applied::Work(cmd::nul_paths(paths)));
            }
            _ => bail!(
                "git merge-tree of the uncommitted work in {} failed ({}): {}",
                dir.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }
    let mut rebase = cmd::in_workspace(dir)?;
    rebase.env("GIT_EDITOR", ":");
    let args = ["rebase", "-q", "--autostash", "--no-update-refs", "--onto"];
    cmd::output_of(&mut rebase, &[&args[..], &[&new_hex, &tip]].concat(), None)?;
    // Moved (git exits 0 even if the work, put back, conflicted: then the
    // paths are unmerged, and the work stays in the stash).
    let unmerged = cmd::nul_paths(&git(&["diff", "-z", "--name-only", "--diff-filter=U"])?);
    Ok(Applied::Done(git::branch_tip(branch)?.id, unmerged))
}

/// What a move of a branch did, or why it didn't.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    /// It sits on the parent's tip already.
    UpToDate,
    /// Moved: `kept` of its own commits onto the parent's tip (the others
    /// were there already); `push`, the force-push if they had been pushed;
    /// `unmerged`, where its uncommitted work, put back, conflicted (quoted).
    Done {
        kept: usize,
        push: Option<String>,
        unmerged: Vec<String>,
    },
    /// Not moved: commits are pushed (moved only with `yes`).
    Pushed(Pushed),
    /// Not moved: a merge among its own commits.
    Merge,
    /// Not moved: its commits conflict with the parent's on these paths
    /// (quoted; recorded).
    Conflict(Vec<String>),
    /// Not moved: its uncommitted work (untracked files included) is in
    /// the way on these paths (quoted; not recorded: it is the work's).
    Work(Vec<String>),
}

/// `paths` as git quotes them (a newline in one can't split it).
fn quoted(paths: &[Vec<u8>]) -> Vec<String> {
    (paths.iter())
        .map(|p| String::from_utf8_lossy(&crate::cli::quote(p)).into_owned())
        .collect()
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
    /// Whether it sits on `onto` already, with nothing of its old parent's
    /// on top of `onto` (its own commits start on `onto`'s history).
    up_to_date: bool,
    /// Whether a merge is among the commits it replays, once asked.
    merges: std::cell::Cell<Option<bool>>,
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
            up_to_date: on_history && is_ancestor(common_dir, onto_id, tip)?,
            merges: Default::default(),
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

    /// See the field `up_to_date`.
    pub fn up_to_date(&self) -> bool {
        self.up_to_date
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

    /// Moves the branch to `new`, its commits rebased: in its `workspace`,
    /// through the mount, by git there (see `move_workspace`); else its
    /// ref, if no worktree has it by now (`update-ref`, refused if it moved
    /// meanwhile).
    fn apply(&self, common_dir: &Path, workspace: Option<&Path>, new: ObjectId) -> Result<Applied> {
        let branch = &self.branch;
        if let Some(dir) = workspace {
            return move_workspace(common_dir, dir, branch, self.tip, new);
        }
        if checked_out(common_dir, branch).is_some() {
            let why = format!(
                "{branch} was checked out meanwhile; then: `gitfold parent {branch} --update`"
            );
            return Err(Refusal::InUse.error(why));
        }
        let name = format!("refs/heads/{branch}");
        let why = format!("gitfold: rebase onto {}", self.onto_ref);
        match git::worktree::set_ref(common_dir, &name, &new, Some(&self.tip), &why) {
            Ok(()) => Ok(Applied::Done(new, Vec::new())),
            // Git refused the swap: it moved meanwhile.
            Err(_) if git::branch_tip(branch)?.id != self.tip => Err(Refusal::InUse.error(
                format!("{branch} moved meanwhile; again: `gitfold parent {branch} --update`"),
            )),
            Err(e) => Err(e),
        }
    }

    /// Whether a merge is among the commits it would replay (which aren't
    /// moved).
    pub fn merges(&self, common_dir: &Path) -> Result<bool> {
        if let Some(merges) = self.merges.get() {
            return Ok(merges);
        }
        let [tip, from] = self.own_range();
        let merges = count(common_dir, &["--merges", &tip, &from])? > 0;
        self.merges.set(Some(merges));
        Ok(merges)
    }

    /// Moves it (see the module doc): its base recorded, a conflict cleared
    /// or recorded. Not with a merge among its own commits, nor commits
    /// already pushed unless `yes`. `moving` is called once it is sure to
    /// try, and says where the branch is checked out: its workspace (a
    /// directory through the mount), or `None`. The caller holds `lock`.
    pub fn run(
        &self,
        common_dir: &Path,
        yes: bool,
        moving: impl FnOnce() -> Result<Option<PathBuf>>,
    ) -> Result<Moved> {
        let branch = &self.branch;
        let outcome = Moving {
            from: self.tip,
            onto: self.onto,
        };
        if self.up_to_date() {
            // Sits on it already (rebased by hand, say).
            outcome.finish(common_dir, branch)?;
            clear_conflict(common_dir, branch)?;
            return Ok(Moved::UpToDate);
        }
        if self.merges(common_dir)? {
            return Ok(Moved::Merge);
        }
        let pushed = self.pushed(common_dir)?;
        if !yes && let Some(pushed) = pushed {
            return Ok(Moved::Pushed(pushed));
        }
        let workspace = moving()?;
        let upstream = (!self.on_history).then_some(self.base);
        settings::set_moving(common_dir, branch, Some(&outcome.encode()))?;
        // On an error (git may have moved it) the record stays, for
        // `recover`.
        let new = match rebase::rebase(common_dir, branch, self.tip, self.onto, upstream)? {
            rebase::Rebased::Done(new) => new,
            rebase::Rebased::Conflict(paths) => {
                settings::set_moving(common_dir, branch, None)?;
                let paths = quoted(&paths);
                let at = self.onto.to_string();
                settings::set_conflict(common_dir, branch, Some((&at, &paths)))?;
                return Ok(Moved::Conflict(paths));
            }
        };
        match self.apply(common_dir, workspace.as_deref(), new)? {
            Applied::Work(paths) => {
                settings::set_moving(common_dir, branch, None)?;
                Ok(Moved::Work(quoted(&paths)))
            }
            Applied::Done(tip, unmerged) => {
                outcome.finish(common_dir, branch)?;
                clear_conflict(common_dir, branch)?;
                let kept = count(common_dir, &[&tip.to_string(), &format!("^{}", self.onto)])?;
                Ok(Moved::Done {
                    kept,
                    push: pushed.map(|p| p.push),
                    unmerged: quoted(&unmerged),
                })
            }
        }
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
