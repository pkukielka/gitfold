//! A restack under way in a workspace (`cli::stack`): its intent file,
//! `restack` in the state directory, and finishing or undoing one a crash
//! interrupted.
//!
//! The intent is `<mode> <C> <C'> <A> <stack ref>`: the branch moves from C
//! to C' onto parent tip A, its stack ref from what it held (`-`: none) to
//! A. The command doing it holds the file locked (`flock`), so `recover`
//! leaves a restack in progress alone. The steps, by mode:
//!
//! - `open` (a mounted workspace): `git read-tree -m -u C C'` in the
//!   workspace (its index and files), then the refs.
//! - `closed` (unmounted): the index (`read-tree -m -i C C'`), then the base
//!   and the stamps of the changed files (`fs::workspace::restack_closed`),
//!   then the refs.
//!
//! What a crash left is told by the branch tip, the index on the paths the
//! restack changes, and (closed) the base:
//!
//! - tip at C': the refs moved; the stack ref is set if it isn't yet;
//! - tip at C, index at C': open, the files moved too, so the refs follow;
//!   closed, they follow if the base moved, else the index goes back;
//! - tip at C, index at C: nothing moved;
//! - anything else is left as it is, and reported.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::BString;

use super::StateDir;
use crate::git::{self, cmd, stack};

/// The intent file's name in a workspace's state directory.
pub const INTENT: &str = "restack";

/// Where the restack moves the workspace's files (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Open,
    Closed,
}

/// A restack to do, as its intent file records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Intent {
    pub mode: Mode,
    /// C: the branch tip before.
    pub from: ObjectId,
    /// C': the branch tip after.
    pub to: ObjectId,
    /// A: the parent's tip, the stack ref's new value.
    pub parent_tip: ObjectId,
    /// What the stack ref held (`None`: no ref).
    pub stack_old: Option<ObjectId>,
}

/// An intent recorded and locked by this process: the restack is in
/// progress until `done`.
pub struct Held {
    _file: fs::File,
    path: PathBuf,
}

impl Held {
    /// The restack is done (or undone): the intent goes.
    pub fn done(self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

impl Intent {
    fn encode(&self) -> String {
        let mode = match self.mode {
            Mode::Open => "open",
            Mode::Closed => "closed",
        };
        let stack = self.stack_old.map_or("-".to_string(), |o| o.to_string());
        let (from, to, a) = (self.from, self.to, self.parent_tip);
        format!("{mode} {from} {to} {a} {stack}\n")
    }

    fn parse(text: &str) -> Result<Intent> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let [mode, from, to, a, stack] = words[..] else {
            bail!("not a restack intent: {text:?}");
        };
        let id = |hex: &str| ObjectId::from_hex(hex.as_bytes());
        Ok(Intent {
            mode: match mode {
                "open" => Mode::Open,
                "closed" => Mode::Closed,
                _ => bail!("not a restack intent: {text:?}"),
            },
            from: id(from)?,
            to: id(to)?,
            parent_tip: id(a)?,
            stack_old: match stack {
                "-" => None,
                hex => Some(id(hex)?),
            },
        })
    }

    /// Records it for the workspace in `state`, durably, locked until
    /// `Held::done`. Fails if an intent is there (`recover` first).
    pub fn begin(&self, state: &StateDir) -> Result<Held> {
        let path = state.dir.join(INTENT);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("recording a restack in {}", path.display()))?;
        if !crate::lock::flock(&file)? {
            bail!("{} is locked", path.display());
        }
        file.write_all(self.encode().as_bytes())?;
        file.sync_all()?;
        Ok(Held { _file: file, path })
    }
}

/// What `recover` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    /// No restack was interrupted.
    Nothing,
    /// Another process is restacking it right now.
    InProgress,
    /// An interrupted restack, now finished.
    Finished,
    /// An interrupted restack that had moved nothing (or only the index,
    /// put back), now forgotten.
    Undone,
    /// One that can't be told apart: left, with why.
    Stuck(String),
}

