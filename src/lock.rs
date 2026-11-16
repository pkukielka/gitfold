//! The repository lock: an `flock` on the git common directory, held by a
//! mounted daemon for its life and by commands changing stored state unmounted
//! (`remove`, `prune`, a fork) while they work.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::state::state_root;

/// The repository lock (see `lock`); released when dropped, also when the
/// process dies.
pub struct Lock {
    _file: fs::File,
}

/// Takes the lock a mounted daemon holds for its life and `remove`/`prune` hold
/// while working, so one repository has at most one daemon, and none while
/// those run. Doesn't wait: `None` if it is taken.
///
/// It is an exclusive `flock` on the git common directory itself, so nothing is
/// created on disk. Where directories can't be flocked (NFS, unless mounted
/// with `local_lock=flock`), the lock file `<common>/gitfold/.lock` is used
/// instead. Hosts sharing a repository over NFS with different `local_lock`
/// options don't see each other's lock. `GITFOLD_TEST_LOCK_FILE` forces the
/// file, for tests.
pub fn lock(common_dir: &Path) -> Result<Option<Lock>> {
    let context = || format!("locking {}", common_dir.display());
    let dir = fs::File::open(common_dir).with_context(context)?;
    let flocked = match std::env::var_os("GITFOLD_TEST_LOCK_FILE") {
        Some(_) => Err(std::io::Error::from_raw_os_error(libc::ENOLCK)),
        None => flock(&dir),
    };
    let locked = match flocked {
        Ok(true) => dir,
        Ok(false) => return Ok(None),
        // Not supported for directories here: the lock file instead.
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOLCK | libc::EINVAL | libc::EBADF | libc::EOPNOTSUPP)
            ) =>
        {
            let root = state_root(common_dir);
            fs::create_dir_all(&root).with_context(context)?;
            return try_file(&root.join(".lock")).with_context(context);
        }
        Err(e) => return Err(e).with_context(context),
    };
    Ok(Some(Lock { _file: locked }))
}

/// An exclusive `flock` on file `path` (created if missing), if no one else
/// holds it: the repository lock's fallback, and moves of one branch at a
/// time (`git::rebase`).
pub fn try_file(path: &Path) -> Result<Option<Lock>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(flock(&file)?.then_some(Lock { _file: file }))
}

/// An exclusive, non-blocking `flock`: whether it was taken.
fn flock(file: &fs::File) -> std::io::Result<bool> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(file);
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(false),
        _ => Err(e),
    }
}

/// Holds the repository's daemon lock for the life of a mount: two daemons
/// would share, and corrupt, the stored workspace state, and `remove` and
/// `prune` must not run meanwhile.
pub fn mount_lock(common_dir: &Path) -> Result<Lock> {
    lock(common_dir)?.with_context(|| {
        format!(
            "{} is already mounted (or a `gitfold remove`/`prune` of it is running); \
             a repository can be mounted only once at a time",
            common_dir.display()
        )
    })
}
