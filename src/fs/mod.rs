//! Read-only FUSE view of git revisions: `<mount>/@<rev>/<path>`.
//!
//! Snapshot contents are immutable, so entries, attributes and page cache are
//! cached by the kernel indefinitely; after the first walk, `find`/`grep`/
//! `git status` are served mostly without calling into this process.
//!
//! Nothing is stored per node: inode numbers encode where to find an entry
//! (see `inode`), trees and files live in the shared `Store`, and a snapshot
//! only owns one `DirRec` per directory.
//!
//! - `mod.rs`: the FUSE adapter, translating requests into the calls below.
//! - `view`: what an inode is (attributes, lookups, listings, content).
//! - `snapshots`: the `@<rev>` directories and their creation.
//! - `content`: content kept between the reads of one file.
//! - `stats`: operation counters.

mod content;
mod snapshots;
mod stats;
mod view;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

use fuser::{
    AccessFlags, Errno, FileHandle, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use log::info;

use crate::inode;
use crate::prewarm::Prewarm;
use crate::store::Store;
use crate::worktree::StatInfo;
use content::ContentCache;
use snapshots::Snapshots;
use stats::{Counters, IoTimer, bump};

/// Snapshots never change; let the kernel cache them "forever".
const IMMUTABLE_TTL: Duration = Duration::from_secs(365 * 24 * 3600);
/// The root lists snapshots created on demand. The kernel caches its listing
/// (there is no opendir to opt out) until it sees the root mtime change, so
/// attributes must never be cached.
const ROOT_TTL: Duration = Duration::ZERO;

pub struct Options {
    /// Let the kernel skip open/release round trips (`FUSE_NO_OPEN_SUPPORT`).
    pub no_open: bool,
    pub prewarm: Prewarm,
    pub prewarm_threads: usize,
}

pub struct GitFold {
    no_open: bool,
    stat: StatInfo,
    store: Store,
    snapshots: Snapshots,
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
            store: Store::default(),
            snapshots: Snapshots::new(
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

    fn ttl(ino: u64) -> &'static Duration {
        if ino == inode::ROOT {
            &ROOT_TTL
        } else {
            &IMMUTABLE_TTL
        }
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
        match self
            .lookup_ino(parent, name.as_bytes())
            .and_then(|ino| self.attr(ino))
        {
            Ok(attr) => reply.entry(Self::ttl(attr.ino.0), &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        bump(&self.counters.getattr);
        match self.attr(ino.0) {
            Ok(attr) => reply.attr(Self::ttl(ino.0), &attr),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        bump(&self.counters.readlink);
        match self.load_content(ino) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        bump(&self.counters.open);
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        if self.no_open {
            // The kernel stops sending opens (and releases) after the first ENOSYS.
            return reply.error(Errno::ENOSYS);
        }
        match Self::is_file(ino) {
            // Content is loaded lazily on the first read; with FOPEN_KEEP_CACHE later
            // opens of the same file are served from the kernel page cache.
            Ok(true) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            Ok(false) => reply.error(Errno::EISDIR),
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
        let data = match self.content.get_or_load(ino.0, || self.load_content(ino)) {
            Ok(data) => data,
            Err(e) => return reply.error(e),
        };
        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(size as usize).min(data.len());
        reply.data(&data[start..end]);
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

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.error(Errno::ENOSYS);
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        bump(&self.counters.opendir);
        // The kernel stops sending opendir/releasedir after the first ENOSYS and still
        // caches listings of immutable snapshot directories.
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
        let result = self.for_each_entry(ino, offset, |next, child, name, attr| {
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
        let result = self.for_each_entry(ino, offset, |next, child, name, attr| {
            reply.add(
                INodeNo(child),
                next,
                OsStr::from_bytes(name),
                Self::ttl(child),
                &attr,
                Generation(0),
            )
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
}
