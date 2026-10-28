//! Telling the kernel when the names in the mount root or a namespace directory
//! changed (a snapshot or workspace was opened or removed), so those
//! directories can have long attribute TTLs like everything else. With a zero
//! TTL instead, every absolute path through the mount cost a `getattr` of the
//! root (the kernel re-checks permissions on each directory a path passes,
//! `default_permissions`).
//!
//! The kernel is told from the lookup that added the name, before it replies,
//! so the caller's next listing already shows the name. `inval_inode` doesn't
//! wait on requests in flight (unlike `inval_entry`, it takes no directory
//! lock). A listing that was in flight meanwhile can still be cached after it;
//! the directories' new mtime drops it when their attributes are next read.

use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

use fuser::{INodeNo, Notifier};
use log::debug;

#[derive(Default)]
pub struct NameChanges {
    notifier: OnceLock<Notifier>,
}

impl NameChanges {
    /// The session's notifier: it exists once mounted, before any request.
    pub fn set_notifier(&self, notifier: Notifier) {
        let _ = self.notifier.set(notifier);
    }

    /// Name `name` in directory `parent` (the root or a namespace) was removed.
    /// Not to be called while serving a request in `parent`: invalidating an
    /// entry takes the directory's lock, which the request holds.
    pub fn removed(&self, parent: u64, name: &[u8]) {
        let Some(notifier) = self.notifier.get() else {
            return;
        };
        let name = std::ffi::OsStr::from_bytes(name);
        if let Err(e) = notifier.inval_entry(INodeNo(parent), name) {
            debug!("invalidating {name:?} in inode {parent}: {e}");
        }
        self.changed(&[parent]);
    }

    /// The names in directories `inodes` changed.
    pub fn changed(&self, inodes: &[u64]) {
        let Some(notifier) = self.notifier.get() else {
            return;
        };
        for &ino in inodes {
            // Offset 0, length 0: the attributes and the whole cached listing.
            // Fails harmlessly if the kernel holds no such inode.
            if let Err(e) = notifier.inval_inode(INodeNo(ino), 0, 0) {
                debug!("invalidating inode {ino}: {e}");
            }
        }
    }
}
