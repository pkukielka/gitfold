//! Background walk of a freshly opened snapshot or workspace through the mount,
//! so the kernel's dentry/inode cache (and optionally page cache) is populated
//! before the first agent command arrives. The walk runs in its own process
//! (`gitfold prewarm-walk`, see `spawn`).

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

use crate::settings;
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

/// Its name on the command line and in git config.
impl std::fmt::Display for Prewarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Prewarm::None => "none",
            Prewarm::Meta => "meta",
            Prewarm::Content => "content",
        })
    }
}

#[derive(Default)]
struct Stats {
    dirs: AtomicU64,
    files: AtomicU64,
    bytes: AtomicU64,
}

/// The prewarm mode set for the workspace of `branch` (`git config
/// branch.<branch>.gitfoldPrewarm none|meta|content`), if any, in the
/// repository's config file (not included ones). Read when it opens, so a
/// change counts from the next open.
pub fn for_branch(common_dir: &Path, branch: &str) -> Option<Prewarm> {
    let value = settings::branch(common_dir, branch, settings::PREWARM)?;
    Prewarm::from_str(&value, true)
        .inspect_err(|_| {
            let key = settings::branch_key(branch, settings::PREWARM);
            warn!("{key}: {value:?} is not none, meta or content")
        })
        .ok()
}

/// A walk to run.
#[derive(Clone)]
struct Walk {
    root: PathBuf,
    mode: Prewarm,
    threads: usize,
}

/// A walk whose process is being started.
struct Starting {
    walk: Walk,
    /// Stopped meanwhile (`stop_under`, `stop_all`): `launch` stops the process
    /// once it exists, and drops this, which they wait for.
    cancelled: bool,
}

struct Walks {
    /// Walker processes running (pid, walk), so they can be stopped before
    /// unmounting or removing what they walk.
    running: Vec<(u32, Walk)>,
    /// Walks whose process is being started (see `launch`).
    starting: Vec<Starting>,
    /// Walks waiting for one of `MAX_WALKS` to finish.
    queued: VecDeque<Walk>,
    /// Stopping began (unmounting): no new walk starts.
    stopped: bool,
    /// The walks `stop_all` stopped or dropped, started again by `resume`.
    paused: Vec<Walk>,
    /// Calls of `stop_all`, so a resume meant for one doesn't end a later one.
    stops: u64,
}

static WALKS: Mutex<Walks> = Mutex::new(Walks {
    running: Vec::new(),
    starting: Vec::new(),
    queued: VecDeque::new(),
    stopped: false,
    paused: Vec::new(),
    stops: 0,
});

/// Threads per walk by default. A walk's requests are served by the daemon's
/// FUSE threads, so more of them only queue there: reported from a monorepo,
/// a request cost under 20 µs with up to 4 in flight and several times that
/// with 8-16.
pub const THREADS: usize = 4;

/// How many walks run at once (`gitfold mount --prewarm-walks`): each uses its
/// own thread pool, and walks of everything agents open at once would compete
/// for the CPUs.
static MAX_WALKS: AtomicUsize = AtomicUsize::new(2);

pub fn set_max_walks(n: usize) {
    MAX_WALKS.store(n, Ordering::Relaxed);
}

/// Walks `root` (see `run`) in a separate `gitfold prewarm-walk` process, now
/// or once fewer than `MAX_WALKS` run, or once walks resume if stopped for an
/// unmount; not if a walk of `root` is known already. Not in the daemon itself: a daemon killed with SIGKILL while
/// its own threads wait on requests to its mount never finishes dying, and the
/// mount hangs; a separate walker's requests just fail once the daemon is gone,
/// and it exits.
pub fn spawn(root: PathBuf, mode: Prewarm, threads: usize) -> Spawned {
    if mode == Prewarm::None {
        return Spawned::Known;
    }
    let walk = Walk {
        root,
        mode,
        threads,
    };
    let (next, spawned) = {
        let mut walks = WALKS.lock();
        if walks.known(&walk.root) {
            return Spawned::Known;
        }
        if walks.stopped {
            walks.paused.push(walk);
            return Spawned::Paused;
        }
        let busy = walks.running.len() + walks.starting.len();
        let spawned = match busy >= MAX_WALKS.load(Ordering::Relaxed) {
            true => {
                info!(
                    "prewarm of {}: queued behind {busy} running",
                    walk.root.display()
                );
                Spawned::Queued
            }
            false => Spawned::Now,
        };
        walks.queued.push_back(walk);
        (next_walks(&mut walks), spawned)
    };
    launch(next);
    spawned
}

/// What `spawn` did with a walk.
pub enum Spawned {
    /// Started it.
    Now,
    /// Queued it behind `MAX_WALKS` running.
    Queued,
    /// Kept it for when walks resume (stopped for an unmount).
    Paused,
    /// Nothing: one of that root is running, queued or kept already (or the
    /// mode is `none`).
    Known,
}

