//! A move under way in a workspace (`cli::layer::moves`, done by
//! `fs::workspace::moves`): its intent file, `move` in the state directory,
//! and finishing or undoing one a crash interrupted.
//!
//! The intent is `<C> <C'> <A> <own ref>`: the branch moves from C to C',
//! its own commits then starting from A, its own ref from what it held
//! (`-`: none) to A. The process doing it holds the file locked (`flock`;
//! locked before it is linked into place), so `recover` leaves a move in
//! progress alone. The steps, mounted or not, layers shown or not:
//!
//! 1. the index, on the paths the move changes (`move_index`; a union
//!    file's entry is the branch's version too, the layers' lines kept out
//!    of it by the filter);
//! 2. the workspace's base, durably, with the view built over C' (its files
//!    show C', its layers over it), and the copies of committed files the
//!    move changes dropped from upper (`stale_copies`);
//! 3. the refs, in one transaction.
//!
//! git's own lock on the index (`IndexLock`) is held from step 1 through 3,
//! so no `git commit` lands in between (it would commit the moved index on
//! the old tip); the branch is checked again once it is taken.
//!
//! What a crash left is told by the branch tip, the index on the paths the
//! move changes, and the base:
//!
//! - tip at C': the refs moved; the own ref is set if it isn't yet;
//! - tip at C, index at C': the refs follow if the base moved, else the
//!   index goes back;
//! - tip at C, index at C: nothing moved;
//! - the branch moved on and the index describes it: forgotten;
//! - anything else is left as it is, and reported (`forget`, for `gitfold
//!   layer remove --all`, deletes it).

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
pub const INTENT: &str = "move";

/// A move to do, as its intent file records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Intent {
    /// C: the branch tip before.
    pub from: ObjectId,
    /// C': the branch tip after.
    pub to: ObjectId,
    /// A: where its own commits start after, the own ref's new value.
    pub own_to: ObjectId,
    /// What the own ref held (`None`: no ref).
    pub own_from: Option<ObjectId>,
}

/// An intent recorded and locked by this process: the move is in
/// progress until `done`. Dropped otherwise (an error before anything
/// moved), the intent goes too, unless `keep` said something may have
/// moved half-way: then it stays for `recover`.
pub struct Held {
    _file: fs::File,
    path: PathBuf,
    keep: bool,
}

impl Held {
    /// The move is done (or undone): the intent goes.
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

/// Whether the workspace in `state` has an intent: a move under way, or
/// one a crash interrupted.
pub fn pending(state: &StateDir) -> bool {
    fs::symlink_metadata(state.dir.join(INTENT)).is_ok()
}

/// Deletes the intent of the workspace in `state` unless a move holds
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
        bail!("a move of {} is running", state.branch);
    }
    remove(&path)?;
    Ok(true)
}

impl Intent {
    fn encode(&self) -> String {
        let own = self.own_from.map_or("-".to_string(), |o| o.to_string());
        let (from, to, a) = (self.from, self.to, self.own_to);
        format!("{from} {to} {a} {own}\n")
    }

