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
    match t {
        Some(t) => {
            let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
            libc::timespec {
                tv_sec: d.as_secs() as libc::time_t,
                tv_nsec: d.subsec_nanos() as _,
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

/// Copies directory `src` (nothing if it doesn't exist) to new directory
/// `dst`: files, symlinks and directories, with their modes and times,
/// leaving out what `skip` says (given the path relative to `src`). Anything
/// else (a socket, a device) is an error.
pub fn copy_tree(src: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
    copy_below(src, dst, Path::new(""), skip)
}

fn copy_below(src: &Path, dst: &Path, rel: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
    let meta = match fs::symlink_metadata(src) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", src.display())),
    };
    fs::create_dir(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let rel = rel.join(entry.file_name());
        if skip(&rel) {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_below(&from, &to, &rel, skip)?;
        } else if kind.is_symlink() {
            symlink(fs::read_link(&from)?, &to)?;
            copy_times(&to, &entry.metadata()?)?;
        } else if kind.is_file() {
            let meta = entry.metadata()?;
            copy_file(&from, &to, &meta).with_context(|| format!("copying {}", from.display()))?;
            copy_times(&to, &meta)?;
        } else {
            bail!("{}: not a file, symlink or directory", from.display());
        }
    }
    fs::set_permissions(dst, meta.permissions())?;
    copy_times(dst, &meta)
}

/// Copies file `from` (with metadata `meta`) to `to`, with its mode. A file its
/// owner can't read (a workspace allows it) is read with the owner's read bit
/// set meanwhile.
fn copy_file(from: &Path, to: &Path, meta: &fs::Metadata) -> io::Result<()> {
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
