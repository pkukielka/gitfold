//! Access to the git object database.
//!
//! `gix::Repository` is not `Sync`, so each thread lazily gets its own handle
//! derived from one shared `ThreadSafeRepository`.
//!
//! - `cmd`: running git itself.
//! - `index`: the git index gitfold writes.
//! - `worktree`: worktree registrations, base pins, git's busy state.
//! - `promisor`: partial clones, fetching what loads miss.
//! - `rebase`: moving a branch without a worktree with `git rebase`.

pub mod cmd;
pub mod index;
pub mod promisor;
pub mod rebase;
pub mod worktree;

use std::cell::RefCell;
use std::fs;
use std::io::{ErrorKind, Write};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::LocalKey;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::objs::tree::EntryKind;

/// The least number of pack indexes the object database can hold. Its slot table
/// is sized once, at open (by default to the packs on disk then, plus 10%), and
/// never grows: after enough fetches or repacks while mounted, lookups of
/// objects in new packs failed ("too few index slots") until a remount. Slots
/// of deleted packs are reused; each costs ~110 bytes (~2 MB in all). gitoxide
/// caps the table at 2^15 - 1 slots, so with its 10% margin, more than ~29,800
/// packs on disk at mount time fail the open (git's auto gc repacks at 50).
const MIN_INDEX_SLOTS: usize = 16384;

static REPO: OnceLock<gix::ThreadSafeRepository> = OnceLock::new();

thread_local! {
    static LOCAL: RefCell<Option<gix::Repository>> = const { RefCell::new(None) };
    /// The same, without rescanning packs on a miss (see `with_repo_no_refresh`).
    static LOCAL_NO_REFRESH: RefCell<Option<gix::Repository>> = const { RefCell::new(None) };
}

/// Opens the repository at `path` and returns its common git dir (`.git`).
/// Once per process: again for the same repository returns that, another is
/// an error.
pub fn init(path: &Path) -> Result<PathBuf> {
    // Trees are decoded once per snapshot and file content is cached by the kernel,
    // so gitoxide's per-thread delta-base cache (96 MB by default) only costs memory
    // (~2 GB on chromium with 24 threads) without making reads faster.
    let slots = gix::odb::store::init::Slots::AsNeededByDiskState {
        multiplier: 1.1,
        minimum: MIN_INDEX_SLOTS,
    };
    let opts = gix::open::Options::default()
        .config_overrides(["core.deltaBaseCacheLimit=0"])
        .object_store_slots(slots);
    let repo = gix::open_opts(path, opts)
        .with_context(|| format!("opening repository {}", path.display()))?;
    let common = repo.common_dir().canonicalize()?;
    if REPO.set(repo.into_sync()).is_err() {
        let open = REPO
            .get()
            .map(|r| r.to_thread_local().common_dir().canonicalize());
        if !open.is_some_and(|c| c.is_ok_and(|c| c == common)) {
            bail!("another repository is already open");
        }
    }
    Ok(common)
}

/// The top of the work tree of the repository `init` opened, if it has one.
pub fn work_tree() -> Option<PathBuf> {
    with_repo(|repo| repo.workdir().map(Path::to_path_buf))
}

pub fn with_repo<R>(f: impl FnOnce(&gix::Repository) -> R) -> R {
    with_local(&LOCAL, false, f)
}

/// `with_repo`, with a handle that doesn't look for new packs when an object
/// isn't found: gitoxide otherwise rescans the pack directory (under a global
/// lock) on every miss, and a tree walk in a partial clone misses once per
/// absent blob. Call `refresh_packs` first to see packs added since.
pub fn with_repo_no_refresh<R>(f: impl FnOnce(&gix::Repository) -> R) -> R {
    with_local(&LOCAL_NO_REFRESH, true, f)
}

fn with_local<R>(
    key: &'static LocalKey<RefCell<Option<gix::Repository>>>,
    no_refresh: bool,
    f: impl FnOnce(&gix::Repository) -> R,
) -> R {
    key.with(|cell| {
        let mut slot = cell.borrow_mut();
        let repo = slot.get_or_insert_with(|| {
            let mut repo = REPO.get().expect("git::init not called").to_thread_local();
            repo.object_cache_size(0);
            if no_refresh {
                repo.objects.refresh_never();
            }
            repo
        });
        f(repo)
    })
}

/// Makes packs added since (a fetch, a repack) visible to all handles: a
/// lookup that misses rescans the pack directory once.
pub fn refresh_packs() {
    // No object has this id.
    let none = ObjectId::from_hex(b"0000000000000000000000000000000000000001").expect("hex");
    with_repo(|repo| repo.has_object(none));
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

/// Objects a tree load needed that the object database doesn't have (a
/// partial clone: see `promisor`).
#[derive(Debug)]
pub struct Missing(pub Vec<ObjectId>);

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.0[..] {
            [one] => write!(f, "object {one} is missing"),
            many => write!(f, "{} objects are missing (first: {})", many.len(), many[0]),
        }
    }
}

