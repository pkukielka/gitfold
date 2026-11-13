//! Background walk of a freshly opened snapshot or workspace through the mount,
//! so the kernel's dentry/inode cache (and optionally page cache) is populated
//! before the first agent command arrives. The walk runs in its own process
//! (`gitfold prewarm-walk`, see `spawn`).

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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

/// How a freshly opened snapshot or workspace is walked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Prewarm {
    None,
    /// List every directory (readdirplus instantiates every inode).
    Meta,
    /// Also read every file into the page cache (costs RAM equal to tree size).
    Content,
}

/// Its name on the command line and in git config, as clap parses it.
impl std::fmt::Display for Prewarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.to_possible_value().expect("not skipped").get_name())
    }
}

#[derive(Default)]
struct Stats {
    dirs: AtomicU64,
    files: AtomicU64,
    bytes: AtomicU64,
}

/// A walker process. Signalled through its pidfd, which, unlike its pid, never
/// names another process once it is reaped (by its reaper, at any time).
struct Walker {
    pid: u32,
    /// None where the kernel has no pidfds (before 5.3): then by pid.
    pidfd: Option<OwnedFd>,
}

impl Walker {
    fn of(pid: u32) -> Walker {
        // SAFETY: pidfd_open returns a new fd, or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        let pidfd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as RawFd) });
        Walker { pid, pidfd }
    }

    /// Sends it SIGTERM: through its pidfd, by pid where that can't be done
    /// (no pidfd, or a policy refusing the call), not once it is gone.
    fn terminate(&self) {
        if let Some(fd) = &self.pidfd {
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            } == 0;
            // Gone already: its pid may name another process now.
            let esrch = std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if sent || esrch || self.gone() {
                return;
            }
        }
        unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGTERM) };
    }

    /// Whether it exited (a pidfd turns readable then).
    fn gone(&self) -> bool {
        match &self.pidfd {
            Some(fd) => {
                let mut poll = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // Not on an error (EINTR): asked again.
                (unsafe { libc::poll(&mut poll, 1, 0) }) > 0
            }
            None => (unsafe { libc::kill(self.pid as libc::pid_t, 0) }) != 0,
        }
    }
}

/// A walk to run.
#[derive(Clone)]
struct Walk {
    root: PathBuf,
    mode: Prewarm,
    threads: usize,
}

/// A walk whose process is being started. A cancelled one still takes a slot
/// until `launch` stopped its process; a new walk of its root may start
/// meanwhile (briefly two walkers of one root, the first being stopped).
struct Starting {
    /// Tells it from a later start of the same root (once it was cancelled).
    id: u64,
    walk: Walk,
    /// Stopped meanwhile (`stop_under`, `stop_all`): `launch` stops the process
    /// once it exists, and drops this, which they wait for.
    cancelled: bool,
}

struct Walks {
    /// Walker processes running, by `Starting::id` (not by pid: a reaped
    /// walker's pid may be a newer walker's), so they can be stopped before
    /// unmounting or removing what they walk.
    running: Vec<(u64, Walker, Walk)>,
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
    /// Walks started so far, for `Starting::id`.
    starts: u64,
}

