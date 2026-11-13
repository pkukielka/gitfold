//! Linked-worktree registrations, so git commands run inside the mount work
//! without a checkout: snapshots (detached HEAD, synthesized index, see
//! `index`) and workspaces (HEAD on the branch, index written once). Also the
//! workspace helpers shared by the daemon and the management commands:
//! `refs/gitfold/<id>` base pins, whether git is busy in a worktree, and
//! workspace ids.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use gix::ObjectId;

use crate::fsutil;
use crate::git::Commit;

/// Snapshot registrations are named `<SNAPSHOT_PREFIX><first SNAPSHOT_HEX hex
/// digits of the commit>`.
const SNAPSHOT_PREFIX: &str = "gitfold-";
const SNAPSHOT_HEX: usize = 12;

/// Creates `<common>/worktrees/gitfold-<sha>` with a detached HEAD at the commit
/// and the given serialized index. Returns the admin dir; the mount's `.git`
/// file must point at it.
pub fn register(
    common_dir: &Path,
    commit: &Commit,
    index: &[u8],
    gitfile: &Path,
) -> Result<PathBuf> {
    let dir = snapshot_admin_dir(common_dir, &commit.id);
    write_admin(&dir, gitfile, index, &format!("{}\n", commit.id))?;
    Ok(dir)
}

/// Admin directory of the snapshot registration of `commit`.
pub fn snapshot_admin_dir(common_dir: &Path, commit: &ObjectId) -> PathBuf {
    common_dir.join("worktrees").join(format!(
        "{SNAPSHOT_PREFIX}{}",
        commit.to_hex_with_len(SNAPSHOT_HEX)
    ))
}

/// Registers a workspace on `branch` with admin directory `dir` (see
/// `ws_admin_dir`). HEAD and the index belong to git once the worktree exists,
/// so they are only written when it is new or incomplete (`index` is called
/// then); otherwise this just points `gitdir` at the current mount path.
pub fn register_branch(
    dir: &Path,
    branch: &str,
    gitfile: &Path,
    index: impl FnOnce() -> Vec<u8>,
) -> Result<()> {
    if dir.join("HEAD").exists() && dir.join("index").exists() {
        fsutil::write_atomic(
            &dir.join("gitdir"),
            format!("{}\n", gitfile.display()).as_bytes(),
        )?;
    } else {
        write_admin(
            dir,
            gitfile,
            &index(),
            &format!("ref: refs/heads/{branch}\n"),
        )?;
    }
    Ok(())
}

/// Writes a worktree admin directory. HEAD goes last: git (and
/// `register_branch`) treat a directory with HEAD as a worktree, so a crash
/// earlier leaves something that is rewritten next time instead of a worktree
/// without an index.
fn write_admin(dir: &Path, gitfile: &Path, index: &[u8], head: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    fsutil::write_atomic(&dir.join("commondir"), b"../..\n")?;
    fsutil::write_atomic(
        &dir.join("gitdir"),
        format!("{}\n", gitfile.display()).as_bytes(),
    )?;
    // Keeps `git worktree prune` from deleting the entry while unmounted.
    fsutil::write_atomic(&dir.join("locked"), b"managed by gitfold\n")?;
    fsutil::write_atomic(&dir.join("index"), index)?;
    fsutil::write_atomic(&dir.join("HEAD"), head.as_bytes())?;
    Ok(())
}

/// Keeps `commit` reachable for `git gc` with ref `name` (a workspace's base is
/// not otherwise referenced once its branch moves on). Goes through `git
/// update-ref`, which handles every ref backend (files, packed, reftable).
pub fn pin(common_dir: &Path, name: &str, commit: &gix::ObjectId) -> Result<()> {
    let commit = commit.to_string();
    super::cmd::run(
        common_dir,
        &["update-ref", "-m", "gitfold: workspace base", name, &commit],
    )
}

/// Deletes ref `name` (a workspace's base pin), if it exists.
pub fn unpin(common_dir: &Path, name: &str) -> Result<()> {
    if crate::git::resolve(name).is_err() {
        return Ok(());
    }
    super::cmd::run(common_dir, &["update-ref", "-d", name])
}

/// The ref keeping workspace `id`'s base from `git gc`.
pub fn pin_ref(id: &str) -> String {
    format!("refs/gitfold/{id}")
}

/// Files in a worktree's admin directory that mean a git operation is under
/// way, or stopped half-way waiting for the user (conflicts, `rebase -i`,
/// `bisect`).
const BUSY: [&str; 9] = [
    "index.lock",
    "HEAD.lock",
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "sequencer",
];

/// Why git is not idle in the worktree with admin directory `admin` on
/// `branch`, if it isn't: HEAD elsewhere (detached for a rebase or bisect, or
/// switched away), an operation running or stopped half-way, or a ref locked.
pub fn git_busy(common_dir: &Path, admin: &Path, branch: &str) -> Option<GitBusy> {
    let head = fs::read_to_string(admin.join("HEAD")).unwrap_or_default();
    if head.trim() != format!("ref: refs/heads/{branch}") {
        return Some(GitBusy::HeadElsewhere {
            branch: branch.to_string(),
            head: head.trim().to_string(),
        });
    }
    let exists = |p: PathBuf| fs::symlink_metadata(p).is_ok();
    if let Some(f) = BUSY.iter().find(|f| exists(admin.join(f))) {
        return Some(GitBusy::Operation(f));
    }
    let ref_lock = format!("refs/heads/{branch}.lock");
    (exists(common_dir.join(ref_lock)) || exists(common_dir.join("packed-refs.lock")))
        .then_some(GitBusy::RefLocked)
}

/// Why git is not idle in a worktree (see `git_busy`).
#[derive(Debug, PartialEq, Eq)]
pub enum GitBusy {
    /// HEAD is not on the branch (detached for a rebase or bisect, switched).
    HeadElsewhere { branch: String, head: String },
    /// An operation running or stopped half-way: the file that says so.
    Operation(&'static str),
    /// A ref is locked: git is updating it right now.
    RefLocked,
}

impl std::fmt::Display for GitBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitBusy::HeadElsewhere { branch, head } => {
                write!(f, "its HEAD is not on {branch} ({head})")
            }
            GitBusy::Operation(file) => write!(f, "a git operation is in progress ({file})"),
            GitBusy::RefLocked => write!(f, "a ref is locked"),
        }
    }
}

/// Whether `name` (in `<common>/worktrees/`) is a snapshot registration
/// (`gitfold-<sha12>`, see `register`).
pub fn is_snapshot_registration(name: &str) -> bool {
    name.strip_prefix(SNAPSHOT_PREFIX)
        .is_some_and(|hex| hex.len() == SNAPSHOT_HEX && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Refuses (a git operation in progress, `Refusal::Changes`) if git is busy in
/// the workspace of `branch`, with registration `admin`; `then` says what to
/// do once it is done.
pub fn refuse_if_busy(common_dir: &Path, admin: &Path, branch: &str, then: &str) -> Result<()> {
    match admin
        .exists()
        .then(|| git_busy(common_dir, admin, branch))
        .flatten()
    {
        Some(why) => {
            Err(crate::refusal::Refusal::Changes.error(format!("{branch}: {why}; {then}")))
        }
        None => Ok(()),
    }
}
