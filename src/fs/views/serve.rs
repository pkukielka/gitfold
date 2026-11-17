//! The views served under the mount (`@committed/<branch>/`), by slot: each
//! a `Shown` swapped whole when it is refreshed (`watch`), and the nodes the
//! kernel holds in it (path-derived ids, as in workspaces, so a path keeps its
//! inode across refreshes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use fuser::Errno;
use log::{error, info};
use parking_lot::{Condvar, Mutex, RwLock, RwLockReadGuard};

use super::lives::Lives;
use super::merge::Private;
use super::shown::{Changed, Obj, Shown};
use super::watch::{self, Trees, Watch};
use crate::fs::NameChanges;
use crate::fs::workspace::nodes::Nodes;
use crate::fs::workspaces::Workspaces;
use crate::inode;
use crate::state::{join, name, parent};
use crate::store::Store;

/// Which views: of committed tips, or of workspaces as they are.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Committed,
    Live,
}

/// How long the kernel keeps an `@live` view's entries and attributes: it
/// asks again soon, so an access wakes a dormant view (`ViewDir::reached`).
pub const LIVE_TTL: Duration = Duration::from_secs(1);
/// How long a view goes unaccessed before it rests (no refreshes): a minute,
/// or `GITFOLD_VIEW_REST_SECS`.
pub fn rests_after() -> Duration {
    static AFTER: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *AFTER.get_or_init(|| {
        let secs = std::env::var("GITFOLD_VIEW_REST_SECS").ok();
        Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(60))
    })
}
/// How long the first access to a resting view waits for it to be fresh.
const WAKE_WAIT: Duration = Duration::from_secs(5);

/// When a view was last accessed, whether it rests, and how many times it
/// was refreshed (or found current) since it was opened.
pub struct Rest {
    pub accessed: Instant,
    pub resting: bool,
    pub refreshed: u64,
}

/// What a view inode is, with its time.
pub enum Item {
    Dir(SystemTime),
    /// A file or symlink: its store file id.
    File(u32, SystemTime),
    Summary(Arc<Vec<u8>>, SystemTime),
}

impl Item {
    /// `obj` at `path` of `shown`.
    fn of(shown: &Shown, obj: &Obj, path: &[u8]) -> Item {
        match obj {
            Obj::Dir => Item::Dir(shown.time(path)),
            Obj::File(id) => Item::File(*id, shown.time(path)),
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
    pub kind: Kind,
    /// The directory its root is listed in (`@committed`, or a namespace
    /// below it).
    pub parent: u64,
    /// Its rest (`watch`), and the requests waiting for it to end.
    pub rest: Mutex<Rest>,
    pub woken: Condvar,
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

    /// Replaces what it shows with `shown`, where `changed` paths differ:
    /// first waits for the requests in flight (and holds new ones off), so no
    /// reply from the old view reaches the kernel after it is told what
    /// changed, and no lookup claims a node from the old view. A path whose
    /// kind changed (a file now a directory) gets a node of its own: the
    /// kernel's inode keeps its type.
    pub fn swap(&self, shown: Shown, changed: &[Changed]) {
        let _gate = self.gate.write();
        let mut nodes = self.nodes.lock();
        for c in changed.iter().filter(|c| c.retyped) {
            if let Some(dir) = held(&nodes, parent(&c.path)) {
                nodes.orphan(dir, name(&c.path));
            }
        }
        *self.shown.write() = Arc::new(shown);
        self.listings.lock().clear();
    }

    /// Held while a request in it is served and replied to (`swap`), once
    /// it is awake (`reached`).
    pub fn serving(&self) -> RwLockReadGuard<'_, ()> {
        self.reached();
        self.gate.read()
    }

    /// The view was accessed: if it rests, its watch refreshes it at once,
    /// and this waits for that (a while at most: then it shows what it
    /// last did).
    pub fn reached(&self) {
        let mut rest = self.rest.lock();
        rest.accessed = Instant::now();
        if !rest.resting {
            return;
        }
        let seen = rest.refreshed;
        self.woken.notify_all();
        let until = Instant::now() + WAKE_WAIT;
        while rest.resting && rest.refreshed == seen {
            if self.woken.wait_until(&mut rest, until).timed_out() {
                break;
            }
        }
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
    workspaces: Arc<Workspaces>,
    /// The live trees of workspaces, shared by the `@live` views.
    lives: Arc<Lives>,
    list: RwLock<Vec<Arc<ViewDir>>>,
    creating: Mutex<()>,
    /// Bumped when a view opens: the namespace listings holding it changed.
    mtime: Mutex<SystemTime>,
    /// Where the views' merges are made, once the first view opens.
    private: Mutex<Option<Arc<Private>>>,
}

impl Views {
    pub fn new(
        common_dir: PathBuf,
        store: Arc<Store>,
        names: Arc<NameChanges>,
        workspaces: Arc<Workspaces>,
    ) -> Views {
        Views {
            common_dir,
            store,
            names,
            workspaces,
            lives: Arc::default(),
            list: RwLock::default(),
            creating: Mutex::default(),
            mtime: Mutex::new(SystemTime::now()),
            private: Mutex::default(),
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

    /// The private repository the views are merged in, made when the first
    /// view opens (`merge::Private::create`). EIO if it can't be.
    fn private(&self) -> Result<Arc<Private>, Errno> {
        let mut private = self.private.lock();
        if let Some(p) = &*private {
            return Ok(p.clone());
        }
        let made = Private::create(&self.common_dir).map_err(|e| {
            error!("views: making their private repository: {e:#}");
            Errno::EIO
        })?;
        Ok(private.insert(Arc::new(made)).clone())
    }

    /// How many views were opened; only grows.
    pub fn len(&self) -> usize {
        self.list.read().len()
    }

    /// The root inode of the open view `name`, reached (`ViewDir::reached`).
    pub fn existing(&self, name: &str) -> Option<u64> {
        let view = self.list.read().iter().find(|v| v.name == name).cloned()?;
        view.reached();
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

    /// Opens view `name` (`@committed/<branch>`, `@live/<branch>`: `kind`)
    /// of local branch `branch`, listed in directory `parent`: built now,
    /// then kept current (`watch`). EIO if it can't be built.
    pub fn open(&self, name: &str, branch: &str, kind: Kind, parent: u64) -> Result<u64, Errno> {
        let _creating = self.creating.lock();
        if let Some(ino) = self.existing(name) {
            return Ok(ino);
        }
        let ctx = watch::Context {
            common_dir: self.common_dir.clone(),
            store: self.store.clone(),
            names: self.names.clone(),
            branch: branch.to_string(),
            private: self.private()?,
            kind,
            workspaces: self.workspaces.clone(),
            lives: self.lives.clone(),
        };
        let mut watch = Watch::default();
        let built = watch.next(&ctx, None, Trees::Tips).and_then(|built| {
            built.ok_or_else(|| anyhow::anyhow!("nothing built")) // the first always is
        });
        let (shown, _, commit) = built.map_err(|e| {
            error!("view {name}: {e:#}");
            Errno::EIO
        })?;
        if let Err(e) = ctx.private.shown(&ctx.key(), commit) {
            error!("view {name}: {e:#}");
        }
        let mut list = self.list.write();
        let slot = list.len() as u32;
        let view = Arc::new(ViewDir {
            slot,
            name: name.to_string(),
            kind,
            parent,
            // An `@live` view opens with the tips: its first requests wait for
            // the workspaces (`ViewDir::reached`).
            rest: Mutex::new(Rest {
                accessed: Instant::now(),
                resting: kind == Kind::Live,
                refreshed: 0,
            }),
            woken: Condvar::new(),
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
