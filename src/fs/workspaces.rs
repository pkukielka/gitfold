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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use fuser::Errno;
use log::{error, info, warn};
use parking_lot::{Mutex, RwLock};

use super::snapshots::Snapshots;
use super::workspace::Workspace;
use crate::git;
use crate::inode;
use crate::prewarm::{self, Prewarm};
use crate::store::Store;
use crate::worktree::{self, IndexWriter, StatInfo, workspace_id};

pub enum Entry {
    Workspace(Arc<Workspace>),
    /// A directory holding the next component of longer names, e.g. `feature`
    /// or `@origin`.
    Namespace,
}

pub struct Workspaces {
    common_dir: PathBuf,
    mount_point: PathBuf,
    stat: StatInfo,
    prewarm: Prewarm,
    prewarm_threads: usize,
    /// Indexed by slot; (full path below the mount root, entry).
    list: RwLock<Vec<(String, Entry)>>,
    by_path: Mutex<HashMap<String, u32>>,
    /// Serializes workspace creation.
    creating: Mutex<()>,
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
    ) -> Self {
        Workspaces {
            common_dir,
            mount_point,
            stat,
            prewarm,
            prewarm_threads,
            list: RwLock::new(Vec::new()),
            by_path: Mutex::new(HashMap::new()),
            creating: Mutex::new(()),
            mtime: Mutex::new(SystemTime::now()),
        }
    }

    /// How many workspaces and namespace directories there are; only grows.
    pub fn len(&self) -> usize {
        self.list.read().len()
    }

    pub fn mtime(&self) -> SystemTime {
        *self.mtime.lock()
    }

    /// The workspace in `slot`, or the full path of the namespace there.
    pub fn get(&self, slot: u32) -> Result<Result<Arc<Workspace>, String>, Errno> {
        let list = self.list.read();
        let (path, entry) = list.get(slot as usize).ok_or(Errno::ENOENT)?;
        Ok(match entry {
            Entry::Workspace(ws) => Ok(ws.clone()),
            Entry::Namespace => Err(path.clone()),
        })
    }

    /// Brings every open workspace's base up to the branch tip (see
    /// `Workspace::sync`), so the state left behind at unmount is current.
    pub fn sync_all(&self, store: &Store) {
        let list = self.list.read();
        for (path, entry) in list.iter() {
            if let Entry::Workspace(ws) = entry
                && let Err(e) = ws.sync(store)
            {
                warn!("workspace {path}: moving the base to the branch tip at unmount: {e:#}");
            }
        }
    }

    /// Inodes of the namespace directories.
    pub fn namespace_inodes(&self) -> Vec<u64> {
        let list = self.list.read();
        (list.iter().enumerate())
            .filter(|(_, (_, entry))| matches!(entry, Entry::Namespace))
            .map(|(slot, _)| inode::ws(slot as u32, 0))
            .collect()
    }

    pub fn is_namespace(&self, slot: u32) -> bool {
        matches!(
            self.list.read().get(slot as usize),
            Some((_, Entry::Namespace))
        )
    }

    /// (inode, last component) of the entries directly below `prefix` ("" for
    /// the root).
    pub fn children(&self, prefix: &str) -> Vec<(u64, Vec<u8>)> {
        let list = self.list.read();
        (list.iter().enumerate())
            .filter_map(|(slot, (path, _))| {
                let rest = super::child_name(prefix, path.as_bytes())?;
                Some((inode::ws(slot as u32, 0), rest.to_vec()))
            })
            .collect()
    }

    fn add(&self, path: &str, entry: Entry) -> u32 {
        let mut list = self.list.write();
        list.push((path.to_string(), entry));
        let slot = (list.len() - 1) as u32;
        self.by_path.lock().insert(path.to_string(), slot);
        *self.mtime.lock() = SystemTime::now();
        slot
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
        inode::ws(self.add(path, Entry::Namespace), 0)
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

    fn open_workspace(&self, store: &Arc<Store>, branch: &str) -> Result<u64, Errno> {
        let _creating = self.creating.lock();
        if let Some(&slot) = self.by_path.lock().get(branch) {
            return Ok(inode::ws(slot, 0));
        }
        let id = workspace_id(branch);
        let admin = worktree::ws_admin_dir(&self.common_dir, &id);
        if let Some(other) = self.checked_out_elsewhere(branch, &admin) {
            warn!(
                "branch {branch} is already checked out in {other}; create another branch from it"
            );
            return Err(Errno::EBUSY);
        }
        let fail = |what: &str, e: anyhow::Error| {
            error!("{what} for workspace {branch}: {e:#}");
            Errno::EIO
        };
        let state_dir = worktree::state_root(&self.common_dir).join(&id);
        let new_base = if state_dir.join("base").exists() {
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
            state_dir,
            admin.clone(),
            branch,
            new_base,
            gitfile,
        )
        .map_err(|e| fail("opening", e))?;
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
        let slot = self.add(branch, Entry::Workspace(ws.clone()));
        watch(Arc::downgrade(&ws), store.clone(), branch.to_string());
        prewarm::spawn(path, self.prewarm, self.prewarm_threads);
        Ok(inode::ws(slot, 0))
    }
}

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
                match ws.sync(&store) {
                    Ok(()) => last_error.clear(),
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