/// Finishes or undoes a restack of the workspace in `state` that a crash
/// interrupted (see the module doc). Needs `git::init`.
pub fn recover(state: &StateDir) -> Result<Recovery> {
    let path = state.dir.join(INTENT);
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Recovery::Nothing),
        Err(e) => return Err(e.into()),
    };
    if !crate::lock::flock(&file)? {
        return Ok(Recovery::InProgress);
    }
    // Deleted (done) between the open and the lock.
    if fs::metadata(&path).map(|m| m.ino()).ok() != Some(file.metadata()?.ino()) {
        return Ok(Recovery::Nothing);
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let held = Held { _file: file, path };
    // Empty: the process died writing it, before anything moved.
    if text.trim().is_empty() {
        held.done()?;
        return Ok(Recovery::Undone);
    }
    let intent = Intent::parse(&text)?;
    let common = &state.common_dir;
    let tip = stack::rev(common, &format!("refs/heads/{}", state.branch))?;
    let recovery = if tip == Some(intent.to) {
        finish(state, &intent, intent.to)?;
        Recovery::Finished
    } else if tip == Some(intent.from) {
        let base = state.read_base()?.id;
        match (index_at(state, &intent)?, intent.mode) {
            (Some(true), Mode::Open) => {
                finish(state, &intent, intent.from)?;
                Recovery::Finished
            }
            (Some(true), Mode::Closed) if base == intent.to => {
                finish(state, &intent, intent.from)?;
                Recovery::Finished
            }
            (Some(true), Mode::Closed) => {
                move_index(state, &intent, true)?;
                Recovery::Undone
            }
            (Some(false), _) => Recovery::Undone,
            (None, _) => Recovery::Stuck(format!(
                "an interrupted restack of {} left its index neither at {} nor at {}",
                state.branch, intent.from, intent.to
            )),
        }
    } else {
        Recovery::Stuck(format!(
            "an interrupted restack of {} from {}: the branch moved since",
            state.branch, intent.from
        ))
    };
    if !matches!(recovery, Recovery::Stuck(_)) {
        held.done()?;
    }
    Ok(recovery)
}

/// Moves the branch from `tip` to the intent's C' (if not there) and its
/// stack ref to A, unless the stack ref was moved past the intent meanwhile.
fn finish(state: &StateDir, intent: &Intent, tip: ObjectId) -> Result<()> {
    let moved = stack::move_refs(
        &state.common_dir,
        &state.branch,
        (tip, intent.to),
        (intent.stack_old, intent.parent_tip),
        "gitfold: restack (recovered)",
    );
    let stored = stack::rev(&state.common_dir, &stack::stack_ref(&state.branch))?;
    if moved.is_err() && stored != Some(intent.parent_tip) && tip == intent.to {
        // Another restack or `stack on` set it since: theirs stands.
        log::warn!(
            "{}: the stack ref moved during a restack; left at {stored:?}",
            state.branch
        );
        return Ok(());
    }
    moved
}

/// The paths the restack changes, as `git` takes them.
pub fn changed_paths(intent: &Intent) -> Result<Vec<BString>> {
    git::with_repo(|repo| {
        let from = git::lens::commit_tree(repo, intent.from)?;
        let to = git::lens::commit_tree(repo, intent.to)?;
        Ok(git::lens::delta(repo, from, to)?
            .into_iter()
            .map(|(path, _)| path)
            .collect())
    })
}

/// Whether the workspace's index is at C' (`Some(true)`) or at C
/// (`Some(false)`) on the paths the restack changes, or neither.
fn index_at(state: &StateDir, intent: &Intent) -> Result<Option<bool>> {
    let paths = changed_paths(intent)?;
    let matches = |commit: ObjectId| -> Result<bool> {
        let status = cmd::command(&state.admin)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .args([
                "diff-index",
                "--cached",
                "--quiet",
                &commit.to_string(),
                "--",
            ])
            .args(paths.iter().map(|p| p.to_string()))
            .status()?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git diff-index in {} failed ({status})",
                state.admin.display()
            ),
        }
    };
    if matches(intent.to)? {
        return Ok(Some(true));
    }
    Ok(matches(intent.from)?.then_some(false))
}

/// Moves the index of the workspace in `state` from C to C' (`back`: from
/// C' to C), without its files: entries git has staged changes in are
/// refused.
pub fn move_index(state: &StateDir, intent: &Intent, back: bool) -> Result<()> {
    let (from, to) = match back {
        false => (intent.from, intent.to),
        true => (intent.to, intent.from),
    };
    let (from, to) = (from.to_string(), to.to_string());
    cmd::output(&state.admin, &["read-tree", "-m", "-i", &from, &to]).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_read_back() {
        let id = |n: u8| ObjectId::from_bytes_or_panic(&[n; 20]);
        for intent in [
            Intent {
                mode: Mode::Open,
                from: id(1),
                to: id(2),
                parent_tip: id(3),
                stack_old: Some(id(4)),
            },
            Intent {
                mode: Mode::Closed,
                from: id(1),
                to: id(2),
                parent_tip: id(3),
                stack_old: None,
            },
        ] {
            assert_eq!(Intent::parse(&intent.encode()).unwrap(), intent);
        }
        assert!(Intent::parse("open 01").is_err());
    }
}
