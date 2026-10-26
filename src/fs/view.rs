//! What an inode is: attributes, lookups, directory listings and content,
//! answered from the snapshot and workspace registries and the shared store.

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{Errno, FileAttr, FileType, INodeNo};
use gix::objs::tree::EntryKind;
use log::error;

use super::workspace::{GITFILE_LOCAL, Obj, Read, Workspace};
use super::{GitFold, IMMUTABLE_TTL, NAMESPACE_TTL, ROOT_TTL};
use crate::git;
use crate::inode::{self, Ino};

fn decode(ino: u64) -> Result<Ino, Errno> {
    inode::decode(ino).ok_or(Errno::ENOENT)
}

/// What a workspace inode refers to.
pub(super) enum WsNode {
    /// A namespace directory (full path, e.g. `feature`).
    Namespace(String),
    GitFile(Arc<Workspace>),
    /// A file or directory; local 0 is the workspace root.
    Entry(Arc<Workspace>, u64),
}

/// Content for a read: a whole cached file, or just the requested range.
pub(super) enum Content {
    Whole(Arc<Vec<u8>>),
    Range(Vec<u8>),
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

    /// Attributes of a stored file. Read-only ones (snapshots) drop the write
    /// bits, so access checks agree with the EROFS that writes get.
    fn file_attr(&self, ino: u64, file_id: u32, writable: bool) -> Result<FileAttr, Errno> {
        let f = self.store.file(file_id).ok_or(Errno::ENOENT)?;
        let kind = if f.kind == EntryKind::Link {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
        let perm = match kind {
            FileType::RegularFile if !writable => f.perm() & !0o222,
            _ => f.perm(),
        };
        Ok(self.make_attr(ino, kind, perm, f.size, inode::file_time()))
    }

    /// Attributes of a workspace entry.
    pub(super) fn obj_attr(&self, ino: u64, obj: &Obj) -> Result<FileAttr, Errno> {
        match obj {
            Obj::Upper(meta) => {
                let ft = meta.file_type();
                let kind = if ft.is_dir() {
                    FileType::Directory
                } else if ft.is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                };
                let mut attr = self.make_attr(
                    ino,
                    kind,
                    (meta.mode() & 0o7777) as u16,
                    meta.len(),
                    inode::ctime(meta),
                );
                attr.mtime = meta.modified().unwrap_or(attr.mtime);
                attr.atime = meta.accessed().unwrap_or(attr.atime);
                Ok(attr)
            }
            Obj::LowerDir => {
                Ok(self.make_attr(ino, FileType::Directory, 0o755, 0, inode::file_time()))
            }
            Obj::LowerFile { file_id, stamp } => {
                let mut attr = self.file_attr(ino, *file_id, true)?;
                if let Some(stamp) = stamp {
                    (attr.atime, attr.mtime, attr.ctime) = (stamp.mtime, stamp.mtime, stamp.ctime);
                }
                Ok(attr)
            }
        }
    }

    /// mtime of the root and namespace directories, whose listings grow.
    fn names_mtime(&self) -> SystemTime {
        self.snapshots.root_mtime().max(self.workspaces.mtime())
    }

    pub(super) fn ws_node(&self, slot: u32, local: u64) -> Result<WsNode, Errno> {
        Ok(match (self.workspaces.get(slot)?, local) {
            (Err(path), 0) => WsNode::Namespace(path),
            (Err(_), _) => return Err(Errno::ENOENT),
            (Ok(ws), GITFILE_LOCAL) => WsNode::GitFile(ws),
            (Ok(ws), local) => WsNode::Entry(ws, local),
        })
    }

