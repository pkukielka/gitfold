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
//! - `nodes`: local id ↔ (parent, name) table.
//! - `markers`: persisted whiteouts and opaque markers.
//! - `write`: mutations and copy-up.

mod markers;
mod nodes;
mod write;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use fuser::Errno;
use gix::ObjectId;
use gix::objs::tree::EntryKind;
use parking_lot::{Mutex, RwLock};

use crate::git;
use crate::inode;
use crate::store::Store;
use crate::worktree;
use markers::Markers;
use nodes::Nodes;

/// `local` of the virtual `.git` file in the workspace root.
pub const GITFILE_LOCAL: u64 = inode::WS_LOCAL_MASK;

/// What a path is in the merged view.
pub enum Obj {
    Upper(Metadata),
    LowerDir,
    LowerFile { file_id: u32 },
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

pub struct Workspace {
    upper: PathBuf,
    pub base: ObjectId,
    pub base_tree: u32,
    /// Content of the virtual `.git` file in the workspace root.
    pub gitfile: Arc<Vec<u8>>,
    /// Whiteouts and opaque markers; also the lock every mutation takes
    /// exclusively and every read shared.
    layers: RwLock<Markers>,
    nodes: Mutex<Nodes>,
    /// Serializes copy-ups, which writers do under a shared `layers`.
    copy_lock: Mutex<()>,
    /// Copy-ups are written here and renamed into place, so a half-written copy
    /// is never visible in upper (or left there by a crash).
    tmp: PathBuf,
    /// Bumped by every change; cached listings of older generations are stale.
    generation: AtomicU64,
    listings: Mutex<HashMap<u64, (u64, Arc<Listing>)>>,
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
            Entry::LowerFile(file_id) => Obj::LowerFile { file_id: *file_id },
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
    /// Opens the workspace stored in `state_dir`, creating it at `new_base` if new.
    pub fn open(
        store: &Store,
        state_dir: PathBuf,
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
            None => {
                let hex = fs::read_to_string(&base_file)
                    .with_context(|| format!("reading {}", base_file.display()))?;
                git::resolve(hex.trim())?
            }
        };
        let layers = Markers::open(&state_dir.join("markers"))?;
        // Copy-ups a crash interrupted; their target was never replaced.
        let tmp = state_dir.join("tmp");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
        Ok(Workspace {
            upper,
            base: base.id,
            base_tree: store.load_tree(base.tree)?,
            gitfile: Arc::new(gitfile),
            layers: RwLock::new(layers),
            nodes: Mutex::new(Nodes::default()),
            copy_lock: Mutex::new(()),
            tmp,
            generation: AtomicU64::new(0),
            listings: Mutex::new(HashMap::new()),
        })
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

    fn lower(&self, store: &Store, path: &[u8]) -> Option<Lower> {
        let mut cur = Lower::Tree(self.base_tree);
        if path.is_empty() {
            return Some(cur);
        }
        for comp in path.split(|b| *b == b'/') {
            let Lower::Tree(tree) = cur else { return None };
            let table = store.tree(tree);
            let e = table.find(comp)?;
            cur = match e.kind {
                EntryKind::Tree => Lower::Tree(e.id),
                EntryKind::Commit => Lower::Gitlink,
                _ => Lower::File(e.id),
            };
        }
        Some(cur)
    }

    /// Whether the lower entry at `path` is hidden by a whiteout or an opaque
    /// ancestor.
    fn lower_hidden(layers: &Markers, path: &[u8]) -> bool {
        layers.whiteouts.contains(path)
            || ancestors(path).any(|p| layers.whiteouts.contains(p) || layers.opaque.contains(p))
    }

    /// The lower entry at `path`, unless hidden.
    fn visible_lower(&self, store: &Store, layers: &Markers, path: &[u8]) -> Option<Lower> {
        if Self::lower_hidden(layers, path) {
            return None;
        }
        self.lower(store, path)
    }

    fn resolve_in(&self, store: &Store, layers: &Markers, path: &[u8]) -> Result<Obj, Errno> {
        match fs::symlink_metadata(self.upper_path(path)) {
            Ok(meta) => return Ok(Obj::Upper(meta)),
            Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => return Err(Errno::ENOENT),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        match self.visible_lower(store, layers, path) {
            Some(Lower::Tree(_) | Lower::Gitlink) => Ok(Obj::LowerDir),
            Some(Lower::File(file_id)) => Ok(Obj::LowerFile { file_id }),
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
            parent: local,
            listing,
        }))
    }

    /// The merged entries of directory `path`, sorted by name.
    fn entries(
        &self,
        store: &Store,
        layers: &Markers,
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
        if !layers.opaque.contains(path)
            && let Some(Lower::Tree(tree)) = self.visible_lower(store, layers, path)
        {
            let table = store.tree(tree);
            let check_whiteouts = !layers.whiteouts.is_empty();
            for e in table.entries() {
                let name = table.name(e);
                if upper_names.contains(name)
                    || (check_whiteouts && layers.whiteouts.contains(&join(path, name)))
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
            Obj::LowerFile { file_id } => Ok(Read::Lower(file_id)),
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
            Obj::LowerFile { file_id } => Ok(Read::Lower(file_id)),
            Obj::LowerDir => Err(Errno::EINVAL),
        }
    }
}
