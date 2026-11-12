//! Partial clones (`git clone --filter=blob:none`): fetching the objects a
//! tree load found missing (`git::Missing`) from the promisor remote, as git's
//! own lazy fetch does. gitoxide has no lazy fetch.
//!
//! - **One place:** `Store::load_tree` goes through `load_tree` here, so every
//!   load fetches what it misses (snapshot and workspace opens, the sync
//!   thread, status), and a commit's missing blobs arrive in one fetch, at open.
//! - **Per-tree locks, no global one:** loads of the same tree wait for its
//!   fetch, then find the objects; loads of other trees fetch in parallel (each
//!   `git fetch` writes its own pack and updates no refs). Callers load before
//!   taking a creation lock, so a fetch doesn't hold up other opens.
//! - **Backoff:** a tree whose fetch failed is refused for `BACKOFF`, checked
//!   before loading (the sync thread retries every second); it lifts early once
//!   the missing objects are there (fetched by hand, say).
//! - **The fetch:** what git's lazy fetch runs (promisor-remote.c), stopped if
//!   it makes no progress for `STALL` (no total limit: a large fetch that makes
//!   progress is let finish), with its whole process group (ssh, index-pack).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, LazyLock, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use gix::ObjectId;
use log::info;
use parking_lot::Mutex;

use crate::git;
use crate::store::Store;

/// How long a tree whose fetch failed is refused.
const BACKOFF: Duration = Duration::from_secs(30);
/// How long a fetch may print nothing before it is stopped
/// (`GITFOLD_FETCH_STALL_SECS` overrides it, for tests).
const STALL: Duration = Duration::from_secs(120);
/// Fetch rounds per load: a fetched tree can reveal objects missing below it.
const ROUNDS: usize = 8;
/// How much of a failed fetch's stderr is kept for its error.
const STDERR_TAIL: usize = 4096;

/// Loads tree `tree` into `store` (`Store::load_tree_local`), fetching what is
/// missing from the promisor remote, if there is one.
pub fn load_tree(store: &Store, tree: ObjectId) -> Result<u32> {
    refuse_in_backoff(tree)?;
    let generation = GENERATION.load(Ordering::Acquire);
    let mut missing = match local(store, tree)? {
        Ok(id) => return Ok(id),
        Err(missing) if remote().is_none() => return Err(git::Missing(missing).into()),
        Err(missing) => missing,
    };
    let lock = tree_lock(tree);
    let _fetching = lock.lock();
    // A fetch of this tree that failed while we waited: no other one.
    refuse_in_backoff(tree)?;
    // Another fetch (of this tree, say) may have brought them meanwhile.
    if GENERATION.load(Ordering::Acquire) != generation {
        missing = match local(store, tree)? {
            Ok(id) => return Ok(id),
            Err(missing) => missing,
        };
    }
    let result = fetch_rounds(store, tree, &mut missing);
    if result.is_err() {
        let mut failures = FAILURES.lock();
        failures.retain(|_, f| f.at.elapsed() < BACKOFF);
        failures.insert(
            tree,
            Failure {
                at: Instant::now(),
                missing,
            },
        );
    }
    result
}

/// Fetches `missing` and loads `tree` again, until it loads.
fn fetch_rounds(store: &Store, tree: ObjectId, missing: &mut Vec<ObjectId>) -> Result<u32> {
    for _ in 0..ROUNDS {
        fetch(missing)?;
        match local(store, tree)? {
            Ok(id) => return Ok(id),
            // Not delivered after all.
            Err(again) if again == *missing => {
                let remote = remote().unwrap_or_default();
                bail!(
                    "{} objects still missing after fetching them from {remote}",
                    again.len()
                );
            }
            // Revealed by fetched trees.
            Err(again) => *missing = again,
        }
    }
    bail!("objects of tree {tree} still missing after {ROUNDS} fetches")
}

/// `store.load_tree_local(tree)`: the tree, or what is missing (sorted).
fn local(store: &Store, tree: ObjectId) -> Result<Result<u32, Vec<ObjectId>>> {
    match store.load_tree_local(tree) {
        Ok(id) => Ok(Ok(id)),
        Err(e) => {
            let mut missing = e.downcast::<git::Missing>()?.0;
            missing.sort_unstable();
            Ok(Err(missing))
        }
    }
}

/// A load refused because the fetch of its tree failed recently. Callers log
/// it at debug level: the failure itself was reported once.
#[derive(Debug)]
pub struct Backoff(ObjectId);

