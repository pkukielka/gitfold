//! Resolution of every name below the mount root (handing `@<rev>` to
//! `snapshots`), and the registry of writable branch workspaces
//! (`<mount>/<branch>`) and namespace directories for names containing `/`
//! (`<mount>/feature/` for branch `feature/x`, `<mount>/@origin/` for rev
//! `origin/main`).
//!
//! Git forbids `feature` and `feature/x` coexisting as branches, so at most one
//! prefix of a path can be a branch: walking components, the first that is a
//! branch is the workspace, the ones before it are namespaces. For revs the
//! namespace reading wins over git's shorthand (`origin` for `origin/HEAD`), or
//! `origin/main` would be unreachable; `origin/HEAD` is then `@origin/HEAD`.
//!
//! - `requests`: the requests for `gitfold branch --with-changes` while
//!   mounted, and their busy marks.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

mod requests;

use fuser::Errno;
use log::{error, info, warn};
use parking_lot::{Mutex, RwLock};

use super::snapshots::Snapshots;
use super::snapshots::load_failed;
use super::workspace::{Workspace, reads};
use crate::daemon::prewarm::{self, Prewarm, Spawned};
use crate::git;
use crate::git::index::{IndexWriter, StatInfo};
use crate::git::promisor;
use crate::git::worktree::{self, workspace_id};
use crate::inode;
use crate::refusal::Refusal;
use crate::state::StateDir;
use crate::store::Store;

/// What a slot holds, for the inodes in it (`Workspaces::get`).
pub enum Slot {
    Workspace(Arc<Workspace>),
    /// A namespace directory, with its full path (e.g. `feature`).
    Namespace(String),
}

/// What is registered in a slot.
pub enum Registered {
    Workspace(Arc<Workspace>),
    /// A directory holding the next component of longer names, e.g. `feature`
    /// or `@origin`.
    Namespace,
    /// A workspace being removed (`Workspaces::remove`): its inodes answer
    /// EBUSY, which the kernel doesn't cache, unlike ENOENT (a negative entry
    /// for a file that exists, if the removal is refused). Forgets still reach
    /// it.
    Removing(Arc<Workspace>),
    /// A removed workspace: its slot (so its inodes) is never reused, and its
    /// inodes the kernel still holds answer ENOENT.
    Removed,
}

pub struct Workspaces {
    common_dir: PathBuf,
    mount_point: PathBuf,
    stat: StatInfo,
    prewarm: Prewarm,
    prewarm_threads: usize,
    /// Whether workspaces record reads unless their branch says (`reads`).
    track_reads: bool,
    /// Indexed by slot; (full path below the mount root, entry).
    list: RwLock<Vec<(String, Registered)>>,
    by_path: Mutex<HashMap<String, u32>>,
    /// Serializes workspace creation.
    creating: Mutex<()>,
    /// Branches whose workspace is being removed, checked while closed, or
    /// worked on by a request (`mark_busy`): not opened, removed or worked on
    /// again meanwhile.
    busy: Mutex<HashMap<String, Marks>>,
    /// Taken shared by requests that may take long (copying a workspace)
    /// while they work, exclusively by `close_requests` at unmount, which waits
    /// for them: a fork cut off half-way would leave a workspace without its
    /// branch.
    requests: RwLock<bool>,
    /// Bumped whenever an entry is added, invalidating cached namespace listings.
    mtime: Mutex<SystemTime>,
}

impl Workspaces {
    pub fn new(
        common_dir: PathBuf,
        mount_point: PathBuf,
        stat: StatInfo,
        prewarm: Prewarm,
        prewarm_threads: usize,
        track_reads: bool,
    ) -> Self {
        Workspaces {
            common_dir,
            mount_point,
            stat,
            prewarm,
            prewarm_threads,
            track_reads,
            list: RwLock::new(Vec::new()),
            by_path: Mutex::new(HashMap::new()),
            creating: Mutex::new(()),
            busy: Mutex::new(HashMap::new()),
            requests: RwLock::new(false),
            mtime: Mutex::new(SystemTime::now()),
        }
    }