    fn parse(text: &str) -> Result<Intent> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let [from, to, a, own] = words[..] else {
            bail!("not a move intent: {text:?}");
        };
        let id = |hex: &str| ObjectId::from_hex(hex.as_bytes());
        Ok(Intent {
            from: id(from)?,
            to: id(to)?,
            own_to: id(a)?,
            own_from: match own {
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
            .with_context(|| format!("recording a move in {}", tmp.display()))?;
        let written = (|| -> Result<()> {
            anyhow::ensure!(crate::lock::flock(&file)?, "{} is locked", tmp.display());
            file.write_all(self.encode().as_bytes())?;
            file.sync_all()?;
            fs::hard_link(&tmp, &path).with_context(|| {
                format!(
                    "recording a move in {} (a move of {} is under way)",
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
    /// No move was interrupted.
    Nothing,
    /// Another process is moving it right now.
    InProgress,
    /// An interrupted move, now finished.
    Finished,
    /// An interrupted move that had moved nothing (or only the index,
    /// put back), now forgotten.
    Undone,
    /// One that can't be told apart: left, with why.
    Stuck(String),
}

/// Finishes or undoes a move of the workspace in `state` that a crash
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
    drop_left_index_lock(state)?;
    let intent = match Intent::parse(text) {
        Ok(intent) => intent,
        // Nothing tells what it moved: as one that can't be told apart.
        Err(e) => {
            return Ok(Recovery::Stuck(format!(
                "an interrupted move of {} left an intent gitfold can't read ({e:#}); once its \
                 workspace is as you want it, `gitfold layer remove --all -w {}` forgets the \
                 interrupted move (and its layers)",
                state.branch, state.branch
            )));
        }
    };
    let common = &state.common_dir;
    let tip = stack::rev(common, &format!("refs/heads/{}", state.branch))?;
    let recovery = if tip == Some(intent.to) {
        finish(state, &intent, intent.to)?;
        Recovery::Finished
    } else if tip == Some(intent.from) {
        let base = state.read_base()?.id;
        match index_at(state, &intent)? {
            Some(true) if base == intent.to => {
                finish(state, &intent, intent.from)?;
                Recovery::Finished
            }
            Some(true) => {
                move_index(state, &intent, true, None)?;
                Recovery::Undone
            }
            Some(false) => Recovery::Undone,
            None => Recovery::Stuck(format!(
                "an interrupted move of {} left its index neither at {} nor at {}",
                state.branch, intent.from, intent.to
            )),
        }
    } else if let Some(tip) = tip
        && index_matches(state, tip, &[])?
    {
        // The branch moved on and git's index describes it (a commit, a
        // reset since): whatever the move left is history now.
        log::warn!(
            "{}: forgetting an interrupted move from {}: the branch moved on since",
            state.branch,
            intent.from
        );
        Recovery::Undone
    } else {
        Recovery::Stuck(format!(
            "an interrupted move of {}, from {}: the branch and its index moved since; once \
             its workspace is as you want it, `gitfold layer remove --all -w {}` forgets the \
             interrupted move (and its layers)",
            state.branch, intent.from, state.branch
        ))
    };
    Ok(recovery)
}

/// Moves the branch from `tip` to the intent's C' (if not there) and its
/// own ref to A, unless the own ref was moved past the intent meanwhile;
/// then drops the copies of committed files the move left (`stale_copies`).
fn finish(state: &StateDir, intent: &Intent, tip: ObjectId) -> Result<()> {
    drop_stale_copies(state, intent)?;
    let stored = stack::rev(&state.common_dir, &stack::own_ref(&state.branch))?;
    // Both moved already (the move itself got that far).
    if tip == intent.to && stored == Some(intent.own_to) {
        return Ok(());
    }
    let moved = stack::move_refs(
        &state.common_dir,
        &state.branch,
        (tip, intent.to),
        (intent.own_from, intent.own_to),
        "gitfold: move (recovered)",
    );
    if moved.is_err() && tip == intent.to {
        // Another move set it since: theirs stands.
        log::warn!(
            "{}: the own ref moved during a move; left at {stored:?}",
            state.branch
        );
        return Ok(());
    }
    moved
}

/// The paths the move changes, as `git` takes them.
pub fn changed_paths(intent: &Intent) -> Result<Vec<BString>> {
    git::with_repo(|repo| {
        let from = git::layers::commit_tree(repo, intent.from)?;
        let to = git::layers::commit_tree(repo, intent.to)?;
        let mut paths: Vec<BString> = (git::layers::delta(repo, from, to)?.into_iter())
            .map(|(path, _)| path)
            .collect();
        // A file turned into a directory: its path once.
        paths.dedup();
        Ok(paths)
    })
}

/// The paths the move changes where the workspace's own work may be in its
/// way: `changed_paths` but the files the workspace in `state` shows mixed
/// with its layers' lines (union files, files editable layers changed or
/// added), whose own lines and waiting edits a switch carries over.
pub fn own_paths(state: &StateDir, intent: &Intent) -> Result<Vec<BString>> {
    let applied = super::layers::ShownLayers::read(state)?;
    let mut paths: Vec<BString> = (changed_paths(intent)?.into_iter())
        .filter(|p| !applied.union.contains(p.as_slice()) && !applied.theirs.contains(p.as_slice()))
        .collect();
    paths.dedup();
    Ok(paths)
}

/// The upper files of the workspace in `state` at paths the move changes
/// that are copies of the branch's committed files (as C has them: content
/// and executable bit), but union files: left by commits made while layers
/// showed (its base waited), they would hide C'.
pub fn stale_copies(state: &StateDir, intent: &Intent) -> Result<Vec<BString>> {
    use std::os::unix::fs::PermissionsExt;
    let from = git::resolve(&intent.from.to_string())?.tree;
    let mut out = Vec::new();
    for path in own_paths(state, intent)? {
        let file = super::upper_path(&state.upper(), &path);
        let Ok(meta) = fs::symlink_metadata(&file) else {
            continue;
        };
        let Some(item) = git::with_repo(|repo| git::layers::lookup(repo, from, path.as_ref()))?
        else {
            continue;
        };
        use gix::objs::tree::EntryKind;
        let same = match item.kind {
            EntryKind::Link if meta.is_symlink() => {
                let target = fs::read_link(&file)?;
                std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str())
                    == git::read_blob(item.oid)?.as_slice()
            }
            EntryKind::Blob | EntryKind::BlobExecutable if meta.is_file() => {
                let exec = meta.permissions().mode() & 0o100 != 0;
                exec == (item.kind == EntryKind::BlobExecutable)
                    && meta.len() == git::object_size(item.oid)?.unwrap_or(u64::MAX)
                    && fs::read(&file)? == git::read_blob(item.oid)?
            }
            _ => false,
        };
        if same {
            out.push(path);
        }
    }
    Ok(out)
}

/// Drops `stale_copies` from upper, for a recovered move whose base moved.
fn drop_stale_copies(state: &StateDir, intent: &Intent) -> Result<()> {
    if state.read_base().map(|b| b.id).ok() != Some(intent.to) {
        return Ok(());
    }
    for path in stale_copies(state, intent)? {
        match fs::remove_file(super::upper_path(&state.upper(), &path)) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

/// Whether the workspace's index is at C' (`Some(true)`) or at C
/// (`Some(false)`) on the paths the move changes, or neither.
fn index_at(state: &StateDir, intent: &Intent) -> Result<Option<bool>> {
    let paths = changed_paths(intent)?;
    // C and C' have the same tree there: the index is at both.
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
pub fn index_matches(state: &StateDir, commit: ObjectId, paths: &[BString]) -> Result<bool> {
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

/// What a move writes into git's index lock (`IndexLock`), so `recover`
/// tells one a crash left from git's own.
const INDEX_LOCK_MARK: &[u8] = b"gitfold: a move of this workspace holds its index\n";

/// git's own lock on the index of a workspace (`index.lock`), held by its
/// move from the index step through the refs (`fs::workspace::layers`): a
/// `git commit` in between would commit the moved index onto the old tip,
/// the layer's new commits squashed into one of the branch's own. It fails
/// instead, as with any git command holding the lock. Removed when
/// dropped; one a crash left, by `recover` (it is marked as a move's).
pub struct IndexLock {
    path: PathBuf,
}

impl IndexLock {
    /// Takes it for the workspace in `state`; `None` if git holds it.
    pub fn take(state: &StateDir) -> Result<Option<IndexLock>> {
        let path = state.admin.join("index.lock");
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        };
        let lock = IndexLock { path };
        file.write_all(INDEX_LOCK_MARK)?;
        Ok(Some(lock))
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if let Err(e) = remove(&self.path) {
            log::warn!("removing {}: {e:#}", self.path.display());
        }
    }
}

/// Removes the index lock a move of the workspace in `state` left (a crash
/// while it held it), unless a move runs: git counts as busy until then,
/// which would keep the recovery away.
pub fn release_left_index_lock(state: &StateDir) -> Result<()> {
    let file = match fs::File::open(state.dir.join(INTENT)) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    match crate::lock::flock(&file)? {
        true => drop_left_index_lock(state),
        false => Ok(()),
    }
}

/// Removes the index lock of the workspace in `state` if a move left it
/// (a crash while it held it): the move's intent held, no move runs.
fn drop_left_index_lock(state: &StateDir) -> Result<()> {
    let path = state.admin.join("index.lock");
    if fs::read(&path).is_ok_and(|data| data == INDEX_LOCK_MARK) {
        log::warn!(
            "{}: removing the index lock an interrupted move left",
            state.branch
        );
        remove(&path)?;
    }
    Ok(())
}

/// Moves the index of the workspace in `state` from C to C' (`back`: from
/// C' to C) on the paths it changes, without its files: each entry C' has there
/// (none: removed), as `git update-index --index-info` sets them. With
/// `locked` (the move holds git's lock on it), written through a copy
/// renamed over it, as git itself would.
pub fn move_index(
    state: &StateDir,
    intent: &Intent,
    back: bool,
    locked: Option<&IndexLock>,
) -> Result<()> {
    let to = match back {
        false => intent.to,
        true => intent.from,
    };
    let tree = git::resolve(&to.to_string())?.tree;
    let mut input = Vec::new();
    for path in changed_paths(intent)? {
        let item = git::with_repo(|repo| git::layers::lookup(repo, tree, path.as_ref()))?;
        match item {
            Some(item) => {
                let mode = match item.kind {
                    gix::objs::tree::EntryKind::BlobExecutable => "100755",
                    gix::objs::tree::EntryKind::Link => "120000",
                    gix::objs::tree::EntryKind::Commit => "160000",
                    _ => "100644",
                };
                input.extend_from_slice(format!("{mode} {}\t", item.oid).as_bytes());
            }
            None => input.extend_from_slice(b"0 0000000000000000000000000000000000000000\t"),
        }
        input.extend_from_slice(&path);
        input.push(0);
    }
    if input.is_empty() {
        return Ok(());
    }
    let args = ["update-index", "-z", "--index-info"];
    if locked.is_none() {
        return cmd::output_with_input(&state.admin, &args, input).map(drop);
    }
    let index = state.index();
    let copy = state.admin.join("index.gitfold-move");
    let written = (|| -> Result<()> {
        match fs::copy(&index, &copy) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("copying {}", index.display()));
            }
            _ => {}
        }
        let env = [(
            "GIT_INDEX_FILE",
            copy.to_str().context("a path that isn't UTF-8")?,
        )];
        cmd::output_with_env_input(&state.admin, &args, &env, input)?;
        fs::File::open(&copy)?.sync_all()?;
        fs::rename(&copy, &index).with_context(|| format!("renaming {}", copy.display()))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&copy);
    }
    written
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
    fn a_finished_move_is_recovered_as_finished() {
        let (r, state, [c1, c2, c3]) = workspace("move-finished");
        let common = r.common();
        // The branch and its own ref both moved: the move got that far.
        stack::set_base(&common, "p2", c1).unwrap();
        let intent = Intent {
            from: c2,
            to: c3,
            own_to: c1,
            own_from: Some(c2),
        };
        intent.begin(&state).unwrap().keep();
        assert!(pending(&state));
        assert_eq!(settled(&state), Recovery::Finished);
        assert!(!pending(&state), "the intent goes");
        assert_eq!(r.id("p2"), c3);
        assert_eq!(
            stack::rev(&common, &stack::own_ref("p2")).unwrap(),
            Some(c1)
        );
        // Only the own ref left to move.
        let intent = Intent {
            own_from: Some(c1),
            own_to: c2,
            ..intent
        };
        intent.begin(&state).unwrap().keep();
        assert_eq!(settled(&state), Recovery::Finished);
        assert_eq!(
            stack::rev(&common, &stack::own_ref("p2")).unwrap(),
            Some(c2)
        );
    }

    #[test]
    fn held_intents_go_unless_kept() {
        let (_r, state, [c1, c2, c3]) = workspace("move-held");
        let intent = Intent {
            from: c1,
            to: c2,
            own_to: c3,
            own_from: None,
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

    /// git's index lock is held through the move: a commit meanwhile fails
    /// rather than commit the moved index onto the old tip; one a crash
    /// left goes with the recovery.
    #[test]
    fn a_move_holds_the_index_against_commits() {
        let (r, state, [c1, c2, c3]) = workspace("move-index-lock");
        let wt = r.dir.join("gitfold-ws-p2");
        // A change staged, as the index step leaves it.
        fs::write(wt.join("f.txt"), "moved\n").unwrap();
        r.git_in(&wt, &["add", "f.txt"]);
        let lock = IndexLock::take(&state).unwrap().expect("not held");
        assert!(IndexLock::take(&state).unwrap().is_none(), "one at a time");
        let commit = r.run_in(&wt, &["commit", "-qm", "meanwhile"]);
        assert!(!commit.status.success(), "a commit meanwhile fails");
        assert!(
            String::from_utf8_lossy(&commit.stderr).contains("index.lock"),
            "on the lock"
        );
        assert_eq!(r.id("p2"), c3, "the branch didn't move");
        drop(lock);
        assert!(!state.admin.join("index.lock").exists(), "dropped: gone");
        // Killed while holding it: the recovery removes it (here the
        // branch moved on since, its index describing it).
        r.git_in(&wt, &["reset", "-q", "--hard"]);
        let intent = Intent {
            from: c1,
            to: c2,
            own_to: c1,
            own_from: None,
        };
        std::mem::forget(IndexLock::take(&state).unwrap());
        intent.begin(&state).unwrap().keep();
        assert_eq!(settled(&state), Recovery::Undone);
        assert!(!state.admin.join("index.lock").exists(), "the lock goes");
        // git's own lock stays.
        fs::write(state.admin.join("index.lock"), "").unwrap();
        intent.begin(&state).unwrap().keep();
        settled(&state);
        assert!(state.admin.join("index.lock").exists(), "git's stays");
    }

    #[test]
    fn an_unreadable_intent_is_stuck() {
        let (_r, state, _) = workspace("move-unreadable");
        fs::write(state.dir.join(INTENT), "junk\n").unwrap();
        let Recovery::Stuck(why) = settled(&state) else {
            panic!("not stuck")
        };
        assert!(why.contains("can't read"), "{why}");
        assert!(pending(&state), "left for `forget`");
    }

    #[test]
    fn an_intent_the_branch_moved_past_is_forgotten() {
        let (r, state, [c1, c2, _]) = workspace("move-moved");
        // The branch is at c3 and its index describes it: neither C nor C'.
        let intent = Intent {
            from: c1,
            to: c2,
            own_to: c1,
            own_from: None,
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
            why.contains("`gitfold layer remove --all -w p2` forgets"),
            "{why}"
        );
        assert!(pending(&state));
    }

    #[test]
    fn intents_read_back() {
        let id = |n: u8| ObjectId::from_bytes_or_panic(&[n; 20]);
        for intent in [
            Intent {
                from: id(1),
                to: id(2),
                own_to: id(3),
                own_from: Some(id(4)),
            },
            Intent {
                from: id(1),
                to: id(2),
                own_to: id(3),
                own_from: None,
            },
        ] {
            assert_eq!(Intent::parse(&intent.encode()).unwrap(), intent);
        }
        assert!(Intent::parse("open 01").is_err());
    }
}
