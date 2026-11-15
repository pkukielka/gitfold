//! Layers in an open workspace: other workspaces' work shown over its base,
//! read-only, under its own changes (see `state::layers` for the model and its
//! storage).
//!
//! The lower layer becomes the view tree; everything that reads the lower
//! layer reads it. A switch (a layer shown, hidden, or moved to a newer snapshot)
//! changes what the kernel has cached, so it is told: the attributes and pages
//! of every file whose content changed, the entries added, removed or changed
//! in type, and the listings of their directories. Changed files get a fresh
//! time, so builds see them. Paths the workspace's own changes hide don't
//! change.
//!
//! Git keeps seeing only the workspace's own changes:
//! - A layer's whole files: the switch marks those git tracks skip-worktree in
//!   the worktree's index (git neither shows nor commits them; it keeps the
//!   base's version), and excludes those layers add, in a per-worktree
//!   excludes file that keeps the user's global ignore file too
//!   (`git::worktree_config`). They can't be written here.
//! - Union files (`state::layers::view`): the workspace writes them, its own
//!   lines on top of the layers'. HEAD stays on the branch, and git sees only
//!   the workspace's own lines through a clean/smudge filter this workspace
//!   answers (`git::filter`, `filter`): the driver and an attributes line per
//!   union path in the worktree's own config (`worktree_config::set_filter`).
//!   A switch zeroes the size recorded in their index entries, so git
//!   re-reads (cleans) them instead of taking the new size as a change
//!   (git's size rule). The guard hooks (`git::guard`) refuse a ref update or
//!   a push bringing the layers' uncommitted lines onto the branch, for what
//!   doesn't run the filter. A switch carries the workspace's own lines in a
//!   union file over (a 3-way merge), or is refused if they depend on lines
//!   that would go.
//! - Files only editable layers changed (`edited`): writable, and seen by
//!   git through the same filter, which also leaves out the workspace's
//!   waiting edits (`state::waiting`); a switch carries them over, a
//!   conflict kept with git's markers. Before a write, their index entry's
//!   size is zeroed (`unstat_edited`), so a waiting edit isn't taken as a
//!   change by its size. Files only they have (`edited_new`) stay excluded.
//! - The sync thread checks the branch's new commits with the guard's rule
//!   (`check_branch`, `state::guard_check`), for commits made without the
//!   filter and the hooks.
//!
//! What was applied (the layers, what git was told) is recorded apart from
//! the layers stored (`state::layers::ShownLayers`), step by step as git is told,
//! the layers last: a workspace opens showing what was applied, so the view
//! and git agree, then switches to the stored layers if they differ (changed
//! while it was closed, or a switch that failed half-way). Switches and the
//! sync thread take `layers_lock`, so they never interleave; git is told
//! after the workspace's own lock is released, so git processes reading the
//! workspace meanwhile don't wait on it.
//!
//! Where git can't be kept right (a bare repository, sparse checkout, a split
//! or sparse index, an index git keeps locked), git is turned off in the
//! workspace instead (git off): its `.git` names a directory that doesn't
//! exist, whose name says why, so every git command fails with it.
//!
//! Meanwhile the base doesn't follow the branch (`rebase`): moving it would
//! copy the layers' files into the workspace's own. Commits still work; the
//! base catches up once no layer shows, and before layers go on in a workspace
//! showing none (`catch_up_before_layers`). A move of the branch (its own
//! commits onto an editable layer's new ones, `moves`) moves the base with
//! the view, layers shown or not.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use fuser::INodeNo;
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::objs::tree::EntryKind;

use super::times::Stamp;
use super::{GITFILE_LOCAL, Layers, Workspace};
use sha1::Digest;

use crate::git::index_edit::{self, Edit};
use crate::git::{self, filter, worktree, worktree_config};
use crate::inode;
use crate::refusal::Refusal;
use crate::state::layers::{self, ShownLayers};
use crate::state::{StateDir, ancestors};
use crate::store::{Lower, Store};

/// A file in the state directory whose presence means git is off
/// (see the module doc): its content is why.
const GIT_OFF: &str = "layers-git-off";

/// How long a switch waits for git to release the index.
const INDEX_WAIT: Duration = Duration::from_secs(3);

/// How long the sync thread waits before retrying a switch to stored layers
/// that was refused.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// How many smudged contents a workspace remembers (see `filter`).
const MEMO_MAX: usize = 1024;

/// What `carry_over` gives: per path, the upper file's new content, or `None`
/// to drop it.
type Carried = Vec<(Vec<u8>, Option<Vec<u8>>)>;

/// What smudge was given for a file (see `Workspace::filter`).
#[derive(Clone)]
pub(super) struct Smudged {
    /// git's version.
    own: Vec<u8>,
    /// For a file only editable layers changed, the waiting edits it kept
    /// (the file shown with them), so a smudge of that content again keeps
    /// the same ones.
    kept: Option<Vec<u8>>,
}

/// What smudge was given, by path and the hash of what it gave back.
pub(super) type FilterMemo = HashMap<(Vec<u8>, [u8; 20]), Smudged>;

/// The index's object ids of some paths, as read from the index file with
/// `stamp` (its mtime, size and inode).
pub(super) struct IndexOids {
    stamp: (SystemTime, u64, u64),
    paths: HashSet<Vec<u8>>,
    oids: HashMap<Vec<u8>, ObjectId>,
}

/// A move of the branch made with a switch (`moves`).
pub(super) struct MoveStep {
    pub intent: crate::state::moves::Intent,
    /// The layers to show over its new tip (a layers file's content).
    pub layers: Vec<u8>,
}

/// What a switch did.
pub(super) enum Switched {
    /// Switched: what it says it did.
    Done(String),
    /// The move it was to make can't be made now: why. Nothing changed.
    Waits(String),
}

/// What the kernel must be told after a switch (local ids of the workspace).
#[derive(Default)]
struct Stale {
    /// Nodes whose attributes and pages changed (directories: their listing).
    inodes: Vec<u64>,
    /// Names added, removed or changed in type, by their directory's node.
    entries: Vec<(u64, Vec<u8>)>,
}

/// What git is to be told after a switch: worked out under the workspace's
/// lock, told after it is released.
struct GitPlan {
    layers_on: bool,
    base: ObjectId,
    /// Layers' tracked whole files the workspace's own changes don't hide.
    skipped: BTreeSet<Vec<u8>>,
    /// Files layers add that the workspace's own changes don't hide.
    added: Vec<Vec<u8>>,
    /// The files git sees through the filter: union files, and files only
    /// editable layers changed that the base has.
    filtered: BTreeSet<Vec<u8>>,
    /// The files only editable layers have: hidden, all changes to them
    /// waiting edits (a move leaves them to the switch).
    theirs: BTreeSet<Vec<u8>>,
}

