//! `gitfold branch --with-changes`: a new branch at the source branch's tip
//! whose workspace starts as a copy of the source's (a fork): its base, upper
//! files, deletion markers, timestamps and git index. The fork shows the same files and the same `git
//! status` (staged changes too), and costs what the changes weigh, not the
//! tree (on btrfs or XFS, `fs::copy` shares the data blocks).
//!
//! Mounted, the daemon copies an open source under its lock
//! (`Workspace::copy_state`), so no write lands half-way; unmounted, the command
//! copies under the repository lock. The copy is made in a dot directory
//! (skipped by `list`) and renamed into place, and the branch is created last:
//! until then the fork can't be opened, so nothing sees it half-made. A source
//! with a git operation in progress (a merge, a rebase, a commit) is refused,
//! also one started during the wait for the lock: its index and HEAD wouldn't
//! describe the copied files.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::task::TaskDir;
use crate::fsutil::{self, Unreadable};
use crate::git;
use crate::git::worktree;
use crate::settings;
use crate::state::{self, StateDir};

/// What a source with a git operation in progress is told.
const AFTER_GIT: &str = "branch from it once that is done";

/// Files of a state directory a fork copies (`upper` is copied as a tree;
/// `branch` is written for the fork; `tmp` holds only interrupted copy-ups).
/// `reads` too: the changes copied were built on those reads.
const COPIED: [&str; 4] = [state::BASE, state::MARKERS, state::TIMES, state::READS];

/// The source's git index, copied into the new state directory with the rest
/// (under the same lock) and moved into the fork's registration.
const INDEX: &str = ".index";

/// Forks workspace `from` into new branch `to`, with `copy` filling the new
/// state directory from the source's (see `copy_state_dir`). `mount` is the
/// mount point, if mounted, for the fork's registration. Returns what was done,
/// for the user.
pub fn fork(
    common_dir: &Path,
    from: &str,
    to: &str,
    mount: Option<&Path>,
    copy: impl FnOnce(&Path) -> Result<()>,
) -> Result<String> {
    ensure!(
        !to.starts_with('@'),
        "branch names starting with @ can't be opened as workspaces (@<rev> is a snapshot)"
    );
    git::require_branch(from)?;
    ensure!(!git::branch_exists(to), "branch {to} exists already");
    let (source, target) = (StateDir::of(common_dir, from), StateDir::of(common_dir, to));
    let (dst, to_admin) = (&target.dir, &target.admin);
    let to_pin = target.pin_ref();
    // Left by an earlier branch of that name: reusing them would mix its state
    // in, and a failure here would delete them.
    ensure!(
        !dst.exists() && !to_admin.exists(),
        "a workspace of an earlier branch {to} is still stored; `gitfold remove` it first"
    );
    // Alone (a removal interrupted after the rest was gone), `remove` finds
    // nothing to remove.
    ensure!(
        git::resolve(&to_pin).is_err(),
        "the base pin {to_pin} of an earlier branch {to} is left; delete it with `git \
         update-ref -d {to_pin}`"
    );
    worktree::refuse_if_busy(common_dir, &source.admin, from, AFTER_GIT)?;
    let before = git::branch_tip(from)?.id;
    let copied = source.opened();
    if copied {
        let tmp = state::state_root(common_dir).join(format!(".fork-{}", target.id));
        // Left by an interrupted fork.
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        let made = copy(&tmp).and_then(|()| {
            state::write_branch(&tmp, to)?;
            fs::rename(&tmp, dst)?;
            Ok(())
        });
        if let Err(e) = made {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e.context(format!("copying the workspace of {from}")));
        }
    }
    // What the task gathered (spec, notes) goes with its changes, copied before
    // the branch exists: once it does, opening it makes an empty folder.
    let local = settings::Local::read(common_dir);
    let to_task = TaskDir::with(&local, common_dir, to);
    let kept_task = to_task.dir.exists();
    let from_task = TaskDir::with(&local, common_dir, from);
    let task_copied = match to_task.copy_from(&from_task, &local.inbox()) {
        Ok(copied) => copied,
        Err(e) => {
            log::warn!("copying the task folder of {from} into {to}: {e:#}");
            false
        }
    };
    let finish = || -> Result<()> {
        // git writes the index itself, not through the mount: a commit during
        // the copy would pair an older index with a newer tip.
        let tip = git::branch_tip(from)?.id;
        ensure!(tip == before, "{from} moved while it was copied; retry");
        if copied {
            // Keeps the fork's base from `git gc` even if the source is removed
            // before the fork is first opened.
            let base = target.read_base()?;
            worktree::pin(common_dir, &to_pin, &base.id)?;
            register(&source.admin, from, to, dst, to_admin, mount)?;
        }
        let branch = format!("refs/heads/{to}");
        let tip = tip.to_string();
        // `""`: the branch must not exist (another fork may have made it).
        git::cmd::run(
            common_dir,
            &[
                "update-ref",
                "-m",
                "gitfold: branch --with-changes",
                &branch,
                &tip,
                "",
            ],
        )
    };
    if let Err(e) = finish() {
        // Undone: the branch is created last, so nothing has opened the fork;
        // the registration and pin were checked absent, so they are this
        // fork's.
        let _ = fs::remove_dir_all(dst);
        let _ = fs::remove_dir_all(to_admin);
        let _ = worktree::unpin(common_dir, &to_pin);
        if task_copied {
            let _ = fs::remove_dir_all(&to_task.dir);
        }
        return Err(e);
    }
    settings::copy_branch(&local, common_dir, from, to);
    let done = match copied {
        true => format!("created branch {to} at {from}, with its uncommitted changes"),
        false => {
            format!("created branch {to} at {from} (it had no workspace yet: no changes to copy)")
        }
    };
    Ok(match kept_task {
        true => format!(
            "{done}; kept the task folder an earlier branch {to} left (not {from}'s): {}",
            to_task.dir.display()
        ),
        false => done,
    })
}

