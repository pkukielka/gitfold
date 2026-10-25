//! `gitfold bench-read`: decompresses every blob of a revision, to measure
//! object-database throughput without FUSE in the way.

use std::time::Instant;

use anyhow::Result;
use gix::ObjectId;
use gix::index::entry::Mode;
use rayon::prelude::*;

use crate::git;

pub fn run(rev: &str, threads: &[usize]) -> Result<()> {
    let tree = git::resolve(rev)?.tree;
    // Index order, i.e. the order of a recursive directory walk.
    let blobs: Vec<ObjectId> = git::with_repo(|repo| -> Result<_> {
        let index = repo.index_from_tree(&tree)?;
        Ok(index
            .entries()
            .iter()
            .filter(|e| matches!(e.mode, Mode::FILE | Mode::FILE_EXECUTABLE))
            .map(|e| e.id)
            .collect())
    })?;
    let total: u64 = blobs
        .par_iter()
        .map(|&oid| git::object_size(oid))
        .sum::<Result<_>>()?;
    println!("{} blobs, {} MB", blobs.len(), total >> 20);
    for &n in threads {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(n).build()?;
        let started = Instant::now();
        let bytes: u64 = pool.install(|| {
            blobs
                .par_iter()
                .with_min_len(64)
                .map(|&oid| git::read_blob(oid).map_or(0, |d| d.len() as u64))
                .sum()
        });
        let secs = started.elapsed().as_secs_f64();
        println!(
            "threads={n:>2}: {:>7.0} ms  {:>6.0} MB/s",
            secs * 1000.0,
            bytes as f64 / secs / 1e6
        );
    }
    Ok(())
}
