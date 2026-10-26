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
//! - `markers`: persisted whiteouts and opaque markers.
//! - `times`: timestamps of files dropped from upper.
//! - `rebase`: moving the base to the branch tip.
//! - `write`: mutations and copy-up.

mod markers;
mod nodes;
mod rebase;
pub mod status;
mod times;
mod write;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use fuser::Errno;
use gix::ObjectId;
use gix::objs::tree::EntryKind;
use parking_lot::{Mutex, RwLock};

use crate::git;
use crate::inode;
use crate::store::{Store, TEntry};
use crate::worktree;
use markers::Markers;
use nodes::Nodes;
use times::{Stamp, Times};

/// `local` of the virtual `.git` file in the workspace root.
pub const GITFILE_LOCAL: u64 = inode::WS_LOCAL_MASK;

/// The base commit recorded in workspace state directory `state_dir`.
fn read_base(state_dir: &Path) -> Result<git::Commit> {
    let base_file = state_dir.join("base");
    let hex = fs::read_to_string(&base_file)
        .with_context(|| format!("reading {}", base_file.display()))?;
    git::resolve(hex.trim())
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

enum Lower {
    Tree(u32),
    /// Submodule: an empty directory.
    Gitlink,
    File(u32),
}

impl Lower {
    fn of(e: &TEntry) -> Lower {
        match e.kind {
            EntryKind::Tree => Lower::Tree(e.id),
            EntryKind::Commit => Lower::Gitlink,
            _ => Lower::File(e.id),
        }
    }
}

pub struct Workspace {
    upper: PathBuf,
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
    /// Copy-ups are written here and renamed into place, so a half-written copy
    /// is never visible in upper (or left there by a crash).
    tmp: PathBuf,
    /// Bumped by every change; cached listings of older generations are stale.
    generation: AtomicU64,
    listings: Mutex<HashMap<u64, (u64, Arc<Listing>)>>,
    /// Paths of upper files written since the last check for ones identical to
    /// the base again (see `rebase`).
    touched: Mutex<HashSet<Vec<u8>>>,
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
    pub entries: Vec<(Vec<u8>, Entry)>,
}

/// A listing entry. Upper entries are stat-ed when served, not when listed, so
/// writes into a file don't invalidate its directory's cached listing.
pub enum Entry {
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

    pub fn obj(&self, name: &[u8], entry: &Entry) -> Result<Obj, Errno> {
        Ok(match entry {
            Entry::Upper => {
                let path = self.ws.upper_path(&join(&self.listing.dir, name));
                Obj::Upper(fs::symlink_metadata(path)?)
            }
            Entry::LowerDir => Obj::LowerDir,
            Entry::LowerFile(file_id) => Obj::LowerFile {
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

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// The path prefixes of `path` at `/` boundaries, shortest first, excluding `path`.
fn ancestors(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, b)| **b == b'/')
        .map(|(i, _)| &path[..i])
}

fn not_found(e: &io::Error) -> bool {
    e.kind() == ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR)
}

impl Workspace {
    /// Opens the workspace stored in `state_dir`, creating it at `new_base` if new;
    /// `admin` is its worktree's admin directory in `common_dir`.
    pub fn open(
        store: &Store,
        common_dir: PathBuf,
        state_dir: PathBuf,
        admin: PathBuf,
        branch: &str,
        new_base: Option<git::Commit>,
        gitfile: Vec<u8>,
    ) -> Result<Workspace> {
        let upper = state_dir.join("upper");
        fs::create_dir_all(&upper).with_context(|| format!("creating {}", upper.display()))?;
        let base_file = state_dir.join("base");
        let base = match new_base {
            Some(base) => {
                // For people looking at the state directory; the id is escaped.
                worktree::write_atomic(
                    &state_dir.join("branch"),
                    format!("{branch}\n").as_bytes(),
                )?;
                // Written last and atomically: its presence means "workspace exists".
                worktree::write_atomic(&base_file, format!("{}\n", base.id).as_bytes())?;
                base
            }
            None => read_base(&state_dir)?,
        };
        let layers = Layers {
            base: base.id,
            base_tree: store.load_tree(base.tree)?,
            markers: Markers::open(&state_dir.join("markers"))?,
            times: Times::open(&state_dir.join("times"))?,
        };
        // Copy-ups a crash interrupted; their target was never replaced.
        let tmp = state_dir.join("tmp");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
        let pin_ref =
            worktree::pin_ref(&state_dir.file_name().unwrap_or_default().to_string_lossy());
        Ok(Workspace {
            upper,
            common_dir,
            state_dir,
            admin,
            branch: branch.to_string(),
            pin_ref,
            pinned: Mutex::new(None),
            rebase_failed: AtomicBool::new(true),
            gitfile: Arc::new(gitfile),
            layers: RwLock::new(layers),
            nodes: Mutex::new(Nodes::default()),
            copy_lock: Mutex::new(()),
            tmp,
            generation: AtomicU64::new(0),
            listings: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashSet::new()),
        })
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
        Self::lower_in(store, layers.base_tree, path)
    }

    /// The entry at `path` below tree `tree`.
    fn lower_in(store: &Store, tree: u32, path: &[u8]) -> Option<Lower> {
        let mut cur = Lower::Tree(tree);
        if path.is_empty() {
            return Some(cur);
        }
        for comp in path.split(|b| *b == b'/') {
            let Lower::Tree(tree) = cur else { return None };
            let table = store.tree(tree);
            cur = Lower::of(table.find(comp)?);
        }
        Some(cur)
    }

    /// Whether the lower entry at `path` is hidden by a whiteout or an opaque
    /// ancestor.
    fn lower_hidden(layers: &Layers, path: &[u8]) -> bool {
        layers.markers.whiteouts.contains(path)
            || ancestors(path)
                .any(|p| layers.markers.whiteouts.contains(p) || layers.markers.opaque.contains(p))
    }

    /// The lower entry at `path`, unless hidden.
    fn visible_lower(&self, store: &Store, layers: &Layers, path: &[u8]) -> Option<Lower> {
        if Self::lower_hidden(layers, path) {
            return None;
        }
        Self::lower(store, layers, path)
    }

    fn resolve_in(&self, store: &Store, layers: &Layers, path: &[u8]) -> Result<Obj, Errno> {
        match fs::symlink_metadata(self.upper_path(path)) {
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
    ) -> Result<Vec<(Vec<u8>, Entry)>, Errno> {
        if !self.resolve_in(store, layers, path)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        // Upper entries win; lower ones come straight from the tree table, without
        // a stat or a path walk each.
        let mut entries: Vec<(Vec<u8>, Entry)> = Vec::new();
        match fs::read_dir(self.upper_path(path)) {
            Ok(dir) => entries.extend(
                dir.flatten()
                    .map(|e| (e.file_name().as_bytes().to_vec(), Entry::Upper)),
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
                    Entry::LowerDir
                } else {
                    Entry::LowerFile(e.id)
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
                let file = File::open(self.upper_path(&path))?;
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
