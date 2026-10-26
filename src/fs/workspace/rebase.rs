//! Moving a workspace's base to its branch's new tip, invisibly.
//!
//! The base is the commit the lower layer shows. After a commit made in the
//! workspace, upper still holds every committed file and the base is the old
//! commit; left alone, upper would grow with every commit, and an external
//! branch move (reset, amend) would make `git status` compare against files the
//! view doesn't show. So a poller (`Workspace::sync`) moves the base to the
//! branch tip once git is idle.
//!
//! The view must not change while doing so: git commands chained after a commit
//! (`git commit && git status`) may run while the base moves, and the kernel
//! caches attributes for a year. A rebase from base B to N only visits the paths
//! B and N disagree on, in three steps that each leave a consistent view, also
//! across a crash:
//!
//! 1. Prepare (same view under B and N): a path visible from B's lower layer
//!    that N changes is copied up with the mtime it showed; a path absent from
//!    the view that N adds gets a whiteout, also below opaque directories where
//!    N has a directory. Made durable before step 2.
//! 2. Switch: persist the stamps of step 3's files, then the new base.
//! 3. Clean up (same view under N): upper files identical to N's (content and
//!    mode) are dropped and keep showing their upper mtime and ctime (`times`),
//!    so the index git just wrote still matches by stat; whiteouts of paths N
//!    doesn't have, and opaque markers step 1 made unnecessary, are dropped.
//!
//! A rebase runs under the exclusive `layers` lock; its cost is proportional to
//! the difference between B and N (the files of the commit), and only files
//! that may be dropped are read.
//!
//! `sync` also drops upper files written since its last pass that are identical
//! to the base again (git rewrites unchanged files in `stash`, `rebase --abort`).
//! Those are read under the shared lock and re-checked by `stat` under the
//! exclusive one; files changed in the last second wait for a later pass.

use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use gix::objs::tree::EntryKind;
use log::info;

use super::markers::Kind;
use super::times::Stamp;
use super::{Layers, Lower, Workspace, ancestors, join, not_found};
use crate::git::{self, Commit};
use crate::inode;
use crate::store::{EMPTY_TREE, FileRec, Store, TEntry};
use crate::worktree;

/// Paths remembered for `drop_unchanged` while git is busy; beyond this (a
/// build writing while a rebase waits for conflict resolution, say) they are
/// forgotten, which only leaves identical files in upper.
const MAX_TOUCHED: usize = 100_000;

/// Files changed more recently than this are not dropped yet (see
/// `drop_unchanged`).
const RECENT: Duration = Duration::from_secs(1);

/// Above this many files created by a rebase, the whole file system is synced
/// instead of each file.
const SYNCFS_ABOVE: usize = 64;

/// Children of a lower entry shown as a directory (submodules are empty).
fn children(e: Option<TEntry>) -> Option<u32> {
    let e = e?;
    match e.kind {
        EntryKind::Tree => Some(e.id),
        EntryKind::Commit => Some(EMPTY_TREE),
        _ => None,
    }
}

fn same(a: Option<TEntry>, b: Option<TEntry>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.kind == b.kind && a.id == b.id,
        _ => false,
    }
}

/// What a comparison of an upper file with a stored one saw, to tell later
/// whether the upper file changed since.
#[derive(PartialEq)]
struct Seen {
    stamp: Stamp,
    len: u64,
    mode: u32,
    ino: u64,
}

impl Seen {
    fn of(meta: &fs::Metadata) -> Seen {
        Seen {
            stamp: Stamp {
                mtime: meta.modified().unwrap_or(UNIX_EPOCH),
                ctime: inode::ctime(meta),
            },
            len: meta.len(),
            mode: meta.mode(),
            ino: meta.ino(),
        }
    }
}

/// What steps 2 and 3 act on, collected by step 1.
#[derive(Default)]
struct Plan {
    /// Upper files whose path N has a file at: (path, N's file id).
    maybe_same: Vec<(Vec<u8>, u32)>,
    /// Whiteouts of paths N doesn't have.
    stale_whiteouts: Vec<Vec<u8>>,
    /// Opaque markers N makes unnecessary (step 1 whited out what they hid).
    stale_opaque: Vec<Vec<u8>>,
    /// Upper files and directories step 1 created, to sync before step 2.
    created: Vec<Vec<u8>>,
    markers_changed: bool,
    copied: usize,
    whited_out: usize,
}