/// The `.git` content while git is off: git prints the path it names.
fn off_gitfile(layers: &layers::Layers, why: &str) -> Vec<u8> {
    let shown: Vec<&str> = layers.layers.iter().map(|l| l.branch.as_str()).collect();
    format!(
        "gitdir: /LAYERS SHOWN ({}): git is off here ({why}). Run gitfold layer remove --all to use git\n",
        shown.join("+")
    )
    .into_bytes()
}

/// The registration's `.git` content.
fn registered(state: &StateDir) -> Vec<u8> {
    format!("gitdir: {}\n", state.admin.display()).into_bytes()
}

/// The ref keeping workspace `id`'s view commit (and through it the
/// snapshots) from `git gc`.
fn layers_ref(id: &str) -> String {
    format!("refs/gitfold-layers/{id}")
}

/// The filter driver of workspace `id` (see `git::filter`): named after it,
/// so other worktrees, which don't define it, ignore the attribute.
fn driver(id: &str) -> String {
    format!("gitfold-{id}")
}

/// Sets up what was last applied of the layers of the workspace in `state`
/// (see the module doc) when it opens. Returns the `.git` content if git is
/// off.
pub(super) fn open_view(
    store: &Store,
    state: &StateDir,
    layers: &mut Layers,
) -> Result<Option<Vec<u8>>> {
    let applied = ShownLayers::read(state)?;
    let shown = applied.layers()?;
    let off = fs::read_to_string(state.dir.join(GIT_OFF))
        .ok()
        .map(|why| off_gitfile(&shown, why.trim()));
    layers.applied = applied;
    let view = match layers::view(&state.common_dir, &layers.base, &shown) {
        Ok(view) => view,
        // A layer's snapshot gone (pruned): open without the layers, git off;
        // the switch that follows reports it.
        Err(e) => {
            log::warn!("{}: its layers can't be shown: {e:#}", state.branch);
            return Ok(Some(off_gitfile(&shown, "its layers are missing")));
        }
    };
    let Some(view) = view else {
        return Ok(off);
    };
    layers.view_tree = store.load_tree(view.tree)?;
    layers.view = view.tree;
    layers.hunk = view.hunk_tree;
    layers.mixed = view.mixed;
    layers.writable = view.writable;
    layers.edited = view.edited;
    layers.edited_new = view.edited_new;
    layers.union = view.union;
    layers.layers_on = true;
    Ok(off)
}

/// A blob's content at `path` in `tree` (empty if none).
fn content_at(tree: ObjectId, path: &[u8]) -> Result<Vec<u8>> {
    let item = git::with_repo(|repo| git::layers::lookup(repo, tree, BStr::new(path)))?;
    match item {
        Some(item) => git::read_blob(item.oid),
        None => Ok(Vec::new()),
    }
}

impl Workspace {
    /// The sync thread's safety net (`state::guard_check`): once its branch
    /// moved, its new commits checked for the layers' uncommitted lines;
    /// what is found is logged and kept for the listing and the hook.
    pub fn check_branch(&self) {
        let mut failed = self.guard_check_failed.lock();
        match crate::state::guard_check::check(&self.state) {
            Ok(_) => *failed = None,
            Err(e) => {
                let e = format!("{e:#}");
                if failed.as_deref() != Some(e.as_str()) {
                    log::warn!("{}: checking its new commits: {e}", self.state.branch);
                    *failed = Some(e);
                }
            }
        }
    }

    /// Before a write to `path`, a file only editable layers changed (git
    /// sees it through the filter): zeroes the size recorded in its index
    /// entry, so git reads (cleans) it again rather than taking the new size
    /// as a change (git's size rule), which a waiting edit isn't. Once per
    /// write burst: again only after the index changed since (git recorded
    /// the file's stat meanwhile). Skipped while git holds the index (it is
    /// writing the file itself, and records its stat).
    pub(super) fn unstat_edited(&self, layers: &Layers, path: &[u8]) {
        if !layers.edited.contains(path) {
            return;
        }
        let index = self.state.index();
        let mtime = || fs::metadata(&index).and_then(|m| m.modified()).ok();
        let mut unstated = self.unstated.lock();
        if unstated.get(path).is_some_and(|at| *at == mtime()) {
            return;
        }
        let edit = Edit {
            stat: Some(index_edit::Stat::default()),
            ..Default::default()
        };
        let edits = HashMap::from([(path.to_vec(), edit)]);
        match index_edit::edit(&index, &edits) {
            Ok(_) => {
                unstated.insert(path.to_vec(), mtime());
            }
            Err(e) if matches!(e.downcast_ref(), Some(index_edit::Refused::Locked(_))) => {}
            Err(e) => log::warn!(
                "{}: {}: zeroing its index entry's size: {e:#}",
                self.state.branch,
                path.to_str_lossy()
            ),
        }
    }

    /// Whether layers are shown.
    pub fn shows_layers(&self) -> bool {
        self.layers.read().layers_on
    }

    /// Whether the stored layers differ from those applied (a cheap read).
    pub(crate) fn layers_pending(&self) -> bool {
        let stored = layers::shown_bytes(&self.state).unwrap_or_default();
        stored != self.layers.read().applied.layers
    }

    /// Gives it the session's notifier and its slot, so it can tell the
    /// kernel what a switch or turning git off changed.
    pub fn set_kernel(&self, notifier: fuser::Notifier, slot: u32) {
        let _ = self.kernel.set((notifier, slot));
    }

    /// Tells the kernel what changed, after the workspace's lock is released:
    /// invalidating an entry waits for lookups in its directory, which wait
    /// for the lock.
    fn invalidate(&self, stale: &Stale) {
        let Some((notifier, slot)) = self.kernel.get() else {
            return;
        };
        for (dir, name) in &stale.entries {
            let name = std::ffi::OsStr::from_bytes(name);
            let _ = notifier.inval_entry(INodeNo(inode::ws(*slot, *dir)), name);
        }
        for &local in &stale.inodes {
            let _ = notifier.inval_inode(INodeNo(inode::ws(*slot, local)), 0, 0);
        }
    }

    /// Shows the layers stored for it (`state::layers`), as they are now, and
    /// says what it did. Refused while git is busy in it (the switch changes
    /// what its index describes), or if its own lines in a union file depend
    /// on lines that would go; then nothing changed.
    pub fn switch(&self, store: &Store) -> Result<String> {
        match self.switch_with(store, None)? {
            Switched::Done(done) => Ok(done),
            Switched::Waits(why) => Err(Refusal::Changes.error(why)),
        }
    }