impl std::fmt::Display for Backoff {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let secs = BACKOFF.as_secs();
        write!(
            f,
            "objects of tree {} are missing, and fetching them failed (retried within {secs} s)",
            self.0
        )
    }
}

impl std::error::Error for Backoff {}

/// Whether `e` is a `Backoff` refusal.
pub fn is_backoff(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Backoff>())
}

/// A tree's fetch that failed: when, and what it fetched.
struct Failure {
    at: Instant,
    missing: Vec<ObjectId>,
}

static FAILURES: LazyLock<Mutex<HashMap<ObjectId, Failure>>> = LazyLock::new(Default::default);

/// Fails with `Backoff` if `tree`'s fetch failed less than `BACKOFF` ago and
/// what it fetched is still missing.
fn refuse_in_backoff(tree: ObjectId) -> Result<()> {
    let mut failures = FAILURES.lock();
    let Some(failure) = failures.get(&tree) else {
        return Ok(());
    };
    if failure.at.elapsed() < BACKOFF && failure.missing.iter().any(|id| git::is_missing(*id)) {
        return Err(Backoff(tree).into());
    }
    failures.remove(&tree);
    Ok(())
}

/// Bumped by every fetch that brought objects.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Locks of trees being fetched, removed once no one holds them.
static FETCHING: LazyLock<Mutex<HashMap<ObjectId, Arc<Mutex<()>>>>> =
    LazyLock::new(Default::default);

fn tree_lock(tree: ObjectId) -> Arc<Mutex<()>> {
    let mut fetching = FETCHING.lock();
    fetching.retain(|_, lock| Arc::strong_count(lock) > 1);
    fetching.entry(tree).or_default().clone()
}

/// The promisor remote, as git picks it: the first remote, in config file
/// order, with `promisor = true` or a `partialclonefilter`, the one
/// `extensions.partialClone` names (repositoryformatversion 1; clones set it
/// before the remote keys) last. Read once.
pub fn remote() -> Option<String> {
    static REMOTE: OnceLock<Option<String>> = OnceLock::new();
    REMOTE
        .get_or_init(|| {
            git::with_repo(|repo| {
                let config = repo.config_snapshot();
                let config = config.plumbing();
                // Only with repositoryformatversion 1, and tried last.
                let extension =
                    matches!(config.integer("core.repositoryformatversion"), Ok(Some(1)))
                        .then(|| config.string("extensions.partialClone"))
                        .flatten()
                        .map(|v| v.to_string());
                let mut flagged = config
                    .sections_by_name("remote")
                    .into_iter()
                    .flatten()
                    .filter_map(|s| {
                        let name = s.header().subsection_name()?.to_string();
                        // `promisor` alone (no `= value`) is true too.
                        let promisor = match s.value_implicit("promisor") {
                            Some(None) => true,
                            Some(Some(v)) => gix::config::Boolean::try_from(v).is_ok_and(|b| b.0),
                            None => false,
                        };
                        (promisor || s.value("partialclonefilter").is_some()).then_some(name)
                    });
                flagged
                    .find(|name| Some(name) != extension.as_ref())
                    .or(extension)
            })
        })
        .clone()
}

