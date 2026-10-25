//! Background walk of a freshly created snapshot through the mount itself, so
//! the kernel's dentry/inode cache (and optionally page cache) is populated
//! before the first agent command arrives.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use clap::ValueEnum;
use log::{info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Prewarm {
    None,
    /// List every directory (readdirplus instantiates every inode).
    Meta,
    /// Also read every file into the page cache (costs RAM equal to tree size).
    Content,
}

#[derive(Default)]
struct Stats {
    dirs: AtomicU64,
    files: AtomicU64,
    bytes: AtomicU64,
}

/// Spawns the walk on its own threads; must not run on a FUSE worker thread,
/// since the walk's requests are served by those workers.
pub fn spawn(root: PathBuf, mode: Prewarm, threads: usize) {
    if mode == Prewarm::None {
        return;
    }
    std::thread::spawn(move || {
        let pool = match rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("prewarm-{i}"))
            .build()
        {
            Ok(pool) => pool,
            Err(e) => return warn!("prewarm: {e}"),
        };
        let stats = Stats::default();
        let started = Instant::now();
        pool.scope(|s| visit(s, root.clone(), mode, &stats));
        info!(
            "prewarm done for {}: {} dirs, {} files, {} MB in {:?}",
            root.display(),
            stats.dirs.load(Ordering::Relaxed),
            stats.files.load(Ordering::Relaxed),
            stats.bytes.load(Ordering::Relaxed) >> 20,
            started.elapsed()
        );
    });
}

fn visit<'s>(s: &rayon::Scope<'s>, dir: PathBuf, mode: Prewarm, stats: &'s Stats) {
    stats.dirs.fetch_add(1, Ordering::Relaxed);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            s.spawn(move |s| visit(s, entry.path(), mode, stats));
        } else {
            stats.files.fetch_add(1, Ordering::Relaxed);
            if mode == Prewarm::Content && ft.is_file() {
                let path = entry.path();
                stats.bytes.fetch_add(read_all(&path), Ordering::Relaxed);
                let _ = fs::symlink_metadata(&path); // see below
            }
        }
    }
    // On a writable mount the kernel marks atime stale after reading a directory
    // or file, so the next stat asks the daemon again. Stat here, in the
    // background, so the agent's first find or git status finds attributes cached
    // (chromium: first find 759 ms without this, 351 ms with).
    let _ = fs::symlink_metadata(&dir);
}

fn read_all(path: &Path) -> u64 {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; 1 << 20]);
    }
    let Ok(mut f) = fs::File::open(path) else {
        return 0;
    };
    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        let mut total = 0;
        while let Ok(n) = f.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    })
}
