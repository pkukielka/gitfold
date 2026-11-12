//! Background walk of a freshly opened snapshot or workspace through the mount,
//! so the kernel's dentry/inode cache (and optionally page cache) is populated
//! before the first agent command arrives. The walk runs in its own process
//! (`gitfold prewarm`, see `spawn`).

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// The git config key holding a branch's prewarm mode, as in
/// `branch.<name>.gitfoldPrewarm`: git keeps it with the branch (and deletes
/// it with it).
pub const BRANCH_KEY: &str = "gitfoldPrewarm";

/// The prewarm mode set for the workspace of `branch` (`git config
/// branch.<branch>.gitfoldPrewarm none|meta|content`), if any. Read when it
/// opens, so a change counts from the next open.
pub fn for_branch(common_dir: &Path, branch: &str) -> Option<Prewarm> {
    let key = format!("branch.{branch}.{BRANCH_KEY}");
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(["config", "--get", &key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout);
    Prewarm::from_str(value.trim(), true)
        .inspect_err(|_| warn!("{key}: {:?} is not none, meta or content", value.trim()))
        .ok()
}

/// A walk to run.
struct Walk {
    root: PathBuf,
    mode: Prewarm,
    threads: usize,
}

struct Walks {
    /// Walker processes running (pid, root), so they can be stopped before
    /// unmounting or removing what they walk.
    running: Vec<(u32, PathBuf)>,
    /// Walks waiting for one of `MAX_WALKS` to finish.
    queued: VecDeque<Walk>,
    /// Stopping began (unmounting): no new walk starts.
    stopped: bool,
}

static WALKS: Mutex<Walks> = Mutex::new(Walks {
    running: Vec::new(),
    queued: VecDeque::new(),
    stopped: false,
});

/// Threads per walk by default. A walk's requests are served by the daemon's
/// FUSE threads, so more of them only queue there: reported from a monorepo,
/// a request cost 16 µs with up to 4 in flight and 87-214 µs with 8-16.
pub const THREADS: usize = 4;

/// How many walks run at once (`gitfold mount --prewarm-walks`): each uses its
/// own thread pool, and walks of everything agents open at once would compete
/// for the CPUs.
static MAX_WALKS: AtomicUsize = AtomicUsize::new(2);

pub fn set_max_walks(n: usize) {
    MAX_WALKS.store(n, Ordering::Relaxed);
}

/// Walks `root` (see `run`) in a separate `gitfold prewarm` process, now or
/// once fewer than `MAX_WALKS` run; not if a walk of `root` is running or
/// queued already. Not in the daemon itself: a daemon killed with SIGKILL while
/// its own threads wait on requests to its mount never finishes dying, and the
/// mount hangs; a separate walker's requests just fail once the daemon is gone,
/// and it exits.
pub fn spawn(root: PathBuf, mode: Prewarm, threads: usize) {
    if mode == Prewarm::None {
        return;
    }
    let mut walks = WALKS.lock();
    let known = walks.running.iter().any(|(_, r)| *r == root)
        || walks.queued.iter().any(|w| w.root == root);
    if walks.stopped || known {
        return;
    }
    let running = walks.running.len();
    if running >= MAX_WALKS.load(Ordering::Relaxed) {
        info!(
            "prewarm of {}: queued behind {running} running",
            root.display()
        );
    }
    walks.queued.push_back(Walk {
        root,
        mode,
        threads,
    });
    start_queued(&mut walks);
}

/// Starts queued walks while fewer than `MAX_WALKS` run.
fn start_queued(walks: &mut Walks) {
    while walks.running.len() < MAX_WALKS.load(Ordering::Relaxed)
        && let Some(walk) = walks.queued.pop_front()
    {
        // This very binary, even if the file was replaced since (a rebuild
        // while mounted), under the name `stop_below` looks for.
        let mut command = Command::new("/proc/self/exe");
        command.arg0("gitfold");
        let mode_name = ValueEnum::to_possible_value(&walk.mode).expect("not skipped");
        command
            .arg("prewarm")
            .arg(&walk.root)
            .args(["--mode", mode_name.get_name()])
            .args(["--threads", &walk.threads.to_string()])
            .stdin(Stdio::null());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                warn!("prewarm of {}: {e}", walk.root.display());
                continue;
            }
        };
        info!("prewarm of {}: started", walk.root.display());
        let pid = child.id();
        walks.running.push((pid, walk.root));
        // Reaped here, so it doesn't stay a zombie; then the next one starts.
        std::thread::spawn(move || {
            let killed = child
                .wait()
                .is_ok_and(|s| s.signal() == Some(libc::SIGTERM));
            let mut walks = WALKS.lock();
            let ours = walks.running.iter().any(|(p, _)| *p == pid);
            walks.running.retain(|(p, _)| *p != pid);
            // Killed while still listed: not by `stop_under` or `stop_all`
            // (they unlist first), so by `gitfold unmount` (`stop_below`).
            // Another walk would keep the mount busy, and the unmount would
            // only detach it: the queued ones are dropped (later opens are
            // walked as usual, if the mount stays).
            if killed && ours {
                warn!(
                    "a prewarm walker was stopped from outside (an unmount?): dropping queued walks"
                );
                walks.queued.clear();
            }
            if !walks.stopped {
                start_queued(&mut walks);
            }
        });
    }
}

/// The pids of this daemon's running walkers.
pub fn walker_pids() -> Vec<u32> {
    WALKS.lock().running.iter().map(|(p, _)| *p).collect()
}

/// Stops the running walkers, and any later one from starting: they keep the
/// mount busy, which would make an unmount only detach it.
pub fn stop_all() {
    let pids = {
        let mut walks = WALKS.lock();
        walks.stopped = true;
        walks.queued.clear();
        std::mem::take(&mut walks.running)
    };
    terminate(pids.iter().map(|(p, _)| *p as libc::pid_t).collect());
}

/// Lets walks start again after `stop_all` (an unmount that failed).
pub fn resume() {
    WALKS.lock().stopped = false;
}

/// Stops this daemon's walks of `path` or below, running or queued (a
/// workspace or snapshot being removed); returns whether there were any.
pub fn stop_under(path: &Path) -> bool {
    let (pids, dropped) = {
        let mut walks = WALKS.lock();
        let queued = walks.queued.len();
        walks.queued.retain(|w| !w.root.starts_with(path));
        let dropped = walks.queued.len() != queued;
        // Unlisted first, so their reaper doesn't take it for an unmount (and
        // meanwhile, while they die, another walk may start).
        let (under, others) = std::mem::take(&mut walks.running)
            .into_iter()
            .partition(|(_, root)| root.starts_with(path));
        walks.running = others;
        let pids: Vec<_> = (under.into_iter())
            .map(|(p, _): (u32, PathBuf)| p as libc::pid_t)
            .collect();
        (pids, dropped)
    };
    let stopped = dropped || !pids.is_empty();
    terminate(pids);
    stopped
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
