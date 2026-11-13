//! A writable workspace: a branch checked out as a linked worktree, shown as its
//! base commit's tree (the lower layer, from the shared store) overlaid with the
//! files changed in it (the upper layer: real files under
//! `.git/gitfold/<id>/upper`).
//!
//! Deleting a lower entry records a whiteout for its path; a directory created
//! over a whiteout is opaque (its lower children stay hidden). Whiteouts and
//! opaque markers are persisted next to `upper` (`markers`), so a workspace
//! survives unmount.
//!
//! Inodes are `inode::ws(slot, local)`, with `local` derived from the path at
//! first lookup (stable across remounts, so the workspace's git index stays
//! valid) and kept for the node's lifetime, so renames keep the inode the kernel
//! holds.
//!
//! The base follows the branch tip (`rebase`), without visible effect.
//!
//! - `nodes`: local id ↔ (parent, name) table.
//! - `fork`: copying a workspace into a new branch (`branch --with-changes`).
//! - `markers`: persisted whiteouts and opaque markers.
//! - `times`: timestamps of files dropped from upper.
//! - `rebase`: moving the base to the branch tip.
//! - `reads`: the files read in it (`gitfold overlaps`).
//! - `task`: the task folder (the `.task` link), kept apart from the changes.
//! - `status`: a workspace's changes, read from its state directory (for
//!   `list`, `changes`, `overlaps` and `remove`).
//! - `retire`: the rules for removing a workspace or a snapshot's
//!   registration.
//! - `write`: mutations and copy-up.

pub mod fork;
mod nodes;
pub mod reads;
mod rebase;
mod write;

use std::collections::{HashMap, HashSet};
use std::fs::{self, Metadata};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use fuser::Errno;
use gix::ObjectId;
use parking_lot::{Mutex, RwLock};

use crate::fsutil::not_found;
use crate::git;
use crate::git::worktree;
use crate::inode;
use crate::state::markers::{self, Markers, hidden};
use crate::state::task::TaskDir;
use crate::state::times::{self, Stamp, Times};
use crate::state::{self, StateDir, ancestors, join};
use crate::store::Lower;
use crate::store::Store;
use nodes::Nodes;

/// `local` of the virtual `.git` file in the workspace root.
pub const GITFILE_LOCAL: u64 = inode::WS_LOCAL_MASK;
/// `local` of the virtual symlink to the task folder in the workspace root.
pub const TASKLINK_LOCAL: u64 = inode::WS_LOCAL_MASK - 1;

/// Whether `local` is a virtual entry of the workspace root, without a node.
pub fn is_virtual_local(local: u64) -> bool {
    local == GITFILE_LOCAL || local == TASKLINK_LOCAL
}

/// What a path is in the merged view.
pub enum Obj {
    Upper(Metadata),
    LowerDir,
    /// `stamp`: the times it showed as before a rebase made it lower (see `rebase`).
    LowerFile {
        file_id: u32,
        stamp: Option<Stamp>,
    },
}

impl Obj {
    pub fn is_dir(&self) -> bool {
        match self {
            Obj::Upper(m) => m.is_dir(),
            Obj::LowerDir => true,
            Obj::LowerFile { .. } => false,
        }
    }
}

