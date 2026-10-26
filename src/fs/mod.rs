//! FUSE view of a git repository: read-only snapshots of any revision
//! (`<mount>/@<rev>/<path>`) and writable branch workspaces
//! (`<mount>/<branch>/<path>`).
//!
//! Snapshot contents are immutable, so entries, attributes and page cache are
//! cached by the kernel indefinitely; after the first walk, `find`/`grep`/
//! `git status` are served mostly without calling into this process.
//! Workspaces change only through this file system, or without visible effect
//! (`workspace::rebase`), so the kernel's caches stay coherent with them too.
//!
//! Nothing is stored per snapshot node: inode numbers encode where to find an
//! entry (see `inode`), trees and files live in the shared `Store`, and a
//! snapshot only owns one `DirRec` per directory.
//!
//! - `mod.rs`: the FUSE adapter, translating requests into the calls below.
//! - `view`: what an inode is (attributes, lookups, listings, content).
//! - `snapshots`: the `@<rev>` directories and their creation.
//! - `workspaces`: branch workspaces and namespace directories by name.
//! - `workspace`: one workspace: lower/upper merged view and its mutations.
//! - `content`: content kept between the reads of one file.
//! - `stats`: operation counters.

mod content;
mod snapshots;
mod stats;
mod view;
pub mod workspace;
mod workspaces;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use log::info;

use crate::inode;
use crate::prewarm::Prewarm;
use crate::store::Store;
use crate::worktree::StatInfo;
use content::ContentCache;
use snapshots::Snapshots;
use stats::{Counters, IoTimer, bump};
use view::Content;
use workspace::{Obj, Workspace};
use workspaces::Workspaces;

/// Snapshots never change, and workspaces change only through this file system
/// (which the kernel sees); let the kernel cache them "forever".
const IMMUTABLE_TTL: Duration = Duration::from_secs(365 * 24 * 3600);
/// The root lists entries created on demand. The kernel caches its listing
/// (there is no opendir to opt out) until it sees its mtime change, so its
/// attributes must never be cached.
const ROOT_TTL: Duration = Duration::ZERO;
/// Namespace directories (`feature/` for `feature/x`) too, but every path walk
/// through them re-checks their permissions (`default_permissions`), which with
/// a zero TTL doubled getattr traffic in workspaces below them. A new entry may
/// take this long to show up in their listing.
const NAMESPACE_TTL: Duration = Duration::from_secs(1);

pub struct Options {
    /// Let the kernel skip open/release round trips (`FUSE_NO_OPEN_SUPPORT`).
    pub no_open: bool,
    pub prewarm: Prewarm,
    pub prewarm_threads: usize,
}

pub struct GitFold {
    no_open: bool,
    stat: StatInfo,
    store: Arc<Store>,
    snapshots: Snapshots,
    workspaces: Workspaces,
    content: ContentCache,
    counters: Counters,
}

impl GitFold {
    pub fn new(common_dir: PathBuf, mount_point: PathBuf, opts: Options) -> Self {
        let stat = StatInfo {
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        GitFold {
            no_open: opts.no_open,
            stat,
            store: Arc::default(),
            snapshots: Snapshots::new(
                common_dir.clone(),
                mount_point.clone(),
                stat,
                opts.prewarm,
                opts.prewarm_threads,
            ),
            workspaces: Workspaces::new(
                common_dir,
                mount_point,
                stat,
                opts.prewarm,
                opts.prewarm_threads,
            ),
            content: ContentCache::new(),
            counters: Counters::default(),
        }
    }

    /// Creates an entry in workspace directory `parent` with `make(ws, local)`;
    /// returns its attributes.
    fn new_entry(
        &self,
        parent: INodeNo,
        make: impl FnOnce(&Workspace, u64) -> Result<(u64, Obj), Errno>,
    ) -> Result<FileAttr, Errno> {
        let (slot, ws, local) = self.writable(parent)?;
        // `make` counts the kernel's reference to the new node (see `workspace::nodes`).
        let (child, obj) = make(&ws, local)?;
        self.obj_attr(inode::ws(slot, child), &obj)
    }

    /// The workspace and local id of a workspace entry inode (not a workspace or
    /// namespace directory itself, nor `.git`, which have no node).
    fn ws_entry(&self, ino: u64) -> Option<(Arc<Workspace>, u64)> {
        match inode::decode(ino)? {
            inode::Ino::Ws { slot, local } if local != 0 && local != workspace::GITFILE_LOCAL => {
                Some((self.workspaces.get(slot).ok()?.ok()?, local))
            }
            _ => None,
        }
    }
}

/// The last component of `path` if it is directly below `prefix` ("" for the
/// mount root).
fn child_name<'a>(prefix: &str, path: &'a [u8]) -> Option<&'a [u8]> {
    let rest = if prefix.is_empty() {
        path
    } else {
        path.strip_prefix(prefix.as_bytes())?.strip_prefix(b"/")?
    };
    (!rest.contains(&b'/')).then_some(rest)
}

