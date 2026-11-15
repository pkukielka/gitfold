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
//!   lines on top of the layers'. HEAD is detached at a layers commit L whose
//!   tree has the unions (`git::layers`), and the index has the unions too, so
//!   `git diff` shows only the workspace's own lines. Commits made on L are
//!   extracted onto the branch by the sync thread (`sync_layers`), with only
//!   the workspace's changes; one that changes a layer's lines can't be, and
//!   waits. A push guard refuses pushing L. A switch carries the workspace's
//!   own lines in a union file over (a 3-way merge), or is refused if they
//!   depend on lines that would go.
//!
//! What was applied (the layers, what git was told) is recorded apart from
//! the layers stored (`state::layers::ShownLayers`), step by step as git is told,
//! the layers last: a workspace opens showing what was applied, so the view
//! and git agree, then switches to the stored layers if they differ (changed
//! while it was closed, or a switch that failed half-way). Before HEAD moves
//! to a layers commit, the commit is recorded as `next`, and adopted if HEAD is
//! found there. Switches and the sync thread take `layers_lock`, so they never
//! interleave; git is told after the workspace's own lock is released, so
//! git processes reading the workspace meanwhile don't wait on it.
//!
//! Where git can't be kept right (a bare repository, sparse checkout, a split
//! or sparse index, an index git keeps locked), git is turned off in the
//! workspace instead (git off): its `.git` names a directory that doesn't
//! exist, whose name says why, so every git command fails with it.
//!
//! Meanwhile the base doesn't follow the branch (`rebase`): moving it would
//! copy the layers' files into the workspace's own. Commits still work; the
//! base catches up once no layer shows, and before layers go on in a workspace
//! showing none (`catch_up_before_layers`). A move of the branch onto its
//! base takes the layers off for the move (`state::layers::OFF`: what shows is
//! then none, whatever is stored) and puts them back after.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use fuser::INodeNo;
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::objs::tree::EntryKind;

use super::times::Stamp;
use super::{GITFILE_LOCAL, Layers, Workspace};
use crate::git::index_edit::{self, Edit};
use crate::git::{self, worktree, worktree_config};
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

/// How far back from HEAD commits made on a layers commit are looked for.
const CHAIN_LIMIT: usize = 10_000;

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
    union: BTreeSet<Vec<u8>>,
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

/// The ref keeping workspace `id`'s view commit (and through it L and the
/// snapshots) from `git gc`.
fn layers_ref(id: &str) -> String {
    format!("refs/gitfold-layers/{id}")
}

/// The ref keeping the commit HEAD was at when it had left the layers commit's
/// line (amended, reset) from `git gc`, after the layers are removed.
fn left_ref(id: &str) -> String {
    format!("refs/gitfold-layers-left/{id}")
}

/// The commit a detached HEAD in the registration at `admin` is at.
fn detached_head(admin: &std::path::Path) -> Option<ObjectId> {
    let head = fs::read_to_string(admin.join("HEAD")).ok()?;
    ObjectId::from_hex(head.trim().as_bytes()).ok()
}

/// Whether HEAD in the registration at `admin` is on `branch`.
fn head_on(admin: &std::path::Path, branch: &str) -> bool {
    fs::read_to_string(admin.join("HEAD"))
        .is_ok_and(|h| h.trim() == format!("ref: refs/heads/{branch}"))
}

/// The layers commit HEAD is at as far as `applied` knows: its `next`, adopted
/// if HEAD got there (a crash after moving it, before recording it).
fn adopt_next(state: &StateDir, applied: &mut ShownLayers) -> Result<()> {
    if let Some(next) = applied.next.take() {
        if detached_head(&state.admin) == Some(next) {
            applied.head = Some(next);
        }
        applied.write(state)?;
    }
    Ok(())
}

