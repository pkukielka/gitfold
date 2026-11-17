//! The daemon's lifecycle around its mount: mount point checks, unmounting
//! (fusermount), signals.
//!
//! - `control`: the control socket.
//! - `follow`: following without an agent.
//! - `serve`: the daemon in the foreground.

pub mod commit_graph;
pub mod control;
pub mod follow;
pub mod serve;

use std::fs;
use std::io::ErrorKind;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result, bail};
use log::{info, warn};

/// Validates the mount point and returns its canonical path. The errors
/// fusermount3 gives for these cases are misleading (a bare ENOENT, or
/// "Permission denied" for a directory that is already mounted).
pub fn check(path: &Path) -> Result<PathBuf> {
    let shown = path.display();
    // A gitfold mount `gitfold unmount` cleans up; another FUSE mount, fusermount.
    let stale = || {
        let gitfold = canonical(path)
            .and_then(|path| crate::mount_table::gitfold_source(&path))
            .is_ok_and(|source| source.is_some());
        match gitfold {
            true => anyhow::anyhow!(
                "{shown} is a stale gitfold mount (its daemon is gone): `gitfold unmount \
                 {shown}`, then mount again"
            ),
            false => anyhow::anyhow!(
                "{shown} is a stale FUSE mount (its daemon is gone); run `fusermount3 -u \
                 {shown}` first"
            ),
        }
    };
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => return Err(stale()),
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
        if is_stale(&path) {
            return Err(stale());
        }
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
/// The handler catches the signals and passes them to a dedicated thread
/// through a pipe. Blocking them for `sigwait` instead would block them in
/// every process the daemon starts too (git, and what git starts: ssh,
/// index-pack, hooks): a blocked mask survives exec, a handler doesn't.
pub fn unmount_on_signal(mountpoint: PathBuf) {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        warn!(
            "no clean unmount on signals: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    // Signals faster than handled are dropped, not blocking the handler.
    unsafe { libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK) };
    SIGNAL_PIPE.store(fds[1], Ordering::Relaxed);
    // A terminal closing (SIGHUP): in the foreground, stop cleanly rather than die.
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            // Ignored when started (`nohup`, a shell's background job): left so.
            libc::sigaction(sig, std::ptr::null(), &mut action);
            if action.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            // System calls the signal interrupts resume.
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
    let mut signals = unsafe { fs::File::from_raw_fd(fds[0]) };
    std::thread::spawn(move || {
        let mut signalled = false;
        loop {
            let mut byte = [0u8];
            if signals.read_exact(&mut byte).is_err() {
                continue;
            }
            let sig = libc::c_int::from(byte[0]);
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
            crate::prewarm::stop_all();
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

/// Write end of the pipe `on_signal` reports to (see `unmount_on_signal`).
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let byte = sig as u8;
    // Async-signal-safe. errno is kept: the interrupted thread may be about to
    // read it (a full pipe sets it).
    unsafe {
        let errno = *libc::__errno_location();
        libc::write(
            SIGNAL_PIPE.load(Ordering::Relaxed),
            (&raw const byte).cast(),
            1,
        );
        *libc::__errno_location() = errno;
    }
}

enum Unmounted {
    Done,
    /// It was busy (e.g. a shell's working directory inside): detached at once;
    /// the daemon keeps serving the processes using it, and exits when the last
    /// one leaves.
    Detached,
}

/// Unmounts `mountpoint`, or detaches it if it is busy.
fn unmount_or_detach(mountpoint: &Path) -> Result<Unmounted> {
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

/// Whether the FUSE mount at `path` lost its daemon (killed, crashed): it
/// answers ENOTCONN. Asked with statfs, which always reaches the daemon (a
/// stat of the mount root may be answered from the kernel's cache); a live
/// daemon too busy to answer within a second isn't stale (the asking thread
/// is left behind, for a process about to exit).
pub fn is_stale(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // No thread to ask from (out of resources): not known to be stale.
    let asked = std::thread::Builder::new().spawn(move || {
        let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: a valid C string, and a buffer statfs fills.
        let rc = unsafe { libc::statfs(path.as_ptr(), st.as_mut_ptr()) };
        let _ = tx.send(
            rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN),
        );
    });
    asked.is_ok()
        && rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap_or(false)
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
