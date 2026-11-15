//! A restack under way in a workspace (`cli::stack`): its intent file,
//! `restack` in the state directory, and finishing or undoing one a crash
//! interrupted.
//!
//! The intent is `<mode> <C> <C'> <A> <stack ref>`: the branch moves from C
//! to C' onto parent tip A, its stack ref from what it held (`-`: none) to
//! A. The command doing it holds the file locked (`flock`; locked before
//! it is linked into place), so `recover` leaves a restack in progress
//! alone. The steps, by mode:
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
//! - the branch moved on and the index describes it: forgotten;
//! - anything else is left as it is, and reported (`forget`, for `gitfold
//!   layer base main`, deletes it).

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
/// progress until `done`. Dropped otherwise (an error before anything
/// moved), the intent goes too, unless `keep` said something may have
/// moved half-way: then it stays for `recover`.
pub struct Held {
    _file: fs::File,
    path: PathBuf,
    keep: bool,
}

impl Held {
    /// The restack is done (or undone): the intent goes.
    pub fn done(mut self) -> Result<()> {
        self.keep = true;
        remove(&self.path)
    }

    /// Something may have moved half-way: the intent stays, for `recover`
    /// to finish or undo once this process lets go of it.
    pub fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if !self.keep
            && let Err(e) = remove(&self.path)
        {
            log::warn!("removing {}: {e:#}", self.path.display());
        }
    }
}

fn remove(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Whether the workspace in `state` has an intent: a restack under way, or
/// one a crash interrupted.
pub fn pending(state: &StateDir) -> bool {
    fs::symlink_metadata(state.dir.join(INTENT)).is_ok()
}

/// Deletes the intent of the workspace in `state` unless a restack holds
/// it: for one `recover` can't tell apart (`Recovery::Stuck`), once the
/// workspace is as wanted. Whether there was one to delete.
pub fn forget(state: &StateDir) -> Result<bool> {
    let path = state.dir.join(INTENT);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !crate::lock::flock(&file)? {
        bail!("a move of {} onto its base is running", state.branch);
    }
    remove(&path)?;
    Ok(true)
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
    /// `Held::done`. Fails if an intent is there (`recover` first). It is
    /// written and locked under another name and linked into place, so
    /// `recover` never finds it unlocked, or empty.
    pub fn begin(&self, state: &StateDir) -> Result<Held> {
        let path = state.dir.join(INTENT);
        let tmp = state
            .dir
            .join(format!("{INTENT}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("recording a move onto its base in {}", tmp.display()))?;
        let written = (|| -> Result<()> {
            anyhow::ensure!(crate::lock::flock(&file)?, "{} is locked", tmp.display());
            file.write_all(self.encode().as_bytes())?;
            file.sync_all()?;
            fs::hard_link(&tmp, &path).with_context(|| {
                format!(
                    "recording a move in {} (a move of {} onto its base is under way)",
                    path.display(),
                    state.branch
                )
            })
        })();
        let _ = fs::remove_file(&tmp);
        written?;
        let held = Held {
            _file: file,
            path,
            keep: false,
        };
        fs::File::open(&state.dir)?.sync_all()?;
        Ok(held)
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
    let held = Held {
        _file: file,
        path,
        keep: false,
    };
    match decide(state, &text) {
        Ok(Recovery::Stuck(why)) => {
            held.keep();
            Ok(Recovery::Stuck(why))
        }
        Ok(recovery) => {
            held.done()?;
            Ok(recovery)
        }
        Err(e) => {
            held.keep();
            Err(e)
        }
    }
}

/// `recover` for the intent `text`, held.
fn decide(state: &StateDir, text: &str) -> Result<Recovery> {
    let intent = Intent::parse(text)?;
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
                "an interrupted move of {} onto its base left its index neither at {} nor at {}",
                state.branch, intent.from, intent.to
            )),
        }
    } else if let Some(tip) = tip
        && index_matches(state, tip, &[])?
    {
        // The branch moved on and git's index describes it (a commit, a
        // reset since): whatever the restack left is history now.
        log::warn!(
            "{}: forgetting an interrupted move onto its base from {}: the branch moved on since",
            state.branch,
            intent.from
        );
        Recovery::Undone
    } else {
        let main =
            crate::cli::stack::land::trunk_name(common, &crate::settings::Local::read(common))
                .unwrap_or_else(|| "main".into());
        Recovery::Stuck(format!(
            "an interrupted move of {} onto its base, from {}: the branch and its index moved \
             since; once its workspace is as you want it, `gitfold layer base {main} -w {}` \
             builds it on {main} and forgets the interrupted move",
            state.branch, intent.from, state.branch
        ))
    };
    Ok(recovery)
}