    /// How many workspaces and namespace directories were added; only grows
    /// (removed ones keep their slot).
    pub fn len(&self) -> usize {
        self.list.read().len()
    }

    pub fn mtime(&self) -> SystemTime {
        *self.mtime.lock()
    }

    /// What `slot` holds: a workspace, or a namespace directory.
    pub fn get(&self, slot: u32) -> Result<Slot, Errno> {
        let list = self.list.read();
        let (path, entry) = list.get(slot as usize).ok_or(Errno::ENOENT)?;
        Ok(match entry {
            Registered::Workspace(ws) => Slot::Workspace(ws.clone()),
            Registered::Namespace => Slot::Namespace(path.clone()),
            Registered::Removing(_) => return Err(Errno::EBUSY),
            Registered::Removed => return Err(Errno::ENOENT),
        })
    }

    /// Brings every open workspace's base up to the branch tip (see
    /// `Workspace::sync`), so the state left behind at unmount is current.
    pub fn sync_all(&self, store: &Store) {
        let list = self.list.read();
        for (path, entry) in list.iter() {
            if let Registered::Workspace(ws) = entry
                && let Err(e) = ws.sync_at_unmount(store)
            {
                match e.chain().any(|c| c.is::<git::Missing>()) {
                    true => {
                        info!("workspace {path}: base left behind the branch tip at unmount: {e:#}")
                    }
                    false => warn!(
                        "workspace {path}: moving the base to the branch tip at unmount: {e:#}"
                    ),
                }
            }
        }
    }

    /// Inodes of the namespace directories.
    pub fn namespace_inodes(&self) -> Vec<u64> {
        let list = self.list.read();
        (list.iter().enumerate())
            .filter(|(_, (_, entry))| matches!(entry, Registered::Namespace))
            .map(|(slot, _)| inode::ws(slot as u32, 0))
            .collect()
    }

    pub fn is_namespace(&self, slot: u32) -> bool {
        matches!(
            self.list.read().get(slot as usize),
            Some((_, Registered::Namespace))
        )
    }

    /// (inode, last component) of the entries directly below `prefix` ("" for
    /// the root).
    pub fn children(&self, prefix: &str) -> Vec<(u64, Vec<u8>)> {
        let list = self.list.read();
        (list.iter().enumerate())
            .filter(|(_, (_, entry))| {
                !matches!(entry, Registered::Removing(_) | Registered::Removed)
            })
            .filter_map(|(slot, (path, _))| {
                let rest = super::child_name(prefix, path.as_bytes())?;
                Some((inode::ws(slot as u32, 0), rest.to_vec()))
            })
            .collect()
    }

    fn add(&self, path: &str, entry: Registered) -> u32 {
        let mut list = self.list.write();
        list.push((path.to_string(), entry));
        let slot = (list.len() - 1) as u32;
        self.by_path.lock().insert(path.to_string(), slot);
        *self.mtime.lock() = SystemTime::now();
        slot
    }

    /// Walks the workspace of `branch` (see `prewarm`), in its own mode if set,
    /// if it is open. `--prewarm none` for the mount wins over a branch's mode.
    pub fn prewarm(&self, branch: &str) {
        if self.prewarm == Prewarm::None || self.get_open(branch).is_none() {
            return;
        }
        let mode = self.branch_mode(branch);
        prewarm::spawn(self.mount_point.join(branch), mode, self.prewarm_threads);
    }

    /// The prewarm mode of the workspace of `branch`: its own if set, else the
    /// mount's. With reads tracked, `content` walks as `meta`: a walk reading
    /// every file first would leave the agent's reads to the kernel's cache,
    /// unseen.
    fn branch_mode(&self, branch: &str) -> Prewarm {
        match prewarm::for_branch(&self.common_dir, branch).unwrap_or(self.prewarm) {
            Prewarm::Content if self.tracks_reads(branch) => {
                warn!("workspace {branch}: prewarm content walks as meta while reads are tracked");
                Prewarm::Meta
            }
            mode => mode,
        }
    }

