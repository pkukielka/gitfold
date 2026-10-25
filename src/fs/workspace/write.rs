//! Workspace mutations: copy-up, writes, attribute changes, creating, removing
//! and renaming entries.
//!
//! Each takes `layers` (exclusively, or shared for writes into one file)
//! *before* resolving the node's path, so a concurrent rename can't redirect
//! it, and calls `changed()` after changing anything, still under the lock.

use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fuser::Errno;
use gix::objs::tree::EntryKind;

use super::markers::{Kind, Markers};
use super::{Obj, Workspace, join, not_found};
use crate::git;
use crate::store::Store;

/// Mode of directories created in upper to hold changed files: what the
/// directory showed as before (lower directories are 0755), independent of the
/// daemon's umask.
const DIR_MODE: u32 = 0o755;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn cstring(path: &Path) -> Result<CString, Errno> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)
}

fn timespec(t: Option<SystemTime>) -> libc::timespec {
    match t {
        Some(t) => {
            let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
            libc::timespec {
                tv_sec: d.as_secs() as libc::time_t,
                tv_nsec: d.subsec_nanos() as _,
            }
        }
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    }
}

impl Workspace {
    /// Makes sure directory `path` (visible in the merged view) exists in upper.
    fn ensure_upper_dir(&self, path: &[u8]) -> Result<(), Errno> {
        let mut prefix: Vec<u8> = Vec::new();
        for comp in path.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(comp);
            let dir = self.upper_path(&prefix);
            match fs::DirBuilder::new().create(&dir) {
                Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(DIR_MODE))?,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Whether a lower entry would show at `path` without an upper one.
    fn has_lower(&self, store: &Store, layers: &Markers, path: &[u8]) -> bool {
        self.visible_lower(store, layers, path).is_some()
    }

    /// Copies a lower file to upper (unless it is there already), so it can be
    /// modified. With `truncate`, creates it empty instead of copying content.
    /// Returns whether anything was copied (a structural change for listings).
    fn copy_up(
        &self,
        store: &Store,
        layers: &Markers,
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
            Obj::LowerFile { file_id } => file_id,
            Obj::LowerDir => return self.ensure_upper_dir(path).map(|()| true),
        };
        let parent = &path[..path.iter().rposition(|b| *b == b'/').unwrap_or(0)];
        self.ensure_upper_dir(parent)?;
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
            fs::rename(&tmp, &target)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        Ok(true)
    }