impl Walks {
    /// Whether a walk of `root` is running, starting, queued or kept.
    fn known(&self, root: &Path) -> bool {
        self.running.iter().any(|(_, w)| w.root == root)
            || self.starting.iter().any(|s| s.walk.root == root)
            || self.queued.iter().any(|w| w.root == root)
            || self.paused.iter().any(|w| w.root == root)
    }
}

/// Takes the queued walks that may start (fewer than `MAX_WALKS` running or
/// starting), marked as starting.
fn next_walks(walks: &mut Walks) -> Vec<Walk> {
    let mut next = Vec::new();
    while walks.running.len() + walks.starting.len() < MAX_WALKS.load(Ordering::Relaxed)
        && let Some(walk) = walks.queued.pop_front()
    {
        walks.starting.push(Starting {
            walk: walk.clone(),
            cancelled: false,
        });
        next.push(walk);
    }
    next
}

/// Starts walker processes for `walks` (from `next_walks`), without holding
/// the lock while spawning; one no longer wanted meanwhile (stopped by a
/// removal or an unmount) is stopped again at once.
fn launch(walks: Vec<Walk>) {
    for walk in walks {
        // This very binary, even if the file was replaced since (a rebuild
        // while mounted), under the name `stop_below` looks for.
        let mut command = Command::new("/proc/self/exe");
        command.arg0("gitfold");
        command
            .arg("prewarm-walk")
            .arg(&walk.root)
            .args(["--mode", &walk.mode.to_string()])
            .args(["--threads", &walk.threads.to_string()])
            .stdin(Stdio::null());
        let spawned = command.spawn();
        let mut state = WALKS.lock();
        let at = state.starting.iter().position(|s| s.walk.root == walk.root);
        let wanted = at.is_some_and(|at| !state.starting[at].cancelled) && !state.stopped;
        // Dropped from `starting` once it can no longer use the mount: the
        // stoppers wait for that.
        let forget = |state: &mut Walks| {
            if let Some(at) = state.starting.iter().position(|s| s.walk.root == walk.root) {
                state.starting.remove(at);
            }
        };
        let child = match spawned {
            Ok(child) if wanted => {
                forget(&mut state);
                Some(child)
            }
            Ok(mut child) => {
                // Stopped meanwhile: stop it now, unlocked.
                drop(state);
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                let _ = child.wait();
                state = WALKS.lock();
                forget(&mut state);
                None
            }
            Err(e) => {
                warn!("prewarm of {}: {e}", walk.root.display());
                forget(&mut state);
                None
            }
        };
        let Some(child) = child else {
            // Its slot is free: the next queued walk may start.
            let next = next_walks(&mut state);
            drop(state);
            launch(next);
            continue;
        };
        let pid = child.id();
        info!("prewarm of {}: started", walk.root.display());
        state.running.push((pid, walk));
        drop(state);
        // Reaped on a thread of its own, so it doesn't stay a zombie; then the
        // next one starts. Without a thread (the system refuses one: process
        // limits, memory), it is stopped now, rather than panicking in a FUSE
        // request or the control thread and leaving its slot taken.
        let reaper = std::thread::Builder::new()
            .name("prewarm reaper".into())
            .spawn(move || reap(child, pid));
        if let Err(e) = reaper {
            warn!("prewarm walker {pid}: no thread to reap it ({e}); stopping it");
            let pid = pid as libc::pid_t;
            unsafe { libc::kill(pid, libc::SIGTERM) };
            while unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) } < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
            }
            // The next ones would most likely fail the same way.
            let mut walks = WALKS.lock();
            walks.running.retain(|(p, _)| *p != pid as u32);
            walks.queued.clear();
        }
    }
}

/// Waits for walker `child` (`pid`) to exit, unlists it, and starts the next
/// queued walks.
fn reap(mut child: std::process::Child, pid: u32) {
    let killed = child
        .wait()
        .is_ok_and(|s| s.signal() == Some(libc::SIGTERM));
    let next = {
        let mut walks = WALKS.lock();
        let ours = walks.running.iter().any(|(p, _)| *p == pid);
        walks.running.retain(|(p, _)| *p != pid);
        // Killed while still listed: not by `stop_under` or `stop_all` (they
        // unlist first), so by `gitfold unmount` (`stop_below`). Another walk
        // would keep the mount busy, and the unmount would only detach it: the
        // queued ones are dropped (later opens are walked as usual, if the
        // mount stays).
        if killed && ours {
            warn!("a prewarm walker was stopped from outside (an unmount?): dropping queued walks");
            walks.queued.clear();
        }
        match walks.stopped {
            true => Vec::new(),
            false => next_walks(&mut walks),
        }
    };
    launch(next);
}

