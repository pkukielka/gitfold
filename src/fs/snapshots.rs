//! The `@<rev>` directories (at the root or in `@` namespaces): resolving
//! names, creating snapshots (shared trees, worktree registration, git index)
//! and listing them.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

use fuser::Errno;
use gix::ObjectId;
use log::{error, info, warn};
use parking_lot::{Mutex, RwLock};

use crate::git::{self, Commit};
use crate::inode;
use crate::prewarm::{self, Prewarm};
use crate::store::{DirRec, Store};
use crate::worktree::{self, IndexWriter, StatInfo};

/// One root entry. Every name gets its own slot, and so its own directory
/// inodes, since a directory can't have two parents in the kernel. Names that
/// resolve to the same commit share `dirs` and the worktree registration.
pub struct Snapshot {
    commit: ObjectId,
    /// Content of the virtual `.git` file in the snapshot root.
    pub gitfile: Arc<Vec<u8>>,
    dirs: Arc<[DirRec]>,
}

struct Root {
    /// Snapshot directory name -> slot.
    names: Vec<(Vec<u8>, u32)>,
    /// Bumped whenever a snapshot is added, invalidating the cached root listing.
    mtime: SystemTime,
}

pub struct Snapshots {
    common_dir: PathBuf,
    mount_point: PathBuf,
    stat: StatInfo,
    prewarm: Prewarm,
    prewarm_threads: usize,
    /// Indexed by slot.
    list: RwLock<Vec<Arc<Snapshot>>>,
    root: Mutex<Root>,
    /// Serializes snapshot creation.
    creating: Mutex<()>,
    /// Size of the last index written; preallocates the next one (~38 MB on chromium).
    last_index_len: AtomicUsize,
}

impl Snapshots {
    pub fn new(
        common_dir: PathBuf,
        mount_point: PathBuf,
        stat: StatInfo,
        prewarm: Prewarm,
        prewarm_threads: usize,
    ) -> Self {
        Snapshots {
            common_dir,
            mount_point,
            stat,
            prewarm,
            prewarm_threads,
            list: RwLock::new(Vec::new()),
            root: Mutex::new(Root {
                names: Vec::new(),
                mtime: SystemTime::now(),
            }),
            creating: Mutex::new(()),
            last_index_len: AtomicUsize::new(0),
        }
    }

    pub fn get(&self, slot: u32) -> Result<Arc<Snapshot>, Errno> {
        self.list
            .read()
            .get(slot as usize)
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    pub fn dir_rec(&self, slot: u32, dir: u32) -> Result<DirRec, Errno> {
        self.get(slot)?
            .dirs
            .get(dir as usize)
            .copied()
            .ok_or(Errno::ENOENT)
    }

    /// How many names the root has; only grows.
    pub fn name_count(&self) -> usize {
        self.root.lock().names.len()
    }

    pub fn root_mtime(&self) -> SystemTime {
        self.root.lock().mtime
    }

    /// (inode, last component) of the snapshots directly below `prefix` ("" for
    /// the mount root, `@origin` for `@origin/main`), copied so callers can stat
    /// entries (which locks the root again) while iterating.
    pub fn children(&self, prefix: &str) -> Vec<(u64, Vec<u8>)> {
        (self.root.lock().names.iter())
            .filter_map(|(name, slot)| {
                let rest = super::child_name(prefix, name)?;
                Some((inode::dir(*slot, 0), rest.to_vec()))
            })
            .collect()
    }

    /// The root inode of an already opened snapshot.
    pub fn existing(&self, name: &[u8]) -> Option<u64> {
        let root = self.root.lock();
        let (_, slot) = root.names.iter().find(|(n, _)| n == name)?;
        Some(inode::dir(*slot, 0))
    }

    /// Resolves `@<rev>` to a snapshot root inode, creating the snapshot on first use.
    pub fn resolve(&self, store: &Store, name: &[u8]) -> Result<u64, Errno> {
        if let Some(ino) = self.existing(name) {
            return Ok(ino);
        }
        // Other names stay silent: shells and file managers probe for files like
        // `.git` or `.hidden` in every directory, including the root.
        let spec = name.strip_prefix(b"@").ok_or(Errno::ENOENT)?;
        let spec = std::str::from_utf8(spec).map_err(|_| {
            warn!(
                "cannot resolve {}: not UTF-8",
                String::from_utf8_lossy(name)
            );
            Errno::ENOENT
        })?;
        let commit = git::resolve(spec).map_err(|e| {
            warn!("cannot resolve @{spec}: {e:#}");
            Errno::ENOENT
        })?;

        let _creating = self.creating.lock();
        if let Some(ino) = self.existing(name) {
            return Ok(ino);
        }
        let path = self.mount_point.join(OsStr::from_bytes(name));
        // Another name may already resolve to the same commit: new slot (own
        // directory inodes), same directory table and worktree.
        let existing = (self.list.read().iter())
            .find(|s| s.commit == commit.id)
            .cloned();
        let snapshot = match existing {
            Some(s) => Snapshot {
                commit: s.commit,
                gitfile: s.gitfile.clone(),
                dirs: s.dirs.clone(),
            },
            None => self.create(store, spec, commit, &path)?,
        };
        let slot = {
            let mut list = self.list.write();
            list.push(Arc::new(snapshot));
            (list.len() - 1) as u32
        };
        {
            let mut root = self.root.lock();
            root.names.push((name.to_vec(), slot));
            root.mtime = SystemTime::now();
        }
        prewarm::spawn(path, self.prewarm, self.prewarm_threads);
        Ok(inode::dir(slot, 0))
    }

    fn create(
        &self,
        store: &Store,
        spec: &str,
        commit: Commit,
        path: &Path,
    ) -> Result<Snapshot, Errno> {
        let fail = |what: &str, e: anyhow::Error| {
            error!("{what} for {spec}: {e:#}");
            Errno::EIO
        };
        let started = Instant::now();
        let before = store.stats();
        let root = store
            .load_tree(commit.tree)
            .map_err(|e| fail("reading tree", e))?;
        let t_trees = started.elapsed();
        let dirs = store.dir_table(root);
        let capacity = self.last_index_len.load(Ordering::Relaxed);
        let mut writer = IndexWriter::new(self.stat, capacity + capacity / 8);
        // The index keeps only the low 32 bits of the inode, which don't depend on the slot.
        store.write_index(&dirs, &mut writer, |_, rec, e| rec.child_ino(0, e) as u32);
        let count = writer.len();
        let index = writer.finish();
        self.last_index_len.store(index.len(), Ordering::Relaxed);
        let t_index = started.elapsed();
        let admin = worktree::register(&self.common_dir, &commit, &index, &path.join(".git"))
            .map_err(|e| fail("registering worktree", e))?;
        let after = store.stats();
        info!(
            "snapshot {spec} -> {}: {count} files, {} dirs; new: {} trees, {} entries, {} files; \
             trees {t_trees:?}, index {:?} ({} MB), write {:?}, total {:?}",
            commit.id,
            dirs.len(),
            after.trees - before.trees,
            after.entries - before.entries,
            after.files - before.files,
            t_index - t_trees,
            index.len() >> 20,
            started.elapsed() - t_index,
            started.elapsed()
        );
        drop(index);
        // Hand memory freed after a large tree load back to the OS instead of keeping
        // it for the life of the mount.
        unsafe { libc::malloc_trim(0) };
        let gitfile = format!("gitdir: {}\n", admin.display()).into_bytes();
        Ok(Snapshot {
            commit: commit.id,
            gitfile: Arc::new(gitfile),
            dirs: dirs.into(),
        })
    }
}