impl Workspace {
    /// While git is idle in the workspace: moves the base to the branch tip if it
    /// moved, drops upper files written since the last call that are identical
    /// to the base (git rewrites unchanged files in `stash`, `rebase --abort`,
    /// `checkout -- .` ...), and keeps the base pinned.
    pub fn sync(&self, store: &Store) -> Result<()> {
        if !self.git_idle() {
            self.limit_touched();
            return Ok(());
        }
        let tip = git::resolve(&format!("refs/heads/{}", self.branch))?;
        let rebased = if tip.id != self.layers.read().base {
            let rebased = self.rebase(store, &tip);
            self.rebase_failed
                .store(rebased.is_err(), Ordering::Relaxed);
            rebased
        } else {
            Ok(())
        };
        let dropped = if rebased.is_ok() {
            self.drop_unchanged(store)
        } else {
            self.limit_touched();
            Ok(())
        };
        // Retried every call until it succeeds, whatever else failed.
        let pinned = self.pin_base();
        rebased.and(dropped).and(pinned)
    }

    fn limit_touched(&self) {
        let mut touched = self.touched.lock();
        if touched.len() > MAX_TOUCHED {
            touched.clear();
        }
    }

    /// Points `refs/gitfold/<id>` at the base, unless it was already set to it.
    pub fn pin_base(&self) -> Result<()> {
        let base = self.layers.read().base;
        let mut pinned = self.pinned.lock();
        if *pinned != Some(base) {
            if git::resolve(&self.pin_ref).map_or(true, |c| c.id != base) {
                worktree::pin(&self.common_dir, &self.pin_ref, &base)?;
            }
            *pinned = Some(base);
        }
        Ok(())
    }

    /// Drops the touched upper files that are identical to the base.
    fn drop_unchanged(&self, store: &Store) -> Result<()> {
        let touched = std::mem::take(&mut *self.touched.lock());
        if touched.is_empty() {
            return Ok(());
        }
        // Read and hash under the shared lock: writes go on meanwhile. A file
        // changed in the last second is left for a later pass: timestamps move
        // in coarse ticks, so a write landing after the hash could leave its
        // `stat` unchanged and be dropped with the file.
        let recent = SystemTime::now() - RECENT;
        let mut same = Vec::new();
        let mut later = Vec::new();
        {
            let layers = self.layers.read();
            for path in touched {
                if let Some(Lower::File(file_id)) = self.visible_lower(store, &layers, &path)
                    && let Some(seen) = self.same_as_stored(store, &path, file_id)?
                {
                    if seen.stamp.ctime >= recent {
                        later.push(path);
                    } else {
                        same.push((path, file_id, seen));
                    }
                }
            }
        }
        self.touched.lock().extend(later);
        if same.is_empty() {
            return Ok(());
        }
        // Drop those still as they were when read.
        let mut layers = self.layers.write();
        let mut dropped = Vec::new();
        for (path, file_id, seen) in same {
            let unchanged = matches!(
                self.visible_lower(store, &layers, &path),
                Some(Lower::File(id)) if id == file_id
            ) && fs::symlink_metadata(self.upper_path(&path))
                .is_ok_and(|meta| Seen::of(&meta) == seen);
            if unchanged {
                layers.times.set(&path, Some(seen.stamp));
                dropped.push(path);
            }
        }
        layers.times.save()?;
        let removed = self.remove_upper(&dropped);
        self.changed();
        removed?;
        info!(
            "workspace {}: {} unchanged files dropped",
            self.branch,
            dropped.len()
        );
        Ok(())
    }

