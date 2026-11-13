//! Workspace mutations: copy-up, writes, attribute changes, creating, removing
//! and renaming entries.
//!
//! Each takes `layers` (exclusively to create, remove or rename; shared for
//! writes and attribute changes) *before* resolving the node's path, so a
//! concurrent rename can't redirect it, and calls `changed()` after any
//! structural change (create, remove, rename, copy-up), still under the lock.

use std::ffi::{CString, OsStr};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use fuser::Errno;
use gix::objs::tree::EntryKind;

use super::times::shown_mtime;
use super::{Layers, ListEntry, Obj, Workspace};
use crate::fsutil::{self, not_found};
use crate::git;
use crate::state::markers::Kind;
use crate::state::{ancestors, join, parent};
use crate::store::{Lower, Store};

/// Mode of directories created in upper to hold changed files: what the
/// directory showed as before (lower directories are 0755), independent of the
/// daemon's umask.
const DIR_MODE: u32 = 0o755;

/// Limits of copy-on-rename (see `Workspace::materialize`): renaming a directory
/// with more lower content returns EXDEV.
const COPY_ON_RENAME_FILES: usize = 10_000;
const COPY_ON_RENAME_BYTES: u64 = 100 << 20;

/// What copy-on-rename copies: lower files (path, file id) and lower
/// directories (also empty ones and submodules), parents first.
#[derive(Default)]
struct CopyPlan {
    files: Vec<(Vec<u8>, u32)>,
    dirs: Vec<Vec<u8>>,
    bytes: u64,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}

