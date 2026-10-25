//! Access to the git object database.
//!
//! `gix::Repository` is not `Sync`, so each thread lazily gets its own handle
//! derived from one shared `ThreadSafeRepository`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use gix::ObjectId;
use gix::objs::tree::EntryKind;

static REPO: OnceLock<gix::ThreadSafeRepository> = OnceLock::new();

thread_local! {
    static LOCAL: RefCell<Option<gix::Repository>> = const { RefCell::new(None) };
}

/// Opens the repository at `path` and returns its common git dir (`.git`).
pub fn init(path: &Path) -> Result<PathBuf> {
    // Trees are decoded once per snapshot and file content is cached by the kernel,
    // so per-thread object and delta-base caches only cost memory (~2 GB on chromium
    // with 24 threads) without making reads faster.
    let opts = gix::open::Options::default().config_overrides(["core.deltaBaseCacheLimit=0"]);
    let repo = gix::open_opts(path, opts)
        .with_context(|| format!("opening repository {}", path.display()))?;
    let common = repo.common_dir().canonicalize()?;
    REPO.set(repo.into_sync())
        .map_err(|_| anyhow!("repository already initialized"))?;
    Ok(common)
}

pub fn with_repo<R>(f: impl FnOnce(&gix::Repository) -> R) -> R {
    LOCAL.with(|cell| {
        let mut slot = cell.borrow_mut();
        let repo = slot.get_or_insert_with(|| {
            let mut repo = REPO.get().expect("git::init not called").to_thread_local();
            repo.object_cache_size(0);
            repo
        });
        f(repo)
    })
}

pub struct Commit {
    pub id: ObjectId,
    pub tree: ObjectId,
}

pub fn resolve(spec: &str) -> Result<Commit> {
    with_repo(|repo| {
        let commit = repo.rev_parse_single(spec)?.object()?.peel_to_commit()?;
        Ok(Commit {
            id: commit.id,
            tree: commit.tree_id()?.detach(),
        })
    })
}

pub struct Entry {
    pub name: Vec<u8>,
    pub kind: EntryKind,
    pub oid: ObjectId,
}

/// Lists one tree level.
pub fn tree_entries(tree: ObjectId) -> Result<Vec<Entry>> {
    with_repo(|repo| {
        let tree = repo.find_tree(tree)?;
        let decoded = tree.decode()?;
        Ok(decoded
            .entries
            .iter()
            .map(|e| Entry {
                name: e.filename.to_vec(),
                kind: e.mode.kind(),
                oid: e.oid.to_owned(),
            })
            .collect())
    })
}

/// Uncompressed size of an object, without decompressing it.
pub fn object_size(oid: ObjectId) -> Result<u64> {
    with_repo(|repo| Ok(repo.find_header(oid)?.size()))
}

pub static BLOB_NS: AtomicU64 = AtomicU64::new(0);
pub static BLOB_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn read_blob(oid: ObjectId) -> Result<Vec<u8>> {
    let started = Instant::now();
    let data = with_repo(|repo| Ok(repo.find_blob(oid)?.take_data()));
    BLOB_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if let Ok(d) = &data {
        BLOB_BYTES.fetch_add(d.len() as u64, Ordering::Relaxed);
    }
    data
}
