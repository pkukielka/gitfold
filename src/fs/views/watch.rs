//! Keeping a view current: every second, its members' refs are read (the
//! tree of branches again when the repository's config changed, or every
//! 30 s; for the root's view, which workspaces are opened); if anything
//! moved, the view is built again and the kernel told which paths changed, so
//! its long-cached entries, attributes and pages stay right.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use log::error;

use super::build;
use super::members::{self, Bases, BuiltOn, Refs};
use super::merge;
use super::merge::ConflictStyle;
use super::serve::ViewDir;
use super::shown::{Changed, Shown, Times};
use crate::fs::NameChanges;
use crate::inode;
use crate::state::{self, name, parent};
use crate::store::Store;
use crate::tree::Tree;

/// How often a view's refs are read.
const INTERVAL: Duration = Duration::from_secs(1);
/// How often the tree is read again even when the config didn't change (a
/// new `origin/HEAD` moves the root without touching it).
const TREE_EVERY: u32 = 30;

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
    /// The config file's change stamp the tree was read at, and how many
    /// rounds since.
    config: Option<Stamp>,
    reads: u32,
    tree: Tree,
    built: Option<(Vec<BuiltOn>, Refs, ConflictStyle)>,
    /// The conflict style the config asks for.
    style: ConflictStyle,
    times: Times,
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
    /// wasn't yet), after `before`: what it shows, and the paths that
    /// changed.
    pub fn next(
        &mut self,
        ctx: &Context,
        before: Option<&Shown>,
    ) -> Result<Option<(Shown, Vec<Changed>)>> {
        let config = stamp(&ctx.common_dir.join("config"));
        if self.config != Some(config) || self.reads >= TREE_EVERY {
            self.tree = Tree::read(&ctx.common_dir)?;
            self.config = Some(config);
            // As git reads it now; a change builds the view again.
            self.style = merge::conflict_style(&ctx.common_dir);
            self.reads = 0;
        }
        self.reads += 1;
        let branch = &ctx.branch;
        // Only the root's view depends on which workspaces are opened.
        let opened: HashSet<String> = match self.tree.root() == Some(branch) {
            true => (state::list(&ctx.common_dir)?.into_iter())
                .filter(|s| s.state.opened())
                .map(|s| s.branch)
                .collect(),
            false => HashSet::new(),
        };
        let origin = members::origin_root(&self.tree, branch);
        let opened = |b: &str| opened.contains(b);
        let members = members::members(&self.tree, branch, origin.as_deref(), opened);
        let refs = members::refs(branch, origin.as_deref(), &members, &mut self.bases)?;
        if let Some((m, r, s)) = &self.built
            && (m, r, *s) == (&members, &refs, self.style)
        {
            return Ok(None);
        }
        let (files, inputs) = members::committed(&ctx.store, &refs, &mut self.bases)?;
        self.bases.next_round();
        let view = build::build(&ctx.store, branch, files, &inputs, self.style)?;
        let next = Shown::next(&ctx.store, view, &refs.notes, before, &self.times);
        self.built = Some((members, refs, self.style));
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
                Ok(next) => {
                    watch.last_error.clear();
                    if let Some((shown, changed)) = next {
                        // Waits for requests served from the old view: their
                        // replies can't land after the kernel is told.
                        view.swap(shown, &changed);
                        tell(&ctx.names, &view, &changed);
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

/// Tells the kernel that `changed` paths of `view` changed: their entries in
/// their directories, the directories' listings, their attributes and pages.
/// Only for what it holds.
fn tell(names: &NameChanges, view: &ViewDir, changed: &[Changed]) {
    let ino = |local: u64| inode::view(view.slot, local);
    // The root holds the summary, which changes with what is merged.
    let mut inodes = vec![ino(0)];
    let mut dirs = HashSet::from([0]);
    for path in changed.iter().map(|c| &c.path) {
        if let Some(dir) = view.held(parent(path)) {
            names.entry(ino(dir), name(path));
            if dirs.insert(dir) {
                inodes.push(ino(dir));
            }
        }
        if let Some(local) = view.held(path) {
            inodes.push(ino(local));
        }
    }
    names.changed(&inodes);
}