impl std::error::Error for Missing {}

/// Whether `oid` is absent from the object database. gitoxide reports a lookup
/// that failed for another reason (a pack it couldn't read) as absent too, so
/// this means "maybe fetchable", not "certainly not there".
pub fn is_missing(oid: ObjectId) -> bool {
    with_repo(|repo| !repo.has_object(oid))
}

/// One tree level, in stored order, its names in one buffer.
pub struct Tree {
    pub entries: Vec<Entry>,
    pub names: Vec<u8>,
}

pub struct Entry {
    /// Where the name is in `Tree::names`.
    pub name: Range<usize>,
    pub kind: EntryKind,
    pub oid: ObjectId,
}

/// Lists one tree level; `None` if the tree isn't in the object database
/// (as of the last `refresh_packs`).
pub fn tree_entries(tree: ObjectId) -> Result<Option<Tree>> {
    with_repo_no_refresh(|repo| {
        let Some(object) = repo.try_find_object(tree)? else {
            return Ok(None);
        };
        let tree = object.try_into_tree()?;
        let decoded = tree.decode()?;
        let len = decoded.entries.iter().map(|e| e.filename.len()).sum();
        let mut names = Vec::with_capacity(len);
        let entries = decoded
            .entries
            .iter()
            .map(|e| {
                let start = names.len();
                names.extend_from_slice(e.filename);
                Entry {
                    name: start..names.len(),
                    kind: e.mode.kind(),
                    oid: e.oid.to_owned(),
                }
            })
            .collect();
        Ok(Some(Tree { entries, names }))
    })
}

