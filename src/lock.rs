//! The repository lock: an `flock` on the git common directory, held by a
//! mounted daemon for its life and by `remove`/`prune` while they work.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::state::state_root;

/// The repository lock (see `lock`); released when dropped, also when the
/// process dies.
pub struct Lock {
    _files: Vec<fs::File>,
}

/// Takes the lock a mounted daemon holds for its life and `remove`/`prune` hold
/// while working, so one repository has at most one daemon, and none while
/// those run. Doesn't wait: `None` if it is taken.
///
/// It is an exclusive `flock` on the git common directory itself, so nothing is
/// created on disk. Where directories can't be flocked (NFS, unless mounted
/// with `local_lock=flock`), the lock file `<common>/gitfold/.lock` is used
/// instead, as earlier builds did; that file, if it exists, is locked too, so a
/// daemon of such a build is still seen.
pub fn lock(common_dir: &Path) -> Result<Option<Lock>> {
    let context = || format!("locking {}", common_dir.display());
    let file_lock = state_root(common_dir).join(".lock");
    let mut files = Vec::new();
    let dir = fs::File::open(common_dir).with_context(context)?;
    match flock(&dir) {
        Ok(true) => files.push(dir),
        Ok(false) => return Ok(None),
        // Not supported for directories here: the lock file instead.
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOLCK | libc::EINVAL | libc::EBADF | libc::EOPNOTSUPP)
            ) =>
        {
            fs::create_dir_all(state_root(common_dir)).with_context(context)?;
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_lock)
                .with_context(context)?;
        }
        Err(e) => return Err(e).with_context(context),
    }
    // The lock file of earlier builds (or of the fallback), only if it exists.
    match fs::OpenOptions::new().append(true).open(&file_lock) {
        Ok(file) => match flock(&file).with_context(context)? {
            true => files.push(file),
            false => return Ok(None),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(context),
    }
    Ok(Some(Lock { _files: files }))
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
