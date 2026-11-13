//! `gitfold branch --with-changes`: a new branch at the source branch's tip
//! whose workspace starts as a copy of the source's (a fork): its base, upper files, deletion markers,
//! timestamps and git index. The fork shows the same files and the same `git
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
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use super::{Workspace, read_base};
use crate::daemon::prewarm::BRANCH_KEY;
use crate::git;
use crate::git::worktree::{self, workspace_id};

/// Files of a state directory a fork copies (`upper` is copied as a tree;
/// `branch` is written for the fork; `tmp` holds only interrupted copy-ups).
/// `reads` too: the changes copied were built on those reads.
const COPIED: [&str; 4] = ["base", "markers", "times", super::reads::READS];

/// The source's git index, copied into the new state directory with the rest
/// (under the same lock) and moved into the fork's registration.
pub const INDEX: &str = ".index";

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
    let root = worktree::state_root(common_dir);
    let (from_id, to_id) = (workspace_id(from), workspace_id(to));
    let dst = root.join(&to_id);
    let to_admin = worktree::ws_admin_dir(common_dir, &to_id);
    let to_pin = worktree::pin_ref(&to_id);
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
    let from_admin = worktree::ws_admin_dir(common_dir, &from_id);
    worktree::refuse_if_busy(common_dir, from, "branch from it once that is done")?;
    let tip_ref = format!("refs/heads/{from}");
    let before = git::resolve(&tip_ref)?.id;
    let copied = root.join(&from_id).join("base").exists();
    if copied {
        let tmp = root.join(format!(".fork-{to_id}"));
        // Left by an interrupted fork.
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        let made = copy(&tmp).and_then(|()| {
            worktree::write_atomic(&tmp.join("branch"), format!("{to}\n").as_bytes())?;
            fs::rename(&tmp, &dst)?;
            Ok(())
        });
        if let Err(e) = made {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e.context(format!("copying the workspace of {from}")));
        }
    }
    // What the task gathered (spec, notes) goes with its changes, copied before
    // the branch exists: once it does, opening it makes an empty folder.
    let to_task = super::task::TaskDir::of(common_dir, to);
    let kept_task = to_task.dir.exists();
    let inbox = super::task::inbox_name(common_dir);
    let task_copied = match to_task.copy_from(&super::task::TaskDir::of(common_dir, from), &inbox) {
        Ok(copied) => copied,
        Err(e) => {
            log::warn!("copying the task folder of {from} into {to}: {e:#}");
            false
        }
    };
    let finish = || -> Result<()> {
        // git writes the index itself, not through the mount: a commit during
        // the copy would pair an older index with a newer tip.
        let tip = git::resolve(&tip_ref)?.id;
        ensure!(tip == before, "{from} moved while it was copied; retry");
        if copied {
            // Keeps the fork's base from `git gc` even if the source is removed
            // before the fork is first opened.
            let base = read_base(&dst)?;
            worktree::pin(common_dir, &to_pin, &base.id)?;
            register(&from_admin, from, to, &dst, &to_admin, mount)?;
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
        let _ = fs::remove_dir_all(&dst);
        let _ = fs::remove_dir_all(&to_admin);
        let _ = worktree::unpin(common_dir, &to_pin);
        if task_copied {
            let _ = fs::remove_dir_all(&to_task.dir);
        }
        return Err(e);
    }
    copy_settings(common_dir, from, to);
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

/// Copies state directory `src` of workspace `branch` into new directory
/// `dst`: the files in `COPIED`, the `upper` tree with modes and times (the
/// index the fork starts with recorded them), and its registration's git index.
/// Run on a source nothing writes to meanwhile.
pub fn copy_state_dir(common_dir: &Path, branch: &str, src: &Path, dst: &Path) -> Result<()> {
    // Checked again here, where nothing writes through the mount: a git command
    // started since (a stash, a commit) holds `index.lock` while it rewrites
    // files, and would be copied half-done.
    worktree::refuse_if_busy(common_dir, branch, "branch from it once that is done")?;
    let index = worktree::ws_admin_dir(common_dir, &workspace_id(branch)).join("index");
    fs::create_dir_all(dst)?;
    for name in COPIED {
        match fs::copy(src.join(name), dst.join(name)) {
            Ok(_) => {}
            // Markers and times exist only once something needed them.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && name != "base" => {}
            Err(e) => return Err(e).with_context(|| format!("copying {name}")),
        }
    }
    match fs::copy(&index, dst.join(INDEX)) {
        Ok(_) => {}
        // No registration (deleted since): the fork's first open writes one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("copying the git index"),
    }
    copy_tree(&src.join("upper"), &dst.join("upper"))
}

/// Copies the tree at `src` (a directory, if it exists) to `dst`, keeping
/// modes and modification times of files, symlinks and directories.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(src) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", src.display())),
    };
    fs::create_dir(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_symlink() {
            symlink(fs::read_link(&from)?, &to)?;
            set_times(&to, &entry.metadata()?)?;
        } else if kind.is_file() {
            let meta = entry.metadata()?;
            copy_file(&from, &to, &meta).with_context(|| format!("copying {}", from.display()))?;
            set_times(&to, &meta)?;
        } else {
            bail!("{}: not a file, symlink or directory", from.display());
        }
    }
    fs::set_permissions(dst, meta.permissions())?;
    set_times(dst, &meta)
}

