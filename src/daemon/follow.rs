//! Following without an agent (layers-design/15-tree-spec.md, section 3):
//! the daemon moves a branch that follows its parent onto the parent's new
//! commits, a workspace's once it has been idle for `IDLE` (no change
//! through the mount, no git operation under way), a branch without one when
//! the parent moves; never while an agent is present in its workspace (its
//! turn start does it, see `cli::claude::parent`). The tree is walked every
//! `POLL`, each branch before its children, so a move cascades down.
//!
//! A move is `gitfold parent B --update` run as a child process: the
//! daemon holds nothing (locks, the workspace) while it runs git in the
//! workspace through this very mount, and a git that hangs is killed after
//! `TIMEOUT`, its state left as git leaves it. Each is tried once per state
//! of the branch, its parent and its workspace; its outcome is logged, and a
//! move in a workspace is kept for its agent's next turn start.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gix::ObjectId;
use log::{info, warn};

use crate::fs::Workspaces;
use crate::git;
use crate::state::StateDir;
use crate::tree::Tree;

/// How often the tree is walked.
const POLL: Duration = Duration::from_secs(1);
/// How long a workspace has had no change before its branch moves.
const IDLE: Duration = Duration::from_secs(3);
/// How long a move may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(600);

/// What the walk knows of a branch.
#[derive(Default)]
struct Seen {
    /// Its workspace's count of writes (`None`: not open), and since when it is.
    writes: Option<u64>,
    since: Option<Instant>,
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
    let tree = Tree::read(common_dir)?;
    let branches = tree.with_parents();
    seen.retain(|b, _| branches.contains(&b.as_str()));
    for branch in branches {
        let seen = seen.entry(branch.to_string()).or_default();
        // Read now: a parent moved earlier in this walk is seen moved.
        let writes = workspaces.get_open(branch).map(|ws| ws.writes());
        if seen.since.is_none() || seen.writes != writes {
            (seen.writes, seen.since) = (writes, Some(Instant::now()));
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
        let idle = seen.since.is_some_and(|t| t.elapsed() >= IDLE);
        if writes.is_some() && (!idle || state.git_busy().is_some()) {
            continue;
        }
        seen.tried = Some(now);
        if is_ancestor(common_dir, on.id, tip.id) {
            continue; // up to date
        }
        update(common_dir, &state);
    }
    Ok(())
}

/// Whether `a` is an ancestor of `b` (`git merge-base --is-ancestor`); not
/// if git can't tell.
fn is_ancestor(common_dir: &Path, a: ObjectId, b: ObjectId) -> bool {
    (git::cmd::command(common_dir))
        .args([
            "merge-base",
            "--is-ancestor",
            &a.to_string(),
            &b.to_string(),
        ])
        .status()
        .is_ok_and(|s| s.success())
}

/// `gitfold parent BRANCH --update` for the branch of `state`, logged; a
/// move in its workspace kept for its agent.
fn update(common_dir: &Path, state: &StateDir) {
    let branch = &state.branch;
    let run = || -> anyhow::Result<(bool, String, String)> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("-C")
            .arg(common_dir)
            .args(["parent", branch, "--update"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Read meanwhile: a full pipe would stop it.
        let read = |mut from: Box<dyn Read + Send>| {
            thread::spawn(move || {
                let mut text = String::new();
                let _ = from.read_to_string(&mut text);
                text
            })
        };
        let out = read(Box::new(child.stdout.take().expect("piped")));
        let err = read(Box::new(child.stderr.take().expect("piped")));
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "gitfold parent {branch} --update took longer than {}s: killed, what git \
                     did left as it is",
                    TIMEOUT.as_secs()
                );
            }
            thread::sleep(Duration::from_millis(50));
        };
        let (out, err) = (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        );
        Ok((status.success(), out, err))
    };
    match run() {
        Ok((true, said, _)) => {
            info!("following: {}", said.trim_end().replace('\n', "; "));
            if let Err(e) = state.add_followed(&said) {
                warn!("following: {branch}: keeping the move for its agent: {e:#}");
            }
        }
        Ok((false, said, err)) => {
            let said = format!("{said}{err}");
            warn!("following {branch} paused: {}", said.trim_end());
        }
        Err(e) => warn!("following {branch}: {e:#}"),
    }
}