impl Workspace {
    /// Opens upper file `path` for writing (`write`) or reading, never through
    /// a symlink.
    ///
    /// There are no file handles, so every read and write reopens the file by
    /// path, also for a caller whose descriptor predates the file losing that
    /// permission: `tar` and git create files with mode 0444 and then write
    /// them, and a file created with mode 0200 can still be read through its
    /// descriptor. The kernel already checked the caller's right (at open, or
    /// at truncate(2)), so the owner's missing bit is lifted for the open and
    /// restored. That happens holding `modes` exclusively, and every `stat` of
    /// an upper file holds it shared (`Workspace::upper_meta`), so nobody sees,
    /// and the kernel never caches, the lifted mode; chmod takes it too, so a
    /// concurrent one isn't undone.
    pub(super) fn open_upper(&self, path: &Path, write: bool) -> io::Result<File> {
        let open = || {
            OpenOptions::new()
                .read(!write)
                .write(write)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
        };
        let denied = match open() {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => e,
            file => return file,
        };
        let _modes = self.modes.write();
        let mode = match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => meta.permissions().mode() & 0o7777,
            _ => return Err(denied),
        };
        let bit = if write { 0o200 } else { 0o400 };
        if mode & bit != 0 {
            return open(); // a chmod came in between
        }
        if fs::set_permissions(path, fs::Permissions::from_mode(mode | bit)).is_err() {
            return Err(denied);
        }
        let file = open();
        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
            log::error!("restoring the mode of {}: {e}", path.display());
        }
        file
    }

    /// Makes sure directory `path` (visible in the merged view) exists in upper.
    pub(super) fn ensure_upper_dir(&self, path: &[u8]) -> Result<(), Errno> {
        // Usually it exists: one stat instead of a mkdir per component.
        if path.is_empty() || self.is_upper_dir(path) {
            return Ok(());
        }
        for dir in ancestors(path).chain([path]) {
            if self.mkdir_upper(dir)? {
                self.set_dir_mode(dir)?;
            }
        }
        Ok(())
    }

    /// Creates directory `path` in upper, if it isn't there; returns whether it
    /// did. Its mode is then the caller's to set (`set_dir_mode`), once it has
    /// recorded the directory for any rollback.
    fn mkdir_upper(&self, path: &[u8]) -> Result<bool, Errno> {
        match fs::DirBuilder::new().create(self.upper_path(path)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Gives upper directory `path` the mode upper directories have (not the
    /// umask's).
    fn set_dir_mode(&self, path: &[u8]) -> Result<(), Errno> {
        let mode = fs::Permissions::from_mode(DIR_MODE);
        Ok(fs::set_permissions(self.upper_path(path), mode)?)
    }

    /// Whether a lower entry would show at `path` without an upper one.
    fn has_lower(&self, store: &Store, layers: &Layers, path: &[u8]) -> bool {
        self.visible_lower(store, layers, path).is_some()
    }

    /// Copies a lower file to upper (unless it is there already), so it can be
    /// modified. With `truncate`, creates it empty instead of copying content.
    /// Returns whether anything was copied (a structural change for listings).
    fn copy_up(
        &self,
        store: &Store,
        layers: &Layers,
        path: &[u8],
        truncate: bool,
    ) -> Result<bool, Errno> {
        if fs::symlink_metadata(self.upper_path(path)).is_ok() {
            return Ok(false);
        }
        // Writers hold `layers` shared; two of them copying up the same file could
        // otherwise rename a fresh copy over the other's first write.
        let _copying = self.copy_lock.lock();
        let file_id = match self.resolve_in(store, layers, path)? {
            Obj::Upper(_) => return Ok(false),
            Obj::LowerFile { file_id, .. } => file_id,
            Obj::LowerDir => return self.ensure_upper_dir(path).map(|()| true),
        };
        self.copy_file(store, path, file_id, truncate, None)?;
        Ok(true)
    }

    /// Writes stored file `file_id` to upper at `path` (empty with `truncate`),
    /// creating its parent directories, with mtime and atime `time` if given.
    pub(super) fn copy_file(
        &self,
        store: &Store,
        path: &[u8],
        file_id: u32,
        truncate: bool,
        time: Option<SystemTime>,
    ) -> Result<(), Errno> {
        self.ensure_upper_dir(parent(path))?;
        let rec = store.file(file_id).ok_or(Errno::EIO)?;
        let target = self.upper_path(path);
        let content = if truncate && rec.kind != EntryKind::Link {
            Vec::new()
        } else {
            git::read_blob(rec.oid).map_err(|_| Errno::EIO)?
        };
        let tmp = self
            .tmp
            .join(TMP_SEQ.fetch_add(1, Ordering::Relaxed).to_string());
        let written = (|| {
            if rec.kind == EntryKind::Link {
                std::os::unix::fs::symlink(OsStr::from_bytes(&content), &tmp)?;
            } else {
                let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
                file.write_all_at(&content, 0)?;
                // Exactly the mode it showed as, whatever the daemon's umask.
                fs::set_permissions(&tmp, fs::Permissions::from_mode(rec.perm().into()))?;
            }
            if let Some(time) = time {
                fsutil::set_times(&tmp, Some(time), Some(time))?;
            }
            fs::rename(&tmp, &target)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        Ok(written?)
    }

    pub fn write(&self, store: &Store, local: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let layers = self.layers.read();
        let path = self.path(local)?;
        self.touch(&path);
        // A copy-up turns a lower entry into an upper one, even if the write then
        // fails (listings stat upper entries when serving them, so plain writes
        // need no invalidation).
        let upper = self.upper_path(&path);
        let open = || self.open_upper(&upper, true);
        let file = match open() {
            Err(e) if not_found(&e) => {
                if self.copy_up(store, &layers, &path, false)? {
                    self.changed();
                }
                open()?
            }
            file => file?,
        };
        file.write_all_at(data, offset)?;
        Ok(data.len() as u32)
    }

    /// Flushes an upper file to stable storage (lower content is in git already).
    pub fn fsync(&self, store: &Store, local: u64) -> Result<(), Errno> {
        let layers = self.layers.read();
        let path = self.path(local)?;
        if let Obj::Upper(meta) = self.resolve_in(store, &layers, &path)?
            && meta.is_file()
        {
            self.open_upper(&self.upper_path(&path), false)?
                .sync_all()?;
        }
        Ok(())
    }

    pub fn setattr(
        &self,
        store: &Store,
        local: u64,
        mode: Option<u32>,
        size: Option<u64>,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> Result<(), Errno> {
        if mode.is_none() && size.is_none() && atime.is_none() && mtime.is_none() {
            return Ok(());
        }
        let layers = self.layers.read();
        let path = self.path(local)?;
        self.touch(&path);
        if self.copy_up(store, &layers, &path, size == Some(0))? {
            self.changed();
        }
        let target = self.upper_path(&path);
        let is_link = fs::symlink_metadata(&target)?.file_type().is_symlink();
        if let Some(size) = size {
            if is_link {
                return Err(Errno::EINVAL);
            }
            self.open_upper(&target, true)?.set_len(size)?;
        }
        // Never follow a symlink: its target may be anywhere on the host. Linux
        // symlinks have no mode of their own, so chmod on one is a no-op.
        if let Some(mode) = mode
            && !is_link
        {
            let _modes = self.modes.write(); // see `open_upper`
            fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o7777))?;
        }
        if atime.is_some() || mtime.is_some() {
            fsutil::set_times(&target, atime, mtime)?;
        }
        Ok(())
    }

    /// Validates a new entry `name` in `parent` (at `parent_path`); returns its path.
    fn new_entry_path(
        &self,
        store: &Store,
        layers: &Layers,
        parent_path: &[u8],
        parent: u64,
        name: &[u8],
    ) -> Result<Vec<u8>, Errno> {
        if self.is_virtual(parent, name) {
            return Err(Errno::EPERM);
        }
        let parent_obj = self.resolve_in(store, layers, parent_path)?;
        if !parent_obj.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let path = join(parent_path, name);
        // An upper entry makes the creation itself fail with EEXIST.
        if self.has_lower(store, layers, &path) {
            return Err(Errno::EEXIST);
        }
        if !matches!(parent_obj, Obj::Upper(_)) {
            self.ensure_upper_dir(parent_path)?;
        }
        Ok(path)
    }

    /// Creates `name` in `parent` with `make(upper path)`, which returns the new
    /// entry's metadata; returns its local id (with the kernel's reference
    /// counted) and object.
    fn new_entry(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        make: impl FnOnce(&Path) -> io::Result<Metadata>,
    ) -> Result<(u64, Obj), Errno> {
        let mut layers = self.layers.write();
        let parent_path = self.path(parent)?;
        let path = self.new_entry_path(store, &layers, &parent_path, parent, name)?;
        let upper = self.upper_path(&path);
        self.touch(&path);
        let made = make(&upper);
        if made.is_ok() || upper.symlink_metadata().is_ok() {
            // The entry exists from here on, whatever fails below.
            self.changed();
        }
        let obj = Obj::Upper(made?);
        // A directory recreated over a deleted lower one must not show its old content.
        if layers.markers.set(Kind::Whiteout, &path, false)? && obj.is_dir() {
            layers.markers.set(Kind::Opaque, &path, true)?;
        }
        Ok((self.nodes.lock().claim(parent, name, &parent_path), obj))
    }

    pub fn create(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        mode: u32,
    ) -> Result<(u64, Obj), Errno> {
        self.new_entry(store, parent, name, |path| {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode & 0o7777)
                .open(path)?;
            // The requested mode (already masked by the caller's umask), without
            // the daemon's umask applied on top.
            file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
            file.metadata()
        })
    }

    pub fn mkdir(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        mode: u32,
    ) -> Result<(u64, Obj), Errno> {
        self.new_entry(store, parent, name, |path| {
            fs::DirBuilder::new().create(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?;
            fs::symlink_metadata(path)
        })
    }

    pub fn symlink(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        target: &Path,
    ) -> Result<(u64, Obj), Errno> {
        // Symlinks have no mode of their own.
        self.new_entry(store, parent, name, |path| {
            std::os::unix::fs::symlink(target, path)?;
            fs::symlink_metadata(path)
        })
    }

    /// Removes `name` from `parent`: the upper copy if any, plus a whiteout if a
    /// lower entry would otherwise show through.
    fn remove(&self, store: &Store, parent: u64, name: &[u8], dir: bool) -> Result<(), Errno> {
        if self.is_virtual(parent, name) {
            return Err(Errno::EPERM);
        }
        let mut layers = self.layers.write();
        let path = join(&self.path(parent)?, name);
        let obj = self.resolve_in(store, &layers, &path)?;
        if obj.is_dir() != dir {
            return Err(if dir { Errno::ENOTDIR } else { Errno::EISDIR });
        }
        // Checked under the same exclusive lock as the removal, so nothing can be
        // created in between.
        if dir && !self.entries(store, &layers, &path)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        // The whiteout is logged before anything is removed: after a crash in
        // between, the upper copy still wins over it, while the other order would
        // let the deleted lower content reappear. After an opaque directory is
        // removed, the lower one it replaced would show again, so it needs one too.
        if self.has_lower(store, &layers, &path) {
            layers.markers.set(Kind::Whiteout, &path, true)?;
        }
        let upper = self.upper_path(&path);
        let removed = if dir {
            fs::remove_dir(&upper)
        } else {
            fs::remove_file(&upper)
        };
        match removed {
            Ok(()) => {}
            Err(e) if not_found(&e) => {}
            Err(e) => return Err(e.into()),
        }
        if dir {
            // Markers below an empty directory are moot once it is gone (and
            // covered by its whiteout, logged above).
            for kind in [Kind::Whiteout, Kind::Opaque] {
                layers.markers.clear_below(kind, &path)?;
            }
        }
        layers.markers.set(Kind::Opaque, &path, false)?;
        self.changed();
        self.nodes.lock().orphan(parent, name);
        Ok(())
    }

    pub fn unlink(&self, store: &Store, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.remove(store, parent, name, false)
    }

    /// Makes directory `path` one held entirely in upper, so it can be renamed
    /// like any upper directory: its visible lower content is copied up (files
    /// keeping the mtime they showed), synced, then the directory is marked
    /// opaque. EXDEV, copying nothing, if that content exceeds the
    /// copy-on-rename limits; also EXDEV, with the partial copies removed, if
    /// copying fails (`mv` then copies by itself).
    ///
    /// Until the opaque marker, the copies change nothing visible but the ctime
    /// of those files and the times (and size) of directories that become
    /// upper ones. The marker goes after the copies are durable and before
    /// clearing the whiteouts below it, which it makes moot.
    fn materialize(&self, store: &Store, layers: &mut Layers, path: &[u8]) -> Result<(), Errno> {
        let mut plan = CopyPlan::default();
        self.plan_copy(store, layers, path, &mut plan)?;
        let mut created = Vec::new();
        let copied = self.copy_plan(store, layers, path, &plan, &mut created);
        if let Err(e) = copied.and_then(|()| Ok(self.sync_created(&created)?)) {
            log::warn!("copy-on-rename of {}: {e:?}", String::from_utf8_lossy(path));
            for p in created.iter().rev() {
                let upper = self.upper_path(p);
                let _ = fs::remove_file(&upper).or_else(|_| fs::remove_dir(&upper));
            }
            return Err(Errno::EXDEV);
        }
        // Listings name lower files that are upper ones from here on.
        self.changed();
        for (file, _) in &plan.files {
            layers.times.set(file, None); // no longer lower files
        }
        layers.times.save()?;
        layers.markers.make_opaque(path)?;
        self.changed();
        Ok(())
    }

    /// Collects the lower entries visible below directory `path` into `plan`;
    /// EXDEV as soon as they exceed the copy-on-rename limits. Upper
    /// subdirectories are only entered if they merge a lower one.
    fn plan_copy(
        &self,
        store: &Store,
        layers: &Layers,
        path: &[u8],
        plan: &mut CopyPlan,
    ) -> Result<(), Errno> {
        for (name, entry) in self.entries(store, layers, path)? {
            let child = join(path, &name);
            match entry {
                ListEntry::LowerFile(id) => {
                    plan.bytes += store.file(id).map_or(0, |f| f.size);
                    plan.files.push((child, id));
                    if plan.files.len() > COPY_ON_RENAME_FILES || plan.bytes > COPY_ON_RENAME_BYTES
                    {
                        return Err(Errno::EXDEV);
                    }
                }
                // Also a submodule: an empty directory.
                ListEntry::LowerDir => {
                    plan.dirs.push(child.clone());
                    self.plan_copy(store, layers, &child, plan)?;
                }
                ListEntry::Upper
                    if matches!(
                        self.visible_lower(store, layers, &child),
                        Some(Lower::Tree(_))
                    ) && self.is_upper_dir(&child) =>
                {
                    self.plan_copy(store, layers, &child, plan)?
                }
                ListEntry::Upper => {}
            }
        }
        Ok(())
    }

    /// Carries out `plan` for directory `path`, recording what it creates
    /// (directories before what they hold) in `created`.
    fn copy_plan(
        &self,
        store: &Store,
        layers: &Layers,
        path: &[u8],
        plan: &CopyPlan,
        created: &mut Vec<Vec<u8>>,
    ) -> Result<(), Errno> {
        self.ensure_upper_dir(parent(path))?;
        let mut mkdir = |dir: &[u8]| -> Result<(), Errno> {
            if self.mkdir_upper(dir)? {
                created.push(dir.to_vec());
                self.set_dir_mode(dir)?;
            }
            Ok(())
        };
        mkdir(path)?;
        for dir in &plan.dirs {
            mkdir(dir)?;
        }
        for (file, id) in &plan.files {
            let shown = shown_mtime(layers.times.get(file));
            self.copy_file(store, file, *id, false, Some(shown))?;
            created.push(file.clone());
        }
        Ok(())
    }

    /// Whether `path` is a directory (not a symlink to one) in upper.
    fn is_upper_dir(&self, path: &[u8]) -> bool {
        fs::symlink_metadata(self.upper_path(path)).is_ok_and(|m| m.is_dir())
    }

    pub fn rmdir(&self, store: &Store, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.remove(store, parent, name, true)
    }

    /// Renames within the workspace. A directory with lower content is first
    /// copied into upper (`materialize`), up to `COPY_ON_RENAME_FILES` files and
    /// `COPY_ON_RENAME_BYTES`; above that, or if copying fails, EXDEV (like
    /// overlayfs without redirect_dir), and `mv` copies and deletes by itself.
    pub fn rename(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        new_parent: u64,
        new_name: &[u8],
        noreplace: bool,
    ) -> Result<(), Errno> {
        if self.is_virtual(parent, name) || self.is_virtual(new_parent, new_name) {
            return Err(Errno::EPERM);
        }
        let mut layers = self.layers.write();
        let src = join(&self.path(parent)?, name);
        let dst_parent = self.path(new_parent)?;
        let dst = join(&dst_parent, new_name);
        if src == dst {
            return Ok(());
        }
        let src_obj = self.resolve_in(store, &layers, &src)?;
        let src_has_lower = self.has_lower(store, &layers, &src);
        // Every check on the destination before copying anything.
        let mut replace_lower_dir = false;
        match self.resolve_in(store, &layers, &dst) {
            Ok(_) if noreplace => return Err(Errno::EEXIST),
            Ok(dst_obj) => {
                if dst_obj.is_dir() != src_obj.is_dir() {
                    return Err(if dst_obj.is_dir() {
                        Errno::EISDIR
                    } else {
                        Errno::ENOTDIR
                    });
                }
                if dst_obj.is_dir() {
                    if !self.entries(store, &layers, &dst)?.is_empty() {
                        return Err(Errno::ENOTEMPTY);
                    }
                    replace_lower_dir = self.has_lower(store, &layers, &dst);
                }
            }
            Err(e) if e == Errno::ENOENT => {}
            Err(e) => return Err(e),
        }
        if src_obj.is_dir() && src_has_lower && !layers.markers.opaque.contains(&src) {
            self.materialize(store, &mut layers, &src)?;
        }
        if replace_lower_dir {
            // Empty in the view: nothing to copy, just an opaque upper directory.
            self.ensure_upper_dir(&dst)?;
            layers.markers.make_opaque(&dst)?;
            self.changed();
        }
        self.copy_up(store, &layers, &src, false)?;
        self.ensure_upper_dir(&dst_parent)?;
        // As in `remove`: the source's whiteout first (its upper copy still wins
        // until the rename), markers about the destination after it.
        if src_has_lower {
            layers.markers.set(Kind::Whiteout, &src, true)?;
        }
        fs::rename(self.upper_path(&src), self.upper_path(&dst))?;
        if src_obj.is_dir() {
            // Markers inside a moved (pure upper) directory move with it.
            for kind in [Kind::Whiteout, Kind::Opaque] {
                for p in layers.markers.below(kind, &src) {
                    layers
                        .markers
                        .set(kind, &[&dst[..], &p[src.len()..]].concat(), true)?;
                    layers.markers.set(kind, &p, false)?;
                }
            }
        } else {
            self.touch(&dst);
        }
        let was_opaque = layers.markers.set(Kind::Opaque, &src, false)?;
        let dst_whiteout = layers.markers.set(Kind::Whiteout, &dst, false)?;
        if src_obj.is_dir() && (was_opaque || dst_whiteout) {
            layers.markers.set(Kind::Opaque, &dst, true)?;
        }
        self.changed();
        self.nodes
            .lock()
            .reparent(parent, name, new_parent, new_name);
        Ok(())
    }

    pub fn statfs(&self) -> Result<libc::statvfs, Errno> {
        let path = cstring(&self.upper)?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(st)
    }
}