/// Copies file `from` (with metadata `meta`) to `to`, with its mode. A file its
/// owner can't read (the mount allows it, see `open_upper`) is read with the
/// owner's read bit set meanwhile.
fn copy_file(from: &Path, to: &Path, meta: &fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    if mode & 0o400 != 0 {
        return fs::copy(from, to).map(drop);
    }
    fs::set_permissions(from, fs::Permissions::from_mode(mode | 0o400))?;
    let copied = fs::copy(from, to);
    fs::set_permissions(from, fs::Permissions::from_mode(mode))?;
    copied?;
    fs::set_permissions(to, fs::Permissions::from_mode(mode))
}

/// Sets `path`'s access and modification times (not following a symlink) to
/// `meta`'s.
fn set_times(path: &Path, meta: &fs::Metadata) -> Result<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timespec {
            tv_sec: meta.atime(),
            tv_nsec: meta.atime_nsec(),
        },
        libc::timespec {
            tv_sec: meta.mtime(),
            tv_nsec: meta.mtime_nsec(),
        },
    ];
    let flags = libc::AT_SYMLINK_NOFOLLOW;
    if unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("setting the times of {}", path.display()));
    }
    Ok(())
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
    let Ok(index) = fs::read(&copied) else {
        return Ok(());
    };
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
    worktree::register_branch(to_admin, to, &gitfile, || index)?;
    fs::remove_file(&copied)?;
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

/// Gives `to` the workspace settings set for `from`, if any: its prewarm mode
/// and read tracking.
fn copy_settings(common_dir: &Path, from: &str, to: &str) {
    for name in [BRANCH_KEY, super::reads::BRANCH_KEY] {
        let key = |b: &str| format!("branch.{b}.{name}");
        // Not set: `git config --get` fails.
        let Ok(value) = git::cmd::output(common_dir, &["config", "--local", "--get", &key(from)])
        else {
            continue;
        };
        if !value.trim().is_empty() {
            let _ = git::cmd::run(common_dir, &["config", "--local", &key(to), value.trim()]);
        }
    }
}

impl Workspace {
    /// Copies this open workspace's state into `dst` (see `copy_state_dir`),
    /// with every write through the mount held off meanwhile.
    pub fn copy_state(&self, dst: &Path) -> Result<()> {
        let mut layers = self.layers.write();
        // Copy-ups write under a shared `layers`; none is half-done now.
        layers.times.save()?;
        layers.markers.sync()?;
        copy_state_dir(&self.common_dir, &self.branch, &self.state_dir, dst)
    }
}

/// `copy_state_dir` for the stored (closed) workspace of `branch`.
pub fn copy_stored(common_dir: &Path, branch: &str, dst: &Path) -> Result<()> {
    let src = worktree::state_root(common_dir).join(workspace_id(branch));
    copy_state_dir(common_dir, branch, &src, dst)
}
