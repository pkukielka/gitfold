//! Mount point checks and clean shutdown.

use std::fs;
use std::io::ErrorKind;
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
    if let Some(parent) = path.parent()
        && fs::metadata(parent)?.dev() != meta.dev()
    {
        bail!("{shown} is already a mount point; unmount it first (`fusermount3 -u {shown}`)");
    }
    Ok(path)
}

/// Unmounts on SIGINT/SIGTERM instead of dying with the mount left behind
/// ("Transport endpoint is not connected"). Unmounting ends the FUSE session,
/// so `fuser::mount` returns and the daemon exits normally. A second signal
/// exits immediately.
///
/// Must be called before any other thread starts: the signals are blocked here
/// and handled by `sigwait` on a dedicated thread, and threads inherit the mask.
pub fn unmount_on_signal(mountpoint: PathBuf) {
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
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
                std::process::exit(128 + sig);
            }
            signalled = true;
            info!("signal {sig}: unmounting {}", mountpoint.display());
            if !fusermount(&["-u"], &mountpoint) {
                // Busy (e.g. a shell's working directory is inside): detach now; the
                // daemon keeps serving until the last process leaves, then exits.
                warn!("mount is busy; detaching it lazily (signal again to exit now)");
                fusermount(&["-u", "-z"], &mountpoint);
            }
        }
    });
}

/// Runs fusermount3, or FUSE 2's fusermount only if fusermount3 isn't installed.
fn fusermount(args: &[&str], mountpoint: &Path) -> bool {
    let run = |bin| Command::new(bin).args(args).arg(mountpoint).status();
    match run("fusermount3") {
        Err(e) if e.kind() == ErrorKind::NotFound => run("fusermount"),
        status => status,
    }
    .is_ok_and(|s| s.success())
}