pub struct Workspace {
    upper: PathBuf,
    /// The task folder, unless the base tracks a path of its name (`task`).
    task: Option<TaskDir>,
    common_dir: PathBuf,
    state_dir: PathBuf,
    /// The worktree's admin directory (`.git/worktrees/gitfold-ws-<id>`).
    admin: PathBuf,
    branch: String,
    /// `refs/gitfold/<id>`, which keeps the base from `git gc`, and the commit it
    /// was last set to.
    pin_ref: String,
    pinned: Mutex<Option<ObjectId>>,
    /// The last rebase failed, maybe after copying files up, or none ran yet in
    /// this process (a killed one may have left unsynced copies; see `rebase`).
    rebase_failed: AtomicBool,
    /// Content of the virtual `.git` file in the workspace root.
    pub gitfile: Arc<Vec<u8>>,
    /// Everything the merged view is computed from but upper; also the lock
    /// every mutation takes exclusively and every read shared.
    layers: RwLock<Layers>,
    nodes: Mutex<Nodes>,
    /// Serializes copy-ups, which writers do under a shared `layers`.
    copy_lock: Mutex<()>,
    /// Held exclusively to change an upper file's mode, shared to `stat` one,
    /// so the mode `open_upper` lifts briefly is never seen (see there).
    modes: RwLock<()>,
    /// Copy-ups are written here and renamed into place, so a half-written copy
    /// is never visible in upper (or left there by a crash).
    tmp: PathBuf,
    /// Bumped by every change; cached listings of older generations are stale.
    generation: AtomicU64,
    listings: Mutex<HashMap<u64, (u64, Arc<Listing>)>>,
    /// Being removed (`Workspaces::remove`): the sync thread leaves it alone.
    pub removing: AtomicBool,
    /// Paths of upper files written since the last check for ones identical to
    /// the base again (see `rebase`).
    touched: Mutex<HashSet<Vec<u8>>>,
    /// The files read, if recorded (see `reads`).
    reads: Mutex<Option<reads::Reads>>,
}

/// The lower layer and how upper overrides it.
struct Layers {
    base: ObjectId,
    base_tree: u32,
    /// Whiteouts and opaque markers.
    markers: Markers,
    times: Times,
}

/// A directory listing: the directory's path and its merged entries, sorted by
/// name. Entries carry no local ids: nodes are claimed only for entries the
/// kernel actually receives (see `nodes`).
pub struct Listing {
    dir: Vec<u8>,
    pub entries: Vec<(Vec<u8>, ListEntry)>,
}

/// A listing entry. Upper entries are stat-ed when served, not when listed, so
/// writes into a file don't invalidate its directory's cached listing.
pub enum ListEntry {
    Upper,
    LowerDir,
    LowerFile(u32),
}

/// One page of a listing being served by `Workspace::page`, with `layers` held.
pub struct Page<'a> {
    ws: &'a Workspace,
    layers: &'a Layers,
    parent: u64,
    pub listing: Arc<Listing>,
}

impl Page<'_> {
    /// Claims the node for an entry sent in a readdirplus reply (one counted
    /// reference; `give_back` it if the entry didn't fit).
    pub fn claim(&self, name: &[u8]) -> u64 {
        self.ws
            .nodes
            .lock()
            .claim(self.parent, name, &self.listing.dir)
    }

    pub fn give_back(&self, local: u64) {
        self.ws.nodes.lock().forget(local, 1);
    }

    /// The local id of an entry, for plain readdir (not counted).
    pub fn peek(&self, name: &[u8]) -> u64 {
        self.ws
            .nodes
            .lock()
            .peek(self.parent, name, &self.listing.dir)
    }

    pub fn obj(&self, name: &[u8], entry: &ListEntry) -> Result<Obj, Errno> {
        Ok(match entry {
            ListEntry::Upper => {
                let path = self.ws.upper_path(&join(&self.listing.dir, name));
                Obj::Upper(self.ws.upper_meta(&path)?)
            }
            ListEntry::LowerDir => Obj::LowerDir,
            ListEntry::LowerFile(file_id) => Obj::LowerFile {
                file_id: *file_id,
                stamp: (!self.layers.times.is_empty())
                    .then(|| self.layers.times.get(&join(&self.listing.dir, name)))
                    .flatten(),
            },
        })
    }

    /// The kernel has read the listing to the end; drop it from the cache.
    pub fn done(&self) {
        self.ws.listings.lock().remove(&self.parent);
    }
}

/// What a read of node `local` needs.
pub enum Read {
    /// Bytes read from the upper file.
    Upper(Vec<u8>),
    /// Unchanged content: the caller reads (and caches) this file id.
    Lower(u32),
}

