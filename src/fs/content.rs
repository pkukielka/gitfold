//! File content kept between the reads of one file. With no open handles
//! (FUSE_NO_OPEN_SUPPORT) reads arrive per inode, possibly in several chunks;
//! the kernel page cache does the real caching after that.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fuser::Errno;
use parking_lot::Mutex;

/// Upper bound for cached contents; only needs to cover files being read now.
const MAX_BYTES: usize = 64 << 20;

#[derive(Default)]
struct Blobs {
    by_ino: HashMap<u64, Arc<Vec<u8>>>,
    bytes: usize,
}

pub struct ContentCache {
    blobs: Arc<Mutex<Blobs>>,
    /// Incremented by every read; lets the trimmer detect when reads have stopped.
    activity: Arc<AtomicU64>,
}

impl ContentCache {
    /// Also starts the idle trimmer: once reads go quiet, it drops the cache and
    /// returns freed heap to the OS. Without it, buffers from a burst of reads stay
    /// in malloc arenas: ~250 MB after one pass over chromium.
    pub fn new() -> Self {
        let cache = ContentCache {
            blobs: Arc::default(),
            activity: Arc::default(),
        };
        // Weak, so the thread exits once the cache is dropped.
        let (blobs, activity) = (Arc::downgrade(&cache.blobs), cache.activity.clone());
        std::thread::spawn(move || {
            let (mut seen, mut trimmed) = (0, 0);
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let Some(blobs) = blobs.upgrade() else { return };
                let now = activity.load(Ordering::Relaxed);
                if now == seen && now != trimmed {
                    *blobs.lock() = Blobs::default();
                    unsafe { libc::malloc_trim(0) };
                    trimmed = now;
                }
                seen = now;
            }
        });
        cache
    }

    pub fn get_or_load(
        &self,
        ino: u64,
        load: impl FnOnce() -> Result<Arc<Vec<u8>>, Errno>,
    ) -> Result<Arc<Vec<u8>>, Errno> {
        self.activity.fetch_add(1, Ordering::Relaxed);
        if let Some(data) = self.blobs.lock().by_ino.get(&ino) {
            return Ok(data.clone());
        }
        let data = load()?;
        let mut blobs = self.blobs.lock();
        if blobs.bytes + data.len() > MAX_BYTES {
            // Crude, but reads of a file are clustered.
            *blobs = Blobs::default();
        }
        blobs.bytes += data.len();
        blobs.by_ino.insert(ino, data.clone());
        Ok(data)
    }
}
