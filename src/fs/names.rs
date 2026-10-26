//! Telling the kernel when the names in the mount root or a namespace directory
//! changed (a snapshot or workspace was opened), so those directories can have
//! long attribute TTLs like everything else. With a zero TTL instead, every
//! absolute path through the mount cost a `getattr` of the root (the kernel
//! re-checks permissions on each directory a path passes, `default_permissions`).
//!
//! The kernel is told on a thread of its own, so the lookup that added the name
//! never waits on the kernel (defensive: `inval_inode` takes no lock a lookup
//! holds). So it may land just after the reply: a listing taken in between can
//! miss the new name, once (the directories' new mtime drops that listing when
//! the invalidated attributes are next read).

use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

use fuser::{INodeNo, Notifier};
use log::debug;

#[derive(Default)]
pub struct NameChanges {
    tx: OnceLock<Sender<Vec<u64>>>,
}

impl NameChanges {
    /// Starts telling the kernel through the session's `notifier` (it exists
    /// once mounted, before any request).
    pub fn set_notifier(&self, notifier: Notifier) {
        let (tx, rx) = channel::<Vec<u64>>();
        if self.tx.set(tx).is_err() {
            return;
        }
        std::thread::spawn(move || {
            for inodes in rx {
                for ino in inodes {
                    // Offset 0, length 0: the attributes and the whole cached
                    // listing. Fails harmlessly if the kernel holds no such inode.
                    if let Err(e) = notifier.inval_inode(INodeNo(ino), 0, 0) {
                        debug!("invalidating inode {ino}: {e}");
                    }
                }
            }
        });
    }

    /// The names in directories `inodes` changed.
    pub fn changed(&self, inodes: Vec<u64>) {
        if let Some(tx) = self.tx.get() {
            let _ = tx.send(inodes);
        }
    }
}