/// Uncompressed size of an object, without decompressing it; `None` if it
/// isn't in the object database (as of the last `refresh_packs`).
pub fn object_size(oid: ObjectId) -> Result<Option<u64>> {
    with_repo_no_refresh(|repo| Ok(repo.try_find_header(oid)?.map(|h| h.size())))
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

/// Refuses (not found: exit 3) unless `name` is a local branch.
pub fn require_branch(name: &str) -> Result<()> {
    match branch_exists(name) {
        true => Ok(()),
        false => Err(crate::refusal::Refusal::NotFound.error(format!("no branch {name}"))),
    }
}

/// The commit branch `name` points at.
pub fn branch_tip(name: &str) -> Result<Commit> {
    resolve(&format!("refs/heads/{name}"))
}

// Two questions about "the main branch" that look alike but must stay
// separate (layers-design/15-tree-spec.md, section 2):
// - `default_compare_target`: which commit to compare a branch with by
//   default (`overlaps`). Local `main` and `origin/main` are DIFFERENT
//   commits here: unpushed work on local `main` counts.
// - `root_branch`: which branch is the root of the tree of branches built on
//   branches (`tree`). Local `main` and `origin/main` are the SAME root here.
// Merging them into one function broke `overlaps main` in a clone
// (2026-11-16): tests/tree.sh "two questions about main" pins the
// difference.

/// The ref to compare with by default (`gitfold overlaps`): the remote's
/// default branch (`origin/HEAD`, normally `origin/main`), else `main`. Not
/// the branch's upstream: after `git push -u` that is its own remote branch,
/// which its work is ahead of. Not `root_branch` (see the note above).
/// `None`: neither exists.
pub fn default_compare_target(common_dir: &Path) -> Result<Option<String>> {
    let out = cmd::command(common_dir)
        .args(["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .output()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !name.is_empty() && name != "origin/HEAD" {
        return Ok(Some(name));
    }
    Ok(resolve("refs/heads/main").ok().map(|_| "main".to_string()))
}

/// The root of the tree of branches, as a local branch name: `origin/HEAD`'s
/// branch (`main` for `origin/main`), else `main`, else `master`, else
/// `origin/main`'s or `origin/master`'s name; each only if it exists (a
/// clone's `origin/HEAD` can point nowhere). `None`: no known root. Local
/// `main` and `origin/main` are the same root. Not `default_compare_target`
/// (see the note above).
pub fn root_branch(common_dir: &Path) -> Option<String> {
    let head = cmd::output(
        common_dir,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .map(|out| String::from_utf8_lossy(&out).trim().to_string());
    let candidates = head.into_iter().chain(
        [
            "refs/heads/main",
            "refs/heads/master",
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
        ]
        .map(String::from),
    );
    candidates
        .filter(|refname| resolve(refname).is_ok())
        .find_map(|refname| {
            let name = (refname.strip_prefix("refs/heads/"))
                .or_else(|| refname.strip_prefix("refs/remotes/origin/"))?;
            Some(name.to_string())
        })
}

/// Whether `refs/heads/<name>` exists.
pub fn branch_exists(name: &str) -> bool {
    with_repo(|repo| {
        repo.try_find_reference(format!("refs/heads/{name}").as_str())
            .is_ok_and(|r| r.is_some())
    })
}

/// The full ref that `name` stands for, as git takes it: a short name by
/// git's rules (gitrevisions, "<refname>": `$GIT_DIR/<name>` for names
/// like `HEAD`, then
/// `refs/<name>`, `refs/tags/<name>`, `refs/heads/<name>`,
/// `refs/remotes/<name>`, `refs/remotes/<name>/HEAD`, the first that
/// exists), and a symbolic ref followed to the ref it points at
/// (`origin/HEAD` stands for `refs/remotes/origin/main`, as `git rev-parse
/// --symbolic-full-name` says).
pub fn full_ref(name: &str) -> Option<String> {
    with_repo(|repo| followed(repo, name))
}

fn followed(repo: &gix::Repository, name: &str) -> Option<String> {
    let mut reference = repo.try_find_reference(name).ok()??;
    // git gives up on longer chains of symbolic refs too.
    for _ in 0..5 {
        match reference.follow() {
            None => return Some(reference.name().as_bstr().to_string()),
            Some(next) => reference = next.ok()?,
        }
    }
    None
}

/// The remote-tracking branch `<remote>/<branch>` of a configured remote, as
/// a full ref (`origin`'s if there are several), if it exists and is a
/// branch itself (`origin/HEAD` is a symbolic ref, not one). Never for
/// `HEAD`, which git refuses as a branch name.
pub fn remote_tracking(branch: &str) -> Option<String> {
    if branch == "HEAD" {
        return None;
    }
    with_repo(|repo| {
        (repo.remote_names().iter())
            .map(|remote| {
                (
                    *remote != "origin",
                    format!("refs/remotes/{remote}/{branch}"),
                )
            })
            .filter(|(_, full)| followed(repo, full).as_ref() == Some(full))
            .min_by_key(|(not_origin, _)| *not_origin)
            .map(|(_, full)| full)
    })
}

/// Whether any reference starts with one of `prefixes` (e.g. `refs/heads/feature/`).
pub fn any_ref_under(prefixes: &[String]) -> bool {
    with_repo(|repo| {
        let Ok(platform) = repo.references() else {
            return false;
        };
        prefixes.iter().any(|prefix| {
            platform
                .prefixed(prefix.as_str())
                .is_ok_and(|mut refs| refs.next().is_some())
        })
    })
}

pub fn is_bare() -> bool {
    with_repo(|repo| repo.is_bare())
}

/// Adds `pattern` to `info/exclude` (shared by every worktree) if missing,
/// after a comment saying `why`, in one append: no temporary file for two
/// processes to share.
pub fn exclude(common_dir: &Path, pattern: &[u8], why: &str) -> Result<()> {
    let path = common_dir.join("info").join("exclude");
    let text = match fs::read(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    if text.split(|b| *b == b'\n').any(|l| l == pattern) {
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("info")?)?;
    let mut add = Vec::new();
    if !text.is_empty() && !text.ends_with(b"\n") {
        add.push(b'\n');
    }
    add.extend_from_slice(format!("# {why}\n").as_bytes());
    add.extend_from_slice(pattern);
    add.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(&add)?;
    Ok(())
}

/// The global ignore file git uses in the repository or worktree with git
/// directory `git_dir`: `core.excludesFile` as git itself reads it there
/// (worktree config, conditional includes, `~`), none if it is set but empty,
/// else git's default `$XDG_CONFIG_HOME/git/ignore` (`~/.config/git/ignore`).
/// Anything else is an error, not a reason to fall back: a fallback could
/// ignore more than git does.
pub fn excludes_file(git_dir: &Path) -> Result<Option<PathBuf>> {
    let out = cmd::command(git_dir)
        .args(["config", "--path", "--get", "core.excludesFile"])
        .output()?;
    match out.status.code() {
        Some(0) => {
            let value = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(value));
            // Git resolves a relative one in the work tree, which isn't there
            // unmounted.
            anyhow::ensure!(
                value.is_empty() || path.is_absolute(),
                "core.excludesFile is a relative path ({}), which gitfold can't resolve",
                path.display()
            );
            Ok((!value.is_empty()).then_some(path))
        }
        // Unset. As for git, an empty environment variable counts as unset too.
        Some(1) => {
            let mut env = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
            Ok(gix::path::env::xdg_config("ignore", &mut env))
        }
        _ => Err(anyhow!(
            "git config core.excludesFile in {}: {}",
            git_dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// The repository `start` is in, found as git finds it (upwards from `start`;
/// in a linked worktree, a gitfold workspace included, its git directory),
/// for `git::init`.
pub fn discover(start: &Path) -> Result<PathBuf> {
    let shown = std::path::absolute(start).unwrap_or_else(|_| start.to_path_buf());
    let (path, _trust) = gix::discover::upwards(&shown)
        .with_context(|| format!("no git repository at or above {}", shown.display()))?;
    Ok(path.into_repository_and_work_tree_directories().0)
}