fn reply_entry(result: Result<FileAttr, Errno>, reply: ReplyEntry) {
    match result {
        Ok(attr) => reply.entry(&IMMUTABLE_TTL, &attr, Generation(0)),
        Err(e) => reply.error(e),
    }
}

fn time(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

fn reply_empty(result: Result<(), Errno>, reply: ReplyEmpty) {
    match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
    }
}

impl Filesystem for GitFold {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let mut flags = vec![
            InitFlags::FUSE_DO_READDIRPLUS,
            InitFlags::FUSE_PARALLEL_DIROPS,
            InitFlags::FUSE_CACHE_SYMLINKS,
            InitFlags::FUSE_NO_OPENDIR_SUPPORT,
        ];
        if self.no_open {
            flags.push(InitFlags::FUSE_NO_OPEN_SUPPORT);
        }
        for flag in flags {
            if config.add_capabilities(flag).is_err() {
                info!("kernel does not support {flag:?}");
            }
        }
        let _ = config.set_max_readahead(1 << 20);
        Ok(())
    }

    fn destroy(&mut self) {
        self.counters.log(self.store.stats());
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        bump(&self.counters.lookup);
        match self.lookup_attr(parent, name.as_bytes()) {
            // Names never go away while mounted, so the entry can be cached long even
            // where attributes can't (root and namespace directories, whose mtime
            // tells the kernel when their listings changed).
            Ok(attr) => {
                reply.entry_with_ttls(self.ttl(attr.ino.0), &IMMUTABLE_TTL, &attr, Generation(0))
            }
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        bump(&self.counters.getattr);
        match self.attr(ino.0) {
            Ok(attr) => reply.attr(self.ttl(ino.0), &attr),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        bump(&self.counters.setattr);
        let result = self.writable(ino).and_then(|(_, ws, local)| {
            if uid.is_some_and(|u| u != self.stat.uid) || gid.is_some_and(|g| g != self.stat.gid) {
                return Err(Errno::EPERM);
            }
            ws.setattr(
                &self.store,
                local,
                mode,
                size,
                atime.map(time),
                mtime.map(time),
            )?;
            self.attr(ino.0)
        });
        match result {
            Ok(attr) => reply.attr(&IMMUTABLE_TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        bump(&self.counters.readlink);
        match self.link_target(ino) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Read-only outside workspaces; special files aren't supported inside.
        reply.error(self.writable(parent).err().unwrap_or(Errno::EPERM));
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        bump(&self.counters.create);
        let result = self.new_entry(parent, |ws, local| {
            ws.mkdir(&self.store, local, name.as_bytes(), mode & !umask)
        });
        reply_entry(result, reply)
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        bump(&self.counters.remove);
        let result = self
            .writable(parent)
            .and_then(|(_, ws, local)| ws.unlink(&self.store, local, name.as_bytes()));
        reply_empty(result, reply)
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        bump(&self.counters.remove);
        let result = self
            .writable(parent)
            .and_then(|(_, ws, local)| ws.rmdir(&self.store, local, name.as_bytes()));
        reply_empty(result, reply)
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        bump(&self.counters.create);
        let result = self.new_entry(parent, |ws, local| {
            ws.symlink(&self.store, local, link_name.as_bytes(), target)
        });
        reply_entry(result, reply)
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        bump(&self.counters.rename);
        let result = (|| {
            if flags.contains(RenameFlags::RENAME_EXCHANGE) {
                return Err(Errno::EINVAL);
            }
            let (slot, ws, local) = self.writable(parent)?;
            let (new_slot, _, new_local) = self.writable(newparent)?;
            if new_slot != slot {
                return Err(Errno::EXDEV);
            }
            let noreplace = flags.contains(RenameFlags::RENAME_NOREPLACE);
            ws.rename(
                &self.store,
                local,
                name.as_bytes(),
                new_local,
                newname.as_bytes(),
                noreplace,
            )
        })();
        reply_empty(result, reply)
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        // Read-only outside workspaces; hard links aren't supported inside.
        reply.error(self.writable(newparent).err().unwrap_or(Errno::EPERM));
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        bump(&self.counters.open);
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY
            && let Err(e) = self.writable(ino)
        {
            return reply.error(e);
        }
        if self.no_open {
            // The kernel stops sending opens (and releases) after the first ENOSYS.
            return reply.error(Errno::ENOSYS);
        }
        match self.is_file(ino) {
            // Content is loaded lazily on the first read; with FOPEN_KEEP_CACHE later
            // opens of the same file are served from the kernel page cache.
            Ok(true) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            Ok(false) => reply.error(Errno::EISDIR),
            Err(e) => reply.error(e),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        bump(&self.counters.create);
        let result = self.new_entry(parent, |ws, local| {
            ws.create(&self.store, local, name.as_bytes(), mode & !umask)
        });
        match result {
            // No file handles: reads and writes are addressed by inode.
            Ok(attr) => reply.created(
                &IMMUTABLE_TTL,
                &attr,
                Generation(0),
                FileHandle(0),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        bump(&self.counters.read);
        let _t = IoTimer::start(&self.counters);
        match self.content(ino, offset, size) {
            Ok(Content::Whole(data)) => {
                let start = (offset as usize).min(data.len());
                let end = start.saturating_add(size as usize).min(data.len());
                reply.data(&data[start..end]);
            }
            Ok(Content::Range(data)) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        bump(&self.counters.write);
        let result = self
            .writable(ino)
            .and_then(|(_, ws, local)| ws.write(&self.store, local, offset, data));
        match result {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    // ENOSYS makes the kernel stop sending these for the rest of the mount.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result = match self.ws_entry(ino.0) {
            Some((ws, local)) => ws.fsync(&self.store, local),
            None => Ok(()), // snapshot content is in git already
        };
        reply_empty(result, reply)
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        if let Some((ws, local)) = self.ws_entry(ino.0) {
            ws.forget(local, nlookup);
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.error(Errno::ENOSYS);
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        bump(&self.counters.opendir);
        // The kernel stops sending opendir/releasedir after the first ENOSYS and still
        // caches listings, invalidating them itself when it changes a directory.
        reply.error(Errno::ENOSYS);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        bump(&self.counters.readdir);
        let result = self.for_each_entry(ino, offset, false, |next, child, name, attr| {
            reply.add(INodeNo(child), next, attr.kind, OsStr::from_bytes(name))
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        bump(&self.counters.readdir);
        let result = self.for_each_entry(ino, offset, true, |next, child, name, attr| {
            reply.add(
                INodeNo(child),
                next,
                OsStr::from_bytes(name),
                self.ttl(child),
                &attr,
                Generation(0),
            )
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        match self.writable(ino).and_then(|(_, ws, _)| ws.statfs()) {
            Ok(st) => reply.statfs(
                st.f_blocks,
                st.f_bfree,
                st.f_bavail,
                st.f_files,
                st.f_ffree,
                st.f_bsize as u32,
                st.f_namemax as u32,
                st.f_frsize as u32,
            ),
            Err(_) => reply.statfs(0, 0, 0, 0, 0, 512, 255, 0),
        }
    }
}
