//! The views served under the mount (`@committed/<branch>/`), by slot: each
//! a `Shown` swapped whole when it is refreshed (`watch`), and the nodes the
//! kernel holds in it (path-derived ids, as in workspaces, so a path keeps its
//! inode across refreshes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use fuser::Errno;
use log::{error, info};
use parking_lot::{Mutex, RwLock, RwLockReadGuard};

use super::build::Entry;
use super::shown::{Obj, Shown};
use super::watch::{self, Watch};
use crate::fs::NameChanges;
use crate::fs::workspace::nodes::Nodes;
use crate::inode;
use crate::state::{join, name, parent};
use crate::store::Store;

/// What a view inode is, with its time.
pub enum Item {
    Dir(SystemTime),
    File(Entry, SystemTime),
    Summary(Arc<Vec<u8>>, SystemTime),
}

impl Item {
    /// `obj` at `path` of `shown`.
    fn of(shown: &Shown, obj: &Obj, path: &[u8]) -> Item {
        match obj {
            Obj::Dir => Item::Dir(shown.time(path)),
            Obj::File(entry) => Item::File(entry.clone(), shown.time(path)),
            Obj::Summary(data) => Item::Summary(data.clone(), shown.time(path)),
        }
    }
}

/// A directory's entries, kept while the kernel pages through them.
pub struct Listing {
    /// What they were listed from.
    shown: Arc<Shown>,
    pub dir: Vec<u8>,
    pub entries: Vec<(Vec<u8>, Obj)>,
}

/// One served view.
pub struct ViewDir {
    pub slot: u32,
    /// Below the mount root, e.g. `@committed/api`.
    pub name: String,
    shown: RwLock<Arc<Shown>>,
    nodes: Mutex<Nodes>,
    listings: Mutex<HashMap<u64, Arc<Listing>>>,
    /// Taken shared by requests while they are served, exclusively by
    /// `swap` to wait for them.
    gate: RwLock<()>,
}

impl ViewDir {
    pub fn shown(&self) -> Arc<Shown> {
        self.shown.read().clone()
    }

    /// Replaces what it shows with `shown`, where `changed` paths differ,
    /// then waits for the requests served from the old one: their replies
    /// reach the kernel before it is told what changed. A path whose kind
    /// changed (a file now a directory) gets a node of its own: the kernel's
    /// inode keeps its type.
    pub fn swap(&self, store: &Store, shown: Shown, changed: &[Vec<u8>]) {
        // Only the watch swaps: the current one can be read unlocked.
        let current = self.shown();
        let kind = |s: &Shown, p: &[u8]| s.obj(store, p).map(|o| o.kind(store));
        let retyped: Vec<&Vec<u8>> = (changed.iter())
            .filter(|p| {
                let was = kind(&current, p);
                was.is_some() && was != kind(&shown, p)
            })
            .collect();
        // No request is served meanwhile: none can claim a node from the old
        // view and answer with the old kind.
        let _gate = self.gate.write();
        let mut nodes = self.nodes.lock();
        for path in retyped {
            if let Some(dir) = held(&nodes, parent(path)) {
                nodes.orphan(dir, name(path));
            }
        }
        *self.shown.write() = Arc::new(shown);
        self.listings.lock().clear();
    }

    /// Held while a request in it is served and replied to (`swap`).
    pub fn serving(&self) -> RwLockReadGuard<'_, ()> {
        self.gate.read()
    }

    /// The path of node `local` (`""`: the root).
    fn path(&self, local: u64) -> Result<Vec<u8>, Errno> {
        self.nodes.lock().path(local).ok_or(Errno::ENOENT)
    }

    /// The node the kernel holds for `path`, if it holds one.
    pub fn held(&self, path: &[u8]) -> Option<u64> {
        held(&self.nodes.lock(), path)
    }

    /// What node `local` is.
    pub fn get(&self, store: &Store, local: u64) -> Result<Item, Errno> {
        let shown = self.shown();
        let path = self.path(local)?;
        let obj = shown.obj(store, &path).ok_or(Errno::ENOENT)?;
        Ok(Item::of(&shown, &obj, &path))
    }

    /// Looks up `name` in directory `parent`, counting the kernel's
    /// reference: its local id and what it is.
    pub fn lookup(&self, store: &Store, parent: u64, name: &[u8]) -> Result<(u64, Item), Errno> {
        let shown = self.shown();
        let dir = self.path(parent)?;
        let path = join(&dir, name);
        let obj = shown.obj(store, &path).ok_or(Errno::ENOENT)?;
        let local = self.nodes.lock().claim(parent, name, &dir);
        Ok((local, Item::of(&shown, &obj, &path)))
    }

    /// The listing of directory `local`, built once per refresh while the
    /// kernel pages through it.
    pub fn listing(&self, store: &Store, local: u64) -> Result<Arc<Listing>, Errno> {
        let shown = self.shown();
        if let Some(l) = self.listings.lock().get(&local)
            && Arc::ptr_eq(&l.shown, &shown)
        {
            return Ok(l.clone());
        }
        let dir = self.path(local)?;
        let entries = match shown.list(store, &dir) {
            Some(entries) => entries,
            None if shown.obj(store, &dir).is_none() => return Err(Errno::ENOENT),
            None => return Err(Errno::ENOTDIR),
        };
        let listing = Arc::new(Listing {
            shown,
            dir,
            entries,
        });
        self.listings.lock().insert(local, listing.clone());
        Ok(listing)
    }

    /// The kernel read the listing of `local` to its end.
    pub fn listed(&self, local: u64) {
        self.listings.lock().remove(&local);
    }

    /// What entry `name` of `listing` is, with its local id: claimed (one
    /// more kernel reference) for readdirplus, else only peeked.
    pub fn entry(
        &self,
        listing: &Listing,
        parent: u64,
        name: &[u8],
        obj: &Obj,
        claim: bool,
    ) -> (u64, Item) {
        let mut nodes = self.nodes.lock();
        let local = match claim {
            true => nodes.claim(parent, name, &listing.dir),
            false => nodes.peek(parent, name, &listing.dir),
        };
        let item = Item::of(&listing.shown, obj, &join(&listing.dir, name));
        (local, item)
    }

    pub fn forget(&self, local: u64, n: u64) {
        self.nodes.lock().forget(local, n);
    }

    /// The conflict attribute of node `local`, if it has one.
    pub fn conflict(&self, local: u64) -> Option<String> {
        let path = self.path(local).ok()?;
        self.shown().conflict(&path).map(String::from)
    }
}

