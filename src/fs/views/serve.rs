//! The views served under the mount (`@committed/<branch>/`), by slot: each
//! a `Shown` swapped whole when it is refreshed (`watch`), and the nodes the
//! kernel holds in it (path-derived ids, as in workspaces, so a path keeps its
//! inode across refreshes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use fuser::Errno;
use log::{error, info};
use parking_lot::{Mutex, RwLock};

use super::build::Entry;
use super::shown::{Obj, SUMMARY, Shown};
use super::watch::{self, Watch};
use crate::fs::NameChanges;
use crate::fs::workspace::nodes::Nodes;
use crate::inode;
use crate::state::join;
use crate::store::Store;

/// `local` of the summary file at a view's root (never a path's).
pub const SUMMARY_LOCAL: u64 = inode::WS_LOCAL_MASK;

/// What a view inode is.
pub enum Item {
    Dir(SystemTime),
    File(Entry, SystemTime),
    Summary(Arc<Vec<u8>>, SystemTime),
}

/// A directory's entries, kept while the kernel pages through them.
pub struct Listing {
    /// Of the `Shown` with this generation.
    generation: u64,
    pub dir: Vec<u8>,
    pub entries: Vec<(Vec<u8>, Obj)>,
}

/// One served view.
pub struct ViewDir {
    pub slot: u32,
    /// Below the mount root, e.g. `@committed/api`.
    pub name: String,
    shown: RwLock<Arc<Shown>>,
    /// Bumped when `shown` is swapped.
    generation: AtomicU64,
    nodes: Mutex<Nodes>,
    listings: Mutex<HashMap<u64, Arc<Listing>>>,
}

impl ViewDir {
    pub fn shown(&self) -> Arc<Shown> {
        self.shown.read().clone()
    }

    /// Replaces what it shows.
    pub fn swap(&self, shown: Shown) {
        let mut current = self.shown.write();
        *current = Arc::new(shown);
        self.generation.fetch_add(1, Ordering::Release);
        self.listings.lock().clear();
    }

    /// The path of node `local` (`""`: the root).
    fn path(&self, local: u64) -> Result<Vec<u8>, Errno> {
        match local {
            0 => Ok(Vec::new()),
            _ => self.nodes.lock().path(local).ok_or(Errno::ENOENT),
        }
    }

    /// The node the kernel holds for `path`, if it holds one.
    pub fn held(&self, path: &[u8]) -> Option<u64> {
        if path.is_empty() {
            return Some(0);
        }
        let nodes = self.nodes.lock();
        let mut local = 0;
        for name in path.split(|b| *b == b'/') {
            local = nodes.existing(local, name)?;
        }
        Some(local)
    }

    fn item(&self, shown: &Shown, store: &Store, path: &[u8]) -> Result<Item, Errno> {
        match shown.obj(store, path).ok_or(Errno::ENOENT)? {
            Obj::Dir => Ok(Item::Dir(shown.made)),
            Obj::File(entry) => Ok(Item::File(entry, shown.time(path))),
        }
    }

    /// What node `local` is.
    pub fn get(&self, store: &Store, local: u64) -> Result<Item, Errno> {
        let shown = self.shown();
        if local == SUMMARY_LOCAL {
            return Ok(Item::Summary(shown.summary.clone(), shown.made));
        }
        self.item(&shown, store, &self.path(local)?)
    }

    /// Looks up `name` in directory `parent`, counting the kernel's
    /// reference: its local id and what it is.
    pub fn lookup(&self, store: &Store, parent: u64, name: &[u8]) -> Result<(u64, Item), Errno> {
        let shown = self.shown();
        if parent == 0 && name == SUMMARY {
            return Ok((
                SUMMARY_LOCAL,
                Item::Summary(shown.summary.clone(), shown.made),
            ));
        }
        let dir = self.path(parent)?;
        let item = self.item(&shown, store, &join(&dir, name))?;
        Ok((self.nodes.lock().claim(parent, name, &dir), item))
    }

    /// The listing of directory `local`, built once per refresh while the
    /// kernel pages through it.
    pub fn listing(&self, store: &Store, local: u64) -> Result<Arc<Listing>, Errno> {
        let shown = self.shown();
        let generation = self.generation.load(Ordering::Acquire);
        if let Some(l) = self.listings.lock().get(&local)
            && l.generation == generation
        {
            return Ok(l.clone());
        }
        let dir = self.path(local)?;
        let entries = shown.list(store, &dir).ok_or(Errno::ENOTDIR)?;
        let listing = Arc::new(Listing {
            generation,
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
        let shown = self.shown();
        let mut nodes = self.nodes.lock();
        let local = match claim {
            true => nodes.claim(parent, name, &listing.dir),
            false => nodes.peek(parent, name, &listing.dir),
        };
        let path = join(&listing.dir, name);
        let item = match obj {
            Obj::Dir => Item::Dir(shown.made),
            Obj::File(entry) => Item::File(entry.clone(), shown.time(&path)),
        };
        (local, item)
    }

    pub fn forget(&self, local: u64, n: u64) {
        if local != 0 && local != SUMMARY_LOCAL {
            self.nodes.lock().forget(local, n);
        }
    }

    /// The conflict attribute of node `local`, if it has one.
    pub fn conflict(&self, local: u64) -> Option<String> {
        if local == 0 || local == SUMMARY_LOCAL {
            return None;
        }
        let path = self.path(local).ok()?;
        self.shown().conflict(&path).map(String::from)
    }
}

/// The served views, by slot (never reused: a slot's inodes stay its own).
pub struct Views {
    common_dir: PathBuf,
    store: Arc<Store>,
    names: Arc<NameChanges>,
    list: RwLock<Vec<Arc<ViewDir>>>,
    creating: Mutex<()>,
}

impl Views {
    pub fn new(common_dir: PathBuf, store: Arc<Store>, names: Arc<NameChanges>) -> Views {
        Views {
            common_dir,
            store,
            names,
            list: RwLock::default(),
            creating: Mutex::default(),
        }
    }

    pub fn get(&self, slot: u32) -> Result<Arc<ViewDir>, Errno> {
        self.list
            .read()
            .get(slot as usize)
            .cloned()
            .ok_or(Errno::ENOENT)
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
            generation: AtomicU64::new(0),
            nodes: Mutex::default(),
            listings: Mutex::default(),
        });
        list.push(view.clone());
        drop(list);
        info!("view {name} opened");
        watch::spawn(Arc::downgrade(&view), ctx, watch);
        Ok(inode::view(slot, 0))
    }
}
