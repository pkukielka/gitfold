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

/// Admin directory of the workspace with directory name `id`:
/// `<common>/worktrees/gitfold-ws-<id>`.
pub fn ws_admin_dir(common_dir: &Path, id: &str) -> PathBuf {
    common_dir
        .join("worktrees")
        .join(format!("gitfold-ws-{id}"))
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
        write_atomic(
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
    write_atomic(&dir.join("commondir"), b"../..\n")?;
    write_atomic(
        &dir.join("gitdir"),
        format!("{}\n", gitfile.display()).as_bytes(),
    )?;
    // Keeps `git worktree prune` from deleting the entry while unmounted.
    write_atomic(&dir.join("locked"), b"managed by gitfold\n")?;
    write_atomic(&dir.join("index"), index)?;
    write_atomic(&dir.join("HEAD"), head.as_bytes())?;
    Ok(())
}

/// Replaces `path` with `data` atomically (via `<path>.gitfold-tmp`).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("gitfold-tmp");
    let mut file = fs::File::create(&tmp)?;
    std::io::Write::write_all(&mut file, data)?;
    // Synced before the rename, so a power loss leaves the old file or the new
    // one, never an empty one.
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Keeps `commit` reachable for `git gc` with ref `name` (a workspace's base is
/// not otherwise referenced once its branch moves on). Goes through `git
/// update-ref`, which handles every ref backend (files, packed, reftable).
pub fn pin(common_dir: &Path, name: &str, commit: &gix::ObjectId) -> Result<()> {
    let commit = commit.to_string();
    git(
        common_dir,
        &["update-ref", "-m", "gitfold: workspace base", name, &commit],
    )
}

/// Deletes ref `name` (a workspace's base pin), if it exists.
pub fn unpin(common_dir: &Path, name: &str) -> Result<()> {
    if crate::git::resolve(name).is_err() {
        return Ok(());
    }
    git(common_dir, &["update-ref", "-d", name])
}

/// The ref keeping workspace `id`'s base from `git gc`.
pub fn pin_ref(id: &str) -> String {
    format!("refs/gitfold/{id}")
}

pub(crate) fn git(common_dir: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(args)
        .status()?;
    anyhow::ensure!(status.success(), "git {} failed ({status})", args.join(" "));
    Ok(())
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
pub fn git_busy(common_dir: &Path, admin: &Path, branch: &str) -> Option<String> {
    let head = fs::read_to_string(admin.join("HEAD")).unwrap_or_default();
    if head.trim() != format!("ref: refs/heads/{branch}") {
        return Some(format!("its HEAD is not on {branch} ({})", head.trim()));
    }
    let exists = |p: PathBuf| fs::symlink_metadata(p).is_ok();
    if let Some(f) = BUSY.iter().find(|f| exists(admin.join(f))) {
        return Some(format!("a git operation is in progress ({f})"));
    }
    let ref_lock = format!("refs/heads/{branch}.lock");
    (exists(common_dir.join(ref_lock)) || exists(common_dir.join("packed-refs.lock")))
        .then(|| REF_LOCKED.to_string())
}

/// What `git_busy` says while a ref is locked (git updating it).
pub const REF_LOCKED: &str = "a ref is locked";

/// `<common>/gitfold`: the workspaces' state directories, by id.
pub fn state_root(common_dir: &Path) -> PathBuf {
    common_dir.join("gitfold")
}

/// A workspace's id, naming its state and registration directories: the branch
/// with `%` and `/` escaped.
pub fn workspace_id(branch: &str) -> String {
    branch.replace('%', "%25").replace('/', "%2F")
}

/// The branch of workspace id `id` (inverse of `workspace_id`).
pub fn workspace_branch(id: &str) -> String {
    id.replace("%2F", "/").replace("%25", "%")
}

/// Whether `name` (in `<common>/worktrees/`) is a snapshot registration
/// (`gitfold-<sha12>`, see `register`).
pub fn is_snapshot_registration(name: &str) -> bool {
    name.strip_prefix(SNAPSHOT_PREFIX)
        .is_some_and(|hex| hex.len() == SNAPSHOT_HEX && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `git` with `args` in `common_dir`, returning its standard output.
pub(crate) fn git_output(common_dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(args)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {} failed ({})",
        args.join(" "),
        out.status
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Refuses (a git operation in progress, `Refusal::Changes`) if git is busy in
/// the workspace of `branch`; `then` says what to do once it is done.
pub fn refuse_if_busy(common_dir: &Path, branch: &str, then: &str) -> Result<()> {
    let admin = ws_admin_dir(common_dir, &workspace_id(branch));
    match admin
        .exists()
        .then(|| git_busy(common_dir, &admin, branch))
        .flatten()
    {
        Some(why) => {
            Err(crate::refusal::Refusal::Changes.error(format!("{branch}: {why}; {then}")))
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids name directories: no `/`, and the branch reads back from its id.
    #[test]
    fn workspace_ids_round_trip() {
        for branch in ["main", "agent/task-17", "a%2Fb", "100%", "x/%25/y"] {
            let id = workspace_id(branch);
            assert!(!id.contains('/'), "{id}");
            assert_eq!(workspace_branch(&id), branch);
        }
        assert_eq!(workspace_id("agent/task-17"), "agent%2Ftask-17");
    }
}