impl Workspace {
    /// Opens the workspace of `branch` stored in `state`, creating it at
    /// `new_base` if new.
    pub fn open(
        store: &Store,
        common_dir: PathBuf,
        state: StateDir,
        branch: &str,
        new_base: Option<git::Commit>,
    ) -> Result<Workspace> {
        let created = new_base.is_some();
        let base = match new_base {
            Some(base) => base,
            None => state.read_base()?,
        };
        // Loaded first (a partial clone fetches): a failed open leaves no state.
        let base_tree = store.load_tree(base.tree)?;
        let upper = state.upper();
        fs::create_dir_all(&upper).with_context(|| format!("creating {}", upper.display()))?;
        let task = TaskDir::of(&common_dir, branch);
        let shown = String::from_utf8_lossy(&task.name).into_owned();
        let in_upper = upper.join(std::ffi::OsStr::from_bytes(&task.name));
        let task = if store.tree(base_tree).find(&task.name).is_some() {
            log::warn!(
                "workspace {branch}: no task folder: its base tracks {shown} (set gitfold.taskDir)"
            );
            None
        } else if fs::symlink_metadata(&in_upper).is_ok() {
            log::warn!(
                "workspace {branch}: no task folder: it has a file {shown} (set gitfold.taskDir)"
            );
            None
        } else {
            match task.prepare(&common_dir) {
                Ok(()) => Some(task),
                Err(e) => {
                    log::warn!("workspace {branch}: no task folder: {e:#}");
                    None
                }
            }
        };
        if created {
            state::write_branch(&state.dir, branch)?;
            // Written last: its presence means "workspace exists".
            state::write_base(&state.dir, &base.id)?;
        }
        let layers = Layers {
            base: base.id,
            base_tree,
            markers: Markers::open(&state.markers())?,
            times: Times::open(&state.times())?,
        };
        // Copy-ups a crash interrupted; their target was never replaced.
        let tmp = state.tmp();
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
        let pin_ref = worktree::pin_ref(&state.id);
        // The registration's directory name is deterministic, so the `.git`
        // file can be written before the workspace exists.
        let gitfile = format!("gitdir: {}\n", state.admin.display()).into_bytes();
        Ok(Workspace {
            upper,
            task,
            common_dir,
            state_dir: state.dir,
            admin: state.admin,
            branch: branch.to_string(),
            pin_ref,
            pinned: Mutex::new(None),
            rebase_failed: AtomicBool::new(true),
            gitfile: Arc::new(gitfile),
            layers: RwLock::new(layers),
            nodes: Mutex::new(Nodes::default()),
            copy_lock: Mutex::new(()),
            modes: RwLock::new(()),
            tmp,
            generation: AtomicU64::new(0),
            listings: Mutex::new(HashMap::new()),
            removing: AtomicBool::new(false),
            touched: Mutex::new(HashSet::new()),
            reads: Mutex::new(None),
        })
    }

    /// Whether `name` in directory `parent` is virtual (`.git`, the task folder's
    /// link): not created, removed or renamed through the mount.
    fn is_virtual(&self, parent: u64, name: &[u8]) -> bool {
        parent == 0 && (name == b".git" || self.task_name() == Some(name))
    }

    /// The name of the task folder's link in the root, if it has one.
    pub fn task_name(&self) -> Option<&[u8]> {
        self.task.as_ref().map(|t| t.name.as_slice())
    }

    /// The task folder's link target: its directory.
    pub fn task_target(&self) -> Option<&[u8]> {
        self.task.as_ref().map(|t| t.dir.as_os_str().as_bytes())
    }

    /// The base commit and its tree.
    pub fn base(&self) -> (ObjectId, u32) {
        let layers = self.layers.read();
        (layers.base, layers.base_tree)
    }

    fn upper_path(&self, path: &[u8]) -> PathBuf {
        if path.is_empty() {
            self.upper.clone()
        } else {
            self.upper.join(std::ffi::OsStr::from_bytes(path))
        }
    }

    /// The path of node `local`. Callers that act on the result hold `layers`,
    /// so a concurrent rename can't change what it names.
    fn path(&self, local: u64) -> Result<Vec<u8>, Errno> {
        self.nodes.lock().path(local).ok_or(Errno::ENOENT)
    }