    fn remove_upper(&self, paths: &[Vec<u8>]) -> io::Result<()> {
        for path in paths {
            match fs::remove_file(self.upper_path(path)) {
                Err(e) if !not_found(&e) => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether the workspace's HEAD is on its branch and no git operation is
    /// running or stopped half-way in it.
    fn git_idle(&self) -> bool {
        worktree::git_busy(&self.common_dir, &self.admin, &self.branch).is_none()
    }

    fn rebase(&self, store: &Store, new: &Commit) -> Result<()> {
        let started = Instant::now();
        let new_tree = store.load_tree(new.tree)?;
        let mut layers = self.layers.write();
        let locked = Instant::now();
        let old = layers.base;
        if old == new.id {
            return Ok(());
        }
        // 1. Prepare.
        let mut plan = Plan::default();
        let old_tree = layers.base_tree;
        let prepared = self
            .prepare(
                store,
                &mut layers,
                b"",
                Some(old_tree),
                Some(new_tree),
                &mut plan,
            )
            .and_then(|()| self.prepare_opaque(store, &mut layers, new_tree, &mut plan));
        if plan.copied > 0 || plan.markers_changed {
            // Listings name entries, not layers; a copy-up turns a lower file into
            // an upper one even if a later step fails.
            self.changed();
        }
        prepared?;
        if plan.markers_changed {
            layers.markers.sync()?;
        }
        // Copies an earlier, failed attempt (or a killed process) made may not be
        // synced: they are in upper now, so this attempt doesn't count them as
        // created. Costs one `syncfs` on the first rebase after mounting.
        if self.rebase_failed.load(Ordering::Relaxed) {
            sync_fs(&self.upper)?;
        } else {
            self.sync_created(&plan.created)?;
        }

        // 2. Switch.
        let mut dropped = Vec::new();
        for (path, file_id) in std::mem::take(&mut plan.maybe_same) {
            if let Some(seen) = self.same_as_stored(store, &path, file_id)? {
                layers.times.set(&path, Some(seen.stamp));
                dropped.push(path);
            }
        }
        layers.times.save()?;
        worktree::write_atomic(
            &self.state_dir.join("base"),
            format!("{}\n", new.id).as_bytes(),
        )?;
        layers.base = new.id;
        layers.base_tree = new_tree;
        // Before anything below can fail: cached listings hold the old base's files.
        self.changed();

        // 3. Clean up. Opaque markers first, durably: a file dropped below one
        // still in place would vanish (the marker hides N's copy).
        for path in &plan.stale_opaque {
            layers.markers.set(Kind::Opaque, path, false)?;
        }
        if !plan.stale_opaque.is_empty() {
            layers.markers.sync()?;
        }
        self.remove_upper(&dropped)?;
        for path in &plan.stale_whiteouts {
            layers.markers.set(Kind::Whiteout, path, false)?;
        }
        self.changed();
        info!(
            "workspace {}: base {old} -> {}: {} copied up, {} whited out, {} dropped, {} whiteouts dropped; {:?} ({:?} locked)",
            self.branch,
            new.id,
            plan.copied,
            plan.whited_out,
            dropped.len(),
            plan.stale_whiteouts.len(),
            started.elapsed(),
            locked.elapsed()
        );
        Ok(())
    }

    /// Step 1 for directory `dir`, visible in the view with lower children
    /// `old` under the old base and `new` under the new one.
    fn prepare(
        &self,
        store: &Store,
        layers: &mut Layers,
        dir: &[u8],
        old: Option<u32>,
        new: Option<u32>,
        plan: &mut Plan,
    ) -> Result<()> {
        if old == new {
            return Ok(());
        }
        let old_t = old.map(|t| store.tree(t));
        let new_t = new.map(|t| store.tree(t));
        // Each name once: the old tree's entries, then the new tree's other ones.
        let in_old = (old_t.iter()).flat_map(|t| t.entries().iter().map(|e| (t.name(e), Some(*e))));
        let only_new = new_t.iter().flat_map(|t| {
            (t.entries().iter())
                .map(|e| t.name(e))
                .filter(|name| old_t.as_ref().is_none_or(|o| o.find(name).is_none()))
                .map(|name| (name, None))
        });
        for (name, o) in in_old.chain(only_new) {
            let n = new_t.as_ref().and_then(|t| t.find(name).copied());
            if same(o, n) {
                continue;
            }
            let path = join(dir, name);
            let stamp = layers.times.get(&path);
            if layers.markers.whiteouts.contains(&path) {
                layers.times.set(&path, None);
                if n.is_none() {
                    plan.stale_whiteouts.push(path);
                }
                continue;
            }
            match fs::symlink_metadata(self.upper_path(&path)) {
                // A merged directory: its lower children change; an opaque one has none.
                Ok(meta) if meta.is_dir() => {
                    if !layers.markers.opaque.contains(&path) {
                        self.prepare(store, layers, &path, children(o), children(n), plan)?;
                        // Left by an attempt that failed before marking it.
                        self.opaque_over_file(layers, &path, n, plan)?;
                    }
                }
                // Upper wins under both bases.
                Ok(_) => {
                    if let Some(n) = n.filter(|n| !n.is_dir()) {
                        plan.maybe_same.push((path.clone(), n.id));
                    }
                }
                // Below an upper file: hidden under both bases.
                Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => match o {
                    // Not in the view: keep it out.
                    None => {
                        layers.markers.set(Kind::Whiteout, &path, true)?;
                        plan.markers_changed = true;
                        plan.whited_out += 1;
                    }
                    // A lower file N changes: keep it as it shows.
                    Some(o) if !o.is_dir() => {
                        let shown = stamp.map_or(inode::file_time(), |s| s.mtime);
                        self.copy_file(store, &path, o.id, false, Some(shown))
                            .map_err(|e| anyhow!("copying up {}: {e:?}", show(&path)))?;
                        plan.created.push(path.clone());
                        plan.copied += 1;
                    }
                    // A lower directory: stays one, with its children kept as they show.
                    Some(o) => {
                        if children(n).is_none() {
                            // N has no directory here: an upper one.
                            self.ensure_upper_dir(&path)
                                .map_err(|e| anyhow!("creating {}: {e:?}", show(&path)))?;
                            plan.created.push(path.clone());
                        }
                        self.prepare(store, layers, &path, children(Some(o)), children(n), plan)?;
                        // Only once its children are all up: opaque earlier, a
                        // failure would hide the rest for good.
                        self.opaque_over_file(layers, &path, n, plan)?;
                    }
                },
                Err(e) => return Err(e.into()),
            }
            // Whatever the path shows from now on, it is not a stamped lower file
            // (step 2 stamps the files it drops again). Cleared only now, so a
            // failed copy-up is retried with the time the file showed.
            layers.times.set(&path, None);
        }
        Ok(())
    }

    /// Step 1 for opaque directories, which `prepare` doesn't enter (whether or
    /// not B and N differ there): where N has a directory, whiting out N's
    /// entries that upper doesn't have keeps them hidden without the marker,
    /// which step 3 then removes, so the directory's files can be compared with
    /// N's like any others from then on. Where N has nothing the marker goes
    /// too; over N's file it stays.
    fn prepare_opaque(
        &self,
        store: &Store,
        layers: &mut Layers,
        new_tree: u32,
        plan: &mut Plan,
    ) -> Result<()> {
        let opaque: Vec<Vec<u8>> = layers.markers.opaque.iter().cloned().collect();
        for path in opaque {
            // A marker left over something else than a directory (a crash in
            // `remove` before it was cleared) means nothing: drop it, and never
            // follow a symlink out of upper.
            // (Other errors fail the rebase: a transient one must not drop a live
            // marker.)
            let real_dir = |p: &[u8]| match fs::symlink_metadata(self.upper_path(p)) {
                Ok(meta) => Ok(meta.file_type().is_dir()),
                Err(e) if not_found(&e) => Ok(false),
                Err(e) => Err(e),
            };
            let mut all_dirs = real_dir(&path)?;
            for dir in ancestors(&path) {
                all_dirs = all_dirs && real_dir(dir)?;
            }
            if !all_dirs {
                plan.stale_opaque.push(path);
                continue;
            }
            let n = Self::lower_in(store, new_tree, &path);
            let children = match n {
                Some(Lower::Tree(tree)) => Some(tree),
                Some(Lower::Gitlink) => Some(EMPTY_TREE),
                Some(Lower::File(_)) => continue,
                None => None,
            };
            if let Some(children) = children {
                // Under B nothing below shows from the lower layer (old: None).
                self.prepare(store, layers, &path, None, Some(children), plan)?;
            }
            plan.stale_opaque.push(path);
        }
        Ok(())
    }

    /// Marks upper directory `path` opaque if N has a file there, like any
    /// directory created where a lower entry was.
    fn opaque_over_file(
        &self,
        layers: &mut Layers,
        path: &[u8],
        n: Option<TEntry>,
        plan: &mut Plan,
    ) -> Result<()> {
        if n.is_some() && children(n).is_none() && layers.markers.set(Kind::Opaque, path, true)? {
            plan.markers_changed = true;
        }
        Ok(())
    }

    /// Syncs the files and directories step 1 created, and the directories
    /// holding them, so they are durable before the base that relies on them is.
    fn sync_created(&self, created: &[Vec<u8>]) -> io::Result<()> {
        // One flush of the whole file system beats thousands of fsyncs (a 2,857-file
        // rebase: 0.40 s vs 2.26 s), though it also writes out others' data.
        if created.len() > SYNCFS_ABOVE {
            return sync_fs(&self.upper);
        }
        let mut dirs = HashSet::new();
        for path in created {
            let upper = self.upper_path(path);
            // A symlink is only its directory entry (and opening it would follow it).
            if !fs::symlink_metadata(&upper)?.file_type().is_symlink() {
                fs::File::open(&upper)?.sync_all()?;
            }
            dirs.insert(Vec::new());
            dirs.extend(ancestors(path).map(<[u8]>::to_vec));
        }
        for dir in dirs {
            fs::File::open(self.upper_path(&dir))?.sync_all()?;
        }
        Ok(())
    }

    /// If the upper file at `path` has the content and mode of stored file
    /// `file_id`, what it looked like.
    fn same_as_stored(&self, store: &Store, path: &[u8], file_id: u32) -> io::Result<Option<Seen>> {
        let Some(rec) = store.file(file_id) else {
            return Ok(None);
        };
        let upper = self.upper_path(path);
        let meta = match fs::symlink_metadata(&upper) {
            Ok(meta) => meta,
            Err(e) if not_found(&e) => return Ok(None), // dropped or replaced already
            Err(e) => return Err(e),
        };
        Ok(matches_stored(&upper, &meta, &rec, ModeCheck::Exact)?.then(|| Seen::of(&meta)))
    }
}

/// How `matches_stored` compares modes.
#[derive(Clone, Copy)]
pub(super) enum ModeCheck {
    /// All permission bits, as shown in the mount (an invisible rebase needs it).
    Exact,
    /// Only the executable bit, as git records it.
    Git,
}

/// Whether the file at `path` (with metadata `meta`) has the content and mode
/// of stored file `rec`.
pub(super) fn matches_stored(
    path: &Path,
    meta: &fs::Metadata,
    rec: &FileRec,
    modes: ModeCheck,
) -> io::Result<bool> {
    let data = if rec.kind == EntryKind::Link {
        if !meta.file_type().is_symlink() {
            return Ok(false);
        }
        fs::read_link(path)?.as_os_str().as_bytes().to_vec()
    } else {
        let same_mode = match modes {
            ModeCheck::Exact => meta.mode() & 0o7777 == rec.perm().into(),
            ModeCheck::Git => (meta.mode() & 0o100 != 0) == (rec.kind == EntryKind::BlobExecutable),
        };
        if !meta.is_file() || meta.len() != rec.size || !same_mode {
            return Ok(false);
        }
        fs::read(path)?
    };
    let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, &data)
        .map_err(io::Error::other)?;
    Ok(oid == rec.oid)
}

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// Flushes the file system holding `dir`.
fn sync_fs(dir: &std::path::Path) -> io::Result<()> {
    let f = fs::File::open(dir)?;
    if unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&f)) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
