//! Keeping the repository's commit-graph current while it is mounted: git
//! answers its commit walks from it (`gitfold changes`' `for-each-ref
//! --no-merged` took 9 s without one on Chromium, under a second with), and
//! a fresh clone has none until `git gc` or `git maintenance` writes one.
//!
//! At mount and every `check_every()` after, git's own maintenance task does
//! it: `git maintenance run --task=commit-graph --auto`, which writes it
//! (`git commit-graph write --reachable --split`, adding only the commits new
//! since) once enough reachable commits are missing from it
//! (`maintenance.commit-graph.auto`, 100 by default; 0: never), not at all
//! with `core.commitGraph=false` or `maintenance.auto=false`, and takes git's
//! maintenance lock. At the lowest CPU and I/O priority, in a process of its
//! own: requests never wait for it.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use log::{info, warn};

use crate::git;

/// How often it is checked after the mount's: ten minutes, or
/// `GITFOLD_COMMIT_GRAPH_SECS` (a second at least).
fn check_every() -> Duration {
    static EVERY: OnceLock<Duration> = OnceLock::new();
    *EVERY.get_or_init(|| {
        let secs = std::env::var("GITFOLD_COMMIT_GRAPH_SECS").ok();
        let secs = secs.and_then(|s| s.parse().ok()).unwrap_or(600);
        Duration::from_secs(secs.max(1))
    })
}

/// Starts the thread keeping the commit-graph of `common_dir` current.
pub fn spawn(common_dir: PathBuf) {
    let spawned = thread::Builder::new()
        .name("commit-graph".into())
        .spawn(move || {
            let mut last_error = String::new();
            loop {
                match keep(&common_dir) {
                    Ok(()) => last_error.clear(),
                    Err(e) => {
                        let e = format!("{e:#}");
                        if e != last_error {
                            warn!("commit-graph: {e}");
                            last_error = e;
                        }
                    }
                }
                thread::sleep(check_every());
            }
        });
    if let Err(e) = spawned {
        warn!("commit-graph: can't start its thread: {e}");
    }
}

/// Has git write the commit-graph of `common_dir` if it is out of date, as
/// git's maintenance decides; logged if it did. Not with `maintenance.auto`
/// false, which git's own automatic maintenance heeds too.
fn keep(common_dir: &Path) -> Result<()> {
    let args = [
        "config",
        "--type=bool",
        "--default=true",
        "maintenance.auto",
    ];
    if git::cmd::output(common_dir, &args)?.trim_ascii() == b"false" {
        return Ok(());
    }
    let before = written(common_dir);
    let started = Instant::now();
    let mut git = git::cmd::command(common_dir);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe { git.pre_exec(lowest_priority) };
    let args = [
        "maintenance",
        "run",
        "--task=commit-graph",
        "--auto",
        "--quiet",
    ];
    git::cmd::output_of(&mut git, &args, None)?;
    if written(common_dir) != before {
        info!("commit-graph: written in {:.1?}", started.elapsed());
    }
    Ok(())
}

/// What tells the commit-graph was written: its chain of split files (each
/// write names a new one in it), and the single file a plain write leaves
/// (its time and size).
fn written(common_dir: &Path) -> (Option<Vec<u8>>, Option<(SystemTime, u64)>) {
    let info = common_dir.join("objects/info");
    let chain = std::fs::read(info.join("commit-graphs/commit-graph-chain")).ok();
    let single = std::fs::metadata(info.join("commit-graph")).ok();
    let single = single.and_then(|m| Some((m.modified().ok()?, m.len())));
    (chain, single)
}

/// The calling process at the lowest CPU and I/O priority (`nice 19`, the
/// idle I/O class), as far as the system lets it: after fork, before exec.
fn lowest_priority() -> std::io::Result<()> {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_IDLE: libc::c_long = 3;
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << 13,
        );
    }
    Ok(())
}