/// Stops the running walkers, and any later one from starting: they keep the
/// mount busy, which would make an unmount only detach it. What it stops is
/// kept for `resume`. Returns this stop's number (see `stop_for_unmount`).
pub fn stop_all() -> u64 {
    let (pids, stop) = {
        let mut walks = WALKS.lock();
        walks.stopped = true;
        walks.stops += 1;
        let Walks {
            running,
            starting,
            queued,
            paused,
            ..
        } = &mut *walks;
        for s in starting.iter_mut() {
            s.cancelled = true;
            paused.push(s.walk.clone());
        }
        let running = std::mem::take(running);
        paused.extend(running.iter().map(|(_, w)| w.clone()));
        paused.extend(queued.drain(..));
        (running, walks.stops)
    };
    terminate(pids.iter().map(|(p, _)| *p as libc::pid_t).collect());
    wait_for_starting(|_| true);
    stop
}

/// How long after `stop_for_unmount` the walks resume if the mount is still
/// there (`GITFOLD_PREWARM_RESUME_SECS` overrides it, for tests).
const RESUME_AFTER: Duration = Duration::from_secs(60);

/// `stop_all` for `gitfold unmount`, which unmounts next. If it never does (it
/// was interrupted), the walks resume once `mountpoint` is still mounted after
/// `RESUME_AFTER`, unless stopped again meanwhile.
pub fn stop_for_unmount(mountpoint: PathBuf) {
    // This mount, by device: one mounted at the same path later (after a lazy
    // unmount detached this one) has another, as does the bare directory.
    let device = |path: &Path| fs::metadata(path).map(|m| m.dev()).ok();
    let ours = device(&mountpoint);
    let stop = stop_all();
    let after = std::env::var("GITFOLD_PREWARM_RESUME_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(RESUME_AFTER, Duration::from_secs);
    let lease = std::thread::Builder::new()
        .name("prewarm lease".into())
        .spawn(move || {
            std::thread::sleep(after);
            if ours.is_none() || device(&mountpoint) != ours {
                return;
            }
            let mut walks = WALKS.lock();
            if walks.stopped && walks.stops == stop {
                let next = restart(&mut walks);
                drop(walks);
                info!(
                    "prewarm resumed: still mounted {} s after an unmount stopped it",
                    after.as_secs()
                );
                launch(next);
            }
        });
    if let Err(e) = lease {
        warn!("prewarm stays stopped until an unmount fails: no thread to resume it ({e})");
    }
}

/// Waits (up to 2 s) until no walk whose root `matches` is being started:
/// `launch` stops cancelled ones once their process exists.
fn wait_for_starting(matches: impl Fn(&Path) -> bool) {
    let started = Instant::now();
    while WALKS.lock().starting.iter().any(|s| matches(&s.walk.root))
        && started.elapsed() < Duration::from_secs(2)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Lets walks start again after `stop_all` (an unmount that failed), the ones
/// it stopped first.
pub fn resume() {
    let next = restart(&mut WALKS.lock());
    launch(next);
}

/// Ends a stop: queues the walks it stopped, and returns those that may start.
fn restart(walks: &mut Walks) -> Vec<Walk> {
    walks.stopped = false;
    for walk in std::mem::take(&mut walks.paused) {
        if !walks.known(&walk.root) {
            walks.queued.push_back(walk);
        }
    }
    next_walks(walks)
}

/// Stops this daemon's walks of `path` or below, running or queued (a
/// workspace or snapshot being removed); returns whether there were any.
pub fn stop_under(path: &Path) -> bool {
    let (pids, dropped) = {
        let mut walks = WALKS.lock();
        let queued = walks.queued.len();
        let paused = walks.paused.len();
        walks.queued.retain(|w| !w.root.starts_with(path));
        walks.paused.retain(|w| !w.root.starts_with(path));
        let mut dropped = walks.queued.len() != queued || walks.paused.len() != paused;
        // Stopped by `launch` once started; waited for below.
        for s in walks
            .starting
            .iter_mut()
            .filter(|s| s.walk.root.starts_with(path))
        {
            s.cancelled = true;
            dropped = true;
        }
        // Unlisted first, so their reaper doesn't take it for an unmount (and
        // meanwhile, while they die, another walk may start).
        let (under, others) = std::mem::take(&mut walks.running)
            .into_iter()
            .partition(|(_, w)| w.root.starts_with(path));
        walks.running = others;
        let pids: Vec<_> = (under.into_iter())
            .map(|(p, _): (u32, Walk)| p as libc::pid_t)
            .collect();
        (pids, dropped)
    };
    let stopped = dropped || !pids.is_empty();
    terminate(pids);
    wait_for_starting(|root| root.starts_with(path));
    stopped
}

/// Stops the walkers of this user working below `mountpoint` (`gitfold
/// prewarm-walk <root>` processes of the same binary name, or `prewarm <root>`
/// from older daemons), for `gitfold unmount`.
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
        let walker = matches!(args.get(1), Some(&b"prewarm-walk" | &b"prewarm"));
        if is_gitfold && walker && below {
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

/// `gitfold prewarm-walk`: walks `root` (see `visit`) and logs what it saw.
pub fn run(root: PathBuf, mode: Prewarm, threads: usize) -> anyhow::Result<()> {
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
