//! Background walk of a freshly opened snapshot or workspace through the mount,
//! so the kernel's dentry/inode cache (and optionally page cache) is populated
//! before the first agent command arrives. The walk runs in its own process
//! (`gitfold prewarm`, see `spawn`).

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use log::{info, warn};
use parking_lot::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Prewarm {
    None,
    /// List every directory (readdirplus instantiates every inode).
    Meta,
    /// Also read every file into the page cache (costs RAM equal to tree size).
    Content,
}

#[derive(Default)]
struct Stats {
    dirs: AtomicU64,
    files: AtomicU64,
    bytes: AtomicU64,
}

/// Walker processes running (pid, root), so they can be stopped before
/// unmounting; `None` once stopping began, so no new one starts.
static WALKERS: Mutex<Option<Vec<(u32, PathBuf)>>> = Mutex::new(Some(Vec::new()));

/// Starts the walk of `root` as a separate `gitfold prewarm` process. Not in
/// the daemon itself: a daemon killed with SIGKILL while its own threads wait
/// on requests to its mount never finishes dying, and the mount hangs; a
/// separate walker's requests just fail once the daemon is gone, and it exits.
pub fn spawn(root: PathBuf, mode: Prewarm, threads: usize) {
    if mode == Prewarm::None {
        return;
    }
    // This very binary, even if the file was replaced since (a rebuild while
    // mounted), under the name `stop_below` looks for.
    let mut command = Command::new("/proc/self/exe");
    command.arg0("gitfold");
    let mode_name = ValueEnum::to_possible_value(&mode).expect("not skipped");
    command
        .arg("prewarm")
        .arg(&root)
        .args(["--mode", mode_name.get_name()])
        .args(["--threads", &threads.to_string()])
        .stdin(Stdio::null());
    let mut walkers = WALKERS.lock();
    let Some(running) = walkers.as_mut() else {
        return; // unmounting
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return warn!("prewarm of {}: {e}", root.display()),
    };
    let pid = child.id();
    running.push((pid, root));
    drop(walkers);
    // Reaped here, so it doesn't stay a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
        if let Some(running) = WALKERS.lock().as_mut() {
            running.retain(|(p, _)| *p != pid);
        }
    });
}

/// Stops the running walkers, and any later one from starting: they keep the
/// mount busy, which would make an unmount only detach it.
pub fn stop_all() {
    let walkers = WALKERS.lock().take().unwrap_or_default();
    terminate(walkers.iter().map(|(p, _)| *p as libc::pid_t).collect());
}

/// Stops this daemon's walkers of `path` or below (a workspace or snapshot
/// being removed).
pub fn stop_under(path: &Path) {
    let pids = (WALKERS.lock().iter().flatten())
        .filter(|(_, root)| root.starts_with(path))
        .map(|(p, _)| *p as libc::pid_t)
        .collect();
    terminate(pids);
}

/// Stops the walkers of this user working below `mountpoint` (`gitfold
/// prewarm <root>` processes of the same binary name), for `gitfold unmount`.
pub fn stop_below(mountpoint: &Path) {
    let Ok(procs) = fs::read_dir("/proc") else {
        return;
    };
    let uid = unsafe { libc::getuid() };
    let mut pids = Vec::new();
    for entry in procs.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
            continue;
        };
        if entry.metadata().map_or(true, |m| m.uid() != uid) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let is_gitfold = args.first().is_some_and(|argv0| {
            Path::new(OsStr::from_bytes(argv0)).file_name() == Some(OsStr::new("gitfold"))
        });
        let below = args
            .get(2)
            .is_some_and(|root| Path::new(OsStr::from_bytes(root)).starts_with(mountpoint));
        if is_gitfold && args.get(1) == Some(&&b"prewarm"[..]) && below {
            pids.push(pid);
        }
    }
    terminate(pids);
}

/// SIGTERMs `pids` and waits (up to 2 s) until they are gone.
fn terminate(pids: Vec<libc::pid_t>) {
    for pid in &pids {
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    let alive = |pid: &libc::pid_t| unsafe { libc::kill(*pid, 0) } == 0;
    let started = Instant::now();
    while pids.iter().any(alive) && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `gitfold prewarm`: walks `root` (see `visit`) and logs what it saw.
pub fn run(root: PathBuf, mode: Prewarm, threads: usize) -> anyhow::Result<()> {
    // The daemon blocks SIGINT/SIGTERM/SIGHUP for its signal thread, and the
    // mask is inherited: unblock them, so the walker can be stopped.
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("prewarm-{i}"))
        .build()?;
    let stats = Stats::default();
    let started = Instant::now();
    pool.scope(|s| visit(s, root.clone(), mode, &stats));
    info!(
        "prewarm done for {}: {} dirs, {} files, {} MB in {:?}",
        root.display(),
        stats.dirs.load(Ordering::Relaxed),
        stats.files.load(Ordering::Relaxed),
        stats.bytes.load(Ordering::Relaxed) >> 20,
        started.elapsed()
    );
    Ok(())
}

fn visit<'s>(s: &rayon::Scope<'s>, dir: PathBuf, mode: Prewarm, stats: &'s Stats) {
    stats.dirs.fetch_add(1, Ordering::Relaxed);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            s.spawn(move |s| visit(s, entry.path(), mode, stats));
        } else {
            stats.files.fetch_add(1, Ordering::Relaxed);
            if mode == Prewarm::Content && ft.is_file() {
                let path = entry.path();
                stats.bytes.fetch_add(read_all(&path), Ordering::Relaxed);
                let _ = fs::symlink_metadata(&path); // see below
            }
        }
    }
    // On a writable mount the kernel marks atime stale after reading a directory
    // or file, so the next stat asks the daemon again. Stat here, in the
    // background, so the agent's first find or git status finds attributes cached
    // (chromium: first find 759 ms without this, 351 ms with).
    let _ = fs::symlink_metadata(&dir);
}

fn read_all(path: &Path) -> u64 {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; 1 << 20]);
    }
    let Ok(mut f) = fs::File::open(path) else {
        return 0;
    };
    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        let mut total = 0;
        while let Ok(n) = f.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    })
}
