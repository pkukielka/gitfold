//! `gitfold watch`: waiting for a change in a directory of a task folder.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::git;
use crate::inotify::{self, Inotify};
use crate::state::task::TaskDir;

use super::message::{is_message, json_str};
use super::{deadline, remaining};

/// How long a watch keeps collecting events after the first, so one burst
/// (a file written then renamed, several messages) comes out together.
const SETTLE: Duration = Duration::from_millis(100);

/// `gitfold watch`: waits until something in `target` (`[<BRANCH>:]<DIR>`, a
/// directory of a task folder; the current workspace's when no branch is
/// given) is written, created, moved in, deleted or moved out, then prints
/// what (`C` or `D`, a tab and the name, each name's last change; or JSON),
/// collecting a burst of them, and exits; `D` and `.` if the directory itself
/// goes. Prints nothing if `timeout` passes first.
pub fn watch(repo: &Path, args: WatchArgs) -> Result<()> {
    let WatchArgs {
        target,
        timeout,
        json,
    } = args;
    let common_dir = git::init(repo)?;
    let (branch, dir) = match target.split_once(':') {
        Some((b, d)) => (b.to_string(), d),
        None => match crate::cli::branch_or_current(None, repo) {
            Ok(b) => (b, target.as_str()),
            Err(_) => bail!("not in a gitfold workspace: name the task as <BRANCH>:<DIR>"),
        },
    };
    let dir = dir.trim_matches('/');
    if dir.split('/').any(|c| c == "..") {
        bail!("{dir}: a directory of the task folder, without `..`");
    }
    git::require_branch(&branch)?;
    let root = TaskDir::of(&common_dir, &branch).dir;
    let path = if dir.is_empty() || dir == "." {
        root
    } else {
        root.join(dir)
    };
    match fs::metadata(&path) {
        Ok(meta) if !meta.is_dir() => {
            bail!("{dir} is a file: watch the directory it is in")
        }
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => fs::create_dir_all(&path)?,
        Err(e) => return Err(e.into()),
    }
    let changed = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE;
    let deleted = libc::IN_DELETE | libc::IN_MOVED_FROM;
    let watch = Inotify::watch(&path, changed | deleted)?;
    let deadline = deadline(timeout);
    // Each name's last change: a file replaced in one burst ends as changed.
    let mut events: BTreeMap<Vec<u8>, char> = BTreeMap::new();
    let mut settle: Option<Instant> = None;
    'reading: loop {
        let batch = watch.read(remaining(settle.or(deadline)))?;
        if batch.is_empty() {
            break; // timed out, or settled
        }
        for (mask, name) in batch {
            if mask & libc::IN_Q_OVERFLOW != 0 {
                bail!("too many changes at once in {dir}: some weren't seen; look at it");
            }
            if mask & inotify::LOST != 0 {
                events.insert(b".".to_vec(), 'D');
                break 'reading;
            }
            // Dot names: files being written or taken, not there yet.
            if is_message(&name) {
                events.insert(name, if mask & deleted != 0 { 'D' } else { 'C' });
            }
        }
        if !events.is_empty() && settle.is_none() {
            settle = Some(Instant::now() + SETTLE);
        }
    }
    for (name, kind) in &events {
        let name = String::from_utf8_lossy(name);
        match json {
            true => println!(
                "{{\"event\":{},\"path\":{}}}",
                json_str(if *kind == 'D' { "deleted" } else { "changed" }),
                json_str(&name)
            ),
            false => println!("{kind}\t{name}"),
        }
    }
    Ok(())
}

/// The arguments of `gitfold watch`.
#[derive(clap::Args)]
pub struct WatchArgs {
    pub target: String,
    /// Give up after this many seconds, printing nothing.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// One JSON object per line: {"event": "changed"|"deleted", "path"}.
    #[arg(long)]
    pub json: bool,
}
