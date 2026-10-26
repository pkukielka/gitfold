//! Operation counters, logged at unmount; used by the benchmarks to see which
//! requests actually reach the daemon.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use log::info;

use crate::git;
use crate::store;

#[derive(Default)]
pub struct Counters {
    pub lookup: AtomicU64,
    pub getattr: AtomicU64,
    pub open: AtomicU64,
    pub read: AtomicU64,
    pub opendir: AtomicU64,
    pub readdir: AtomicU64,
    pub readlink: AtomicU64,
    pub write: AtomicU64,
    pub setattr: AtomicU64,
    /// create, mkdir, symlink.
    pub create: AtomicU64,
    /// unlink, rmdir.
    pub remove: AtomicU64,
    pub rename: AtomicU64,
    /// Time spent in read handlers, and peak concurrency of those.
    io_ns: AtomicU64,
    inflight: AtomicU64,
    max_inflight: AtomicU64,
}

pub fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    pub fn log(&self, store: store::Stats) {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        info!(
            "ops: lookup={} getattr={} open={} read={} opendir={} readdir={} readlink={} write={} setattr={} create={} remove={} rename={} io_ms={} \
             max_inflight={} blob_ms={} blob_mb={}; store: {} trees, {} entries, {} files",
            get(&self.lookup),
            get(&self.getattr),
            get(&self.open),
            get(&self.read),
            get(&self.opendir),
            get(&self.readdir),
            get(&self.readlink),
            get(&self.write),
            get(&self.setattr),
            get(&self.create),
            get(&self.remove),
            get(&self.rename),
            get(&self.io_ns) / 1_000_000,
            get(&self.max_inflight),
            git::BLOB_NS.load(Ordering::Relaxed) / 1_000_000,
            git::BLOB_BYTES.load(Ordering::Relaxed) >> 20,
            store.trees,
            store.entries,
            store.files
        );
    }
}

/// Measures one I/O handler call for `io_ns` and `max_inflight`.
pub struct IoTimer<'a>(&'a Counters, Instant);

impl<'a> IoTimer<'a> {
    pub fn start(c: &'a Counters) -> Self {
        let n = c.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        c.max_inflight.fetch_max(n, Ordering::Relaxed);
        IoTimer(c, Instant::now())
    }
}

impl Drop for IoTimer<'_> {
    fn drop(&mut self) {
        self.0
            .io_ns
            .fetch_add(self.1.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}