static WALKS: Mutex<Walks> = Mutex::new(Walks {
    running: Vec::new(),
    starting: Vec::new(),
    queued: VecDeque::new(),
    stopped: false,
    paused: Vec::new(),
    stops: 0,
    starts: 0,
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
/// unmount; not if a walk of `root` is known already. Not in the daemon itself:
/// a daemon killed with SIGKILL while its own threads wait on requests to its
/// mount never finishes dying, and the mount hangs; a separate walker's
/// requests just fail once the daemon is gone, and it exits.
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
    /// Nothing: one of that root is running, starting (still wanted), queued
    /// or kept already (or the mode is `none`).
    Known,
}

impl Walks {
    /// Whether a walk of `root` is running, starting (and still wanted), queued
    /// or kept.
    fn known(&self, root: &Path) -> bool {
        self.running.iter().any(|(_, _, w)| w.root == root)
            || self
                .starting
                .iter()
                .any(|s| !s.cancelled && s.walk.root == root)
            || self.queued.iter().any(|w| w.root == root)
            || self.paused.iter().any(|w| w.root == root)
    }

    /// `stop_all`'s bookkeeping: stops and unlists everything, keeping for
    /// `resume` what was wanted (not a walk `stop_under` cancelled while it
    /// started: it walks something being removed). Returns the walkers to
    /// stop.
    fn stop(&mut self) -> Vec<Walker> {
        self.stopped = true;
        self.stops += 1;
        for s in self.starting.iter_mut() {
            if !s.cancelled {
                self.paused.push(s.walk.clone());
            }
            s.cancelled = true;
        }
        let walkers = (self.running.drain(..))
            .map(|(_, walker, w)| {
                self.paused.push(w);
                walker
            })
            .collect();
        self.paused.extend(self.queued.drain(..));
        walkers
    }
}

/// Takes the queued walks that may start (fewer than `MAX_WALKS` running or
/// starting; none while stopped), marked as starting, with their `Starting::id`.
fn next_walks(walks: &mut Walks) -> Vec<(u64, Walk)> {
    let mut next = Vec::new();
    while !walks.stopped
        && walks.running.len() + walks.starting.len() < MAX_WALKS.load(Ordering::Relaxed)
        && let Some(walk) = walks.queued.pop_front()
    {
        walks.starts += 1;
        let id = walks.starts;
        walks.starting.push(Starting {
            id,
            walk: walk.clone(),
            cancelled: false,
        });
        next.push((id, walk));
    }
    next
}

/// Starts walker processes for `walks` (from `next_walks`), without holding
/// the lock while spawning; one no longer wanted meanwhile (stopped by a
/// removal or an unmount) is stopped again at once.
fn launch(walks: Vec<(u64, Walk)>) {
    // A loop, not recursion, over the walks whose slot frees up: spawns that
    // keep failing (process limits) go through a long queue one by one.
    let mut pending = VecDeque::from(walks);
    while let Some((id, walk)) = pending.pop_front() {
        let wanted = |state: &Walks| {
            let at = state.starting.iter().position(|s| s.id == id);
            at.is_some_and(|at| !state.starting[at].cancelled) && !state.stopped
        };
        {
            // Stopped before its process is made: none is.
            let mut state = WALKS.lock();
            if !wanted(&state) {
                state.starting.retain(|s| s.id != id);
                pending.extend(next_walks(&mut state));
                continue;
            }
        }
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
        let child = match spawned {
            Ok(child) if wanted(&state) => Some(child),
            Ok(mut child) => {
                // Stopped meanwhile: stop it now, unlocked.
                drop(state);
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                let _ = child.wait();
                state = WALKS.lock();
                None
            }
            Err(e) => {
                warn!("prewarm of {}: {e}", walk.root.display());
                None
            }
        };
        // Dropped from `starting` once it can no longer use the mount: the
        // stoppers wait for that.
        if let Some(at) = state.starting.iter().position(|s| s.id == id) {
            state.starting.remove(at);
        }
        let Some(child) = child else {
            // Its slot is free: the next queued walk may start.
            pending.extend(next_walks(&mut state));
            continue;
        };
        let pid = child.id();
        info!("prewarm of {}: started", walk.root.display());
        let walker = Walker::of(pid);
        static WARNED: std::sync::Once = std::sync::Once::new();
        if walker.pidfd.is_none() {
            let e = std::io::Error::last_os_error();
            WARNED.call_once(|| warn!("prewarm walkers: no pidfd ({e}); stopped by pid"));
        }
        state.running.push((id, walker, walk));
        drop(state);
        // Reaped on a thread of its own, so it doesn't stay a zombie; then the
        // next one starts. Without a thread (the system refuses one: process
        // limits, memory), it is stopped now, rather than panicking in a FUSE
        // request or the control thread and leaving its slot taken.
        let reaper = std::thread::Builder::new()
            .name("prewarm reaper".into())
            .spawn(move || reap(child, id));
        if let Err(e) = reaper {
            warn!("prewarm walker {pid}: no thread to reap it ({e}); stopping it");
            let pid = pid as libc::pid_t;
            unsafe { libc::kill(pid, libc::SIGTERM) };
            while unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) } < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
            }
            // The next ones would most likely fail the same way: neither the
            // queued ones nor those taken to start here are started.
            let mut walks = WALKS.lock();
            walks.running.retain(|(i, _, _)| *i != id);
            let taken = pending.len();
            for (taken, _) in pending.drain(..) {
                walks.starting.retain(|s| s.id != taken);
            }
            let dropped = taken + walks.queued.len();
            if dropped > 0 {
                warn!("prewarm: {dropped} walks dropped");
                walks.queued.clear();
            }
        }
    }
}

