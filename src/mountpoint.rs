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

/// The source of the gitfold mount at `path` (`admin::fs_name` of the
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

/// A process using a mount.
pub struct User {
    pub pid: u32,
    /// Its command line, shortened.
    pub command: String,
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} {}", self.pid, self.command)
    }
}

/// The processes of this user (only they can use the mount) using something
/// at or below `path` in the mount at `mountpoint`: their working or root
/// directory, an open file or a mapped one. Read before detaching: after it,
/// their paths no longer resolve to the mount point. Nothing below the mount
/// point is looked up through the mount.
pub fn users(mountpoint: &Path, path: &Path) -> Vec<User> {
    let Ok(procs) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    // Only needed (and only stat-ed: through the mount) for links to the
    // mount point itself, never for a path below it.
    let mount_dev = std::cell::OnceCell::new();
    let uid = unsafe { libc::getuid() };
    let below = |p: &Path| p.starts_with(path);
    // A link to the mount point itself may be the directory under the mount
    // (entered before mounting): the device tells. Stat-ing it goes through
    // the mount root, which the daemon still serves.
    let in_mount = |link: &Path| {
        fs::read_link(link).is_ok_and(|p| {
            p.starts_with(path)
                && (p != mountpoint || {
                    let dev =
                        mount_dev.get_or_init(|| fs::metadata(mountpoint).map(|m| m.dev()).ok());
                    fs::metadata(link).is_ok_and(|m| Some(m.dev()) == *dev)
                })
        })
    };
    let mut users = Vec::new();
    for proc in procs.flatten() {
        let Some(pid) = proc.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        if proc.metadata().map_or(true, |m| m.uid() != uid) {
            continue;
        }
        let dir = proc.path();
        let uses = ["cwd", "root", "exe"]
            .iter()
            .any(|n| in_mount(&dir.join(n)))
            || fs::read_dir(dir.join("fd"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|fd| in_mount(&fd.path()))
            || fs::read_to_string(dir.join("maps")).is_ok_and(|maps| {
                maps.lines()
                    .filter_map(|l| l.find('/').map(|at| Path::new(&l[at..])))
                    .any(below)
            });
        if uses {
            let cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
            let mut command = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            command = command.trim().to_string();
            if command.is_empty() {
                // It cleared its command line.
                command = fs::read_to_string(dir.join("comm")).unwrap_or_default();
                command = format!("[{}]", command.trim());
            }
            if command.chars().count() > 80 {
                command = command.chars().take(77).collect::<String>() + "...";
            }
            users.push(User { pid, command });
        }
    }
    users
}

/// This process and its ancestors (its parent, the parent's parent, ...).
pub fn self_and_ancestors() -> Vec<u32> {
    let mut pid = std::process::id();
    let mut pids = vec![pid];
    while let Some(ppid) = parent(pid) {
        pids.push(ppid);
        pid = ppid;
    }
    pids
}

/// The parent of process `pid`, if it has one (and is visible here).
fn parent(pid: u32) -> Option<u32> {
    // pid (comm) state ppid ...; comm may contain spaces and parentheses.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let at = stat.rfind(')')?;
    let ppid = stat[at + 1..].split_whitespace().nth(1)?.parse().ok()?;
    (ppid > 0).then_some(ppid)
}