/// The node held for `path` in `nodes`, if any.
fn held(nodes: &Nodes, path: &[u8]) -> Option<u64> {
    if path.is_empty() {
        return Some(0);
    }
    let mut local = 0;
    for name in path.split(|b| *b == b'/') {
        local = nodes.existing(local, name)?;
    }
    Some(local)
}

/// The served views, by slot (never reused: a slot's inodes stay its own).
pub struct Views {
    common_dir: PathBuf,
    store: Arc<Store>,
    names: Arc<NameChanges>,
    list: RwLock<Vec<Arc<ViewDir>>>,
    creating: Mutex<()>,
    /// Bumped when a view opens: the namespace listings holding it changed.
    mtime: Mutex<SystemTime>,
}

impl Views {
    pub fn new(common_dir: PathBuf, store: Arc<Store>, names: Arc<NameChanges>) -> Views {
        Views {
            common_dir,
            store,
            names,
            list: RwLock::default(),
            creating: Mutex::default(),
            mtime: Mutex::new(SystemTime::now()),
        }
    }

    pub fn get(&self, slot: u32) -> Result<Arc<ViewDir>, Errno> {
        self.list
            .read()
            .get(slot as usize)
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    /// When a view last opened.
    pub fn mtime(&self) -> SystemTime {
        *self.mtime.lock()
    }

    /// How many views were opened; only grows.
    pub fn len(&self) -> usize {
        self.list.read().len()
    }

    /// The root inode of the open view `name`.
    pub fn existing(&self, name: &str) -> Option<u64> {
        let list = self.list.read();
        let view = list.iter().find(|v| v.name == name)?;
        Some(inode::view(view.slot, 0))
    }

    /// (inode, last component) of the views directly below `prefix`.
    pub fn children(&self, prefix: &str) -> Vec<(u64, Vec<u8>)> {
        (self.list.read().iter())
            .filter_map(|v| {
                let rest = crate::fs::child_name(prefix, v.name.as_bytes())?;
                Some((inode::view(v.slot, 0), rest.to_vec()))
            })
            .collect()
    }

    /// Opens view `name` (`@committed/<branch>`) of local branch `branch`:
    /// built now, then kept current (`watch`). EIO if it can't be built.
    pub fn open(&self, name: &str, branch: &str) -> Result<u64, Errno> {
        let _creating = self.creating.lock();
        if let Some(ino) = self.existing(name) {
            return Ok(ino);
        }
        let ctx = watch::Context {
            common_dir: self.common_dir.clone(),
            store: self.store.clone(),
            names: self.names.clone(),
            branch: branch.to_string(),
        };
        let mut watch = Watch::default();
        let built = watch.next(&ctx, None).and_then(|built| {
            built.ok_or_else(|| anyhow::anyhow!("nothing built")) // the first always is
        });
        let (shown, _) = built.map_err(|e| {
            error!("view {name}: {e:#}");
            Errno::EIO
        })?;
        let mut list = self.list.write();
        let slot = list.len() as u32;
        let view = Arc::new(ViewDir {
            slot,
            name: name.to_string(),
            shown: RwLock::new(Arc::new(shown)),
            nodes: Mutex::default(),
            listings: Mutex::default(),
            gate: RwLock::default(),
        });
        list.push(view.clone());
        *self.mtime.lock() = SystemTime::now();
        drop(list);
        info!("view {name} opened");
        watch::spawn(Arc::downgrade(&view), ctx, watch);
        Ok(inode::view(slot, 0))
    }
}