    fn tracks_reads(&self, branch: &str) -> bool {
        reads::tracked(&self.common_dir, branch, self.track_reads)
    }

    /// `gitfold track-reads`: starts or stops recording reads in the workspace
    /// of `branch` as its setting (just set) says, if it is open. Says what it
    /// did.
    pub fn track_reads_now(&self, branch: &str) -> anyhow::Result<String> {
        let Some(ws) = self.get_open(branch) else {
            return Ok(format!("{branch} isn't open: applied when it opens"));
        };
        let on = self.tracks_reads(branch);
        ws.track_reads(on)?;
        // A content walk running would be recorded as the agent's reads: walked
        // again as meta (see `branch_mode`).
        let path = self.mount_point.join(branch);
        let content = prewarm::for_branch(&self.common_dir, branch).unwrap_or(self.prewarm)
            == Prewarm::Content;
        if on && content && self.prewarm != Prewarm::None && prewarm::stop_under(&path) {
            prewarm::spawn(path, Prewarm::Meta, self.prewarm_threads);
        }
        Ok(match on {
            // Files already read are cached by the kernel: their next reads
            // aren't seen.
            true => format!("recording reads in {branch} from now on"),
            false => format!("stopped recording reads in {branch}"),
        })
    }

    /// `gitfold prewarm`: walks the workspace of `branch` in its mode (just set)
    /// if it is open, from the start (a walk in another mode may be running),
    /// or stops walking it for `none`. Says what it did.
    pub fn prewarm_now(&self, branch: &str) -> String {
        let path = self.mount_point.join(branch);
        if self.get_open(branch).is_none() {
            return format!("{branch} isn't open: walked when it opens");
        }
        match self.branch_mode(branch) {
            Prewarm::None => match prewarm::stop_under(&path) {
                true => format!("stopped walking {}", path.display()),
                false => format!("{} isn't being walked", path.display()),
            },
            _ if self.prewarm == Prewarm::None => {
                "this mount walks nothing (--prewarm none): remount to walk".to_string()
            }
            mode => {
                prewarm::stop_under(&path);
                let spawned = prewarm::spawn(path.clone(), mode, self.prewarm_threads);
                let path = path.display();
                match spawned {
                    Spawned::Now => format!("walking {path} ({mode})"),
                    Spawned::Queued => format!("{path} queued for a walk ({mode})"),
                    Spawned::Paused => {
                        format!(
                            "walks are stopped for an unmount: {path} is walked ({mode}) if it fails"
                        )
                    }
                    // Started meanwhile, by another request.
                    Spawned::Known => format!("{path} is being walked"),
                }
            }
        }
    }

    /// Runs `check` for the workspace of `branch` unless something is open
    /// under that name (then `None`), while it can't be opened: lookups of it
    /// answer EBUSY.
    pub fn check_closed<T>(
        &self,
        branch: &str,
        check: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        {
            let _creating = self.creating.lock();
            if self.by_path.lock().contains_key(branch) {
                return Ok(None);
            }
            if !self.try_mark(branch, Mark::Exclusive) {
                return Err(busy_error(branch));
            }
        }
        // Unmarked even if `check` panics.
        let _busy = Busy {
            set: &self.busy,
            branch: branch.to_string(),
            mark: Mark::Exclusive,
        };
        check().map(Some)
    }

    /// The open workspace of `branch`.
    pub fn get_open(&self, branch: &str) -> Option<Arc<Workspace>> {
        let slot = *self.by_path.lock().get(branch)?;
        match &self.list.read()[slot as usize].1 {
            Registered::Workspace(ws) => Some(ws.clone()),
            _ => None,
        }
    }

