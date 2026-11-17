//! Keeping a view current: every second, its members' refs are read (the
//! tree of branches again when the repository's config changed, or every
//! 30 s; for the root's view, which workspaces are opened); if anything
//! moved, git merges the view again (`merge`) and the kernel is told which
//! paths changed, so its long-cached entries, attributes and pages stay
//! right.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use gix::ObjectId;
use log::error;

use super::members::{self, Bases, BuiltOn, Refs};
use super::merge::{Member, Private};
use super::serve::ViewDir;
use super::shown::{Changed, Shown, Times};
use crate::fs::NameChanges;
use crate::git;
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
    pub private: Arc<Private>,
}

/// What a view was last built from.
#[derive(Default)]
pub struct Watch {
    /// The config file's change stamp the tree was read at, and how many
    /// rounds since.
    config: Option<Stamp>,
    reads: u32,
    tree: Tree,
    /// With the config's stamp: git reads its merge settings there.
    built: Option<(Vec<BuiltOn>, Refs, Stamp)>,
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
    /// The view again, if its refs (or the config) moved since it was last
    /// built (or it wasn't yet), after `before`: what it shows, and the
    /// paths that changed.
    pub fn next(
        &mut self,
        ctx: &Context,
        before: Option<&Shown>,
    ) -> Result<Option<(Shown, Vec<Changed>, ObjectId)>> {
        let config = stamp(&ctx.common_dir.join("config"));
        if self.config != Some(config) || self.reads >= TREE_EVERY {
            self.tree = Tree::read(&ctx.common_dir)?;
            self.config = Some(config);
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
        if let Some((m, r, c)) = &self.built
            && (m, r, *c) == (&members, &refs, config)
        {
            return Ok(None);
        }
        let bases = &mut self.bases;
        let inputs = (refs.members.iter())
            .map(|(branch, (tip, _), parent)| {
                let base = bases.get(*tip, *parent)?;
                Ok(Member {
                    branch,
                    tip: *tip,
                    base,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.bases.next_round();
        // What git will read, loaded first: in a partial clone, fetched as
        // for a snapshot (git doesn't fetch for the views).
        ctx.store.load_tree(refs.own.1)?;
        for ((_, (_, tree), _), m) in refs.members.iter().zip(&inputs) {
            ctx.store.load_tree(*tree)?;
            if let Some(base) = m.base {
                ctx.store.load_tree(git::resolve(&base.to_string())?.tree)?;
            }
        }
        // No lock the requests need is held meanwhile.
        let merged = ctx
            .private
            .merge(branch, origin.as_deref(), refs.own, &inputs)?;
        let tree = ctx.store.load_tree(merged.tree)?;
        let (conflicts, notes) = (&merged.conflicts, &refs.notes);
        let (shown, changed) = Shown::next(&ctx.store, tree, conflicts, notes, before, &self.times);
        self.built = Some((members, refs, config));
        Ok(Some((shown, changed, merged.commit)))
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
                    if let Some((shown, changed, commit)) = next {
                        // Waits for requests served from the old view: their
                        // replies can't land after the kernel is told.
                        view.swap(shown, &changed);
                        tell(&ctx.names, &view, &changed);
                        if let Err(e) = ctx.private.shown(&ctx.branch, commit) {
                            error!("view {}: {e:#}", view.name);
                        }
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