    pub fn write(&self, store: &Store, local: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let layers = self.layers.read();
        let path = self.path(local)?;
        // A copy-up turns a lower entry into an upper one, even if the write then
        // fails (listings stat upper entries when serving them, so plain writes
        // need no invalidation).
        if self.copy_up(store, &layers, &path, false)? {
            self.changed();
        }
        let file = OpenOptions::new()
            .write(true)
            .open(self.upper_path(&path))?;
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
            File::open(self.upper_path(&path))?.sync_all()?;
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
        if self.copy_up(store, &layers, &path, size == Some(0))? {
            self.changed();
        }
        let target = self.upper_path(&path);
        let is_link = fs::symlink_metadata(&target)?.file_type().is_symlink();
        if let Some(size) = size {
            if is_link {
                return Err(Errno::EINVAL);
            }
            OpenOptions::new()
                .write(true)
                .open(&target)?
                .set_len(size)?;
        }
        // Never follow a symlink: its target may be anywhere on the host. Linux
        // symlinks have no mode of their own, so chmod on one is a no-op.
        if let Some(mode) = mode
            && !is_link
        {
            fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o7777))?;
        }
        if atime.is_some() || mtime.is_some() {
            let times = [timespec(atime), timespec(mtime)];
            let path = cstring(&target)?;
            let flags = libc::AT_SYMLINK_NOFOLLOW;
            if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
        }
        Ok(())
    }

    /// Validates a new entry `name` in `parent` (at `parent_path`); returns its path.
    fn new_entry_path(
        &self,
        store: &Store,
        layers: &Markers,
        parent_path: &[u8],
        parent: u64,
        name: &[u8],
    ) -> Result<Vec<u8>, Errno> {
        if parent == 0 && name == b".git" {
            return Err(Errno::EPERM);
        }
        if !self.resolve_in(store, layers, parent_path)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let path = join(parent_path, name);
        if self.resolve_in(store, layers, &path).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.ensure_upper_dir(parent_path)?;
        Ok(path)
    }

    /// Creates `name` in `parent` with `make(upper path)` and gives it `mode`;
    /// returns its local id (with the kernel's reference counted) and object.
    fn new_entry(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        mode: Option<u32>,
        make: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<(u64, Obj), Errno> {
        let mut layers = self.layers.write();
        let parent_path = self.path(parent)?;
        let path = self.new_entry_path(store, &layers, &parent_path, parent, name)?;
        let upper = self.upper_path(&path);
        make(&upper)?;
        // The entry exists from here on, whatever fails below.
        self.changed();
        let obj = self.resolve_in(store, &layers, &path)?;
        // The requested mode (already masked by the caller's umask), without the
        // daemon's umask applied on top; symlinks have none.
        if let (Some(mode), Obj::Upper(meta)) = (mode, &obj)
            && !meta.file_type().is_symlink()
        {
            fs::set_permissions(&upper, fs::Permissions::from_mode(mode & 0o7777))?;
        }
        let obj = self.resolve_in(store, &layers, &path)?;
        // A directory recreated over a deleted lower one must not show its old content.
        if layers.set(Kind::Whiteout, &path, false)? && obj.is_dir() {
            layers.set(Kind::Opaque, &path, true)?;
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
        self.new_entry(store, parent, name, Some(mode), |path| {
            let file = OpenOptions::new().write(true).create_new(true).open(path);
            file.map(drop)
        })
    }

    pub fn mkdir(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        mode: u32,
    ) -> Result<(u64, Obj), Errno> {
        self.new_entry(store, parent, name, Some(mode), |path| {
            fs::DirBuilder::new().create(path)
        })
    }

    pub fn symlink(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        target: &Path,
    ) -> Result<(u64, Obj), Errno> {
        self.new_entry(store, parent, name, None, |path| {
            std::os::unix::fs::symlink(target, path)
        })
    }

    /// Removes `name` from `parent`: the upper copy if any, plus a whiteout if a
    /// lower entry would otherwise show through.
    fn remove(&self, store: &Store, parent: u64, name: &[u8], dir: bool) -> Result<(), Errno> {
        if parent == 0 && name == b".git" {
            return Err(Errno::EPERM); // virtual
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
            layers.set(Kind::Whiteout, &path, true)?;
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
                for p in layers.below(kind, &path) {
                    layers.set(kind, &p, false)?;
                }
            }
        }
        layers.set(Kind::Opaque, &path, false)?;
        self.changed();
        self.nodes.lock().orphan(parent, name);
        Ok(())
    }

    pub fn unlink(&self, store: &Store, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.remove(store, parent, name, false)
    }

    pub fn rmdir(&self, store: &Store, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.remove(store, parent, name, true)
    }

    /// Renames within the workspace. Directories that have lower content return
    /// EXDEV (like overlayfs without redirect_dir); `mv` then copies and deletes.
    pub fn rename(
        &self,
        store: &Store,
        parent: u64,
        name: &[u8],
        new_parent: u64,
        new_name: &[u8],
        noreplace: bool,
    ) -> Result<(), Errno> {
        if new_parent == 0 && new_name == b".git" {
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
        if src_obj.is_dir() && src_has_lower && !layers.opaque.contains(&src) {
            return Err(Errno::EXDEV);
        }
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
                if dst_obj.is_dir() && self.has_lower(store, &layers, &dst) {
                    return Err(Errno::EXDEV);
                }
            }
            Err(e) if e == Errno::ENOENT => {}
            Err(e) => return Err(e),
        }
        self.copy_up(store, &layers, &src, false)?;
        self.ensure_upper_dir(&dst_parent)?;
        // As in `remove`: the source's whiteout first (its upper copy still wins
        // until the rename), markers about the destination after it.
        if src_has_lower {
            layers.set(Kind::Whiteout, &src, true)?;
        }
        fs::rename(self.upper_path(&src), self.upper_path(&dst))?;
        if src_obj.is_dir() {
            // Markers inside a moved (pure upper) directory move with it.
            for kind in [Kind::Whiteout, Kind::Opaque] {
                for p in layers.below(kind, &src) {
                    layers.set(kind, &[&dst[..], &p[src.len()..]].concat(), true)?;
                    layers.set(kind, &p, false)?;
                }
            }
        }
        let was_opaque = layers.set(Kind::Opaque, &src, false)?;
        let dst_whiteout = layers.set(Kind::Whiteout, &dst, false)?;
        if src_obj.is_dir() && (was_opaque || dst_whiteout) {
            layers.set(Kind::Opaque, &dst, true)?;
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