/// Sets up what was last applied of the layers of the workspace in `state`
/// (see the module doc) when it opens. Returns the `.git` content if git is
/// off.
pub(super) fn open_view(
    store: &Store,
    state: &StateDir,
    layers: &mut Layers,
) -> Result<Option<Vec<u8>>> {
    let mut applied = ShownLayers::read(state)?;
    adopt_next(state, &mut applied)?;
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
    /// Whether layers are shown.
    pub fn shows_layers(&self) -> bool {
        self.layers.read().layers_on
    }

    /// Whether the stored layers differ from those applied (a cheap read).
    pub(super) fn layers_pending(&self) -> bool {
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
    /// what its index describes), while a commit made on L can't be extracted
    /// yet, or if its own lines in a union file depend on lines that would
    /// go; then nothing changed.
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
        if let Some(busy) = worktree::git_running(&self.state.common_dir, &self.state.admin, branch)
        {
            return Err(Refusal::Changes.error(format!(
                "{branch}: git is busy ({busy}); switch its layers when it is done"
            )));
        }
        if self.layers.read().applied.head.is_some() {
            self.sync_layers_locked()?;
            if let Some(blocked) = self.extraction_blocked() {
                return Err(Refusal::Changes.error(format!(
                    "{branch}: {blocked}; switch its layers once it can go onto {branch} \
                     (or amend it so it doesn't change their lines)"
                )));
            }
        } else if let Some(busy) = self.state.git_busy() {
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
        let writable = view
            .as_ref()
            .map(|v| v.writable.clone())
            .unwrap_or_default();
        let new_tree = store.load_tree(new_view)?;
        let mut held = None;
        let (stale, mut failed, plan, applied, changed) = {
            let mut layers = self.layers.write();
            // Under the write lock: no write to a union file can land
            // between reading it and replacing it.
            let carried = self.carry_over(&layers, layers.hunk, new_hunk, &union)?;
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
            layers.union = union;
            layers.view_tree = new_tree;
            layers.view = new_view;
            layers.hunk = new_hunk;
            layers.layers_on = stored.is_on();
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
                    if let Err(e) = self.rewrite_upper(path, data) {
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
            && (plan.layers_on || applied.head.is_some() || !applied.skipped.is_empty());
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
    /// soon, since the base waits while layers show. After a move of the
    /// branch onto its base (`cli::layer::moves`, which takes the layers off for
    /// it), the files the move wrote are then the new base's, not the
    /// workspace's own, which would hide the layers' at those paths. Fails
    /// (the switch refused, retried) if it can't catch up now, and while a
    /// move of the branch is under way or was interrupted (`state::moves`):
    /// the base follows the branch only once that is finished or undone.
    fn catch_up_before_layers(&self, store: &Store, stored: &layers::Layers) -> Result<()> {
        let (none_shown, base) = {
            let layers = self.layers.read();
            (
                !layers.layers_on && layers.applied.head.is_none(),
                layers.base,
            )
        };
        if !stored.is_on() || !none_shown {
            return Ok(());
        }
        if crate::state::moves::pending(&self.state) {
            return Err(Refusal::Changes.error(format!(
                "{} is being moved onto its base; its layers show once the move is done",
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

    /// The new content of the upper files at union paths (union before or
    /// now) when the unions change from tree `old_hunk` to `new_hunk`: the
    /// workspace's own lines merged onto the new union. Refused if any
    /// conflicts.
    fn carry_over(
        &self,
        layers: &Layers,
        old_hunk: ObjectId,
        new_hunk: ObjectId,
        union: &HashMap<Vec<u8>, Vec<usize>>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        if old_hunk == new_hunk {
            return Ok(out);
        }
        let paths: HashSet<&Vec<u8>> = layers.union.keys().chain(union.keys()).collect();
        for path in paths {
            let upper = self.upper_path(path);
            let Ok(meta) = fs::symlink_metadata(&upper) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let (old, new) = (content_at(old_hunk, path)?, content_at(new_hunk, path)?);
            if old == new {
                continue;
            }
            let ours = fs::read(&upper)?;
            let merged = git::merge_file(&self.tmp, &old, &ours, &new)?;
            match merged.or_else(|| git::layers::line_set(&old, &ours, &new)) {
                Some(merged) if merged != ours => out.push((path.clone(), merged)),
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
            union: layers.union.keys().cloned().collect(),
        };
        for path in layers.mixed.keys() {
            if self.shadowed(layers, path) {
                continue;
            }
            match store.entry_at(layers.base_tree, path) {
                // A file only editable layers changed is writable: git sees
                // it whole, so what is written to it can be committed.
                Some(Lower::File(_)) if layers.writable.contains(path) => {}
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
    /// skip-worktree, the files they add excluded, and with union files,
    /// HEAD at a layers commit and the unions in the index. What was applied
    /// before is given back where no longer shown. The errors' text is the
    /// reason the `.git` gives with git off.
    ///
    /// The order is what a crash leaves right: the index edits are recorded
    /// before HEAD moves (a next switch gives back what was skipped), the
    /// layers commit is recorded as `next` before HEAD moves to it (adopted if
    /// HEAD got there), and the caller records the layers file last, so a
    /// switch cut short stays pending and is done again.
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
        // What the branch tip has: the base, or past it by commits made with
        // layers shown (the base waits for the layers to go).
        let tip = git::branch_tip(&self.state.branch)?;
        // Union files: the index has the union over the tip (what L has),
        // or the tip's version again.
        let hunk = match (view, plan.union.is_empty()) {
            (Some(v), false) if tip.id != plan.base => {
                let at_tip = layers::view(&self.state.common_dir, &tip.id, stored)?;
                at_tip.map_or(v.hunk_tree, |t| t.hunk_tree)
            }
            (Some(v), false) => v.hunk_tree,
            _ => tip.tree,
        };
        self.index_to_plan(plan, applied, hunk, tip.tree)?;
        match plan.layers_on {
            true => worktree_config::set_excludes(admin, &plan.added)?,
            false => worktree_config::remove_excludes(admin)?,
        }
        // HEAD: at a layers commit while files are union-merged, else on the branch.
        match (view, plan.union.is_empty()) {
            (Some(v), false) => self.head_to_layers(applied, stored, v, hunk, tip.id)?,
            _ => self.head_to_branch(applied)?,
        }
        applied.write(&self.state)?;
        Ok(())
    }

    /// `tell_git`'s index edits: skip-worktree on `plan`'s skipped paths (off
    /// on those no longer skipped), and union paths' entries the union in
    /// tree `hunk` (or `tip_tree`'s version, no longer union). Recorded in
    /// `applied` once done.
    fn index_to_plan(
        &self,
        plan: &GitPlan,
        applied: &mut ShownLayers,
        hunk: ObjectId,
        tip_tree: ObjectId,
    ) -> Result<()> {
        let mut edits: HashMap<Vec<u8>, Edit> = HashMap::new();
        for path in applied.skipped.iter().chain(&plan.skipped) {
            let edit = Edit {
                skip_worktree: Some(plan.skipped.contains(path)),
                ..Default::default()
            };
            edits.insert(path.clone(), edit);
        }
        for path in applied.union.iter().chain(&plan.union) {
            let tree = match plan.union.contains(path) {
                true => hunk,
                false => tip_tree,
            };
            let item = git::with_repo(|repo| git::layers::lookup(repo, tree, BStr::new(path)))?;
            if let Some(item) = item {
                // The size git compares first is the blob's; zero times make
                // it read the file once to be sure (a recorded size that
                // differs would be taken as a change unread).
                let size = git::object_size(item.oid)?.unwrap_or_default() as u32;
                let edit = Edit {
                    skip_worktree: Some(false),
                    stat: Some(index_edit::Stat {
                        size,
                        ..Default::default()
                    }),
                    oid: Some(item.oid),
                };
                edits.insert(path.clone(), edit);
            }
        }
        self.edit_index(&edits)?;
        applied.skipped = plan.skipped.clone();
        applied.union = plan.union.clone();
        applied.write(&self.state)
    }

    /// `tell_git`'s HEAD with union files: detached at a new layers commit L
    /// (tree `hunk`, over branch tip `tip`), with the view commit pinned and
    /// the push guard installed. Moved only from where it was left
    /// (compare-and-swap): the branch, or the layers commit `applied` has.
    fn head_to_layers(
        &self,
        applied: &mut ShownLayers,
        stored: &layers::Layers,
        view: &layers::View,
        hunk: ObjectId,
        tip: ObjectId,
    ) -> Result<()> {
        let (admin, branch) = (&self.state.admin, &self.state.branch);
        let subject = stored.subject(branch);
        let all_snapshots: Vec<ObjectId> = stored.layers.iter().map(|l| l.snapshot).collect();
        let (l, vc) = git::with_repo(|repo| -> Result<(ObjectId, ObjectId)> {
            let l = git::layers::layers_commit(repo, hunk, tip, &view.hunk_snapshots, &subject)?;
            let vc = git::layers::view_commit(repo, view.tree, l, &all_snapshots, &subject)?;
            Ok((l, vc))
        })?;
        let pin = layers_ref(&self.state.id);
        git::cmd::run(
            &self.state.common_dir,
            &["update-ref", &pin, &vc.to_string()],
        )?;
        let old = match (detached_head(admin), applied.head) {
            (Some(h), Some(l)) if h == l => l.to_string(),
            (None, _) if head_on(admin, branch) => tip.to_string(),
            _ => anyhow::bail!("HEAD isn't where gitfold left it"),
        };
        // Recorded before HEAD moves: a crash in between leaves it adopted.
        applied.next = Some(l);
        applied.write(&self.state)?;
        let (msg, new) = ("gitfold: layers", l.to_string());
        git::cmd::run(
            admin,
            &["update-ref", "--no-deref", "-m", msg, "HEAD", &new, &old],
        )?;
        worktree_config::install_push_guard(admin)?;
        (applied.head, applied.next) = (Some(l), None);
        Ok(())
    }

    /// `tell_git`'s HEAD without union files: back on the branch if it was
    /// at a layers commit (a commit it had moved to off L's line kept by a
    /// ref), the push guard and the view commit's pin gone.
    fn head_to_branch(&self, applied: &mut ShownLayers) -> Result<()> {
        let Some(l) = applied.head else {
            return Ok(());
        };
        let (admin, branch) = (&self.state.admin, &self.state.branch);
        let common_dir = &self.state.common_dir;
        let head = detached_head(admin);
        if let Some(h) = head.filter(|&h| h != l) {
            // Left L's line (amended, reset): kept, and said so.
            let left = left_ref(&self.state.id);
            git::cmd::run(common_dir, &["update-ref", &left, &h.to_string()])?;
            log::warn!("{branch}: HEAD had left the layers commit; {h} is kept at {left}");
        }
        if head.is_some() {
            let full = format!("refs/heads/{branch}");
            let msg = "gitfold: layers removed";
            git::cmd::run(admin, &["symbolic-ref", "-m", msg, "HEAD", &full])?;
        }
        worktree_config::remove_push_guard(admin)?;
        git::cmd::run(
            common_dir,
            &["update-ref", "-d", &layers_ref(&self.state.id)],
        )?;
        applied.head = None;
        Ok(())
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

    /// Why a commit made on the layers commit can't go onto the branch yet, if
    /// one can't: HEAD (detached) isn't L after `sync_layers`, and L is on its
    /// first-parent line.
    fn extraction_blocked(&self) -> Option<String> {
        let l = self.layers.read().applied.head?;
        let head = detached_head(&self.state.admin)?;
        if head == l {
            return None;
        }
        let on_line = git::with_repo(|repo| git::layers::chain(repo, l, head, CHAIN_LIMIT))
            .ok()
            .flatten()
            .is_some();
        on_line.then(|| {
            format!(
                "commit {} changes lines of a layer's, so it can't go onto the branch alone",
                &head.to_string()[..12]
            )
        })
    }

    /// The sync thread's part while layers show or are pending (`rebase`):
    /// applies stored layers that differ from those applied (a switch that
    /// failed half-way, a change made while closed), retrying a refused one
    /// every `RETRY_AFTER`, then `sync_layers_locked`.
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
        self.rehide()?;
        self.sync_layers_locked()
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

    /// Commits made on the layers commit are extracted onto the branch
    /// (`git::layers::extract`), and HEAD moves to the new layers commit (the tree
    /// of the last one, so nothing git sees changes). Git must be idle (but
    /// for HEAD being detached). An amend of L is extracted as a commit on L.
    /// If HEAD left L's line (git switch, reset), git is turned off: the
    /// index holds the unions, which mustn't be committed on the branch;
    /// `gitfold layer remove --all` gives it back.
    fn sync_layers_locked(&self) -> Result<()> {
        let Some(l) = self.layers.read().applied.head else {
            return Ok(());
        };
        let (admin, branch) = (&self.state.admin, &self.state.branch);
        if worktree::git_running(&self.state.common_dir, admin, branch).is_some() {
            return Ok(());
        }
        let Some(head) = detached_head(admin) else {
            return self.git_off("HEAD left the layers commit");
        };
        if head == l || *self.layers_blocked.lock() == Some(head) {
            return Ok(());
        }
        let stored = self.layers.read().applied.layers()?;
        let tip = git::branch_tip(branch)?;
        let Some(view) = layers::view(&self.state.common_dir, &tip.id, &stored)? else {
            return Ok(());
        };
        let subject = stored.subject(branch);
        let union = &view.union;
        let owners =
            |p: &BStr| -> Vec<usize> { union.get(p.as_bytes()).cloned().unwrap_or_default() };
        let is_hunk = |p: &BStr| union.contains_key(p.as_bytes());
        let done = git::with_repo(|repo| -> Result<Option<git::layers::Extraction>> {
            let chain = match git::layers::chain(repo, l, head, CHAIN_LIMIT)? {
                Some(chain) => chain,
                // An amend of L: a commit on L's first parent instead.
                None if first_parent(repo, head)? == first_parent(repo, l)? => vec![head],
                None => return Ok(None),
            };
            let layers = git::layers::Extractor {
                snapshots: &view.hunk_snapshots,
                subject: &subject,
                is_hunk: &is_hunk,
                owners: &owners,
            };
            Ok(Some(git::layers::extract(
                repo, &layers, l, tip.id, &chain,
            )?))
        })?;
        let Some(done) = done else {
            return self.git_off("HEAD left the layers commit's line");
        };
        if let Some(blocked) = &done.blocked {
            let paths: Vec<String> = (blocked.conflicts.iter())
                .map(|c| c.path.to_str_lossy().into_owned())
                .collect();
            log::warn!(
                "{branch}: commit {} changes layers' lines ({}): it waits until amended",
                blocked.commit,
                paths.join(", ")
            );
            *self.layers_blocked.lock() = Some(head);
        }
        if let Some(new_l) = done.layers {
            let mut applied = self.layers.read().applied.clone();
            applied.next = Some(new_l);
            applied.write(&self.state)?;
            git::layers::advance(admin, branch, (tip.id, done.tip), (head, new_l))?;
            (applied.head, applied.next) = (Some(new_l), None);
            applied.write(&self.state)?;
            self.layers.write().applied = applied;
            *self.layers_blocked.lock() = None;
            log::info!(
                "{branch}: {} commit(s) made on the layers commit moved onto the branch",
                done.extracted.len()
            );
        }
        Ok(())
    }

    /// Turns git off in it, saying why, and tells the kernel.
    fn git_off(&self, why: &str) -> Result<()> {
        let stored = self.layers.read().applied.layers()?;
        if self.gitfile.read().starts_with(&off_gitfile(&stored, why)) {
            return Ok(());
        }
        self.set_gitfile(&stored, Some(why))
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

/// A commit's first parent, if any.
fn first_parent(repo: &gix::Repository, commit: ObjectId) -> Result<Option<ObjectId>> {
    Ok(repo
        .find_commit(commit)?
        .parent_ids()
        .next()
        .map(|p| p.detach()))
}

/// (directory, name) of `path`.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (b"", path),
    }
}