    fn lower(store: &Store, layers: &Layers, path: &[u8]) -> Option<Lower> {
        store.entry_at(layers.base_tree, path)
    }

    /// Whether the lower entry at `path` is hidden by a whiteout or an opaque
    /// ancestor.
    fn lower_hidden(layers: &Layers, path: &[u8]) -> bool {
        hidden(&layers.markers.whiteouts, &layers.markers.opaque, path)
    }

    /// The lower entry at `path`, unless hidden.
    fn visible_lower(&self, store: &Store, layers: &Layers, path: &[u8]) -> Option<Lower> {
        if Self::lower_hidden(layers, path) {
            return None;
        }
        Self::lower(store, layers, path)
    }

    /// `lstat` of an upper file, never seeing a mode `open_upper` lifted.
    fn upper_meta(&self, path: &Path) -> io::Result<Metadata> {
        let _modes = self.modes.read();
        fs::symlink_metadata(path)
    }

    fn resolve_in(&self, store: &Store, layers: &Layers, path: &[u8]) -> Result<Obj, Errno> {
        match self.upper_meta(&self.upper_path(path)) {
            Ok(meta) => return Ok(Obj::Upper(meta)),
            Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => return Err(Errno::ENOENT),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        match self.visible_lower(store, layers, path) {
            Some(Lower::Tree(_) | Lower::Gitlink) => Ok(Obj::LowerDir),
            Some(Lower::File(file_id)) => Ok(Obj::LowerFile {
                file_id,
                stamp: layers.times.get(path),
            }),
            None => Err(Errno::ENOENT),
        }
    }

    /// The object of node `local` (0: the workspace directory).
    pub fn resolve_local(&self, store: &Store, local: u64) -> Result<Obj, Errno> {
        let layers = self.layers.read();
        let path = self.path(local)?;
        self.resolve_in(store, &layers, &path)
    }

    /// Looks up `name` in directory `parent` (the adapter answers the root's
    /// `.git`) and counts the kernel's reference; returns its local id and object.
    pub fn lookup(&self, store: &Store, parent: u64, name: &[u8]) -> Result<(u64, Obj), Errno> {
        let layers = self.layers.read();
        let dir = self.path(parent)?;
        let obj = self.resolve_in(store, &layers, &join(&dir, name))?;
        Ok((self.nodes.lock().claim(parent, name, &dir), obj))
    }

    /// Serves one page of directory `local`'s listing: `f` runs with `layers`
    /// held (shared), so no mutation can interleave between building or reusing
    /// the listing, claiming nodes for its entries and stat-ing them. Without
    /// that, an `rm x` racing a readdirplus could hand the kernel a fresh node for
    /// the deleted `x`, cached for a year.
    ///
    /// The kernel reads a directory in pages of a few dozen entries, so the
    /// listing is built once and cached until `Page::done` (when a request starts
    /// past its end) or a structural change; rebuilding it per page made a
    /// 7.5k-entry directory quadratic.
    pub fn page<R>(
        &self,
        store: &Store,
        local: u64,
        f: impl FnOnce(&Page) -> R,
    ) -> Result<R, Errno> {
        let layers = self.layers.read();
        // Read before building: a copy-up (which runs under the read lock too)
        // finishing mid-build bumps it, so that listing is cached as stale.
        let generation = self.generation.load(Ordering::Acquire);
        let cached = self.listings.lock().get(&local).cloned();
        let listing = match cached {
            Some((g, listing)) if g == generation => listing,
            _ => {
                let dir = self.path(local)?;
                let entries = self.entries(store, &layers, &dir)?;
                let listing = Arc::new(Listing { dir, entries });
                self.listings
                    .lock()
                    .insert(local, (generation, listing.clone()));
                listing
            }
        };
        Ok(f(&Page {
            ws: self,
            layers: &layers,
            parent: local,
            listing,
        }))
    }

    /// The merged entries of directory `path`, sorted by name.
    fn entries(
        &self,
        store: &Store,
        layers: &Layers,
        path: &[u8],
    ) -> Result<Vec<(Vec<u8>, ListEntry)>, Errno> {
        if !self.resolve_in(store, layers, path)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        // Upper entries win; lower ones come straight from the tree table, without
        // a stat or a path walk each.
        let mut entries: Vec<(Vec<u8>, ListEntry)> = Vec::new();
        match fs::read_dir(self.upper_path(path)) {
            Ok(dir) => entries.extend(
                dir.flatten()
                    .map(|e| (e.file_name().as_bytes().to_vec(), ListEntry::Upper)),
            ),
            Err(e) if not_found(&e) => {}
            Err(e) => return Err(e.into()),
        }
        let upper_names: HashSet<Vec<u8>> = entries.iter().map(|(n, _)| n.clone()).collect();
        if !layers.markers.opaque.contains(path)
            && let Some(Lower::Tree(tree)) = self.visible_lower(store, layers, path)
        {
            let table = store.tree(tree);
            let check_whiteouts = !layers.markers.whiteouts.is_empty();
            for e in table.entries() {
                let name = table.name(e);
                if upper_names.contains(name)
                    || (check_whiteouts && layers.markers.whiteouts.contains(&join(path, name)))
                {
                    continue;
                }
                let entry = if e.is_dir() {
                    ListEntry::LowerDir
                } else {
                    ListEntry::LowerFile(e.id)
                };
                entries.push((name.to_vec(), entry));
            }
        }
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Invalidates cached listings. Called after every structural change (an
    /// entry created, removed or renamed, or a lower file copied up) after the
    /// change and while still holding `layers` (exclusively, except for copy-up):
    /// invalidating first would let a concurrent readdir cache the old listing
    /// as current. Writes into an upper file don't need it: listings re-stat
    /// upper entries when serving them.
    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        self.listings.lock().clear();
    }

    fn touched(&self, path: &[u8]) {
        let mut touched = self.touched.lock();
        if !touched.contains(path) {
            touched.insert(path.to_vec());
        }
    }

    /// The kernel dropped `n` references to `local`.
    pub fn forget(&self, local: u64, n: u64) {
        self.nodes.lock().forget(local, n);
    }

    /// Reads node `local`: upper bytes directly, or the lower file id for the
    /// caller's cache. Holds `layers` throughout, so a concurrent rename can't
    /// substitute another file's bytes (which the kernel would then cache).
    pub fn read(&self, store: &Store, local: u64, offset: u64, size: u32) -> Result<Read, Errno> {
        let layers = self.layers.read();
        let path = self.path(local)?;
        match self.resolve_in(store, &layers, &path)? {
            Obj::Upper(meta) if meta.is_file() => {
                let file = self.open_upper(&self.upper_path(&path), false)?;
                let mut buf = vec![0; size as usize];
                let mut done = 0;
                while done < buf.len() {
                    match file.read_at(&mut buf[done..], offset + done as u64)? {
                        0 => break,
                        n => done += n,
                    }
                }
                buf.truncate(done);
                Ok(Read::Upper(buf))
            }
            Obj::LowerFile { file_id, .. } => Ok(Read::Lower(file_id)),
            _ => Err(Errno::EISDIR),
        }
    }

    /// The target of symlink `local` (upper bytes, or the lower file id).
    pub fn readlink(&self, store: &Store, local: u64) -> Result<Read, Errno> {
        let layers = self.layers.read();
        let path = self.path(local)?;
        match self.resolve_in(store, &layers, &path)? {
            Obj::Upper(_) => {
                let target = fs::read_link(self.upper_path(&path))?;
                Ok(Read::Upper(target.as_os_str().as_bytes().to_vec()))
            }
            Obj::LowerFile { file_id, .. } => Ok(Read::Lower(file_id)),
            Obj::LowerDir => Err(Errno::EINVAL),
        }
    }
}

/// Brings the stored base of the closed workspace of `branch` up to its branch
/// tip, as opening it does (`state::retire` checks it then).
pub fn catch_up(store: &Store, common_dir: &Path, branch: &str) -> Result<()> {
    let state = StateDir::of(common_dir, branch);
    Workspace::open(store, common_dir.to_path_buf(), state, branch, None)?.sync(store)
}