    /// `switch`, with the branch moved as `mv` says (`moves`): the view is
    /// then built over its new tip with `mv`'s layers, its base moved there
    /// with it. A move that can't be made now changes nothing
    /// (`Switched::Waits`).
    pub(super) fn switch_with(&self, store: &Store, mv: Option<&MoveStep>) -> Result<Switched> {
        let _layers = self.layers_lock.lock();
        let branch = &self.state.branch;
        if let Some(busy) = self.state.git_busy() {
            return Err(Refusal::Changes.error(format!(
                "{branch}: git is busy ({busy}); switch its layers when it is done"
            )));
        }
        let (stored, stored_bytes) = match mv {
            Some(mv) => (layers::Layers::parse(&mv.layers)?, mv.layers.clone()),
            None => layers::to_show(&self.state)?,
        };
        let new_base = match mv {
            Some(mv) => {
                if let Some(why) = self.move_waits(store, &mv.intent)? {
                    return Ok(Switched::Waits(why));
                }
                Some(git::resolve(&mv.intent.to.to_string())?)
            }
            None => {
                self.catch_up_before_layers(store, &stored)?;
                None
            }
        };
        let base = new_base.as_ref().map_or(self.layers.read().base, |c| c.id);
        let base_tree = git::resolve(&base.to_string())?.tree;
        let new_base_tree = match &new_base {
            Some(c) => Some(store.load_tree(c.tree)?),
            None => None,
        };
        let view = layers::view(&self.state.common_dir, &base, &stored)?;
        let (new_view, new_hunk, mixed, union) = match &view {
            Some(v) => (v.tree, v.hunk_tree, v.mixed.clone(), v.union.clone()),
            None => (base_tree, base_tree, Default::default(), Default::default()),
        };
        let (writable, edited, edited_new) = view
            .as_ref()
            .map(|v| (v.writable.clone(), v.edited.clone(), v.edited_new.clone()))
            .unwrap_or_default();

        let new_tree = store.load_tree(new_view)?;
        let mut held = None;
        let (stale, mut failed, plan, applied, changed) = {
            let mut layers = self.layers.write();
            // Under the write lock: no write to a union file can land
            // between reading it and replacing it.
            let read_only = |p: &[u8]| mixed.contains_key(p) && !writable.contains(p);
            let carried = self.carry_over(
                &layers,
                (new_view, base_tree),
                &union,
                (&edited, &edited_new),
                read_only,
            )?;
            let changes = git::with_repo(|repo| git::layers::delta(repo, layers.view, new_view))
                .context("comparing the views")?;
            // The move's steps 1 and 2 (`state::moves`): the index, then
            // the base, the copies of committed files it changes dropped.
            let mut dropped = Vec::new();
            if let (Some(mv), Some(new_base), Some(new_base_tree)) = (mv, &new_base, new_base_tree)
            {
                let begun = mv.intent.begin(&self.state)?;
                // Nothing moved if it fails: the index is written whole.
                crate::state::moves::move_index(&self.state, &mv.intent, false)?;
                // Past here, a failure leaves the intent for `recover`.
                if let Err(e) = self.state.write_base(&new_base.id) {
                    begun.keep();
                    return Err(e.into());
                }
                layers.base = new_base.id;
                layers.base_tree = new_base_tree;
                match self.drop_for_move(store, &mut layers, &mv.intent) {
                    Ok(d) => dropped = d,
                    Err(e) => {
                        begun.keep();
                        return Err(e);
                    }
                }
                held = Some(begun);
            }
            // Past here the view changes: every failure is reported after the
            // kernel is told what changed.
            let old_tree = layers.view_tree;
            layers.mixed = mixed;
            layers.writable = writable;
            layers.edited = edited;
            layers.edited_new = edited_new;
            layers.union = union;
            layers.view_tree = new_tree;
            layers.view = new_view;
            layers.hunk = new_hunk;
            layers.layers_on = stored.is_on();
            // What was smudged with the old view isn't what clean must give.
            self.filter_memo.lock().clear();
            let mut stale = self.stale(store, &mut layers, old_tree, new_tree, &changes);
            let mut failed: Option<anyhow::Error> = None;
            {
                let nodes = self.nodes.lock();
                // What the move dropped shows the new base's file now.
                let now = SystemTime::now();
                for path in &dropped {
                    let stamp = Stamp {
                        mtime: now,
                        ctime: now,
                    };
                    layers.times.set(path, Some(stamp));
                    if let Some(local) = nodes.held(path) {
                        stale.inodes.push(local);
                    }
                }
                for (path, data) in &carried {
                    let done = match data {
                        Some(data) => self.rewrite_upper(path, data),
                        // The same as what shows below it now: dropped.
                        None => match fs::remove_file(self.upper_path(path)) {
                            Err(e) if !crate::fsutil::not_found(&e) => Err(e.into()),
                            _ => Ok(()),
                        },
                    };
                    if let Err(e) = done {
                        failed.get_or_insert(e);
                    }
                    if let Some(local) = nodes.held(path) {
                        stale.inodes.push(local);
                    }
                }
            }
            if let Err(e) = layers.times.save() {
                failed.get_or_insert(e.into());
            }
            self.changed();
            let plan = self.git_plan(store, &layers);
            (stale, failed, plan, layers.applied.clone(), changes.len())
        };
        self.invalidate(&stale);
        // The move's step 3: the refs.
        if let (Some(mv), Some(held)) = (mv, held) {
            let i = &mv.intent;
            let refs = git::stack::move_refs(
                &self.state.common_dir,
                branch,
                (i.from, i.to),
                (i.own_from, i.own_to),
                "gitfold: move",
            );
            if let Err(e) = refs {
                held.keep();
                return Err(e);
            }
            held.done()?;
            if let Err(e) = self.pin_base() {
                log::warn!("{branch}: pinning its base: {e:#}");
            }
        }
        // Git is told with the workspace usable meanwhile: the plan is the
        // view just installed, and `layers_lock` keeps other switches away.
        let mut applied = applied;
        let git = self.tell_git(&plan, &mut applied, &stored, view.as_ref());
        let why = git.err().map(|e| format!("{e:#}"));
        // ShownLayers even where git couldn't be told: git off is how the layers
        // show then, and the sync thread doesn't retry every second. A switch
        // asked for again (`gitfold layer add`, `layer remove`) tells git again.
        // Recorded last (see `tell_git`): until then the switch is pending.
        applied.layers = stored_bytes;
        let off = why.is_some()
            && (plan.layers_on || !applied.union.is_empty() || !applied.skipped.is_empty());
        {
            let mut layers = self.layers.write();
            if let Err(e) = applied.write(&self.state) {
                failed.get_or_insert(e);
            }
            layers.applied = applied;
        }
        let git_off = why.as_deref().filter(|_| off);
        if let Err(e) = self.set_gitfile(&stored, git_off) {
            failed.get_or_insert(e);
        }
        if let (Some(why), false) = (&why, off) {
            log::warn!("{branch}: giving git back after the layers: {why}");
        }
        let notes = view.iter().flat_map(|v| &v.notes);
        match failed {
            Some(e) => Err(e),
            None => Ok(Switched::Done(switched(
                branch,
                plan.layers_on,
                changed,
                git_off,
                notes,
            ))),
        }
    }