    /// The workspace in `slot`, also while being removed: for forgets, which
    /// must not be lost if the removal is refused.
    pub fn get_for_forget(&self, slot: u32) -> Option<Arc<Workspace>> {
        match &self.list.read().get(slot as usize)?.1 {
            Registered::Workspace(ws) | Registered::Removing(ws) => Some(ws.clone()),
            _ => None,
        }
    }

    /// The inode of the directory holding `path` (the root or a namespace),
    /// and `path`'s last component.
    pub fn parent_and_leaf<'a>(&self, path: &'a str) -> (u64, &'a str) {
        let Some((parent, leaf)) = path.rsplit_once('/') else {
            return (inode::ROOT, path);
        };
        let slot = self.by_path.lock().get(parent).copied();
        (slot.map_or(inode::ROOT, |slot| inode::ws(slot, 0)), leaf)
    }

    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Removes the workspace of `branch` if `remove` succeeds: `remove(open)`
    /// deletes its stored state, given the workspace if it is open. An open one
    /// is taken out first, so no request reaches it (requests in flight finish
    /// first, and its sync thread stops), then `refuse_if_used()` may refuse,
    /// and it is put back if either fails. Meanwhile lookups of `branch` answer
    /// EBUSY, so it isn't opened again. Returns `remove`'s result and whether
    /// it was open.
    pub fn remove<T>(
        &self,
        branch: &str,
        refuse_if_used: impl FnOnce() -> anyhow::Result<()>,
        remove: impl FnOnce(Option<&Workspace>) -> anyhow::Result<T>,
    ) -> anyhow::Result<(T, bool)> {
        let taken = self.take(branch)?;
        let ws = taken.as_ref().map(|(_, ws)| ws);
        let result = match ws {
            Some(ws) => settle(branch, ws, refuse_if_used),
            None => Ok(()),
        }
        .and_then(|()| remove(ws.map(|ws| &**ws)));
        let _creating = self.creating.lock();
        unmark(&self.busy, branch, Mark::Exclusive);
        let open = taken.is_some();
        if let Some((slot, ws)) = taken {
            let entry = &mut self.list.write()[slot as usize].1;
            if result.is_ok() {
                *entry = Registered::Removed;
            } else {
                ws.removing.store(false, Ordering::Release);
                *entry = Registered::Workspace(ws);
                self.by_path.lock().insert(branch.to_string(), slot);
            }
            // Listings without it (taken out) or with it (removed) are stale.
            *self.mtime.lock() = SystemTime::now();
        }
        result.map(|t| (t, open))
    }

    /// Marks `branch` as being removed, and takes its workspace out of the
    /// registry if open.
    fn take(&self, branch: &str) -> anyhow::Result<Option<(u32, Arc<Workspace>)>> {
        let _creating = self.creating.lock();
        if !self.try_mark(branch, Mark::Exclusive) {
            return Err(busy_error(branch));
        }
        let Some(slot) = self.by_path.lock().get(branch).copied() else {
            return Ok(None);
        };
        let entry = &mut self.list.write()[slot as usize].1;
        let ws = match std::mem::replace(entry, Registered::Removed) {
            Registered::Workspace(ws) => {
                *entry = Registered::Removing(ws.clone());
                ws
            }
            other => {
                *entry = other;
                unmark(&self.busy, branch, Mark::Exclusive);
                return Err(Refusal::NotFound.error(format!(
                    "{branch} is a namespace directory, not a workspace"
                )));
            }
        };
        self.by_path.lock().remove(branch);
        ws.removing.store(true, Ordering::Release);
        Ok(Some((slot, ws)))
    }

    /// Resolves a name below the mount root (`path` has no leading `/`; names
    /// in namespaces are full paths, like `feature/x` or `@origin/main`).
    pub fn resolve(
        &self,
        store: &Arc<Store>,
        snapshots: &Snapshots,
        path: &str,
    ) -> Result<u64, Errno> {
        if let Some(&slot) = self.by_path.lock().get(path) {
            return Ok(inode::ws(slot, 0));
        }
        if let Some(spec) = path.strip_prefix('@') {
            if let Some(ino) = snapshots.existing(path.as_bytes()) {
                return Ok(ino);
            }
            let prefixes = ["refs/heads/", "refs/remotes/", "refs/tags/", "refs/"]
                .map(|base| format!("{base}{spec}/"));
            if git::any_ref_under(&prefixes) {
                return Ok(self.namespace(path));
            }
            return snapshots.resolve(store, path.as_bytes());
        }
        // Git forbids ref components starting with `.`; this skips the ref lookups
        // for the `.git`, `.ignore`, ... probes tools send to every directory.
        if path.split('/').any(|c| c.starts_with('.')) {
            return Err(Errno::ENOENT);
        }
        if git::branch_exists(path) {
            // Cheap refusals first: before loading (and fetching) its trees.
            self.refuse_open(path)?;
            self.preload(store, path)?;
            return self.open_workspace(store, path);
        }
        if git::any_ref_under(&[format!("refs/heads/{path}/")]) {
            return Ok(self.namespace(path));
        }
        Err(Errno::ENOENT)
    }

    fn namespace(&self, path: &str) -> u64 {
        let _creating = self.creating.lock();
        if let Some(&slot) = self.by_path.lock().get(path) {
            return inode::ws(slot, 0);
        }
        inode::ws(self.add(path, Registered::Namespace), 0)
    }

    /// The branch's checkout elsewhere, if any: git allows one checkout per branch.
    /// A worktree in the middle of a rebase or bisect of the branch (detached
    /// meanwhile) counts too, as for git.
    fn checked_out_elsewhere(&self, branch: &str, own_admin: &Path) -> Option<String> {
        let full = format!("refs/heads/{branch}");
        let head = format!("ref: {full}");
        let reads = |dir: &Path, file: &str, want: &str| {
            fs::read_to_string(dir.join(file)).is_ok_and(|s| s.trim() == want)
        };
        let is_ours = |dir: &Path| {
            reads(dir, "HEAD", &head)
                || reads(dir, "rebase-merge/head-name", &full)
                || reads(dir, "rebase-apply/head-name", &full)
                || reads(dir, "BISECT_START", branch)
        };
        // A bare repository's HEAD names a branch but checks nothing out.
        if !git::is_bare() && is_ours(&self.common_dir) {
            return Some("the main worktree".into());
        }
        let worktrees = fs::read_dir(self.common_dir.join("worktrees")).ok()?;
        worktrees.flatten().find_map(|w| {
            let name = w.file_name().to_string_lossy().into_owned();
            (w.path() != own_admin && is_ours(&w.path())).then(|| {
                fs::read_to_string(w.path().join("gitdir"))
                    .map_or(name, |g| g.trim().trim_end_matches("/.git").to_string())
            })
        })
    }

    /// Loads the trees opening the workspace of `branch` needs (its tip and, if
    /// it resumes, its stored base), before `open_workspace` takes the creation
    /// lock: a partial clone fetches what is missing, which can take long.
    /// Failing here leaves no state behind.
    fn preload(&self, store: &Store, branch: &str) -> Result<(), Errno> {
        let state_dir = StateDir::of(&self.common_dir, branch).dir;
        let load = |commit: &git::Commit| {
            store
                .load_tree(commit.tree)
                .map_err(|e| load_failed(&format!("loading workspace {branch}"), e))
        };
        let tip = git::resolve(&format!("refs/heads/{branch}")).ok();
        match super::workspace::read_base(&state_dir).ok() {
            // Resuming needs only the base: the sync thread moves it to the tip,
            // fetching then if it can't now.
            Some(base) => {
                load(&base)?;
                if let Some(tip) = tip {
                    let _ = load(&tip);
                }
            }
            None => {
                if let Some(tip) = tip {
                    load(&tip)?;
                }
            }
        }
        Ok(())
    }

    /// Waits (lock-free, until `OPEN_WAIT` after `started`) while requests read
    /// `branch` (a copy of the closed workspace), so an agent's open
    /// doesn't fail because a supervisor read it; EBUSY while it is marked
    /// exclusively (removed, checked, copied into).
    fn wait_for_readers(&self, branch: &str, started: Instant) -> Result<(), Errno> {
        loop {
            match self.busy.lock().get(branch) {
                Some(m) if m.exclusive => return Err(Errno::EBUSY),
                Some(m) if m.readers > 0 => {}
                _ => return Ok(()),
            }
            if started.elapsed() > OPEN_WAIT {
                return Err(Errno::EBUSY);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// EBUSY if the workspace of `branch` can't be opened now: it is being
    /// removed, or the branch is checked out elsewhere (git allows one checkout
    /// per branch).
    fn refuse_open(&self, branch: &str) -> Result<(), Errno> {
        self.wait_for_readers(branch, Instant::now())?;
        let admin = worktree::ws_admin_dir(&self.common_dir, &workspace_id(branch));
        if let Some(other) = self.checked_out_elsewhere(branch, &admin) {
            warn!(
                "branch {branch} is already checked out in {other}; create another branch from it"
            );
            return Err(Errno::EBUSY);
        }
        Ok(())
    }

    fn open_workspace(&self, store: &Arc<Store>, branch: &str) -> Result<u64, Errno> {
        // Never waits for readers while holding `creating`: a copy marks its
        // source a reader, and every open, removal and request needs `creating`.
        let started = Instant::now();
        let _creating = loop {
            let creating = self.creating.lock();
            if let Some(&slot) = self.by_path.lock().get(branch) {
                return Ok(inode::ws(slot, 0));
            }
            if !self.busy.lock().get(branch).is_some_and(|m| m.readers > 0) {
                break creating;
            }
            drop(creating);
            self.wait_for_readers(branch, started)?;
        };
        self.refuse_open(branch)?;
        let id = workspace_id(branch);
        let admin = worktree::ws_admin_dir(&self.common_dir, &id);
        let fail = |what: &str, e: anyhow::Error| {
            load_failed(&format!("{what} for workspace {branch}"), e)
        };
        let state = StateDir::of_id(&self.common_dir, &id);
        let new_base = if state.opened() {
            None
        } else {
            Some(git::resolve(&format!("refs/heads/{branch}")).map_err(|e| fail("resolving", e))?)
        };
        let created = new_base.is_some();
        let path = self.mount_point.join(branch);
        // The gitfile content must be known before the workspace exists; the admin
        // directory name is deterministic.
        let gitfile = format!("gitdir: {}\n", admin.display()).into_bytes();
        let ws = Workspace::open(
            store,
            self.common_dir.clone(),
            state.dir,
            admin.clone(),
            branch,
            new_base,
            gitfile,
        )
        .map_err(|e| fail("opening", e))?;
        if let Err(e) = ws.track_reads(self.tracks_reads(branch)) {
            warn!("workspace {branch}: not recording reads: {e:#}");
        }
        let (base, base_tree) = ws.base();
        // Nothing else keeps the base reachable once the branch moves on (amend,
        // rebase, reset): without the ref, `git gc` could prune it.
        ws.pin_base().map_err(|e| fail("pinning base", e))?;
        // Written only when the registration is (re)created. It must describe what
        // HEAD points at, the branch tip: that is the base for a new workspace,
        // and if the registration was deleted after commits made here, the tip's
        // files are the ones upper holds (the old base would show them as changed).
        worktree::register_branch(&admin, branch, &path.join(".git"), || {
            let tip = git::resolve(&format!("refs/heads/{branch}")).ok();
            // A load under `creating`, but rarely a fetch: `preload` loaded the tip
            // (or it is in backoff, refused at once), unless the branch moved since.
            let tree = tip.and_then(|c| store.load_tree(c.tree).ok());
            let dirs = store.dir_table(tree.unwrap_or(base_tree));
            let mut writer = IndexWriter::new(self.stat, 1 << 20);
            // Workspace inodes come from the path, so the index can be written up front.
            store.write_index(&dirs, &mut writer, |path, _, _| {
                inode::path_local(path) as u32
            });
            writer.finish()
        })
        .map_err(|e| fail("registering worktree", e))?;
        info!(
            "workspace {branch}: {} at {}",
            if created { "created" } else { "resumed" },
            base
        );
        let ws = Arc::new(ws);
        let slot = self.add(branch, Registered::Workspace(ws.clone()));
        watch(Arc::downgrade(&ws), store.clone(), branch.to_string());
        self.prewarm(branch);
        Ok(inode::ws(slot, 0))
    }
}

/// How long removing a workspace waits for requests in flight in it.
const IN_FLIGHT_WAIT: Duration = Duration::from_secs(5);

/// How often workspaces check whether their branch moved (see `workspace::rebase`).
const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// Keeps the workspace's base at its branch tip while the workspace exists.
fn watch(ws: Weak<Workspace>, store: Arc<Store>, branch: String) {
    let name = format!("sync {branch}");
    let spawned = thread::Builder::new().name(name).spawn({
        let branch = branch.clone();
        move || {
            let mut last_error = String::new();
            loop {
                thread::sleep(SYNC_INTERVAL);
                let Some(ws) = ws.upgrade() else { return };
                if ws.removing.load(Ordering::Acquire) {
                    continue;
                }
                match ws.sync(&store) {
                    Ok(()) => last_error.clear(),
                    Err(e) if promisor::is_backoff(&e) => {
                        log::debug!("workspace {branch}: moving the base to the branch tip: {e:#}");
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        if e != last_error {
                            error!("workspace {branch}: moving the base to the branch tip: {e}");
                            last_error = e;
                        }
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        error!("workspace {branch}: can't start its sync thread: {e}");
    }
}

/// Waits until requests in flight in taken-out workspace `ws` (of `branch`)
/// finished (the sync thread's too; the registry holds one reference, the
/// caller another), then asks `refuse_if_used`.
fn settle(
    branch: &str,
    ws: &Arc<Workspace>,
    refuse_if_used: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    while Arc::strong_count(ws) > 2 && started.elapsed() < IN_FLIGHT_WAIT {
        thread::sleep(Duration::from_millis(10));
    }
    if Arc::strong_count(ws) > 2 {
        return Err(Refusal::InUse.error(format!("workspace {branch} is busy; try again")));
    }
    refuse_if_used()
}

/// How long opening a workspace waits for requests reading it.
const OPEN_WAIT: Duration = Duration::from_secs(30);

/// How a request marks a branch busy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// Reading it (the source of a copy): any number at once.
    Reader,
    /// Removing, checking or writing it: alone.
    Exclusive,
}

/// A branch's marks in `Workspaces::busy`.
#[derive(Default, PartialEq)]
struct Marks {
    /// Requests reading it (the source of a copy).
    readers: usize,
    /// Being removed, checked while closed, or copied into.
    exclusive: bool,
}

/// A branch marked busy by `Workspaces::mark_busy`, until dropped.
struct Busy<'a> {
    set: &'a Mutex<HashMap<String, Marks>>,
    branch: String,
    mark: Mark,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        unmark(self.set, &self.branch, self.mark);
    }
}

/// Undoes a mark of `branch` (see `Workspaces::try_mark`).
fn unmark(set: &Mutex<HashMap<String, Marks>>, branch: &str, mark: Mark) {
    let mut busy = set.lock();
    if let Some(marks) = busy.get_mut(branch) {
        match mark {
            Mark::Reader => marks.readers = marks.readers.saturating_sub(1),
            Mark::Exclusive => marks.exclusive = false,
        }
        if *marks == Marks::default() {
            busy.remove(branch);
        }
    }
}

/// The refusal of a branch busy with something else.
fn busy_error(branch: &str) -> anyhow::Error {
    let message = format!("{branch} is busy (being removed, checked or copied); retry");
    Refusal::InUse.error(message)
}