/// Waits for walker `child` (start `id`) to exit, unlists it, and starts the next
/// queued walks.
fn reap(mut child: std::process::Child, id: u64) {
    let killed = child
        .wait()
        .is_ok_and(|s| s.signal() == Some(libc::SIGTERM));
    let next = {
        let mut walks = WALKS.lock();
        let ours = walks.running.iter().any(|(i, _, _)| *i == id);
        walks.running.retain(|(i, _, _)| *i != id);
        // Killed while still listed: not by `stop_under` or `stop_all` (they
        // unlist first), so by `gitfold unmount` (`stop_below`). Another walk
        // would keep the mount busy, and the unmount would only detach it: the
        // queued ones are dropped (later opens are walked as usual, if the
        // mount stays).
        if killed && ours {
            warn!("a prewarm walker was stopped from outside (an unmount?): dropping queued walks");
            walks.queued.clear();
            // Being started: stopped by `launch` once their process exists.
            for s in walks.starting.iter_mut() {
                s.cancelled = true;
            }
        }
        next_walks(&mut walks)
    };
    launch(next);
}

/// Stops the running walkers, and any later one from starting: they keep the
/// mount busy, which would make an unmount only detach it. What it stops is
/// kept for `resume`. Returns this stop's number (see `stop_for_unmount`).
pub fn stop_all() -> u64 {
    let (stopped, stop) = {
        let mut walks = WALKS.lock();
        (walks.stop(), walks.stops)
    };
    terminate(&stopped);
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

/// Waits (up to 2 s) until no cancelled walk whose root `matches` is being
/// started: `launch` stops those once their process exists.
fn wait_for_starting(matches: impl Fn(&Path) -> bool) {
    let started = Instant::now();
    while WALKS
        .lock()
        .starting
        .iter()
        .any(|s| s.cancelled && matches(&s.walk.root))
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
fn restart(walks: &mut Walks) -> Vec<(u64, Walk)> {
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
    let (walkers, dropped) = {
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
            .filter(|s| !s.cancelled && s.walk.root.starts_with(path))
        {
            s.cancelled = true;
            dropped = true;
        }
        // Unlisted first, so their reaper doesn't take it for an unmount (and
        // meanwhile, while they die, another walk may start).
        let walkers: Vec<_> = (walks.running)
            .extract_if(.., |(_, _, w)| w.root.starts_with(path))
            .map(|(_, walker, _)| walker)
            .collect();
        (walkers, dropped)
    };
    let stopped = dropped || !walkers.is_empty();
    terminate(&walkers);
    wait_for_starting(|root| root.starts_with(path));
    stopped
}

/// Stops the walkers of this user working below `mountpoint` (`gitfold
/// prewarm-walk <root>` processes of the same binary name, or `prewarm <root>`
/// from older daemons), for `gitfold unmount`.
pub fn stop_below(mountpoint: &Path) {
    let mut walkers = Vec::new();
    let is_walker = |dir: &Path| {
        let Ok(cmdline) = fs::read(dir.join("cmdline")) else {
            return false;
        };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let is_gitfold = args.first().is_some_and(|argv0| {
            Path::new(OsStr::from_bytes(argv0)).file_name() == Some(OsStr::new("gitfold"))
        });
        let below = args
            .get(2)
            .is_some_and(|root| Path::new(OsStr::from_bytes(root)).starts_with(mountpoint));
        let walker = matches!(args.get(1), Some(&b"prewarm-walk" | &b"prewarm"));
        is_gitfold && walker && below
    };
    for (pid, dir) in crate::procs::own() {
        // Checked again once its pidfd is open: if the pid was reused
        // meanwhile, the second check sees the process the pidfd names.
        if is_walker(&dir) {
            let candidate = Walker::of(pid);
            if is_walker(&dir) {
                walkers.push(candidate);
            }
        }
    }
    terminate(&walkers);
}

/// SIGTERMs `walkers` and waits (up to 2 s) until they are gone.
fn terminate(walkers: &[Walker]) {
    for w in walkers {
        w.terminate();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let left: Vec<&Walker> = walkers.iter().filter(|w| !w.gone()).collect();
        let now = Instant::now();
        if left.is_empty() || now >= deadline {
            return;
        }
        // Pidfds turn readable on exit: waited on together; by pid, polled.
        let mut fds: Vec<libc::pollfd> = (left.iter())
            .filter_map(|w| w.pidfd.as_ref())
            .map(|fd| libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let mut wait = deadline - now;
        if fds.len() < left.len() {
            wait = wait.min(Duration::from_millis(20));
        }
        match fds.is_empty() {
            true => std::thread::sleep(wait),
            false => unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    wait.as_millis().max(1) as i32,
                );
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(root: &str) -> Walk {
        Walk {
            root: PathBuf::from(root),
            mode: Prewarm::Meta,
            threads: 1,
        }
    }

    /// A walker is signalled through its pidfd, and known to be gone once it
    /// exits; signalling it again after it was reaped reaches nothing.
    #[test]
    fn walker_signalled_through_its_pidfd() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let walker = Walker::of(child.id());
        if walker.pidfd.is_none() {
            eprintln!("no pidfds here (kernel or sandbox): skipped");
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        assert!(!walker.gone());
        terminate(std::slice::from_ref(&walker));
        assert!(walker.gone());
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
        // Reaped: its pid may name another process now; the pidfd doesn't.
        walker.terminate();
        assert!(walker.gone());
    }

    /// A stop keeps for `resume` what it stops, but not a walk being started
    /// that `stop_under` cancelled (it walks something being removed).
    #[test]
    fn stop_keeps_only_wanted_walks() {
        let mut walks = Walks {
            running: vec![(
                3,
                Walker {
                    pid: 7,
                    pidfd: None,
                },
                walk("/m/run"),
            )],
            starting: vec![
                Starting {
                    id: 1,
                    walk: walk("/m/start"),
                    cancelled: false,
                },
                Starting {
                    id: 2,
                    walk: walk("/m/removed"),
                    cancelled: true,
                },
            ],
            queued: VecDeque::from([walk("/m/queued")]),
            stopped: false,
            paused: Vec::new(),
            stops: 0,
            starts: 3,
        };
        let stopped: Vec<_> = walks.stop().iter().map(|w| w.pid).collect();
        assert_eq!(stopped, [7]);
        let kept: Vec<_> = walks.paused.iter().map(|w| w.root.clone()).collect();
        assert_eq!(kept, ["/m/start", "/m/run", "/m/queued"].map(PathBuf::from));
        assert!(walks.stopped && walks.running.is_empty() && walks.queued.is_empty());
        assert!(walks.starting.iter().all(|s| s.cancelled));
        // Resumed while the cancelled starts are still being stopped: what
        // was kept is queued again, not taken for walks already known.
        let next = restart(&mut walks);
        let mut back: Vec<_> = next
            .iter()
            .map(|(_, w)| &w.root)
            .chain(walks.queued.iter().map(|w| &w.root))
            .cloned()
            .collect();
        back.sort();
        assert_eq!(back, ["/m/queued", "/m/run", "/m/start"].map(PathBuf::from));
    }
}
