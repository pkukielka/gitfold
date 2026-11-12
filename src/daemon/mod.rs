//! The daemon's lifecycle around its mount: mount point checks, the mount
//! table, unmounting (fusermount), signals, and the mount's source name.
//!
//! - `procs`: processes using a mount.
//! - `control`: the control socket.
//! - `prewarm`: walks of what was opened.

pub mod control;
pub mod prewarm;
pub mod procs;

use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use log::{info, warn};

/// Validates the mount point and returns its canonical path. The errors
/// fusermount3 gives for these cases are misleading (a bare ENOENT, or
/// "Permission denied" for a directory that is already mounted).
pub fn check(path: &Path) -> Result<PathBuf> {
    let shown = path.display();
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => bail!(
            "{shown} is a stale FUSE mount (its daemon is gone); run `fusermount3 -u {shown}` first"
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            bail!("mount point {shown} does not exist; create it with `mkdir -p {shown}`")
        }
        Err(e) => return Err(e).with_context(|| format!("mount point {shown}")),
    };
    if !meta.is_dir() {
        bail!("mount point {shown} is not a directory");
    }
    let path = path.canonicalize()?;
    if is_mounted(&path) {
        bail!("{shown} is already a mount point; unmount it first (`gitfold unmount {shown}`)");
    }
    Ok(path)
}

/// The canonical form of mount point `path` (as the mount table records it),
/// also for a stale mount that can't be stat-ed: the parent is canonicalized
/// and the last component appended.
pub fn canonical(path: &Path) -> Result<PathBuf> {
    if let Ok(path) = path.canonicalize() {
        return Ok(path);
    }
    let path = std::path::absolute(path)?;
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok(parent.canonicalize()?.join(name)),
        _ => Ok(path),
    }
}

/// Unmounts on SIGINT/SIGTERM/SIGHUP instead of dying with the mount left behind
/// ("Transport endpoint is not connected"). Unmounting ends the FUSE session,
/// so `fuser::mount` returns and the daemon exits normally. A second SIGINT or
/// SIGTERM exits immediately.
///
/// Must be called before any other thread starts: the signals are blocked here
/// and handled by `sigwait` on a dedicated thread, and threads inherit the mask.
pub fn unmount_on_signal(mountpoint: PathBuf) {
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        // A terminal closing: in the foreground, stop cleanly rather than die.
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    std::thread::spawn(move || {
        let mut signalled = false;
        loop {
            let mut sig = 0;
            if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                continue;
            }
            if signalled {
                // A closing terminal often sends SIGHUP twice; only an explicit
                // second signal skips the clean exit.
                if sig != libc::SIGHUP {
                    std::process::exit(128 + sig);
                }
                continue;
            }
            signalled = true;
            info!("signal {sig}: unmounting {}", mountpoint.display());
            // Walkers keep the mount busy; stop them so it unmounts rather than detaches.
            prewarm::stop_all();
            match unmount_or_detach(&mountpoint) {
                Ok(Unmounted::Done) => {}
                Ok(Unmounted::Detached) => {
                    warn!("mount is busy; detached it (signal again to exit now)")
                }
                // Nothing mounted yet (still opening the repository): just exit.
                Err(_) if !is_mounted(&mountpoint) => std::process::exit(128 + sig),
                Err(e) => warn!("{e:#}"),
            }
        }
    });
}

pub enum Unmounted {
    Done,
    /// It was busy (e.g. a shell's working directory inside): detached at once;
    /// the daemon keeps serving the processes using it, and exits when the last
    /// one leaves.
    Detached,
}

/// Unmounts `mountpoint`, or detaches it if it is busy.
pub fn unmount_or_detach(mountpoint: &Path) -> Result<Unmounted> {
    if unmount(mountpoint)? {
        return Ok(Unmounted::Done);
    }
    detach(mountpoint)?;
    Ok(Unmounted::Detached)
}

/// Unmounts `mountpoint`; false if it is busy (then still mounted).
pub fn unmount(mountpoint: &Path) -> Result<bool> {
    let out = fusermount(&["-u"], mountpoint)?;
    if out.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&out.stderr);
    if !error.contains("busy") {
        bail!("{}", error.trim());
    }
    Ok(false)
}

/// Detaches `mountpoint` (a lazy unmount): it disappears at once, and the
/// daemon serves the processes still using it until the last one leaves.
pub fn detach(mountpoint: &Path) -> Result<()> {
    let out = fusermount(&["-u", "-z"], mountpoint)?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Runs fusermount3, or FUSE 2's fusermount only if fusermount3 isn't installed.
fn fusermount(args: &[&str], mountpoint: &Path) -> std::io::Result<std::process::Output> {
    let run = |bin| Command::new(bin).args(args).arg(mountpoint).output();
    match run("fusermount3") {
        Err(e) if e.kind() == ErrorKind::NotFound => run("fusermount"),
        out => out,
    }
}

/// Whether something is mounted at `path` (it is on another device than its
/// parent).
pub fn is_mounted(path: &Path) -> bool {
    let dev = |p: &Path| fs::metadata(p).map(|m| m.dev());
    match (dev(path), path.parent().map(dev)) {
        (Ok(d), Some(Ok(parent))) => d != parent,
        _ => false,
    }
}

/// The source of the gitfold mount at `path` (`fs_name` of the
/// repository's git common directory), or `None` if no gitfold mount is there.
pub fn gitfold_source(path: &Path) -> Result<Option<String>> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let want = path.display().to_string();
    for line in mountinfo.lines() {
        // id parent dev root mount-point options ... - fstype source super-options
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        if fields.get(dash + 1) == Some(&"fuse.gitfold")
            && fields.get(4).is_some_and(|m| unescape(m) == want)
        {
            return Ok(fields.get(dash + 2).map(|s| unescape(s)));
        }
    }
    Ok(None)
}

/// Undoes the mount table's escapes: the kernel octal-escapes space, tab,
/// newline and backslash (the backslash last, so `\134040` stays `\040`).
fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// The source a mount of `common_dir` shows in the mount table (`FSName`): the
/// path, with `%`, `,` (mount options are comma-separated), `\` (fusermount
/// strips it) and non-ASCII bytes percent-escaped, so it reads back exactly.
/// `gitfold unmount` reads it back to wait for the daemon.
pub fn fs_name(common_dir: &Path) -> String {
    let mut name = String::new();
    for &b in common_dir.as_os_str().as_bytes() {
        if matches!(b, b'%' | b',' | b'\\') || !b.is_ascii() {
            name.push_str(&format!("%{b:02X}"));
        } else {
            name.push(b as char);
        }
    }
    name
}

/// The git common directory of a mount with source `source` (see `fs_name`).
pub fn common_dir_of_source(source: &str) -> PathBuf {
    let s = source.as_bytes();
    let mut bytes = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let hex = source
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) if s[i] == b'%' => {
                bytes.push(b);
                i += 3;
            }
            _ => {
                bytes.push(s[i]);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}
