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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gix::ObjectId;

use crate::git::worktree::GitBusy;
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
pub fn is_ancestor(common_dir: &Path, a: ObjectId, b: ObjectId) -> Result<bool> {
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

/// Where the own commits of `branch` start, as recorded.
pub fn recorded_base(branch: &str) -> Option<ObjectId> {
    rev(&base_ref(branch))
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

/// Where the own commits of local branch `branch` start, built on `parent`
/// (see `own_base`).
pub fn own_start(
    common_dir: &Path,
    branch: &str,
    parent: Option<&str>,
    roots: &[String],
) -> Result<ObjectId> {
    own_base(
        common_dir,
        branch,
        parent,
        roots,
        git::branch_tip(branch)?.id,
    )
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

/// The refusal to move `branch`, checked out in git worktree `path`
/// (`meanwhile`: since the move began): git rebase there, `rebase` if known.
pub fn in_worktree(
    branch: &str,
    path: &str,
    meanwhile: bool,
    rebase: Option<&str>,
) -> anyhow::Error {
    let (is, meanwhile) = match meanwhile {
        true => ("was", " meanwhile"),
        false => ("is", ""),
    };
    let rebase = rebase.map(|r| format!(" ({r})")).unwrap_or_default();
    Refusal::InUse.error(format!(
        "{branch} {is} checked out in {path}{meanwhile}: gitfold doesn't move it there; take \
         the new commits with git rebase in it{rebase}"
    ))
}

/// How moving the branch to its rebased commits went.
enum Applied {
    /// Moved to this tip; the paths where its uncommitted work, put back,
    /// conflicted all the same (git keeps the work in the stash).
    Done(ObjectId, Vec<Vec<u8>>),
    /// Not moved: its uncommitted work conflicts with the move on these
    /// paths.
    Work(Vec<Vec<u8>>),
    /// Not moved: git refused (untracked files in the way, say), saying
    /// this.
    Refused(String),
}

/// The paths with a conflict git left unresolved in the index of the
/// worktree with admin directory `admin` (no work tree needed), as
/// `git ls-files -u` reads them from its index.
pub fn unmerged(admin: &Path) -> Result<Vec<Vec<u8>>> {
    let out = cmd::output_of(
        &mut cmd::command_in_other(admin)?,
        &["ls-files", "-u", "-z"],
        None,
    )?;
    // `<mode> <id> <stage>\t<path>`, one entry per stage.
    let mut paths: Vec<Vec<u8>> = (out.split(|b| *b == 0))
        .filter_map(|e| {
            e.iter()
                .position(|b| *b == b'\t')
                .map(|t| e[t + 1..].to_vec())
        })
        .collect();
    paths.dedup();
    Ok(paths)
}

/// Moves the checkout of workspace `dir` (through the mount) of `branch`,
/// at `tip`, to `new`, the uncommitted work carried along, all by git: if
/// git was used there since (HEAD elsewhere, the branch moved), refused,
/// to retry; uncommitted work (as `git stash create` takes it, nothing
/// changed) that conflicts with the move (`git merge-tree` of it onto
/// `new`, from `tip`): nothing moves, the paths are said; else
/// `git rebase --autostash --onto <new> <tip>`, which replays nothing (the
/// branch was at `tip`), so the branch and its files go to `new`, and puts
/// the work back (staged changes come back unstaged, as with git's
/// autostash). Git refusing (untracked files in the way, say) is read from
/// its state, not its message: no rebase under way, the branch where it
/// was; then nothing moved, and git's message is said. If git stops
/// half-way (a hook, a race), its state stays as git leaves it: a rebase in
/// progress, the work kept in the stash.
fn move_workspace(
    common_dir: &Path,
    dir: &Path,
    branch: &str,
    tip: ObjectId,
    new: ObjectId,
) -> Result<Applied> {
    let git = |args: &[&str]| cmd::output_of(&mut cmd::in_workspace(dir)?, args, None);
    // HEAD read from its admin directory, as git keeps it.
    let state = crate::state::StateDir::of(common_dir, branch);
    let on_branch = !matches!(state.git_busy(), Some(GitBusy::HeadElsewhere { .. }));
    if !on_branch || git::branch_tip(branch)?.id != tip {
        let why = format!(
            "{branch} moved in its workspace meanwhile; again: `gitfold parent {branch} --update`"
        );
        return Err(Refusal::InUse.error(why));
    }
    let (tip_hex, new_hex) = (tip.to_string(), new.to_string());
    // Git refuses here too (paths unmerged meanwhile, say): nothing moved.
    let stash = match git(&["stash", "create"]) {
        Ok(stash) => stash,
        Err(e) => {
            let said = e.downcast::<cmd::Failed>()?.stderr.trim().to_string();
            return Ok(Applied::Refused(said));
        }
    };
    let work = String::from_utf8_lossy(&stash).trim().to_string();
    if !work.is_empty() {
        let mut check = cmd::command(common_dir);
        let args = ["merge-tree", "--write-tree", "-z", "--name-only"];
        let out = (check.args(args))
            .args(["--merge-base", &tip_hex, &new_hex, &work])
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
    if let Err(e) = cmd::output_of(
        &mut rebase,
        &[&args[..], &[&new_hex, &tip_hex]].concat(),
        None,
    ) {
        let failed = e.downcast::<cmd::Failed>()?;
        let said = failed.stderr.trim().to_string();
        // Git's state, not its message: no rebase under way (HEAD on the
        // branch, no rebase's directory: a lock another git holds says
        // nothing of this one), the branch where it was.
        let started = matches!(
            state.git_busy(),
            Some(
                GitBusy::HeadElsewhere { .. } | GitBusy::Operation("rebase-merge" | "rebase-apply")
            )
        );
        if !started && git::branch_tip(branch)?.id == tip {
            return Ok(Applied::Refused(said));
        }
        bail!(
            "git rebase in {} stopped ({}), left as git leaves it: {said}",
            dir.display(),
            failed.status
        );
    }
    // Moved (git exits 0 even if the work, put back, conflicted: then the
    // paths are unmerged, and the work stays in the stash).
    Ok(Applied::Done(
        git::branch_tip(branch)?.id,
        unmerged(&state.admin)?,
    ))
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
    /// Not moved: its uncommitted work is in the way on these paths
    /// (quoted; not recorded: it is the work's).
    Work(Vec<String>),
    /// Not moved: git refused in its workspace, saying this (not recorded).
    Refused(String),
}

/// `paths` as git quotes them (a newline in one can't split it).
pub fn quoted(paths: &[Vec<u8>]) -> Vec<String> {
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
        match checked_out(common_dir, branch) {
            None => {}
            Some(CheckedOut::Workspace) => {
                return Err(Refusal::InUse.error(format!(
                    "{branch} was checked out in a workspace meanwhile; then: `gitfold parent \
                     {branch} --update`"
                )));
            }
            Some(CheckedOut::Worktree(path)) => {
                let rebase = self.by_hand(crate::tree::short(&self.onto_ref));
                return Err(in_worktree(branch, &path, true, Some(&rebase)));
            }
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
    /// already pushed unless `yes`. `moving` is called before the pushed
    /// commits are checked (what is in the way in its workspace is said
    /// first), with whether it moves (no pushed commits in the way), and
    /// says where the branch is checked out: its workspace (a directory
    /// through the mount), or `None`. The caller holds `lock`.
    pub fn run(
        &self,
        common_dir: &Path,
        yes: bool,
        moving: impl FnOnce(bool) -> Result<Option<PathBuf>>,
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
        let workspace = moving(yes || pushed.is_none())?;
        if !yes && let Some(pushed) = pushed {
            return Ok(Moved::Pushed(pushed));
        }
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
            Applied::Refused(said) => {
                settings::set_moving(common_dir, branch, None)?;
                Ok(Moved::Refused(said))
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

/// The most commits of a target `landed` tries a merge on in one check:
/// the newest (a later check takes only the target's commits after them).
const MAX_TRIED: usize = 500;

/// What `landing` checked for a branch: its base, and per target (a full
/// ref) the tip it checked and whether the base had landed there
/// (`gitfoldLanding`: the base and whether the parent existed (`fresh`),
/// then `<ref> <tip> <0|1>` per line).
/// Forgotten when either changes.
struct Checked {
    base: ObjectId,
    fresh: bool,
    targets: Vec<(String, ObjectId, bool)>,
}

impl Checked {
    fn encode(&self) -> String {
        let lines =
            (self.targets.iter()).map(|(t, tip, yes)| format!("\n{t} {tip} {}", *yes as u8));
        std::iter::once(format!("{} {}", self.base, self.fresh as u8))
            .chain(lines)
            .collect()
    }

    fn decode(text: &str) -> Option<Checked> {
        let mut lines = text.lines();
        let (base, fresh) = lines.next()?.split_once(' ')?;
        let base = ObjectId::from_hex(base.as_bytes()).ok()?;
        let fresh = fresh == "1";
        let targets = lines
            .map(|line| {
                let mut words = line.split(' ');
                let target = words.next()?.to_string();
                let tip = ObjectId::from_hex(words.next()?.as_bytes()).ok()?;
                Some((target, tip, words.next()? == "1"))
            })
            .collect::<Option<_>>()?;
        Some(Checked {
            base,
            fresh,
            targets,
        })
    }
}

/// Where the parent of `branch` landed, as `landing` last recorded it, if
/// it found a landing: of its own commits as they start now (`recorded_base`)
/// and of its parent as it is now (`fresh`: its branch still there; one
/// found while it was holds once it is gone, the check then being looser).
pub fn landed_recorded(local: &Local, branch: &str, fresh: bool) -> Option<String> {
    let checked = Checked::decode(&local.landing(branch)?)?;
    if (fresh && !checked.fresh) || Some(checked.base) != recorded_base(branch) {
        return None;
    }
    let mut targets = checked.targets.into_iter();
    targets.find(|&(_, _, yes)| yes).map(|(target, ..)| target)
}

/// The first of `targets` (full refs) that the commits of the parent of
/// `branch` it sits on, up to `base`, landed in (spec section 4): see
/// `landed`; `fresh` while the parent's branch exists. Incremental: the
/// commits of a target checked before (`Checked`) aren't checked again;
/// what it checks is recorded if `record`.
pub fn landing(
    common_dir: &Path,
    local: &Local,
    branch: &str,
    base: ObjectId,
    targets: &[String],
    fresh: bool,
    record: bool,
) -> Result<Option<String>> {
    let recorded = local.landing(branch);
    let before = (recorded.as_deref())
        .and_then(Checked::decode)
        .filter(|c| c.base == base && c.fresh == fresh);
    let mut checked = Checked {
        base,
        fresh,
        targets: Vec::new(),
    };
    let mut found = None;
    for target in targets {
        let Some(tip) = rev(target) else { continue };
        // Checked at that tip, or one before it (still in its history).
        let since = match (before.iter().flat_map(|c| &c.targets)).find(|(t, ..)| t == target) {
            Some(&(_, at, yes)) if at == tip || is_ancestor(common_dir, at, tip)? => {
                Some((at, yes))
            }
            _ => None,
        };
        let yes = match since {
            Some((at, yes)) if yes || at == tip => yes,
            _ => landed(common_dir, base, tip, since.map(|(at, _)| at), fresh)?,
        };
        checked.targets.push((target.clone(), tip, yes));
        if yes {
            found = Some(target.clone());
            break;
        }
    }
    let text = checked.encode();
    if record && recorded.as_deref() != Some(text.as_str()) {
        settings::set_landing(common_dir, branch, &text)?;
    }
    Ok(found)
}

/// Whether the commits of a parent a branch sits on, up to `base`, are in
/// `tip` (a target's), as git answers it: in its history (`git merge-base
/// --is-ancestor`: a merge commit, or a fast-forward unless `fresh`), else
/// as other commits (`squashed`). `since`: an older tip of the target they
/// weren't in, whose commits aren't checked again.
///
/// With `fresh` (the parent's branch still exists) a base on the target's
/// first-parent line isn't taken: a fast-forward looks there just like a
/// parent with no commits of its own.
fn landed(
    common_dir: &Path,
    base: ObjectId,
    tip: ObjectId,
    since: Option<ObjectId>,
    fresh: bool,
) -> Result<bool> {
    if !is_ancestor(common_dir, base, tip)? {
        return squashed(common_dir, base, tip, since);
    }
    if !fresh {
        return Ok(true);
    }
    if let Some(since) = since
        && is_ancestor(common_dir, base, since)?
    {
        // On its first-parent line then already.
        return Ok(false);
    }
    // The oldest commit of the target's first-parent line after the base
    // (and `since`): a merge has the base as a second parent; on the line,
    // as its first.
    let mut args = ["rev-list", "--first-parent", "--parents"]
        .map(String::from)
        .to_vec();
    args.extend([tip.to_string(), format!("^{base}")]);
    args.extend(since.map(|at| format!("^{at}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let line = cmd::output(common_dir, &args)?;
    let line = String::from_utf8_lossy(&line);
    let first_parent = line.lines().last().and_then(|l| l.split(' ').nth(1));
    Ok(first_parent.is_some_and(|p| ObjectId::from_hex(p.as_bytes()).ok() != Some(base)))
}

/// Whether the changes of `base` since it and the target (at `tip`)
/// diverged are in the target as other commits (a squash merge; a rebase
/// merge): merging `base` into the target changes nothing (`git
/// merge-tree`, from where they diverged). First into its tip; else (the
/// target changed those lines since) into each of its commits touching
/// their paths, after `since`, the newest `MAX_TRIED`. It is a guess: the
/// target making the same change on its own looks the same.
fn squashed(
    common_dir: &Path,
    base: ObjectId,
    tip: ObjectId,
    since: Option<ObjectId>,
) -> Result<bool> {
    let (base_hex, tip_hex) = (base.to_string(), tip.to_string());
    let Some(fork) = merge_base(common_dir, &[&base_hex, &tip_hex])? else {
        return Ok(false);
    };
    let fork_hex = fork.to_string();
    let args = [
        "diff",
        "--no-renames",
        "--name-only",
        "-z",
        &fork_hex,
        &base_hex,
    ];
    let paths = cmd::nul_paths(&cmd::output(common_dir, &args)?);
    if paths.is_empty() {
        return Ok(false);
    }
    let tree = cmd::output(common_dir, &["rev-parse", &format!("{tip}^{{tree}}")])?;
    let tree = String::from_utf8_lossy(&tree).trim().to_string();
    if no_op_merge(common_dir, fork, base, &[(tip_hex.as_str(), tree.as_str())])? {
        return Ok(true);
    }
    // The paths on standard input: a large change's would overflow the
    // command line.
    let mut input = format!("{tip}\n^{fork}\n").into_bytes();
    if let Some(since) = since {
        input.extend(format!("^{since}\n").bytes());
    }
    input.extend(b"--\n");
    for path in paths {
        input.extend(path);
        input.push(b'\n');
    }
    let mut rev_list = cmd::command(common_dir);
    rev_list.env("GIT_LITERAL_PATHSPECS", "1");
    let args = [
        "rev-list",
        "--no-commit-header",
        "--format=%H %T",
        "--stdin",
    ];
    let touching = cmd::output_of(&mut rev_list, &args, Some(input))?;
    let touching = String::from_utf8_lossy(&touching);
    let mut commits: Vec<(&str, &str)> = (touching.lines())
        .filter_map(|line| line.split_once(' '))
        // The tip merged into already.
        .filter(|(commit, _)| *commit != tip_hex)
        .collect();
    if commits.len() > MAX_TRIED {
        log::warn!(
            "whether {base} landed in {tip}: only the newest {MAX_TRIED} of {} commits touching \
             its files tried",
            commits.len()
        );
        commits.truncate(MAX_TRIED);
    }
    no_op_merge(common_dir, fork, base, &commits)
}

/// Whether merging `base` into one of `commits` (each with its tree), from
/// `fork`, gives that commit's tree: one `git merge-tree --stdin` for all.
fn no_op_merge(
    common_dir: &Path,
    fork: ObjectId,
    base: ObjectId,
    commits: &[(&str, &str)],
) -> Result<bool> {
    if commits.is_empty() {
        return Ok(false);
    }
    let merges: String = (commits.iter())
        .map(|(commit, _)| format!("{fork} -- {commit} {base}\n"))
        .collect();
    let args = [
        "merge-tree",
        "--stdin",
        "--write-tree",
        "--no-messages",
        "--name-only",
        "-z",
    ];
    let out = cmd::output_of(
        &mut cmd::command(common_dir),
        &args,
        Some(merges.into_bytes()),
    )?;
    // Each merge: its status (1: clean), its tree, the paths it conflicts
    // on, an empty field.
    let mut fields = out.split(|b| *b == 0);
    for (_, tree) in commits {
        let (Some(clean), Some(merged)) = (fields.next(), fields.next()) else {
            bail!("git merge-tree --stdin said less than asked");
        };
        if clean == b"1" && merged == tree.as_bytes() {
            return Ok(true);
        }
        fields.by_ref().take_while(|f| !f.is_empty()).for_each(drop);
    }
    Ok(false)
}

/// What the agent of `branch` is told of a move of it (`said`, what
/// `--update` says): by its turn start, or kept by the daemon.
pub fn moved_said(branch: &str, said: &str) -> String {
    format!("{branch} took its parent's new commits:\n{said}")
}