    /// Before layers go on in a workspace showing none: its base catches up
    /// with its branch first (`rebase`), as the sync thread would have
    /// soon, since the base waits while layers show: the files committed
    /// meanwhile (copies in upper) are then the new base's, not the
    /// workspace's own, which would hide the layers' at those paths. Fails
    /// (the switch refused, retried) if it can't catch up now, and while a
    /// move of the branch is under way or was interrupted (`state::moves`):
    /// the base follows the branch only once that is finished or undone.
    fn catch_up_before_layers(&self, store: &Store, stored: &layers::Layers) -> Result<()> {
        let (none_shown, base) = {
            let layers = self.layers.read();
            (!layers.layers_on, layers.base)
        };
        if !stored.is_on() || !none_shown {
            return Ok(());
        }
        if crate::state::moves::pending(&self.state) {
            return Err(Refusal::Changes.error(format!(
                "{} is being moved; its layers show once the move is done",
                self.state.branch
            )));
        }
        let tip = git::branch_tip(&self.state.branch)?;
        if tip.id == base {
            return Ok(());
        }
        let rebased = self.rebase(store, &tip, true);
        self.rebase_failed
            .store(rebased.is_err(), std::sync::atomic::Ordering::Relaxed);
        rebased?;
        if self.layers.read().base != tip.id {
            return Err(Refusal::Changes.error(format!(
                "{}: its base is catching up with its branch; its layers show once it has",
                self.state.branch
            )));
        }
        Ok(())
    }

    /// The `.git` content after a switch: the registration, or with git not
    /// kept right (`why`), git off (persisted, so a remount keeps it);
    /// the kernel is told.
    fn set_gitfile(&self, stored: &layers::Layers, why: Option<&str>) -> Result<()> {
        let off = self.state.dir.join(GIT_OFF);
        let content = match why {
            Some(why) => {
                log::warn!("{}: git off: {why}", self.state.branch);
                crate::fsutil::write_atomic(&off, why.as_bytes())?;
                off_gitfile(stored, why)
            }
            None => {
                let _ = fs::remove_file(&off);
                registered(&self.state)
            }
        };
        let changed = {
            let mut gitfile = self.gitfile.write();
            let changed = **gitfile != content;
            *gitfile = Arc::new(content);
            changed
        };
        if changed {
            self.invalidate(&Stale {
                inodes: vec![GITFILE_LOCAL],
                entries: Vec::new(),
            });
        }
        Ok(())
    }

    /// The new content of the upper files at filtered paths (union files and
    /// files editable layers changed, before or now) when the view changes
    /// from `layers`' to tree `new_view`, with `union` and `edited` (files
    /// the base has, and those only the layers have) the new view's: the
    /// workspace's own lines, and its waiting edits, merged onto
    /// what shows there now. A union file's by a 3-way merge, the line-set
    /// rule, or as git's filter sees them, else the switch is refused; any
    /// other's by a 3-way merge, a conflict kept with git's markers (listed
    /// as the waiting edits' conflict). `None`: a file no longer filtered
    /// whose copy is what shows below it now, dropped (a copy of a see-only
    /// layer's file would hide it, and be writable). A crash before the
    /// switch is recorded leaves the files carried and the old view
    /// applied: the switch is made again, and carrying again changes
    /// nothing (both sides have the change).
    fn carry_over(
        &self,
        layers: &Layers,
        (new_view, new_base_tree): (ObjectId, ObjectId),
        union: &HashMap<Vec<u8>, Vec<usize>>,
        (edited, edited_new): (&HashSet<Vec<u8>>, &HashSet<Vec<u8>>),
        read_only: impl Fn(&[u8]) -> bool,
    ) -> Result<Carried> {
        let mut out = Vec::new();
        let base_tree = git::with_repo(|repo| git::layers::commit_tree(repo, layers.base))?;
        let applied = layers.applied.layers().unwrap_or_default();
        let paths: HashSet<&Vec<u8>> = (layers.union.keys())
            .chain(union.keys())
            .chain(&layers.edited)
            .chain(edited)
            .chain(&layers.edited_new)
            .chain(edited_new)
            .collect();
        for path in paths {
            let upper = self.upper_path(path);
            let Ok(meta) = fs::symlink_metadata(&upper) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let (old, new) = (content_at(layers.view, path)?, content_at(new_view, path)?);
            let ours = fs::read(&upper)?;
            let stays =
                union.contains_key(path) || edited.contains(path) || edited_new.contains(path);
            let is_union = layers.union.contains_key(path) || union.contains_key(path);
            // A file an editable layer changed that a see-only layer's change
            // now shows in (the layer made see-only, another added): the
            // upper copy, which wins whole and which git sees unfiltered,
            // keeps only the workspace's own lines, carried onto the new
            // base; the layers' show read-only below it if that is all.
            if layers.edited.contains(path) && !stays && read_only(path) {
                let own = self.own_only(path, base_tree, new_base_tree, &old, &ours)?;
                let below = content_at(new_base_tree, path)?;
                out.push((path.clone(), (own != below).then_some(own)));
                continue;
            }
            let merged = match (old == new, is_union) {
                (true, _) => Some(ours.clone()),
                (false, true) => self.carry_union(base_tree, path, &old, &ours, &new)?,
                (false, false) => {
                    let names: Vec<&str> = (layers.mixed.get(path).into_iter().flatten())
                        .filter_map(|&i| applied.layers.get(i).map(|l| l.branch.as_str()))
                        .collect();
                    let theirs = match names.is_empty() {
                        true => "the layers' newer work".to_string(),
                        false => format!("{}'s newer work", names.join(" + ")),
                    };
                    // Yours: the workspace's own changes, or its waiting
                    // edits, or both.
                    Some(git::layers::merge3_marked(&old, &ours, &new, "yours", &theirs).0)
                }
            };
            let new_exists = git::with_repo(|repo| {
                git::layers::lookup(repo, new_view, BStr::new(path.as_slice()))
            })?
            .is_some();
            match merged {
                Some(merged) if !stays && new_exists && merged == new => {
                    out.push((path.clone(), None));
                }
                Some(merged) if merged != ours => out.push((path.clone(), Some(merged))),
                Some(_) => {}
                None => {
                    return Err(Refusal::Changes.error(format!(
                        "{}: your changes to it depend on lines the layers would no longer \
                         show; commit them first, or keep the layers as they are",
                        path.to_str_lossy()
                    )));
                }
            }
        }
        Ok(out)
    }

