//! Keeping a view current: every second, its members' refs are read (and
//! the tree of branches again when the repository's config or the set of
//! workspaces changed); if they moved, the view is built again and the
//! kernel told which paths changed, so its long-cached entries, attributes and
//! pages stay right.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use fuser::{INodeNo, Notifier};
use log::{debug, error};

use super::build;
use super::members::{self, Bases, BuiltOn, Refs};
use super::merge;
use super::serve::{SUMMARY_LOCAL, ViewDir};
use super::shown::Shown;
use crate::fs::NameChanges;
use crate::inode;
use crate::state::{self, parent};
use crate::store::Store;
use crate::tree::Tree;

/// How often a view's refs are read.
const INTERVAL: Duration = Duration::from_secs(1);
/// How often the tree is read again even when nothing says it changed (a
/// new `origin/HEAD` moves the root without touching the config).
const TREE_EVERY: u32 = 30;
/// When the kernel is told a second time: a lookup or listing in flight while
/// the view was swapped can be cached after the first.
const AGAIN_AFTER: Duration = Duration::from_millis(100);

/// What a view's watch needs from the mount.
pub struct Context {
    pub common_dir: PathBuf,
    pub store: Arc<Store>,
    pub names: Arc<NameChanges>,
    pub branch: String,
}

/// What a view was last built from.
#[derive(Default)]
pub struct Watch {
    /// The config file's and the workspaces' directory's change stamps the
    /// tree was read at, and how many reads since.
    stamps: Option<(Stamp, Stamp)>,
    reads: u32,
    tree: Tree,
    opened: HashSet<String>,
    built: Option<(Vec<BuiltOn>, Refs)>,
    bases: Bases,
    last_error: String,
}

/// A file's modification time and size, if it has them.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl Watch {
    /// The view again, if its refs moved since it was last built (or it
    /// wasn't yet): what it shows, and the paths that changed.
    pub fn next(
        &mut self,
        ctx: &Context,
        before: Option<&Shown>,
    ) -> Result<Option<(Shown, Vec<Vec<u8>>)>> {
        let stamps = (
            stamp(&ctx.common_dir.join("config")),
            stamp(&state::state_root(&ctx.common_dir)),
        );
        if self.stamps != Some(stamps) || self.reads >= TREE_EVERY {
            self.tree = Tree::read(&ctx.common_dir)?;
            self.opened = (state::list(&ctx.common_dir)?.into_iter())
                .filter(|s| s.state.opened())
                .map(|s| s.branch)
                .collect();
            self.stamps = Some(stamps);
            self.reads = 0;
        }
        self.reads += 1;
        let branch = &ctx.branch;
        let origin = members::origin_root(&self.tree, branch);
        let opened = |b: &str| self.opened.contains(b);
        let members = members::members(&self.tree, branch, origin.as_deref(), opened);
        let refs = members::refs(branch, origin.as_deref(), &members, &mut self.bases)?;
        if before.is_some()
            && let Some((m, r)) = &self.built
            && (m, r) == (&members, &refs)
        {
            return Ok(None);
        }
        let style = merge::conflict_style(&ctx.common_dir);
        let (files, inputs) = members::committed(&ctx.store, &refs, &mut self.bases)?;
        self.bases.next_round();
        let view = build::build(&ctx.store, branch, files, &inputs, style)?;
        let next = Shown::next(&ctx.store, view, &refs.notes, before);
        self.built = Some((members, refs));
        Ok(Some(next))
    }
}

/// Keeps `view` current while it exists.
pub fn spawn(view: Weak<ViewDir>, ctx: Context, mut watch: Watch) {
    let name = format!("view {}", ctx.branch);
    let spawned = thread::Builder::new().name(name).spawn(move || {
        loop {
            thread::sleep(INTERVAL);
            let Some(view) = view.upgrade() else { return };
            let before = view.shown();
            match watch.next(&ctx, Some(&before)) {
                Ok(None) => watch.last_error.clear(),
                Ok(Some((shown, changed))) => {
                    watch.last_error.clear();
                    let summary = shown.summary != before.summary;
                    view.swap(shown);
                    if let Some(notifier) = ctx.names.notifier() {
                        tell(notifier, &view, &changed, summary);
                        thread::sleep(AGAIN_AFTER);
                        tell(notifier, &view, &changed, summary);
                    }
                }
                Err(e) => {
                    // It keeps showing what it last could.
                    let e = format!("{e:#}");
                    if e != watch.last_error {
                        error!("view {}: {e}", view.name);
                        watch.last_error = e;
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        error!("can't keep a view current: {e}");
    }
}

/// Tells the kernel that `changed` paths of `view` (and its summary, if
/// `summary`) changed: their entries in their directories, the directories'
/// listings, their attributes and pages. Only for what it holds.
fn tell(notifier: &Notifier, view: &ViewDir, changed: &[Vec<u8>], summary: bool) {
    let ino = |local: u64| INodeNo(inode::view(view.slot, local));
    let inval_inode = |local: u64| {
        if let Err(e) = notifier.inval_inode(ino(local), 0, 0) {
            debug!("view {}: invalidating {local}: {e}", view.name);
        }
    };
    // The root's time is the view's: its attributes change every time.
    inval_inode(0);
    if summary {
        inval_inode(SUMMARY_LOCAL);
    }
    let mut dirs = HashSet::new();
    for path in changed {
        let dir = parent(path);
        let name = &path[dir.len() + usize::from(!dir.is_empty())..];
        if let Some(dir) = view.held(dir) {
            let name = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name);
            if let Err(e) = notifier.inval_entry(ino(dir), name) {
                debug!("view {}: invalidating an entry: {e}", view.name);
            }
            if dirs.insert(dir) && dir != 0 {
                inval_inode(dir);
            }
        }
        if let Some(local) = view.held(path) {
            inval_inode(local);
        }
    }
}