    pub(super) fn ttl(&self, ino: u64) -> &'static Duration {
        match inode::decode(ino) {
            Some(Ino::Root) => &ROOT_TTL,
            Some(Ino::Ws { slot, local: 0 }) if self.workspaces.is_namespace(slot) => {
                &NAMESPACE_TTL
            }
            _ => &IMMUTABLE_TTL,
        }
    }

    pub(super) fn attr(&self, ino: u64) -> Result<FileAttr, Errno> {
        let time = inode::file_time();
        match decode(ino)? {
            Ino::Root => Ok(self.make_attr(ino, FileType::Directory, 0o555, 0, self.names_mtime())),
            Ino::Dir { slot, dir } => {
                self.snapshots.dir_rec(slot, dir)?;
                Ok(self.make_attr(ino, FileType::Directory, 0o555, 0, time))
            }
            Ino::GitFile { slot } => {
                let size = self.snapshots.get(slot)?.gitfile.len() as u64;
                Ok(self.make_attr(ino, FileType::RegularFile, 0o444, size, time))
            }
            Ino::File(id) => self.file_attr(ino, id, false),
            Ino::Ws { slot, local } => match self.ws_node(slot, local)? {
                WsNode::Namespace(_) => {
                    Ok(self.make_attr(ino, FileType::Directory, 0o555, 0, self.names_mtime()))
                }
                WsNode::GitFile(ws) => {
                    let size = ws.gitfile.len() as u64;
                    Ok(self.make_attr(ino, FileType::RegularFile, 0o444, size, time))
                }
                WsNode::Entry(ws, local) => {
                    let obj = ws.resolve_local(&self.store, local)?;
                    self.obj_attr(ino, &obj)
                }
            },
        }
    }

    pub(super) fn is_file(&self, ino: INodeNo) -> Result<bool, Errno> {
        Ok(self.attr(ino.0)?.kind != FileType::Directory)
    }

    /// Resolves a name below the mount root (a full path for names in namespaces).
    fn resolve_name(&self, path: &[u8]) -> Result<u64, Errno> {
        let path = std::str::from_utf8(path).map_err(|_| Errno::ENOENT)?;
        self.workspaces.resolve(&self.store, &self.snapshots, path)
    }

    /// Attributes of `name` in directory `parent`.
    pub(super) fn lookup_attr(&self, parent: INodeNo, name: &[u8]) -> Result<FileAttr, Errno> {
        if let Ino::Ws { slot, local } = decode(parent.0)?
            && let WsNode::Entry(ws, _) = self.ws_node(slot, local)?
            && !(local == 0 && name == b".git")
        {
            // The lookup already resolved the entry; `attr` would do it again.
            let (child, obj) = ws.lookup(&self.store, local, name)?;
            return self.obj_attr(inode::ws(slot, child), &obj);
        }
        self.attr(self.lookup_ino(parent, name)?)
    }

    fn lookup_ino(&self, parent: INodeNo, name: &[u8]) -> Result<u64, Errno> {
        match decode(parent.0)? {
            Ino::Root => self.resolve_name(name),
            Ino::Dir { slot, dir } => {
                let rec = self.snapshots.dir_rec(slot, dir)?;
                if dir == 0 && name == b".git" {
                    return Ok(inode::gitfile(slot));
                }
                let table = self.store.tree(rec.tree);
                let e = table.find(name).ok_or(Errno::ENOENT)?;
                Ok(rec.child_ino(slot, e))
            }
            Ino::Ws { slot, local } => match self.ws_node(slot, local)? {
                WsNode::Namespace(prefix) => {
                    self.resolve_name(&[prefix.as_bytes(), b"/", name].concat())
                }
                WsNode::Entry(_, 0) if name == b".git" => Ok(inode::ws(slot, GITFILE_LOCAL)),
                // Workspace entries are looked up (and their nodes claimed) only by
                // `lookup_attr`.
                WsNode::Entry(..) => Err(Errno::EIO),
                WsNode::GitFile(_) => Err(Errno::ENOTDIR),
            },
            _ => Err(Errno::ENOTDIR),
        }
    }

    /// Calls `emit(next_offset, ino, name, attr)` for the entries of directory
    /// `ino` (including `.` and `..`) after `offset`, until it returns true
    /// (reply buffer full). With
    /// `plus` (readdirplus), each workspace entry the kernel receives counts as a
    /// lookup.
    pub(super) fn for_each_entry(
        &self,
        ino: INodeNo,
        offset: u64,
        plus: bool,
        emit: impl FnMut(u64, u64, &[u8], FileAttr) -> bool,
    ) -> Result<(), Errno> {
        let dots = [(ino.0, &b"."[..]), (inode::ROOT, &b".."[..])];
        match decode(ino.0)? {
            Ino::Root => self.emit_names("", dots, offset, emit),
            Ino::Dir { slot, dir } => {
                let rec = self.snapshots.dir_rec(slot, dir)?;
                let table = self.store.tree(rec.tree);
                let entries =
                    (table.entries().iter()).map(|e| (rec.child_ino(slot, e), table.name(e)));
                let gitfile = (dir == 0).then(|| (inode::gitfile(slot), &b".git"[..]));
                self.emit_entries(dots.into_iter().chain(entries).chain(gitfile), offset, emit)
            }
            Ino::Ws { slot, local } => match self.ws_node(slot, local)? {
                WsNode::Namespace(prefix) => self.emit_names(&prefix, dots, offset, emit),
                WsNode::Entry(ws, _) => self.emit_ws(&ws, slot, local, offset, plus, emit),
                WsNode::GitFile(_) => Err(Errno::ENOTDIR),
            },
            _ => Err(Errno::ENOTDIR),
        }
    }

    /// Lists the root or a namespace directory: snapshots, workspaces and
    /// namespaces directly below `prefix`.
    fn emit_names(
        &self,
        prefix: &str,
        dots: [(u64, &[u8]); 2],
        offset: u64,
        emit: impl FnMut(u64, u64, &[u8], FileAttr) -> bool,
    ) -> Result<(), Errno> {
        let mut names = self.snapshots.children(prefix);
        names.extend(self.workspaces.children(prefix));
        let entries = names.iter().map(|(ino, name)| (*ino, name.as_slice()));
        self.emit_entries(dots.into_iter().chain(entries), offset, emit)
    }

    fn emit_ws(
        &self,
        ws: &Workspace,
        slot: u32,
        local: u64,
        offset: u64,
        plus: bool,
        mut emit: impl FnMut(u64, u64, &[u8], FileAttr) -> bool,
    ) -> Result<(), Errno> {
        // Before `page` takes the workspace lock: the directory's own attributes
        // take it too (parking_lot read locks don't nest while a writer waits).
        let dot = inode::ws(slot, local);
        let dots = [
            (dot, &b"."[..], self.attr(dot)?),
            (inode::ROOT, &b".."[..], self.attr(inode::ROOT)?),
        ];
        let gitfile = (local == 0).then(|| inode::ws(slot, GITFILE_LOCAL));
        let gitfile_attr = gitfile.map(|ino| self.attr(ino)).transpose()?;
        ws.page(&self.store, local, |page| -> Result<(), Errno> {
            let entries = &page.listing.entries;
            let total = (dots.len() + entries.len() + usize::from(gitfile.is_some())) as u64;
            if offset >= total {
                // The kernel's closing request past the end: it won't page this
                // listing again. (Dropping it after the last full page would make
                // this request rebuild it.)
                page.done();
                return Ok(());
            }
            let skip = offset as usize;
            for (i, (ino, name, attr)) in dots.iter().enumerate().skip(skip) {
                if emit(i as u64 + 1, *ino, name, *attr) {
                    return Ok(());
                }
            }
            let first = skip.saturating_sub(dots.len());
            for (j, (name, entry)) in entries.iter().enumerate().skip(first) {
                let i = dots.len() + j;
                // Readdirplus entries count as kernel references, so the node is
                // claimed (and given back if the entry doesn't fit); plain readdir
                // only needs an id.
                let child = if plus {
                    page.claim(name)
                } else {
                    page.peek(name)
                };
                let ino = inode::ws(slot, child);
                let full = page
                    .obj(name, entry)
                    .and_then(|obj| self.obj_attr(ino, &obj))
                    .map(|attr| emit(i as u64 + 1, ino, name, attr));
                if !matches!(full, Ok(false)) {
                    if plus {
                        page.give_back(child);
                    }
                    return full.map(drop);
                }
            }
            if let (Some(ino), Some(attr)) = (gitfile, gitfile_attr) {
                emit(total, ino, b".git", attr);
            }
            Ok(())
        })?
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

    fn blob(&self, file_id: u32) -> Result<Arc<Vec<u8>>, Errno> {
        let f = self.store.file(file_id).ok_or(Errno::ENOENT)?;
        let data = git::read_blob(f.oid).map_err(|e| {
            error!("reading blob {}: {e:#}", f.oid);
            Errno::EIO
        })?;
        Ok(Arc::new(data))
    }

    /// Content for reading `size` bytes at `offset`. Immutable content is cached
    /// whole by inode; upper workspace files are read directly.
    pub(super) fn content(&self, ino: INodeNo, offset: u64, size: u32) -> Result<Content, Errno> {
        let cached = |key: u64, load: &dyn Fn() -> Result<Arc<Vec<u8>>, Errno>| {
            self.content.get_or_load(key, load).map(Content::Whole)
        };
        match decode(ino.0)? {
            Ino::File(id) => cached(ino.0, &|| self.blob(id)),
            Ino::GitFile { slot } => Ok(Content::Whole(self.snapshots.get(slot)?.gitfile.clone())),
            Ino::Ws { slot, local } => match self.ws_node(slot, local)? {
                WsNode::GitFile(ws) => Ok(Content::Whole(ws.gitfile.clone())),
                WsNode::Entry(ws, local) => match ws.read(&self.store, local, offset, size)? {
                    Read::Upper(data) => Ok(Content::Range(data)),
                    // Keyed like snapshot files, so the cache is shared with them.
                    Read::Lower(file_id) => cached(inode::file(file_id), &|| self.blob(file_id)),
                },
                _ => Err(Errno::EISDIR),
            },
            _ => Err(Errno::EISDIR),
        }
    }

    /// Target of a symlink.
    pub(super) fn link_target(&self, ino: INodeNo) -> Result<Vec<u8>, Errno> {
        match decode(ino.0)? {
            Ino::File(id) => Ok(self.blob(id)?.to_vec()),
            Ino::Ws { slot, local } => match self.ws_node(slot, local)? {
                WsNode::Entry(ws, local) => match ws.readlink(&self.store, local)? {
                    Read::Upper(target) => Ok(target),
                    Read::Lower(file_id) => Ok(self.blob(file_id)?.to_vec()),
                },
                _ => Err(Errno::EINVAL),
            },
            _ => Err(Errno::EINVAL),
        }
    }

    /// The workspace and local id of a writable directory or file inode;
    /// everything else is read-only.
    pub(super) fn writable(&self, ino: INodeNo) -> Result<(u32, Arc<Workspace>, u64), Errno> {
        match decode(ino.0)? {
            Ino::Ws { slot, local } => match self.ws_node(slot, local)? {
                WsNode::Entry(ws, local) => Ok((slot, ws, local)),
                WsNode::GitFile(_) => Err(Errno::EPERM),
                WsNode::Namespace(_) => Err(Errno::EROFS),
            },
            _ => Err(Errno::EROFS),
        }
    }
}