    /// The workspace's own lines of upper copy `ours` of `path`, a file only
    /// editable layers changed when shown as `old` over base tree
    /// `base_tree`, carried onto base tree `new_base_tree` (see
    /// `carry_over`). Refused (the switch with it) if it has waiting edits,
    /// which can't stand without the layers' lines, or if its own changes
    /// can't be carried.
    fn own_only(
        &self,
        path: &[u8],
        base_tree: ObjectId,
        new_base_tree: ObjectId,
        old: &[u8],
        ours: &[u8],
    ) -> Result<Vec<u8>> {
        let before = content_at(base_tree, path)?;
        let split = filter::Edited {
            base: before.clone(),
            shown: old.to_vec(),
        }
        .split(ours);
        let refused = |why: &str| {
            Err(Refusal::Changes.error(format!(
                "{}: {why}; the layers stay as they are",
                path.to_str_lossy()
            )))
        };
        if split.waiting > 0 {
            return refused(
                "your waiting edits in it change an editable layer's uncommitted lines, which \
                 would show read-only now",
            );
        }
        let after = content_at(new_base_tree, path)?;
        if after == before {
            return Ok(split.own);
        }
        match git::merge_file(&self.tmp, &before, &split.own, &after)? {
            Some(own) => Ok(own),
            None => refused("your changes to it depend on lines that would no longer show"),
        }
    }

    /// A union file's upper copy `ours` carried from union `old` onto `new`
    /// (see `carry_over`): `None` if nothing keeps its own lines.
    fn carry_union(
        &self,
        base_tree: ObjectId,
        path: &[u8],
        old: &[u8],
        ours: &[u8],
        new: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        if let Some(merged) = git::merge_file(&self.tmp, old, ours, new)? {
            return Ok(Some(merged));
        }
        if let Some(merged) = git::layers::line_set(old, ours, new) {
            return Ok(Some(merged));
        }
        // As git's filter sees it: the workspace's own lines (clean with the
        // old union), with the new union's (smudge), if that keeps them all.
        let base = content_at(base_tree, path)?;
        let before = filter::Versions {
            base: base.clone(),
            union: old.to_vec(),
        };
        let after = filter::Versions {
            base,
            union: new.to_vec(),
        };
        let own = before.clean(ours);
        let merged = after.smudge(&own);
        Ok((after.clean(&merged) == own).then_some(merged))
    }