/// Fetches objects `ids` from the promisor remote.
fn fetch(ids: &[ObjectId]) -> Result<()> {
    // With nothing on stdin, git would fetch the remote's refspecs instead:
    // everything (a full-history download).
    if ids.is_empty() {
        bail!("nothing to fetch");
    }
    let remote = remote().ok_or_else(|| anyhow!("no promisor remote to fetch from"))?;
    let started = Instant::now();
    let git_dir = git::with_repo(|repo| repo.common_dir().to_path_buf());
    // What git's own lazy fetch runs (promisor-remote.c, `fetch_objects`):
    // blob:none whatever the clone's filter (a wanted tree comes with all its
    // trees, a wanted blob despite the filter), no negotiation.
    let args = [
        "-c",
        "fetch.negotiationAlgorithm=noop",
        "fetch",
        &remote,
        "--no-tags",
        "--no-write-fetch-head",
        "--recurse-submodules=no",
        // A repack (gc --auto) would run inside the stall window, silently.
        "--no-auto-maintenance",
        "--filter=blob:none",
        "--stdin",
        "--progress",
    ];
    let failed = |why: String| {
        anyhow!(
            "fetching {} missing objects from {remote}: `git {}` {why}",
            ids.len(),
            args[2..].join(" ")
        )
    };
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(&git_dir)
        .args(args)
        // No terminal to ask on: fail rather than wait for a password.
        .env("GIT_TERMINAL_PROMPT", "0")
        // Messages as written, for the error (its `fatal:` line).
        .env("LC_ALL", "C")
        // A system-wide trace would log every fetch, and its index-pack.
        .env_remove("GIT_TRACE2_EVENT")
        .env_remove("GIT_TRACE2")
        .env_remove("GIT_TRACE2_PERF")
        .env_remove("GIT_TRACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The daemon (no terminal) runs it in its own group, so a stall stops ssh,
    // index-pack and the rest too. From a terminal (`gitfold list`), it stays in
    // the foreground group, where ssh can ask for a passphrase.
    let group = std::fs::File::open("/dev/tty").is_err();
    if group {
        // Nor any program to ask, like a desktop's ssh-askpass, which would
        // block it until the stall limit.
        command
            .process_group(0)
            .env("GIT_ASKPASS", "true")
            .env("SSH_ASKPASS_REQUIRE", "never");
    }
    let mut child = command.spawn()?;
    let pid = child.id() as libc::pid_t;
    let stop = |signal| unsafe { libc::kill(if group { -pid } else { pid }, signal) };
    let mut wanted = String::with_capacity(41 * ids.len());
    for id in ids {
        let _ = writeln!(wanted, "{id}");
    }
    // Any stderr output is progress; its tail is kept for the error.
    let (progress, made) = mpsc::channel();
    let tail = Arc::new(Mutex::new(Vec::new()));
    let (mut stdin, mut stderr) = (
        child.stdin.take().expect("piped"),
        child.stderr.take().expect("piped"),
    );
    let threads = (|| -> std::io::Result<()> {
        // A git that never reads must not block us.
        thread::Builder::new().spawn(move || stdin.write_all(wanted.as_bytes()))?;
        let tail = tail.clone();
        thread::Builder::new().spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut buf) {
                let mut tail = tail.lock();
                tail.extend_from_slice(&buf[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
                let _ = progress.send(());
            }
        })?;
        Ok(())
    })();
    if let Err(e) = threads {
        stop(libc::SIGKILL);
        child.wait()?;
        return Err(failed(format!("couldn't be watched: {e}")));
    }
    let stall = std::env::var("GITFOLD_FETCH_STALL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(STALL, Duration::from_secs);
    let (mut last, mut stderr_open) = (Instant::now(), true);
    // Git is reaped only at the end (`child.wait()`), after any signal to its
    // group: until then its id, which names the group, can't be reused.
    loop {
        // Exited, even if something it started still holds stderr open.
        if exited(pid)? {
            break;
        }
        if last.elapsed() > stall {
            // SIGTERM first: git removes the lock and .keep files it holds.
            stop(libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !exited(pid)? && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            stop(libc::SIGKILL);
            child.wait()?;
            return Err(failed(format!(
                "made no progress for {} s",
                stall.as_secs()
            )));
        }
        match stderr_open {
            true => match made.recv_timeout(Duration::from_millis(200)) {
                Ok(()) => last = Instant::now(),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => stderr_open = false,
            },
            false => thread::sleep(Duration::from_millis(20)),
        }
    }
    // A helper still holding stderr (it would keep the reader thread blocked)
    // goes with git, once the reader had a moment to see stderr close. One that
    // let go of it is meant to outlive git (a credential cache daemon) and is
    // left alone.
    let deadline = Instant::now() + Duration::from_millis(200);
    while stderr_open && Instant::now() < deadline {
        if let Err(RecvTimeoutError::Disconnected) = made.recv_timeout(Duration::from_millis(20)) {
            stderr_open = false;
        }
    }
    if group && stderr_open {
        stop(libc::SIGTERM);
    }
    let status = child.wait()?;
    if !status.success() {
        // git's last `fatal:` (or `error:`) line says why; else its last line.
        let tail = String::from_utf8_lossy(&tail.lock()).into_owned();
        let lines = || tail.lines().rev().map(str::trim).filter(|l| !l.is_empty());
        let why = |l: &&str| l.starts_with("fatal:") || l.starts_with("error:");
        let last = lines().find(why).or_else(|| lines().next()).unwrap_or("");
        return Err(failed(format!("ended with {status}: {}", last.trim())));
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
    info!(
        "fetched {} missing objects from {remote} in {:.1?}",
        ids.len(),
        started.elapsed()
    );
    Ok(())
}

/// Whether child `pid` has exited, leaving it unreaped (`WNOWAIT`): its id
/// still names its process group, which can then still be signalled safely.
fn exited(pid: libc::pid_t) -> std::io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // WNOHANG leaves si_pid 0 while it runs.
    Ok(unsafe { info.si_pid() } != 0)
}
