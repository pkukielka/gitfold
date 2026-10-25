//! What an inode is: attributes, lookups, directory listings and content,
//! answered from the snapshot registry and the shared store.

use std::sync::Arc;
use std::time::SystemTime;

use fuser::{Errno, FileAttr, FileType, INodeNo};
use gix::objs::tree::EntryKind;
use log::error;

use super::GitFold;
use crate::git;
use crate::inode::{self, Ino};

fn decode(ino: u64) -> Result<Ino, Errno> {
    inode::decode(ino).ok_or(Errno::ENOENT)
}

impl GitFold {
    fn make_attr(
        &self,
        ino: u64,
        kind: FileType,
        perm: u16,
        size: u64,
        time: SystemTime,
    ) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.stat.uid,
            gid: self.stat.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    pub(super) fn attr(&self, ino: u64) -> Result<FileAttr, Errno> {
        let time = inode::file_time();
        match decode(ino)? {
            Ino::Root => {
                let mtime = self.snapshots.root_mtime();
                Ok(self.make_attr(ino, FileType::Directory, 0o755, 0, mtime))
            }
            Ino::Dir { slot, dir } => {
                self.snapshots.dir_rec(slot, dir)?;
                Ok(self.make_attr(ino, FileType::Directory, 0o755, 0, time))
            }
            Ino::GitFile { slot } => {
                let size = self.snapshots.get(slot)?.gitfile.len() as u64;
                Ok(self.make_attr(ino, FileType::RegularFile, 0o644, size, time))
            }
            Ino::File(id) => {
                let f = self.store.file(id).ok_or(Errno::ENOENT)?;
                let (kind, perm) = match f.kind {
                    EntryKind::BlobExecutable => (FileType::RegularFile, 0o755),
                    EntryKind::Link => (FileType::Symlink, 0o777),
                    _ => (FileType::RegularFile, 0o644),
                };
                Ok(self.make_attr(ino, kind, perm, f.size, time))
            }
        }
    }

    pub(super) fn is_file(ino: INodeNo) -> Result<bool, Errno> {
        Ok(matches!(decode(ino.0)?, Ino::File(_) | Ino::GitFile { .. }))
    }

    pub(super) fn lookup_ino(&self, parent: INodeNo, name: &[u8]) -> Result<u64, Errno> {
        match decode(parent.0)? {
            Ino::Root => self.snapshots.resolve(&self.store, name),
            Ino::Dir { slot, dir } => {
                let rec = self.snapshots.dir_rec(slot, dir)?;
                if dir == 0 && name == b".git" {
                    return Ok(inode::gitfile(slot));
                }
                let table = self.store.tree(rec.tree);
                let e = table.find(name).ok_or(Errno::ENOENT)?;
                Ok(rec.child_ino(slot, e))
            }
            _ => Err(Errno::ENOTDIR),
        }
    }

    /// Calls `emit(next_offset, ino, name, attr)` for the entries of directory
    /// `ino` (including `.` and `..`) after `offset`, until it returns true
    /// (reply buffer full).
    pub(super) fn for_each_entry(
        &self,
        ino: INodeNo,
        offset: u64,
        emit: impl FnMut(u64, u64, &[u8], FileAttr) -> bool,
    ) -> Result<(), Errno> {
        let dots = [(ino.0, &b"."[..]), (inode::ROOT, &b".."[..])];
        match decode(ino.0)? {
            Ino::Root => {
                let names = self.snapshots.root_entries();
                let entries = names.iter().map(|(ino, name)| (*ino, name.as_slice()));
                self.emit_entries(dots.into_iter().chain(entries), offset, emit)
            }
            Ino::Dir { slot, dir } => {
                let rec = self.snapshots.dir_rec(slot, dir)?;
                let table = self.store.tree(rec.tree);
                let entries =
                    (table.entries().iter()).map(|e| (rec.child_ino(slot, e), table.name(e)));
                let gitfile = (dir == 0).then(|| (inode::gitfile(slot), &b".git"[..]));
                self.emit_entries(dots.into_iter().chain(entries).chain(gitfile), offset, emit)
            }
            _ => Err(Errno::ENOTDIR),
        }
    }

    fn emit_entries<'a>(
        &self,
        entries: impl Iterator<Item = (u64, &'a [u8])>,
        offset: u64,
        mut emit: impl FnMut(u64, u64, &[u8], FileAttr) -> bool,
    ) -> Result<(), Errno> {
        for (i, (ino, name)) in entries.enumerate().skip(offset as usize) {
            if emit(i as u64 + 1, ino, name, self.attr(ino)?) {
                break;
            }
        }
        Ok(())
    }

    /// Full content of a file or symlink (the target), bypassing the content cache.
    pub(super) fn load_content(&self, ino: INodeNo) -> Result<Arc<Vec<u8>>, Errno> {
        match decode(ino.0)? {
            Ino::File(id) => {
                let f = self.store.file(id).ok_or(Errno::ENOENT)?;
                let data = git::read_blob(f.oid).map_err(|e| {
                    error!("reading blob {}: {e:#}", f.oid);
                    Errno::EIO
                })?;
                Ok(Arc::new(data))
            }
            Ino::GitFile { slot } => Ok(self.snapshots.get(slot)?.gitfile.clone()),
            _ => Err(Errno::EISDIR),
        }
    }
}