    /// Replaces upper file `path`'s content durably, keeping its mode.
    fn rewrite_upper(&self, path: &[u8], data: &[u8]) -> Result<()> {
        let target = self.upper_path(path);
        let mode = fs::metadata(&target)?.permissions().mode();
        let tmp = self.tmp.join(format!("carry-{}", std::process::id()));
        let file = fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut &file, data)?;
        file.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, &target)?;
        Ok(())
    }

    /// What git is to be told for the view in `layers` (see `GitPlan`).
    fn git_plan(&self, store: &Store, layers: &Layers) -> GitPlan {
        let mut plan = GitPlan {
            layers_on: layers.layers_on,
            base: layers.base,
            skipped: BTreeSet::new(),
            added: Vec::new(),
            theirs: layers.edited_new.iter().cloned().collect(),
            filtered: (layers.union.keys())
                .chain(&layers.edited)
                .cloned()
                .collect(),
        };
        for path in layers.mixed.keys() {
            // A file only editable layers changed, which the base has: git
            // sees it through the filter, its own lines only.
            if layers.edited.contains(path) {
                continue;
            }
            let in_base = store.entry_at(layers.base_tree, path);
            // A file only they have stays hidden while the workspace edits
            // it: every change to it waits until a layer commits it.
            let theirs = layers.writable.contains(path) && in_base.is_none();
            if !theirs && self.shadowed(layers, path) {
                continue;
            }
            match in_base {
                Some(Lower::File(_)) => {
                    plan.skipped.insert(path.clone());
                }
                _ => plan.added.push(path.clone()),
            }
        }
        plan.added.sort_unstable();
        plan
    }

    /// Has git see only the workspace's own changes with the layers in
    /// `view` shown (or none), as `plan` says, recording each step in
    /// `applied` (and on disk) as it is done: the layers' tracked whole files
    /// skip-worktree, the files they add excluded, and with union files, the
    /// filter and the guard on them and their index entries' size zeroed.
    /// What was applied before is given back where no longer shown. The
    /// errors' text is the reason the `.git` gives with git off.
    ///
    /// The order is what a crash leaves right: the index edits are recorded
    /// as done (a next switch gives back what was skipped), and the caller
    /// records the layers file last, so a switch cut short stays pending and
    /// is done again.
    fn tell_git(
        &self,
        plan: &GitPlan,
        applied: &mut ShownLayers,
        stored: &layers::Layers,
        view: Option<&layers::View>,
    ) -> Result<()> {
        let admin = &self.state.admin;
        if plan.layers_on {
            // Worktree config would make git apply the shared `core.bare`
            // to every worktree.
            anyhow::ensure!(!git::is_bare(), "a bare repository");
            anyhow::ensure!(
                !index_edit::sparse_checkout(admin)?,
                "sparse checkout is on"
            );
        }
        match (view, plan.filtered.is_empty()) {
            (Some(_), false) => self.filter_on(plan)?,
            _ => self.filter_off()?,
        }
        self.guard_to(plan, stored, view)?;
        self.index_to_plan(plan, applied)?;
        match plan.layers_on {
            true => worktree_config::set_excludes(admin, &plan.added)?,
            false => worktree_config::remove_excludes(admin)?,
        }
        self.pin_view(stored, view)?;
        applied.write(&self.state)?;
        Ok(())
    }

    /// `tell_git`'s index edits: skip-worktree on `plan`'s skipped paths (off
    /// on those no longer skipped), and on union paths, now or before, the
    /// recorded size and times zeroed: their content changed, and git takes
    /// a size other than the one recorded as a change without reading the
    /// file (cleaning it); a zero size makes it read the file, once, and
    /// record its stat data again. Recorded in `applied` once done.
    fn index_to_plan(&self, plan: &GitPlan, applied: &mut ShownLayers) -> Result<()> {
        let mut edits: HashMap<Vec<u8>, Edit> = HashMap::new();
        for path in applied.skipped.iter().chain(&plan.skipped) {
            let edit = Edit {
                skip_worktree: Some(plan.skipped.contains(path)),
                ..Default::default()
            };
            edits.insert(path.clone(), edit);
        }
        for path in applied.union.iter().chain(&plan.filtered) {
            let edit = Edit {
                skip_worktree: Some(plan.skipped.contains(path)),
                stat: Some(index_edit::Stat::default()),
                oid: None,
            };
            edits.insert(path.clone(), edit);
        }
        self.edit_index(&edits)?;
        applied.skipped = plan.skipped.clone();
        applied.union = plan.filtered.clone();
        applied.theirs = plan.theirs.clone();
        applied.write(&self.state)
    }

    /// `tell_git` with filtered files (union files, files only editable
    /// layers changed): the filter driver and its attributes on them
    /// (`worktree_config::set_filter`), answered by `gitfold filter-process`
    /// relaying to this workspace (`filter`).
    fn filter_on(&self, plan: &GitPlan) -> Result<()> {
        let admin = &self.state.admin;
        let exe = std::env::current_exe().context("finding the gitfold binary")?;
        let process = format!(
            "{} filter-process {} {}",
            worktree_config::shell_quote(exe.as_os_str().as_bytes()),
            worktree_config::shell_quote(self.state.common_dir.as_os_str().as_bytes()),
            worktree_config::shell_quote(self.state.id.as_bytes()),
        );
        let paths: Vec<&Vec<u8>> = plan.filtered.iter().collect();
        worktree_config::set_filter(admin, &driver(&self.state.id), &process, &paths)
    }

    /// `tell_git` without filtered files: no filter.
    fn filter_off(&self) -> Result<()> {
        worktree_config::remove_filter(&self.state.admin, &driver(&self.state.id))
    }

    /// `tell_git`'s guard (`git::guard`): whenever an editable or union
    /// layer shows uncommitted work (filtered files or not: a layer that
    /// only adds or deletes files has its guard too), what it is and the
    /// hooks that check it; else neither.
    fn guard_to(
        &self,
        plan: &GitPlan,
        stored: &layers::Layers,
        view: Option<&layers::View>,
    ) -> Result<()> {
        let admin = &self.state.admin;
        let guard = match view {
            Some(view) => guard_of(&self.state.branch, plan.base, stored, view)?,
            None => Default::default(),
        };
        if guard.is_empty() {
            worktree_config::remove_guard(admin)?;
            return git::guard::Guard::remove(admin);
        }
        let exe = std::env::current_exe().context("finding the gitfold binary")?;
        guard.write(admin)?;
        worktree_config::install_guard(admin, &exe)
    }

    /// Keeps the view's commits (the layers' snapshots) from `git gc` while
    /// layers show: `refs/gitfold-layers/<id>` at a view commit (tree M,
    /// parents the base and the snapshots); removed with the layers.
    fn pin_view(&self, stored: &layers::Layers, view: Option<&layers::View>) -> Result<()> {
        let common_dir = &self.state.common_dir;
        let pin = layers_ref(&self.state.id);
        let Some(view) = view else {
            return git::cmd::run(common_dir, &["update-ref", "-d", &pin]);
        };
        let subject = stored.subject(&self.state.branch);
        let snapshots: Vec<ObjectId> = stored.layers.iter().map(|l| l.snapshot).collect();
        let base = self.layers.read().base;
        let vc = git::with_repo(|repo| {
            git::layers::view_commit(repo, view.tree, base, &snapshots, &subject)
        })?;
        git::cmd::run(common_dir, &["update-ref", &pin, &vc.to_string()])
    }

    /// Answers git's filter (`git::filter`) for `path`'s `content`, from the
    /// view as it is now: a union file's own lines (clean) or all of them
    /// (smudge); any other path's content as it is (its attribute is left
    /// over from an earlier view). Clean gives back exactly what smudge was
    /// given for content it smudged (remembered, `MEMO_MAX` of them, by path,
    /// until the view changes), so a checkout's files are clean in git's
    /// eyes, but only while git's index has that version of the file: once
    /// it has another (`git reset <path>` after `git checkout <rev> --
    /// <path>`), the content is cleaned as any other.
    pub fn filter(&self, op: filter::Op, path: &[u8], content: Vec<u8>) -> Result<Vec<u8>> {
        let (base, hunk, view, edited) = {
            let layers = self.layers.read();
            let edited = layers.edited.contains(path);
            if !layers.union.contains_key(path) && !edited {
                return Ok(content);
            }
            (layers.base, layers.hunk, layers.view, edited)
        };
        let base_tree = git::with_repo(|repo| git::layers::commit_tree(repo, base))?;
        if edited {
            return self.filter_edited(op, path, content, base_tree, view);
        }
        let versions = filter::Versions {
            base: content_at(base_tree, path)?,
            union: content_at(hunk, path)?,
        };
        match op {
            filter::Op::Smudge => {
                let out = versions.smudge(&content);
                self.remember(path, &out, content, None);
                Ok(out)
            }
            filter::Op::Clean => match self.remembered(path, &content) {
                Some(smudged) => Ok(smudged.own),
                None => Ok(versions.clean(&content)),
            },
        }
    }

    /// `filter` for a file only editable layers changed, on base tree
    /// `base_tree`, with view tree `view`: clean strips the layers' lines and
    /// the waiting edits; smudge puts them back, the waiting edits as the
    /// file has them now (its upper copy, else as shown; as they were before
    /// a smudge of this content, if it is one).
    ///
    /// A file with a conflict of the workspace's changes and the layers'
    /// (git's markers, `Edited::conflicted`) is git's version as it is
    /// (the index's) to clean until the conflict is resolved: nothing of it
    /// reaches git, markers included, and git sees no change it didn't
    /// make. Smudge leaves it as it is when git writes that same version
    /// back (`stash`, `reset --hard` to it); another version (a checkout of
    /// another commit) is merged into the workspace's side of the conflict.
    fn filter_edited(
        &self,
        op: filter::Op,
        path: &[u8],
        content: Vec<u8>,
        base_tree: ObjectId,
        view: ObjectId,
    ) -> Result<Vec<u8>> {
        let tip = git::branch_tip(&self.state.branch).map_or(base_tree, |c| c.tree);
        let edited = filter::Edited::on_tip(
            content_at(base_tree, path)?,
            content_at(view, path)?,
            content_at(tip, path)?,
        );
        match op {
            filter::Op::Smudge => {
                let mut now = match fs::read(self.upper_path(path)) {
                    Ok(now) => now,
                    Err(e) if crate::fsutil::not_found(&e) => edited.shown.clone(),
                    Err(e) => return Err(e.into()),
                };
                let earlier = self.remembered(path, &now).and_then(|s| s.kept);
                if earlier.is_none() && edited.conflicted(&now) {
                    if self.index_has(path, &content) {
                        return Ok(now);
                    }
                    now = filter::Edited::first_sides(&now);
                }
                let out = edited.smudge(&content, &now, earlier.as_deref());
                if edited.conflicted(&out) {
                    log::warn!(
                        "{}: {}: git's version conflicts with the layers' lines or your waiting \
                         edits: the file shows git's conflict markers",
                        self.state.branch,
                        path.to_str_lossy()
                    );
                }
                let kept = match earlier {
                    Some(kept) => kept,
                    None => edited.split(&now).kept,
                };
                self.remember(path, &out, content, Some(kept));
                Ok(out)
            }
            filter::Op::Clean => {
                if let Some(smudged) = self.remembered(path, &content) {
                    return Ok(smudged.own);
                }
                if edited.conflicted(&content)
                    && let Some(oid) = self.index_oid(path)
                {
                    return git::read_blob(oid);
                }
                Ok(edited.clean(&content))
            }
        }
    }

    /// Remembers that smudge gave `out` at `path` for `own`, keeping the
    /// waiting edits `kept` (see `filter`).
    fn remember(&self, path: &[u8], out: &[u8], own: Vec<u8>, kept: Option<Vec<u8>>) {
        let mut memo = self.filter_memo.lock();
        if memo.len() >= MEMO_MAX {
            memo.clear();
        }
        let key = (path.to_vec(), sha1::Sha1::digest(out).into());
        memo.insert(key, Smudged { own, kept });
    }

    /// What smudge was given at `path` for content it gave `out` for, if
    /// remembered and git's index still has it there.
    fn remembered(&self, path: &[u8], out: &[u8]) -> Option<Smudged> {
        let key = (path.to_vec(), sha1::Sha1::digest(out).into());
        let smudged = self.filter_memo.lock().get(&key).cloned()?;
        self.index_has(path, &smudged.own).then_some(smudged)
    }

    /// Whether git's index has `content` at `path`.
    fn index_has(&self, path: &[u8], content: &[u8]) -> bool {
        let Some(oid) = self.index_oid(path) else {
            return false;
        };
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, content)
            .is_ok_and(|id| id == oid)
    }

    /// The object id of `path`'s entry in git's index (a filtered path), if
    /// it has one; read once per change of the index file.
    fn index_oid(&self, path: &[u8]) -> Option<ObjectId> {
        let index = self.state.index();
        let meta = fs::metadata(&index).ok()?;
        let stamp = (meta.modified().ok()?, meta.len(), meta.ino());
        let mut cached = self.index_oids.lock();
        if let Some(c) = cached.as_ref()
            && c.stamp == stamp
            && c.paths.contains(path)
        {
            return c.oids.get(path).copied();
        }
        let mut paths: HashSet<Vec<u8>> = {
            let layers = self.layers.read();
            layers.union.keys().chain(&layers.edited).cloned().collect()
        };
        paths.insert(path.to_vec());
        let data = fs::read(&index).ok()?;
        let oids = match index_edit::oids_of(&data, &paths) {
            Ok(oids) => oids,
            Err(e) => {
                log::warn!("{}: reading the index: {e:#}", self.state.branch);
                return None;
            }
        };
        let oid = oids.get(path).copied();
        *cached = Some(IndexOids { stamp, paths, oids });
        oid
    }

    /// Edits the worktree's index, waiting up to `INDEX_WAIT` for git to
    /// release it.
    fn edit_index(&self, edits: &HashMap<Vec<u8>, Edit>) -> Result<()> {
        let started = Instant::now();
        loop {
            match index_edit::edit(&self.state.index(), edits) {
                Err(e)
                    if matches!(e.downcast_ref(), Some(index_edit::Refused::Locked(_)))
                        && started.elapsed() < INDEX_WAIT =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                done => return done.map(drop),
            }
        }
    }

    /// The sync thread's part while layers show or are pending (`rebase`):
    /// applies stored layers that differ from those applied (a switch that
    /// failed half-way, a change made while closed), retrying a refused one
    /// every `RETRY_AFTER`, then `rehide`.
    pub(super) fn sync_layers(&self, store: &Store) -> Result<()> {
        if self.layers_pending() {
            let stored = layers::shown_bytes(&self.state).unwrap_or_default();
            let retry = {
                let refused = self.layers_refused.lock();
                refused
                    .as_ref()
                    .is_none_or(|(bytes, at)| *bytes != stored || at.elapsed() > RETRY_AFTER)
            };
            if retry {
                match self.switch(store) {
                    Ok(done) => {
                        log::info!("{done}");
                        *self.layers_refused.lock() = None;
                    }
                    Err(e) => {
                        let mut refused = self.layers_refused.lock();
                        if refused.as_ref().is_none_or(|(bytes, _)| *bytes != stored) {
                            log::warn!("{}: its layers not switched yet: {e:#}", self.state.branch);
                        }
                        *refused = Some((stored, Instant::now()));
                    }
                }
            }
        }
        let _layers = self.layers_lock.lock();
        self.rehide()
    }

    /// Puts back the skip-worktree bits git dropped from the layers' files
    /// (`git read-tree`, `update-index --no-skip-worktree` drop them; then
    /// `git commit -a` would commit those files as the workspace's own).
    /// Looked at when the index changed since the last look, while git is
    /// idle; a locked index waits for the next pass.
    fn rehide(&self) -> Result<()> {
        let skipped = self.layers.read().applied.skipped.clone();
        if skipped.is_empty() {
            return Ok(());
        }
        let (admin, branch) = (&self.state.admin, &self.state.branch);
        if worktree::git_running(&self.state.common_dir, admin, branch).is_some() {
            return Ok(());
        }
        let mtime = fs::metadata(self.state.index())
            .and_then(|m| m.modified())
            .ok();
        let mut checked = self.index_checked.lock();
        if mtime.is_none() || *checked == mtime {
            return Ok(());
        }
        let edits = (skipped.into_iter())
            .map(|p| {
                let edit = Edit {
                    skip_worktree: Some(true),
                    ..Default::default()
                };
                (p, edit)
            })
            .collect();
        match index_edit::edit(&self.state.index(), &edits) {
            Ok(edited) => {
                if edited.changed > 0 {
                    log::warn!(
                        "{branch}: git had dropped the skip-worktree bit of {} of the layers' files; put back",
                        edited.changed
                    );
                }
                *checked = fs::metadata(self.state.index())
                    .and_then(|m| m.modified())
                    .ok();
            }
            Err(e) if matches!(e.downcast_ref(), Some(index_edit::Refused::Locked(_))) => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }
    /// What changed in the view between lower trees `old` and `new` (the
    /// file `changes` between them), as the kernel holds it; stamps the
    /// changed files and forgets nodes of entries gone or changed in type.
    fn stale(
        &self,
        store: &Store,
        layers: &mut Layers,
        old: u32,
        new: u32,
        changes: &[(BString, git::layers::Change)],
    ) -> Stale {
        let stamp = {
            let now = SystemTime::now();
            Stamp {
                mtime: now,
                ctime: now,
            }
        };
        // 0 none, 1 a directory, 2 a file, 3 a symlink: a change between
        // them is a new entry (the kernel can't see an inode change type).
        let kind = |tree: u32, path: &[u8]| match store.entry_at(tree, path) {
            None => 0,
            Some(Lower::Tree(_) | Lower::Gitlink) => 1,
            Some(Lower::File(id)) => match store.file(id).map(|f| f.kind) {
                Some(EntryKind::Link) => 3,
                _ => 2,
            },
        };
        let mut stale = Stale::default();
        let mut entries = HashSet::new();
        let mut nodes = self.nodes.lock();
        for (path, _) in changes {
            let path: &[u8] = path.as_ref();
            if self.shadowed(layers, path) {
                continue;
            }
            // The highest path that appeared, vanished or changed type is an
            // entry change in its directory; below it, nothing was cached.
            let changed_at = ancestors(path)
                .chain(std::iter::once(path))
                .find(|p| kind(old, p) != kind(new, p));
            match changed_at {
                Some(at) if self.shadowed(layers, at) => {}
                Some(at) => {
                    if kind(new, at) >= 2 {
                        layers.times.set(at, Some(stamp));
                    }
                    if entries.insert(at.to_vec()) {
                        let (dir, name) = split(at);
                        if let Some(parent) = nodes.held(dir) {
                            nodes.orphan(parent, name);
                            stale.entries.push((parent, name.to_vec()));
                            stale.inodes.push(parent);
                        }
                    }
                }
                None => {
                    layers.times.set(path, Some(stamp));
                    if let Some(local) = nodes.held(path) {
                        stale.inodes.push(local);
                    }
                }
            }
        }
        stale.inodes.push(GITFILE_LOCAL);
        stale.inodes.sort_unstable();
        stale.inodes.dedup();
        stale
    }

    /// Whether the workspace's own changes hide `path` from the lower layer:
    /// an upper entry there (or a file where an ancestor would be), or a
    /// whiteout or opaque directory over it.
    fn shadowed(&self, layers: &Layers, path: &[u8]) -> bool {
        Self::lower_hidden(layers, path)
            || match fs::symlink_metadata(self.upper_path(path)) {
                Ok(_) => true,
                Err(e) => e.raw_os_error() == Some(libc::ENOTDIR),
            }
    }

    /// Whether `path` is a see-only layer's whole file (or its deletion):
    /// read-only. One only editable layers changed is writable.
    pub(super) fn is_mixed(layers: &Layers, path: &[u8]) -> bool {
        layers.mixed.contains_key(path) && !layers.writable.contains(path)
    }

    /// Whether any path below directory `path` is a layer's: a whole file or
    /// deletion, or a union file (whose upper copy holds the layers' lines:
    /// not to be moved).
    pub(super) fn has_mixed_below(layers: &Layers, path: &[u8]) -> bool {
        if layers.mixed.is_empty() && layers.union.is_empty() {
            return false;
        }
        let prefix = [path, b"/"].concat();
        path.is_empty()
            || (layers.mixed.keys().chain(layers.union.keys())).any(|p| p.starts_with(&prefix))
    }
}

