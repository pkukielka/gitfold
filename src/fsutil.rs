//! File helpers with nothing gitfold-specific in them: writing a file
//! atomically, setting times without following symlinks, copying a tree with
//! its modes and times.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// Replaces `path` with `data` atomically (a temporary file renamed over it),
/// synced first, so a power loss leaves the old file or the new one, never an
/// empty one.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("gitfold-tmp");
    let mut file = fs::File::create(&tmp)?;
    io::Write::write_all(&mut file, data)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Sets `path`'s access and modification times (`None`: left as they are),
/// not following a symlink.
pub fn set_times(
    path: &Path,
    atime: Option<SystemTime>,
    mtime: Option<SystemTime>,
) -> io::Result<()> {
    let times = [timespec(atime), timespec(mtime)];
    let path =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let flags = libc::AT_SYMLINK_NOFOLLOW;
    // SAFETY: a valid path and two timespecs.
    if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn timespec(t: Option<SystemTime>) -> libc::timespec {
    match t.map(|t| t.duration_since(UNIX_EPOCH)) {
        Some(Ok(d)) => libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as _,
        },
        // Before 1970: negative seconds, and nanoseconds counted forward.
        Some(Err(e)) => {
            let d = e.duration();
            let (secs, nanos) = (d.as_secs() as libc::time_t, d.subsec_nanos());
            match nanos {
                0 => libc::timespec {
                    tv_sec: -secs,
                    tv_nsec: 0,
                },
                n => libc::timespec {
                    tv_sec: -secs - 1,
                    tv_nsec: (1_000_000_000 - n) as _,
                },
            }
        }
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    }
}

/// Gives `path` the access and modification times in `meta`.
fn copy_times(path: &Path, meta: &fs::Metadata) -> Result<()> {
    set_times(path, meta.accessed().ok(), meta.modified().ok())
        .with_context(|| format!("setting the times of {}", path.display()))
}

/// What `copy_tree` does with a file its owner can't read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// Reads it with the owner's read bit set meanwhile: for a tree nothing
    /// else uses at the time (a workspace's upper, under its lock).
    Lift,
    /// Fails, leaving the source as it is (a tree others may be using).
    Fail,
}

/// Copies directory `src` to new directory `dst`: files, symlinks and
/// directories, with their modes and times, leaving out what `skip` says
/// (given the path relative to `src`). Anything else (a socket, a device) is
/// an error.
pub fn copy_tree(
    src: &Path,
    dst: &Path,
    skip: &dyn Fn(&Path) -> bool,
    unreadable: Unreadable,
) -> Result<()> {
    copy_below(src, dst, Path::new(""), skip, unreadable)
}

fn copy_below(
    src: &Path,
    dst: &Path,
    rel: &Path,
    skip: &dyn Fn(&Path) -> bool,
    unreadable: Unreadable,
) -> Result<()> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if !meta.is_dir() {
        bail!("{}: not a directory", src.display());
    }
    let entries = fs::read_dir(src).with_context(|| format!("listing {}", src.display()))?;
    fs::create_dir(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in entries {
        let entry = entry?;
        let rel = rel.join(entry.file_name());
        if skip(&rel) {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_below(&from, &to, &rel, skip, unreadable)?;
        } else if kind.is_symlink() {
            symlink(fs::read_link(&from)?, &to)?;
            copy_times(&to, &entry.metadata()?)?;
        } else if kind.is_file() {
            let meta = entry.metadata()?;
            copy_file(&from, &to, &meta, unreadable)
                .with_context(|| format!("copying {}", from.display()))?;
            copy_times(&to, &meta)?;
        } else {
            bail!("{}: not a file, symlink or directory", from.display());
        }
    }
    fs::set_permissions(dst, meta.permissions())?;
    copy_times(dst, &meta)
}

/// Copies file `from` (with metadata `meta`) to `to`, with its mode; one its
/// owner can't read as `unreadable` says.
fn copy_file(
    from: &Path,
    to: &Path,
    meta: &fs::Metadata,
    unreadable: Unreadable,
) -> io::Result<()> {
    let mode = meta.permissions().mode();
    if mode & 0o400 != 0 || unreadable == Unreadable::Fail {
        return fs::copy(from, to).map(drop);
    }
    fs::set_permissions(from, fs::Permissions::from_mode(mode | 0o400))?;
    let copied = fs::copy(from, to);
    fs::set_permissions(from, fs::Permissions::from_mode(mode))?;
    copied?;
    fs::set_permissions(to, fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gitfold-fsutil-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Times before 1970 are kept, fractions of a second included.
    #[test]
    fn times_before_1970() {
        let dir = scratch("times");
        let file = dir.join("f");
        fs::write(&file, "x").unwrap();
        for t in [
            UNIX_EPOCH - Duration::new(86_400, 250_000_000),
            UNIX_EPOCH - Duration::from_secs(1),
            UNIX_EPOCH + Duration::new(1_700_000_000, 5),
        ] {
            set_times(&file, Some(t), Some(t)).unwrap();
            assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), t);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// A missing source is an error, not an empty copy.
    #[test]
    fn copy_of_nothing_fails() {
        let dir = scratch("missing");
        assert!(
            copy_tree(
                &dir.join("none"),
                &dir.join("dst"),
                &|_| false,
                Unreadable::Fail
            )
            .is_err()
        );
        assert!(!dir.join("dst").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