/// Moves the branch from `tip` to the intent's C' (if not there) and its
/// stack ref to A, unless the stack ref was moved past the intent meanwhile.
fn finish(state: &StateDir, intent: &Intent, tip: ObjectId) -> Result<()> {
    let stored = stack::rev(&state.common_dir, &stack::stack_ref(&state.branch))?;
    // Both moved already (the restack itself got that far).
    if tip == intent.to && stored == Some(intent.parent_tip) {
        return Ok(());
    }
    let moved = stack::move_refs(
        &state.common_dir,
        &state.branch,
        (tip, intent.to),
        (intent.stack_old, intent.parent_tip),
        "gitfold: move onto base (recovered)",
    );
    if moved.is_err() && tip == intent.to {
        // Another move or `layer base` set it since: theirs stands.
        log::warn!(
            "{}: the stack ref moved during a move onto its base; left at {stored:?}",
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
    // C and C' have the same tree: the index is at both.
    if paths.is_empty() {
        return Ok(Some(true));
    }
    if index_matches(state, intent.to, &paths)? {
        return Ok(Some(true));
    }
    Ok(index_matches(state, intent.from, &paths)?.then_some(false))
}

/// Whether the workspace's index has `commit`'s entries at `paths` (none:
/// the whole index).
fn index_matches(state: &StateDir, commit: ObjectId, paths: &[BString]) -> Result<bool> {
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
    use crate::git::test_repo::Repo;

    /// `recover`, once no other thread's child (forked by a test running
    /// alongside, before its exec) holds a copy of a released intent's lock.
    fn settled(state: &StateDir) -> Recovery {
        for _ in 0..100 {
            match recover(state).unwrap() {
                Recovery::InProgress => std::thread::sleep(std::time::Duration::from_millis(10)),
                done => return done,
            }
        }
        panic!("still held")
    }

    /// A repository with commits c1 < c2 < c3 on main, branch p2 checked
    /// out in a linked worktree registered as gitfold's workspace of p2,
    /// and that workspace's state directory.
    fn workspace(name: &str) -> (Repo, StateDir, [ObjectId; 3]) {
        let r = Repo::new(name);
        let mut ids = [ObjectId::null(gix::hash::Kind::Sha1); 3];
        for (i, id) in ids.iter_mut().enumerate() {
            r.write("f.txt", &format!("{i}\n"));
            *id = r.commit(&format!("c{i}"));
        }
        r.git(&["branch", "p2"]);
        let wt = r.dir.join("gitfold-ws-p2");
        r.git(&["worktree", "add", "-q", wt.to_str().unwrap(), "p2"]);
        let state = StateDir::of(&r.common(), "p2");
        fs::create_dir_all(&state.dir).unwrap();
        (r, state, ids)
    }

    #[test]
    fn a_finished_restack_is_recovered_as_finished() {
        let (r, state, [c1, c2, c3]) = workspace("restack-finished");
        let common = r.common();
        // The branch and its stack ref both moved: the restack got that far.
        stack::set_base(&common, "p2", c1).unwrap();
        let intent = Intent {
            mode: Mode::Open,
            from: c2,
            to: c3,
            parent_tip: c1,
            stack_old: Some(c2),
        };
        intent.begin(&state).unwrap().keep();
        assert!(pending(&state));
        assert_eq!(settled(&state), Recovery::Finished);
        assert!(!pending(&state), "the intent goes");
        assert_eq!(r.id("p2"), c3);
        assert_eq!(
            stack::rev(&common, &stack::stack_ref("p2")).unwrap(),
            Some(c1)
        );
        // Only the stack ref left to move.
        let intent = Intent {
            stack_old: Some(c1),
            parent_tip: c2,
            ..intent
        };
        intent.begin(&state).unwrap().keep();
        assert_eq!(settled(&state), Recovery::Finished);
        assert_eq!(
            stack::rev(&common, &stack::stack_ref("p2")).unwrap(),
            Some(c2)
        );
    }

    #[test]
    fn held_intents_go_unless_kept() {
        let (_r, state, [c1, c2, c3]) = workspace("restack-held");
        let intent = Intent {
            mode: Mode::Open,
            from: c1,
            to: c2,
            parent_tip: c3,
            stack_old: None,
        };
        let held = intent.begin(&state).unwrap();
        assert!(intent.begin(&state).is_err(), "one at a time");
        assert_eq!(recover(&state).unwrap(), Recovery::InProgress);
        drop(held);
        assert!(!pending(&state), "dropped: gone");
        intent.begin(&state).unwrap().keep();
        assert!(pending(&state), "kept");
        // (Retried: see `settled`.)
        let forgot = (0..100).find_map(|_| {
            forget(&state).ok().or_else(|| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                None
            })
        });
        assert_eq!(forgot, Some(true));
        assert!(!pending(&state));
    }

    #[test]
    fn an_intent_the_branch_moved_past_is_forgotten() {
        let (r, state, [c1, c2, _]) = workspace("restack-moved");
        // The branch is at c3 and its index describes it: neither C nor C'.
        let intent = Intent {
            mode: Mode::Open,
            from: c1,
            to: c2,
            parent_tip: c1,
            stack_old: None,
        };
        intent.begin(&state).unwrap().keep();
        assert_eq!(settled(&state), Recovery::Undone);
        assert!(!pending(&state));
        // With staged changes, it can't be told apart: left, saying how out.
        let wt = r.dir.join("gitfold-ws-p2");
        fs::write(wt.join("f.txt"), "staged\n").unwrap();
        r.git_in(&wt, &["add", "f.txt"]);
        intent.begin(&state).unwrap().keep();
        let Recovery::Stuck(why) = settled(&state) else {
            panic!("not stuck")
        };
        assert!(
            why.contains("`gitfold layer base main -w p2` builds it on main"),
            "{why}"
        );
        assert!(pending(&state));
    }

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