/// What a switch of `branch`'s layers says it did: whether layers show (and
/// how many paths changed), why git is off if it is, and the view's notes.
fn switched<'a>(
    branch: &str,
    layers_on: bool,
    changed: usize,
    git_off: Option<&str>,
    notes: impl Iterator<Item = &'a String>,
) -> String {
    let mut done = match layers_on {
        true => format!(
            "{branch}: layers shown ({})",
            crate::cli::plural(changed, "path changed", "paths changed")
        ),
        false => format!("{branch}: no layers shown"),
    };
    if let Some(why) = git_off {
        done += &format!("\nnote: git is off in it: {why}");
    }
    for note in notes {
        done += &format!("\nnote: {note}");
    }
    done
}

/// What the guard keeps from `branch` (`git::guard`) with `view` of
/// `stored` over `base`: at each union file, and each path only editable
/// layers changed (their changes to the base's files, the files they add
/// and those they delete), each of those layers' uncommitted work there:
/// its version in its snapshot of a workspace's work, the lines it adds
/// that neither the commit the snapshot is on nor the base has, and the
/// base's lines that commit has and the snapshot doesn't (compared as
/// `git::guard::norm` does).
fn guard_of(
    branch: &str,
    base: ObjectId,
    stored: &layers::Layers,
    view: &layers::View,
) -> Result<git::guard::Guard> {
    use git::guard::{Entry, norm, significant};
    let mut guard = git::guard::Guard {
        branch: branch.to_string(),
        ..Default::default()
    };
    let base_tree = git::with_repo(|repo| git::layers::commit_tree(repo, base))?;
    let mut paths: Vec<(&Vec<u8>, &Vec<usize>)> = view.union.iter().collect();
    paths.extend((view.writable.iter()).filter_map(|p| Some((p, view.mixed.get(p)?))));
    let blob_at = |tree: ObjectId, path: &[u8]| -> Result<Option<ObjectId>> {
        let item = git::with_repo(|repo| git::layers::lookup(repo, tree, BStr::new(path)))?;
        Ok(item
            .filter(|i| matches!(i.kind, EntryKind::Blob | EntryKind::BlobExecutable))
            .map(|i| i.oid))
    };
    let read = |blob: Option<ObjectId>| blob.map_or(Ok(Vec::new()), git::read_blob);
    let mut trees: HashMap<ObjectId, (ObjectId, Option<ObjectId>)> = HashMap::new();
    for (path, owners) in paths {
        let at_base = read(blob_at(base_tree, path)?)?;
        let base_lines: HashSet<Vec<u8>> = git::layers::lines(&at_base)
            .map(|l| norm(git::layers::key(l)))
            .collect();
        let mut entries = Vec::new();
        for &ix in owners {
            let layer = &stored.layers[ix];
            if layer.shows != layers::Shows::Work {
                continue;
            }
            let (snapshot, committed) = match trees.get(&layer.snapshot) {
                Some(&t) => t,
                None => {
                    let t = git::with_repo(|repo| -> Result<_> {
                        let commit = repo.find_commit(layer.snapshot)?;
                        let parent = commit.parent_ids().next().map(|p| p.detach());
                        Ok((
                            git::layers::commit_tree(repo, layer.snapshot)?,
                            parent
                                .map(|p| git::layers::commit_tree(repo, p))
                                .transpose()?,
                        ))
                    })?;
                    trees.insert(layer.snapshot, t);
                    t
                }
            };
            let theirs = blob_at(snapshot, path)?;
            let before = match committed {
                Some(tree) => blob_at(tree, path)?,
                None => None,
            };
            if theirs == before {
                continue;
            }
            let (theirs_data, before_data) = (read(theirs)?, read(before)?);
            let before_lines: HashSet<Vec<u8>> = git::layers::lines(&before_data)
                .map(|l| norm(git::layers::key(l)))
                .collect();
            let theirs_lines: HashSet<Vec<u8>> = git::layers::lines(&theirs_data)
                .map(|l| norm(git::layers::key(l)))
                .collect();
            let mut seen = HashSet::new();
            let mut entry = Entry {
                layer: layer.branch.clone(),
                blob: theirs,
                ..Default::default()
            };
            for line in git::layers::lines(&theirs_data).map(git::layers::key) {
                let n = norm(line);
                if significant(line)
                    && !before_lines.contains(&n)
                    && !base_lines.contains(&n)
                    && seen.insert(n)
                {
                    entry.added.push(line.to_vec());
                }
            }
            if theirs.is_some() {
                for line in git::layers::lines(&at_base).map(git::layers::key) {
                    let n = norm(line);
                    if significant(line)
                        && before_lines.contains(&n)
                        && !theirs_lines.contains(&n)
                        && seen.insert(n)
                    {
                        entry.removed.push(line.to_vec());
                    }
                }
            }
            entries.push(entry);
        }
        if !entries.is_empty() {
            guard.paths.insert(path.clone(), entries);
        }
    }
    Ok(guard)
}

/// (directory, name) of `path`.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (b"", path),
    }
}
