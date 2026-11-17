//! Following without an agent (layers-design/15-tree-spec.md, section 3):
//! the daemon moves a branch that follows its parent onto the parent's new
//! commits, a workspace's once it has been idle for `IDLE` (no change
//! through the mount, no git operation under way), a branch without one when
//! the parent moves; never while an agent is present in its workspace (its
//! turn start does it, see `cli::claude::parent`). The tree is walked every
//! `POLL`, each branch before its children, so a move cascades down.
//!
//! A move is `gitfold parent B --update --auto` (the policy of
//! `cli::tree::follow_once`, an agent's turn start's too) run as a child
//! process (`/proc/self/exe`, which an upgrade in place leaves): the
//! daemon holds nothing (locks, the workspace) while it runs git in the
//! workspace through this very mount, and a git that hangs is killed after
//! `TIMEOUT`, its state left as git leaves it. Each is tried once per state
//! of the branch, its parent and its workspace; its outcome is logged, and a
//! move in a workspace is kept for its agent's next turn start.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gix::ObjectId;
use log::{info, warn};

use crate::fs::Workspaces;
use crate::git;
use crate::settings::Local;
use crate::state::StateDir;
use crate::tree::Tree;

/// How often the tree is walked.
const POLL: Duration = Duration::from_secs(1);
/// How long a workspace has had no change before its branch moves.
const IDLE: Duration = Duration::from_secs(3);
/// How long a move may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(600);

/// What the walk knows of a branch.
struct Seen {
    /// Its workspace's count of writes (`None`: not open), and since when
    /// it is that.
    writes: Option<u64>,
    since: Instant,
    /// The state last tried: its tip, its parent's, its workspace's
    /// writes.
    tried: Option<(ObjectId, ObjectId, Option<u64>)>,
}

/// Starts the thread following the branches of `common_dir`, mounted with
/// `workspaces` open.
pub fn spawn(common_dir: PathBuf, workspaces: Arc<Workspaces>) {
    let spawned = thread::Builder::new().name("follow".into()).spawn(move || {
        let mut seen = HashMap::new();
        let mut last_error = String::new();
        loop {
            thread::sleep(POLL);
            match walk(&common_dir, &workspaces, &mut seen) {
                Ok(()) => last_error.clear(),
                Err(e) => {
                    let e = format!("{e:#}");
                    if e != last_error {
                        warn!("following: {e}");
                        last_error = e;
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        warn!("following: can't start its thread: {e}");
    }
}

/// One walk of the tree (see the module doc).
fn walk(
    common_dir: &Path,
    workspaces: &Workspaces,
    seen: &mut HashMap<String, Seen>,
) -> anyhow::Result<()> {
    let local = Local::read_checked(common_dir)?;
    // Nothing built on anything: no tree to read (it asks git for the root).
    if local.parents().is_empty() {
        seen.clear();
        return Ok(());
    }
    let tree = Tree::read(common_dir)?;
    let branches = tree.with_parents();
    seen.retain(|b, _| branches.contains(&b.as_str()));
    for branch in branches {
        // Read now: a parent moved earlier in this walk is seen moved.
        let writes = workspaces.get_open(branch).map(|ws| ws.writes());
        let seen = seen.entry(branch.to_string()).or_insert_with(|| Seen {
            writes,
            since: Instant::now(),
            tried: None,
        });
        if seen.writes != writes {
            (seen.writes, seen.since) = (writes, Instant::now());
        }
        let Some(parent) = tree.parent(branch).filter(|_| tree.follows(branch)) else {
            continue;
        };
        let (Ok(tip), Ok(on)) = (git::branch_tip(branch), git::resolve(parent)) else {
            continue; // parent gone (T3), or the branch meanwhile
        };
        let state = StateDir::of(common_dir, branch);
        let now = (tip.id, on.id, writes);
        if seen.tried == Some(now) || state.agent_present() {
            continue;
        }
        let idle = seen.since.elapsed() >= IDLE;
        if writes.is_some() && (!idle || state.git_busy().is_some()) {
            continue;
        }
        seen.tried = Some(now);
        update(common_dir, &state, tip.id);
    }
    Ok(())
}

/// A pipe of a child read to its end by a thread of its own (a full pipe
/// would stop the child).
fn drain<R: Read + Send + 'static>(mut from: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        let _ = from.read_to_string(&mut text);
        text
    })
}

/// `gitfold parent BRANCH --update --auto` for `branch`, in a process
/// group of its own (the git it runs killed with it after `TIMEOUT`): what
/// it said.
fn run_update(common_dir: &Path, branch: &str) -> anyhow::Result<String> {
    let mut child = Command::new("/proc/self/exe")
        .arg("-C")
        .arg(common_dir)
        .args(["parent", branch, "--update", "--auto"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group: killed with the git it runs.
        .process_group(0)
        .spawn()?;
    let out = drain(child.stdout.take().expect("piped"));
    let err = drain(child.stderr.take().expect("piped"));
    let started = Instant::now();
    while child.try_wait()?.is_none() {
        if started.elapsed() > TIMEOUT {
            // SAFETY: a plain syscall, on the group of our own child.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            anyhow::bail!(
                "gitfold parent {branch} --update --auto took longer than {}s: killed, what \
                 git did left as it is",
                TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(out.join().unwrap_or_default() + &err.join().unwrap_or_default())
}

/// Moves the branch of `state` (at `tip`): a move (the branch isn't at
/// `tip` any more) logged and kept for its agent, anything else said
/// logged as a pause.
fn update(common_dir: &Path, state: &StateDir, tip: ObjectId) {
    let branch = &state.branch;
    let said = run_update(common_dir, branch).unwrap_or_else(|e| format!("{e:#}\n"));
    if git::branch_tip(branch).is_ok_and(|now| now.id != tip) {
        info!("following: {}", said.trim_end().replace('\n', "; "));
        if let Err(e) = state.add_followed(&said) {
            warn!("following: {branch}: keeping the move for its agent: {e:#}");
        }
    } else if !said.trim().is_empty() {
        warn!("following {branch}: {}", said.trim_end());
    }
}