/// Copies the stored state `src` of a workspace into new directory `dst`: the
/// files in `COPIED`, the `upper` tree with modes and times (the index the fork
/// starts with recorded them), and its registration's git index.
/// Run on a source nothing writes to meanwhile.
pub fn copy_state_dir(src: &StateDir, dst: &Path) -> Result<()> {
    let (common_dir, branch) = (&src.common_dir, &src.branch);
    // Checked again here, where nothing writes through the mount: a git command
    // started since (a stash, a commit) holds `index.lock` while it rewrites
    // files, and would be copied half-done.
    worktree::refuse_if_busy(common_dir, &src.admin, branch, AFTER_GIT)?;
    fs::create_dir_all(dst)?;
    for name in COPIED {
        match fs::copy(src.dir.join(name), dst.join(name)) {
            Ok(_) => {}
            // Markers and times exist only once something needed them.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && name != state::BASE => {}
            Err(e) => return Err(e).with_context(|| format!("copying {name}")),
        }
    }
    match fs::copy(src.index(), dst.join(INDEX)) {
        Ok(_) => {}
        // No registration (deleted since): the fork's first open writes one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("copying the git index"),
    }
    // Made when the workspace opens; a workspace never written to may lack it.
    // Under the workspace's lock (or unmounted): its unreadable files can be
    // lifted for the copy.
    let upper = src.upper();
    match fs::symlink_metadata(&upper) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => fsutil::copy_tree(
            &upper,
            &dst.join(state::UPPER),
            &|_| false,
            Unreadable::Lift,
        ),
    }
}

/// Registers the fork's worktree at `to_admin` with the source's index copied
/// into `dst`, so `git status` there starts where the source was. Without one
/// (the source's registration was deleted), the fork's first open writes one.
fn register(
    from_admin: &Path,
    from: &str,
    to: &str,
    dst: &Path,
    to_admin: &Path,
    mount: Option<&Path>,
) -> Result<()> {
    let copied = dst.join(INDEX);
    if !copied.exists() {
        return Ok(());
    }
    // Where the fork's `.git` file is; every open rewrites it, so unmounted a
    // guess from the source's (`<mount>/<from>/.git`) is enough.
    let mount = match mount {
        Some(mount) => Some(mount.to_path_buf()),
        None => fs::read_to_string(from_admin.join("gitdir"))
            .ok()
            .and_then(|g| mount_of(Path::new(g.trim_end()), from)),
    };
    let gitfile = match mount {
        Some(mount) => mount.join(to).join(".git"),
        None => dst.join(".git"),
    };
    // Moved into place, not read and written again: at 1M files it is 100 MB+.
    let index = |path: &Path| fsutil::rename_synced(&copied, path);
    worktree::register_branch(to_admin, to, &gitfile, index)?;
    // Still there if the registration existed already (and kept its index).
    match fs::remove_file(&copied) {
        Err(e) if !fsutil::not_found(&e) => return Err(e.into()),
        _ => {}
    }
    // With `core.splitIndex`, the index refers to a shared index file next to
    // it. git never changes one, and keeps old ones for weeks, so linking them
    // after the copy is enough.
    for entry in fs::read_dir(from_admin)? {
        let name = entry?.file_name();
        if name.as_bytes().starts_with(b"sharedindex.") {
            let (src, dst) = (from_admin.join(&name), to_admin.join(&name));
            if fs::hard_link(&src, &dst).is_err() {
                fs::copy(&src, &dst)?;
            }
        }
    }
    Ok(())
}

/// The mount point in `gitfile`, the `.git` path of workspace `branch`.
fn mount_of(gitfile: &Path, branch: &str) -> Option<PathBuf> {
    let suffix = Path::new(branch).join(".git");
    if !gitfile.ends_with(&suffix) {
        return None;
    }
    let mut mount = gitfile.to_path_buf();
    for _ in suffix.components() {
        mount.pop();
    }
    Some(mount)
}

/// `copy_state_dir` for the stored (closed) workspace of `branch`.
pub fn copy_stored(common_dir: &Path, branch: &str, dst: &Path) -> Result<()> {
    copy_state_dir(&StateDir::of(common_dir, branch), dst)
}
