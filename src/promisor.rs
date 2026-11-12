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
        Err(_) if remote().is_none() => return store.load_tree_local(tree),
        Err(missing) => missing,
    };
    let lock = tree_lock(tree);
    let _fetching = lock.lock();
    // Another fetch (of this tree, say) may have brought them meanwhile.
    if GENERATION.load(Ordering::Acquire) != generation {
        missing = match local(store, tree)? {
            Ok(id) => return Ok(id),
            Err(missing) => missing,
        };
    }
    let result = fetch_rounds(store, tree, &mut missing);
    if result.is_err() {
        FAILURES.lock().insert(
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
/// order, with `promisor = true` or a `partialclonefilter`, else the one
/// `extensions.partialClone` names (set by clones before the remote keys).
/// Read once.
pub fn remote() -> Option<String> {
    static REMOTE: OnceLock<Option<String>> = OnceLock::new();
    REMOTE
        .get_or_init(|| {
            git::with_repo(|repo| {
                let config = repo.config_snapshot();
                let config = config.plumbing();
                let promisor = config
                    .sections_by_name("remote")
                    .into_iter()
                    .flatten()
                    .find_map(|s| {
                        let name = s.header().subsection_name()?.to_string();
                        let flagged = s
                            .value("promisor")
                            .is_some_and(|v| gix::config::Boolean::try_from(v).is_ok_and(|b| b.0));
                        (flagged || s.value("partialclonefilter").is_some()).then_some(name)
                    });
                promisor.or_else(|| {
                    config
                        .string("extensions.partialClone")
                        .map(|v| v.to_string())
                })
            })
        })
        .clone()
}

/// Fetches objects `ids` from the promisor remote.
fn fetch(ids: &[ObjectId]) -> Result<()> {
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
    let mut child = Command::new("git")
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
        // Its own group, so a stall stops ssh, index-pack and the rest too.
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id() as libc::pid_t;
    // From a thread: a git that never reads must not block us.
    let mut stdin = child.stdin.take().expect("piped");
    let mut wanted = String::with_capacity(41 * ids.len());
    for id in ids {
        let _ = writeln!(wanted, "{id}");
    }
    thread::spawn(move || stdin.write_all(wanted.as_bytes()));
    // Any stderr output is progress; its tail is kept for the error.
    let (progress, made) = mpsc::channel();
    let mut stderr = child.stderr.take().expect("piped");
    let reader = thread::spawn(move || {
        let (mut buf, mut tail) = ([0u8; 4096], Vec::new());
        while let Ok(n @ 1..) = stderr.read(&mut buf) {
            tail.extend_from_slice(&buf[..n]);
            tail.drain(..tail.len().saturating_sub(STDERR_TAIL));
            let _ = progress.send(());
        }
        tail
    });
    let stall = std::env::var("GITFOLD_FETCH_STALL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(STALL, Duration::from_secs);
    loop {
        match made.recv_timeout(stall) {
            Ok(()) => continue,
            // stderr closed: git is exiting.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                // SIGTERM first: git removes the lock and .keep files it holds;
                // not reaped yet, so the group id can't have been reused.
                unsafe { libc::kill(-pid, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(2);
                while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    if let Err(RecvTimeoutError::Disconnected) = made.recv_timeout(left) {
                        break;
                    }
                }
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                child.wait()?;
                return Err(failed(format!(
                    "made no progress for {} s",
                    stall.as_secs()
                )));
            }
        }
    }
    let status = child.wait()?;
    let tail = reader.join().unwrap_or_default();
    if !status.success() {
        // git's last `fatal:` (or `error:`) line says why; else its last line.
        let tail = String::from_utf8_lossy(&tail);
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
