//! Keeping a view current: every second, its members' refs are read (the
//! tree of branches again when the repository's config changed, or every
//! 30 s; for the root's view, which workspaces are opened); if anything
//! moved, git merges the view again (`merge`) and the kernel is told which
//! paths changed, so its long-cached entries, attributes and pages stay
//! right.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use gix::ObjectId;
use log::error;

use super::lives::Lives;
use super::members::{self, Bases, BuiltOn, Note, Refs, Tip};
use super::merge::{Member, Private};
use super::serve::{Kind, Tick, ViewDir};
use super::shown::{Changed, Shown, Times};
use crate::fs::NameChanges;
use crate::fs::workspaces::Workspaces;
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
    /// Its name in the private repository (`merge::Private::merge`):
    /// `<kind>/<its branch's workspace id>`.
    pub key: String,
    /// Its name below the mount root (`@live/api`).
    pub name: String,
    pub kind: Kind,
    pub workspaces: Arc<Workspaces>,
    pub lives: Arc<Lives>,
}

/// Which trees an `@live` view merges.
#[derive(Clone, Copy, PartialEq)]
pub enum Trees {
    /// The tips: the view opens with them, not waiting for git to read the
    /// workspaces (through the mount, from a request).
    Tips,
    /// The workspaces' live trees, made again once quiet (`lives`).
    Quiet,
    /// Made again at once if changed (the view woke).
    Now,
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
    /// The last error of each workspace whose live tree failed.
    live_failed: HashMap<String, String>,
}

/// A file's modification time and size, if it has them.
pub(super) type Stamp = Option<(SystemTime, u64)>;

pub(super) fn stamp(path: &Path) -> Stamp {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl Watch {
    /// The view again, if its refs, its members' live trees (`@live`, as
    /// `trees` says) or the config moved since it was last built (or it
    /// wasn't yet), after `before`: what it shows, and the paths that changed.
    pub fn next(
        &mut self,
        ctx: &Context,
        before: Option<&Shown>,
        trees: Trees,
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
        let mut refs = members::refs(branch, origin.as_deref(), &members, &mut self.bases)?;
        self.bases.next_round();
        if ctx.kind == Kind::Live && trees != Trees::Tips {
            let now = trees == Trees::Now;
            // Each workspace as it is: its live tree for its tip (one that
            // can't be read: its tip, noted). Opens the workspaces not open
            // yet (`Workspaces::open_quietly`).
            let mut notes = Vec::new();
            // Errors logged once, while they last.
            let before = std::mem::take(&mut self.live_failed);
            let failed = &mut self.live_failed;
            let view = &ctx.name;
            let mut live =
                |branch: &str, tip: &mut Tip| match ctx.lives.tip(ctx, branch, tip.0, now) {
                    Ok(Some(live)) => *tip = live,
                    Ok(None) => {}
                    Err(e) => {
                        // Logged once; noted as such (the error's words may vary).
                        let e = format!("{e:#}");
                        if before.get(branch) != Some(&e) {
                            error!("view {view}: the workspace of {branch}: {e}");
                        }
                        failed.insert(branch.to_string(), e);
                        notes.push(Note {
                            kind: "unread",
                            what: branch.to_string(),
                            why: "its workspace (see the log); its uncommitted work is left out"
                                .into(),
                        });
                    }
                };
            for (member, tip, _) in &mut refs.members {
                live(member, tip);
            }
            if refs.own == refs.local {
                live(branch, &mut refs.own);
            } else {
                // The view shows its origin root's files (its local branch only
                // behind them): the workspace's work merged in first, against
                // the local tip, if it has any.
                let mut own = refs.local;
                live(branch, &mut own);
                if own.1 != refs.local.1 {
                    let name = format!("{branch}'s workspace");
                    refs.members.insert(0, (name, own, refs.local.0));
                }
            }
            refs.notes.extend(notes);
        }
        if let Some((m, r, c)) = &self.built
            && (m, r, *c) == (&members, &refs, config)
        {
            return Ok(None);
        }
        let inputs: Vec<Member> = (refs.members.iter())
            .map(|(branch, (tip, _), base)| Member {
                branch,
                tip: *tip,
                base: *base,
            })
            .collect();
        // What git will read, loaded first: in a partial clone, fetched as
        // for a snapshot (git doesn't fetch for the views).
        ctx.store.load_tree(refs.own.1)?;
        for (_, (_, tree), base) in &refs.members {
            ctx.store.load_tree(*tree)?;
            ctx.store.load_tree(git::resolve(&base.to_string())?.tree)?;
        }
        // No lock the requests need is held meanwhile.
        let merged = ctx.private.merge(&ctx.key, branch, refs.own, &inputs)?;
        let tree = ctx.store.load_tree(merged.tree)?;
        let (conflicts, notes) = (&merged.conflicts, &refs.notes);
        let (shown, changed) = Shown::next(&ctx.store, tree, conflicts, notes, before, &self.times);
        self.built = Some((members, refs, config));
        Ok(Some((shown, changed, merged.commit)))
    }
}

/// Keeps `view` current while it exists: every second, unless it rests (not
/// accessed for `rests_after()`); an access wakes it (`ViewDir::reached`), and
/// it is refreshed at once.
pub fn spawn(view: Weak<ViewDir>, ctx: Context, mut watch: Watch) {
    let name = format!("view {}", ctx.branch);
    let spawned = thread::Builder::new().name(name).spawn(move || {
        loop {
            let Some(view) = view.upgrade() else { return };
            let woken = match view.rest.tick(INTERVAL) {
                Tick::Refresh => false,
                Tick::Wake => true,
                Tick::Idle => {
                    // Told first: a lookup through it then waits for nothing.
                    resting(&ctx.names, &view);
                    view.rest.rest();
                    continue;
                }
                Tick::Rests => continue,
            };
            let before = view.shown();
            let trees = if woken { Trees::Now } else { Trees::Quiet };
            // Requests wait for a woken view until it is refreshed.
            let mut waiting = woken;
            match watch.next(&ctx, Some(&before), trees) {
                Ok(next) => {
                    watch.last_error.clear();
                    if let Some((shown, changed, commit)) = next {
                        // Waits for requests served from the old view: their
                        // replies can't land after the kernel is told.
                        view.swap(shown, &changed);
                        let entries = tell(&ctx.names, &view, &changed);
                        // Woken: the requests waiting go on before the kernel
                        // forgets entries; they hold their directories, which
                        // that takes. (Pages and attributes were told: they
                        // read the new ones.)
                        if std::mem::take(&mut waiting) {
                            view.rest.woke();
                        }
                        for (dir, name) in entries {
                            ctx.names.entry(dir, &name);
                        }
                        if let Err(e) = ctx.private.shown(&ctx.key, commit) {
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
            if waiting {
                view.rest.woke();
            }
        }
    });
    if let Err(e) = spawned {
        error!("can't keep a view current: {e}");
    }
}

/// `view` rests: the kernel forgets its root's entry, so that the next path
/// through it asks again, which wakes it (an `@committed` view's are cached
/// long; an `@live` view's ask again soon anyway).
fn resting(names: &NameChanges, view: &ViewDir) {
    names.entry(view.parent, name(view.name.as_bytes()));
}

/// Tells the kernel that `changed` paths of `view` changed: the directories'
/// listings, their attributes and pages; their entries in their directories
/// are returned, for the caller to tell. Only for what it holds.
fn tell(names: &NameChanges, view: &ViewDir, changed: &[Changed]) -> Vec<(u64, Vec<u8>)> {
    let ino = |local: u64| inode::view(view.slot, local);
    // The root holds the summary, which changes with what is merged.
    let mut inodes = vec![ino(0)];
    let mut dirs = HashSet::from([0]);
    let mut entries = Vec::new();
    for path in changed.iter().map(|c| &c.path) {
        if let Some(dir) = view.held(parent(path)) {
            entries.push((ino(dir), name(path).to_vec()));
            if dirs.insert(dir) {
                inodes.push(ino(dir));
            }
        }
        if let Some(local) = view.held(path) {
            inodes.push(ino(local));
        }
    }
    names.changed(&inodes);
    entries
}
