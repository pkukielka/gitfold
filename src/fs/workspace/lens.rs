//! Layers in an open workspace: other workspaces' work shown over its base,
//! read-only, under its own changes (see `state::lens` for the model and its
//! storage).
//!
//! The lower layer becomes the view tree; everything that reads the lower
//! layer reads it. A switch (a layer shown, hidden, or moved to a newer seal)
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
//! - Blended files (`state::lens::view`): the workspace writes them, its own
//!   lines on top of the layers'. HEAD is detached at a lens commit L whose
//!   tree has the blends (`git::lens`), and the index has the blends too, so
//!   `git diff` shows only the workspace's own lines. Commits made on L are
//!   extracted onto the branch by the sync thread (`sync_lens`), with only
//!   the workspace's changes; one that changes a layer's lines can't be, and
//!   waits. A push guard refuses pushing L. A switch carries the workspace's
//!   own lines in a blended file over (a 3-way merge), or is refused if they
//!   depend on lines that would go.
//!
//! What was applied (the layers, what git was told) is recorded apart from
//! the layers stored (`state::lens::Applied`): a workspace opens showing what
//! was applied, so the view and git agree, then switches to the stored
//! layers if they differ (changed while it was closed, or a switch that
//! failed half-way). Switches and the sync thread take `lens_lock`, so they
//! never interleave.
//!
//! Where git can't be kept right (a bare repository, sparse checkout, a split
//! or sparse index, an index git keeps locked), git is turned off in the
//! workspace instead (surrendered): its `.git` names a directory that doesn't
//! exist, whose name says why, so every git command fails with it.
//!
//! Meanwhile the base doesn't follow the branch (`rebase`): moving it would
//! copy the layers' files into the workspace's own. Commits still work; the
//! base catches up once no layer shows.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use fuser::INodeNo;
use gix::ObjectId;
use gix::bstr::{BStr, ByteSlice};
use gix::objs::tree::EntryKind;

use super::times::Stamp;
use super::{GITFILE_LOCAL, Layers, Workspace};
use crate::git::index_edit::{self, Edit};
use crate::git::{self, worktree, worktree_config};
use crate::inode;
use crate::refusal::Refusal;
use crate::state::lens::{self, Applied, Lens};
use crate::state::{StateDir, ancestors};
use crate::store::{Lower, Store};

/// A file in the state directory whose presence means git is surrendered
/// (see the module doc): its content is why.
const GIT_OFF: &str = "lens-git-off";

/// How long a switch waits for git to release the index.
const INDEX_WAIT: Duration = Duration::from_secs(3);

/// What the kernel must be told after a switch (local ids of the workspace).
#[derive(Default)]
struct Stale {
    /// Nodes whose attributes and pages changed (directories: their listing).
    inodes: Vec<u64>,
    /// Names added, removed or changed in type, by their directory's node.
    entries: Vec<(u64, Vec<u8>)>,
}

/// The `.git` content while git is surrendered: git prints the path it names.
fn surrendered(lens: &Lens, why: &str) -> Vec<u8> {
    let shown: Vec<&str> = (lens.layers.iter())
        .filter(|l| l.on)
        .map(|l| l.branch.as_str())
        .collect();
    format!(
        "gitdir: /LENS ON ({}): git is off here ({why}). Run gitfold lens off to use git\n",
        shown.join("+")
    )
    .into_bytes()
}

/// The registration's `.git` content.
fn registered(state: &StateDir) -> Vec<u8> {
    format!("gitdir: {}\n", state.admin.display()).into_bytes()
}

/// The ref keeping workspace `id`'s view commit (and through it L and the
/// seals) from `git gc`.
fn lens_ref(id: &str) -> String {
    format!("refs/gitfold-lens/{id}")
}

/// The ref keeping the commit HEAD was at when it had left the lens commit's
/// line (amended, reset) from `git gc`, after `lens off`.
fn left_ref(id: &str) -> String {
    format!("refs/gitfold-lens-left/{id}")
}

/// The commit a detached HEAD in the registration at `admin` is at.
fn detached_head(admin: &std::path::Path) -> Option<ObjectId> {
    let head = fs::read_to_string(admin.join("HEAD")).ok()?;
    ObjectId::from_hex(head.trim().as_bytes()).ok()
}

/// Sets up what was last applied of the layers of the workspace in `state`
/// (see the module doc) when it opens. Returns the `.git` content if git is
/// surrendered, and whether the stored layers differ (it then switches).
pub(super) fn open_view(
    store: &Store,
    state: &StateDir,
    layers: &mut Layers,
) -> Result<(Option<Vec<u8>>, bool)> {
    let applied = Applied::read(state)?;
    let pending = lens::stored_bytes(state)? != applied.lens;
    let shown = applied.layers()?;
    layers.lens_head = applied.head;
    layers.applied = applied;
    let view = match lens::view(&state.common_dir, &layers.base, &shown) {
        Ok(view) => view,
        // A layer's seal gone (pruned): open without the layers, git off;
        // the switch that follows reports it.
        Err(e) => {
            log::warn!("{}: its layers can't be shown: {e:#}", state.branch);
            return Ok((Some(surrendered(&shown, "its layers are missing")), true));
        }
    };
    let Some(view) = view else {
        return Ok((None, pending));
    };
    layers.view_tree = store.load_tree(view.tree)?;
    layers.view = view.tree;
    layers.hunk = view.hunk_tree;
    layers.mixed = view.mixed;
    layers.blended = view.blended;
    layers.lens_on = true;
    let off = fs::read_to_string(state.dir.join(GIT_OFF))
        .ok()
        .map(|why| surrendered(&shown, why.trim()));
    Ok((off, pending))
}

/// A blob's content at `path` in `tree` (empty if none).
fn content_at(tree: ObjectId, path: &[u8]) -> Result<Vec<u8>> {
    let item = git::with_repo(|repo| git::lens::lookup(repo, tree, BStr::new(path)))?;
    match item {
        Some(item) => git::read_blob(item.oid),
        None => Ok(Vec::new()),
    }
}

/// `git merge-file`: `ours` with the change from `base` to `theirs` merged
/// in, or `None` on a conflict. Its files are written in `dir`.
fn merge3(
    dir: &std::path::Path,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Option<Vec<u8>>> {
    let files = [("ours", ours), ("base", base), ("theirs", theirs)];
    let paths: Vec<std::path::PathBuf> = files
        .iter()
        .map(|(name, data)| {
            let p = dir.join(format!("merge-{}-{name}", std::process::id()));
            fs::write(&p, data).map(|()| p)
        })
        .collect::<std::io::Result<_>>()?;
    let out = std::process::Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .args(&paths)
        .output()?;
    for p in &paths {
        let _ = fs::remove_file(p);
    }
    // The number of conflicts (capped at 127), or a negative error (255).
    match out.status.code() {
        Some(0) => Ok(Some(out.stdout)),
        Some(1..=127) => Ok(None),
        _ => anyhow::bail!(
            "git merge-file: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The line-set rule (`git::lens`) for carrying the workspace's own lines
/// over in a blended file whose merge conflicts, where it only added lines
/// (next to a layer's, which are then their context): `new`'s lines, then
/// the lines `ours` added, in its order. `None` if it changed or removed any
/// line (that conflict is real), or for anything but text.
fn line_set(old: &[u8], ours: &[u8], new: &[u8]) -> Option<Vec<u8>> {
    if [old, ours, new].iter().any(|d| d.contains(&0)) {
        return None;
    }
    let lines = |d: &[u8]| -> Vec<Vec<u8>> { d.lines().map(<[u8]>::to_vec).collect() };
    let (old, ours, new) = (lines(old), lines(ours), lines(new));
    // Every old line still there, in order: ours only added.
    let mut rest = ours.iter();
    if !old.iter().all(|l| rest.by_ref().any(|o| o == l)) {
        return None;
    }
    let mut left: HashMap<&Vec<u8>, usize> = HashMap::new();
    for l in &old {
        *left.entry(l).or_default() += 1;
    }
    let mut added = Vec::new();
    for l in &ours {
        match left.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => added.push(l),
        }
    }
    let mut out = Vec::new();
    for line in new.iter().chain(added) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    Some(out)
}

impl Workspace {
    /// Whether layers are shown.
    pub fn lensed(&self) -> bool {
        self.layers.read().lens_on
    }

    /// Gives it the session's notifier and its slot, so it can tell the
    /// kernel what a switch or a surrender changed.
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

    /// Shows the layers stored for it (`state::lens`), as they are now, and
    /// says what it did. Refused while git is busy in it (the switch changes
    /// what its index describes), while a commit made on L can't be extracted
    /// yet, or if its own lines in a blended file depend on lines that would
    /// go; then nothing changed.
    pub fn switch(&self, store: &Store) -> Result<String> {
        let _lens = self.lens_lock.lock();
        let branch = &self.state.branch;
        if let Some(busy) = worktree::git_running(&self.state.common_dir, &self.state.admin, branch)
        {
            return Err(Refusal::Changes.error(format!(
                "{branch}: git is busy ({busy}); switch its layers when it is done"
            )));
        }
        if self.layers.read().lens_head.is_some() {
            self.sync_lens_locked()?;
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
        let stored = Lens::read(&self.state)?;
        let stored_bytes = lens::stored_bytes(&self.state)?;
        let base = self.layers.read().base;
        let base_tree = git::resolve(&base.to_string())?.tree;
        let view = lens::view(&self.state.common_dir, &base, &stored)?;
        let (new_view, new_hunk, mixed, blended) = match &view {
            Some(v) => (v.tree, v.hunk_tree, v.mixed.clone(), v.blended.clone()),
            None => (base_tree, base_tree, Default::default(), Default::default()),
        };
        let new_tree = store.load_tree(new_view)?;
        let (stale, failed, done) = {
            let mut layers = self.layers.write();
            // Under the write lock: no write to a blended file can land
            // between reading it and replacing it.
            let carried = self.carry_over(&layers, layers.hunk, new_hunk, &blended)?;
            let changes = lens::changes(&self.state.common_dir, &layers.view, &new_view)
                .context("comparing the views")?;
            // Past here the view changes: every failure is reported after the
            // kernel is told what changed.
            let old_tree = layers.view_tree;
            layers.mixed = mixed;
            layers.blended = blended;
            layers.view_tree = new_tree;
            layers.view = new_view;
            layers.hunk = new_hunk;
            layers.lens_on = stored.is_on();
            let mut stale = self.stale(store, &mut layers, old_tree, new_tree, &changes);
            let mut failed: Option<anyhow::Error> = None;
            {
                let nodes = self.nodes.lock();
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
            let git = self.tell_git(store, &mut layers, &stored, &stored_bytes, view.as_ref());
            let why = git.as_ref().err().map(|e| format!("{e:#}"));
            if let Err(e) = self.set_gitfile(&stored, why.as_deref()) {
                failed.get_or_insert(e);
            }
            let mut done = match layers.lens_on {
                true => format!("{branch}: layers shown ({} paths changed)", changes.len()),
                false => format!("{branch}: no layers shown"),
            };
            if let Some(why) = why {
                done += &format!("\nnote: git is off in it until the next switch: {why}");
            }
            for note in view.iter().flat_map(|v| &v.notes) {
                done += &format!("\nnote: {note}");
            }
            (stale, failed, done)
        };
        self.invalidate(&stale);
        match failed {
            Some(e) => Err(e),
            None => Ok(done),
        }
    }

    /// The `.git` content after a switch: the registration, or with git not
    /// kept right (`why`), the surrender (persisted, so a remount keeps it).
    fn set_gitfile(&self, stored: &Lens, why: Option<&str>) -> Result<()> {
        let off = self.state.dir.join(GIT_OFF);
        let content = match why {
            Some(why) => {
                log::warn!("{}: git off: {why}", self.state.branch);
                crate::fsutil::write_atomic(&off, why.as_bytes())?;
                surrendered(stored, why)
            }
            None => {
                let _ = fs::remove_file(&off);
                registered(&self.state)
            }
        };
        *self.gitfile.write() = Arc::new(content);
        Ok(())
    }

    /// The new content of the upper files at blended paths (blended before or
    /// now) when the blends change from tree `old_hunk` to `new_hunk`: the
    /// workspace's own lines merged onto the new blend. Refused if any
    /// conflicts.
    fn carry_over(
        &self,
        layers: &Layers,
        old_hunk: ObjectId,
        new_hunk: ObjectId,
        blended: &HashMap<Vec<u8>, Vec<usize>>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        if old_hunk == new_hunk {
            return Ok(out);
        }
        let paths: HashSet<&Vec<u8>> = layers.blended.keys().chain(blended.keys()).collect();
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
            match merge3(&self.tmp, &old, &ours, &new)?.or_else(|| line_set(&old, &ours, &new)) {
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

    /// Has git see only the workspace's own changes with the layers in
    /// `view` shown (or none), and records it (`Applied`): the layers'
    /// tracked whole files skip-worktree, the files they add excluded, and
    /// with blended files, HEAD at a lens commit and the blends in the
    /// index. What was applied before is given back where no longer shown.
    fn tell_git(
        &self,
        store: &Store,
        layers: &mut Layers,
        stored: &Lens,
        stored_bytes: &[u8],
        view: Option<&lens::View>,
    ) -> Result<()> {
        let admin = &self.state.admin;
        let common_dir = &self.state.common_dir;
        let branch = &self.state.branch;
        if layers.lens_on {
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
        let tip = git::branch_tip(branch)?;
        let mut skipped = BTreeSet::new();
        let mut added = Vec::new();
        for path in layers.mixed.keys() {
            if self.shadowed(layers, path) {
                continue;
            }
            match store.entry_at(layers.base_tree, path) {
                Some(Lower::File(_)) => {
                    skipped.insert(path.clone());
                }
                _ => added.push(path.clone()),
            }
        }
        let blended: BTreeSet<Vec<u8>> = layers.blended.keys().cloned().collect();
        let mut edits: HashMap<Vec<u8>, Edit> = HashMap::new();
        for path in layers.applied.skipped.iter().chain(&skipped) {
            let edit = Edit {
                skip_worktree: Some(skipped.contains(path)),
                ..Default::default()
            };
            edits.insert(path.clone(), edit);
        }
        // Blended files: the index has the blend over the tip (what L has),
        // or the tip's version again.
        let hunk = match (view, blended.is_empty()) {
            (Some(v), false) if tip.id != layers.base => {
                lens::view(common_dir, &tip.id, stored)?.map_or(v.hunk_tree, |t| t.hunk_tree)
            }
            (Some(v), false) => v.hunk_tree,
            _ => tip.tree,
        };
        for path in layers.applied.blended.iter().chain(&blended) {
            let tree = match blended.contains(path) {
                true => hunk,
                false => tip.tree,
            };
            let item = git::with_repo(|repo| git::lens::lookup(repo, tree, BStr::new(path)))?;
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
        layers.applied.skipped = skipped;
        layers.applied.blended = blended;
        layers.applied.lens = stored_bytes.to_vec();
        layers.applied.write(&self.state)?;
        added.sort_unstable();
        match layers.lens_on {
            true => worktree_config::set_excludes(admin, &added)?,
            false => worktree_config::remove_excludes(admin)?,
        }
        // HEAD: at a lens commit while files are blended, else on the branch.
        // Moved only from where it was left (compare-and-swap).
        let head = detached_head(admin);
        match (view, layers.blended.is_empty()) {
            (Some(v), false) => {
                let names: Vec<&str> = (stored.layers.iter())
                    .filter(|l| l.on)
                    .map(|l| l.branch.as_str())
                    .collect();
                let subject = git::lens::lens_subject(branch, &names);
                let all_seals: Vec<ObjectId> = (stored.layers.iter())
                    .filter(|l| l.on)
                    .map(|l| l.seal)
                    .collect();
                let (l, vc) = git::with_repo(|repo| -> Result<(ObjectId, ObjectId)> {
                    let l = git::lens::lens_commit(repo, hunk, tip.id, &v.hunk_seals, &subject)?;
                    let vc = git::lens::view_commit(repo, v.tree, l, &all_seals, &subject)?;
                    Ok((l, vc))
                })?;
                let pin = lens_ref(&self.state.id);
                git::cmd::run(common_dir, &["update-ref", &pin, &vc.to_string()])?;
                let old = match (head, layers.lens_head) {
                    (Some(h), Some(l)) if h == l => l.to_string(),
                    (None, _) => tip.id.to_string(),
                    _ => anyhow::bail!("HEAD moved meanwhile"),
                };
                let (msg, new) = ("gitfold: lens", l.to_string());
                git::cmd::run(
                    admin,
                    &["update-ref", "--no-deref", "-m", msg, "HEAD", &new, &old],
                )?;
                worktree_config::install_push_guard(admin)?;
                layers.lens_head = Some(l);
            }
            _ => {
                if let Some(l) = layers.lens_head.take() {
                    if let Some(h) = head.filter(|&h| h != l) {
                        // Left L's line (amended, reset): kept, and said so.
                        let left = left_ref(&self.state.id);
                        git::cmd::run(common_dir, &["update-ref", &left, &h.to_string()])?;
                        log::warn!(
                            "{branch}: HEAD had left the lens commit; {h} is kept at {left}"
                        );
                    }
                    if head.is_some() {
                        let full = format!("refs/heads/{branch}");
                        let msg = "gitfold: lens off";
                        git::cmd::run(admin, &["symbolic-ref", "-m", msg, "HEAD", &full])?;
                    }
                    worktree_config::remove_push_guard(admin)?;
                    git::cmd::run(common_dir, &["update-ref", "-d", &lens_ref(&self.state.id)])?;
                }
            }
        }
        layers.applied.head = layers.lens_head;
        layers.applied.write(&self.state)?;
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

    /// Why a commit made on the lens commit can't go onto the branch yet, if
    /// one can't: HEAD (detached) isn't L after `sync_lens`, and L is on its
    /// first-parent line.
    fn extraction_blocked(&self) -> Option<String> {
        let l = self.layers.read().lens_head?;
        let head = detached_head(&self.state.admin)?;
        if head == l {
            return None;
        }
        let on_line = git::with_repo(|repo| git::lens::chain(repo, l, head, 100))
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

    /// The sync thread's part while layers show (`rebase`): applies stored
    /// layers that differ from those applied (a switch that failed half-way,
    /// a change made while closed), then `sync_lens_locked`.
    pub(super) fn sync_lens(&self, store: &Store) -> Result<()> {
        let pending = {
            let layers = self.layers.read();
            lens::stored_bytes(&self.state).is_ok_and(|b| b != layers.applied.lens)
        };
        if pending {
            match self.switch(store) {
                Ok(done) => log::info!("{done}"),
                Err(e) => log::debug!("{}: layers not switched yet: {e:#}", self.state.branch),
            }
        }
        let _lens = self.lens_lock.lock();
        self.sync_lens_locked()
    }

    /// Commits made on the lens commit are extracted onto the branch
    /// (`git::lens::extract`), and HEAD moves to the new lens commit (the tree
    /// of the last one, so nothing git sees changes). Git must be idle (but
    /// for HEAD being detached). An amend of L is extracted as a commit on L.
    /// If HEAD left L's line (git switch, reset), git is surrendered: the
    /// index holds the blends, which mustn't be committed on the branch;
    /// `gitfold lens off` gives it back.
    fn sync_lens_locked(&self) -> Result<()> {
        let Some(l) = self.layers.read().lens_head else {
            return Ok(());
        };
        let (admin, branch) = (&self.state.admin, &self.state.branch);
        if worktree::git_running(&self.state.common_dir, admin, branch).is_some() {
            return Ok(());
        }
        let Some(head) = detached_head(admin) else {
            return self.surrender("HEAD left the lens commit");
        };
        if head == l {
            return Ok(());
        }
        let stored = Lens::parse(&self.layers.read().applied.lens)?;
        let tip = git::branch_tip(branch)?;
        let Some(view) = lens::view(&self.state.common_dir, &tip.id, &stored)? else {
            return Ok(());
        };
        let names: Vec<&str> = (stored.layers.iter())
            .filter(|l| l.on)
            .map(|l| l.branch.as_str())
            .collect();
        let subject = git::lens::lens_subject(branch, &names);
        let blended = &view.blended;
        let owners =
            |p: &BStr| -> Vec<usize> { blended.get(p.as_bytes()).cloned().unwrap_or_default() };
        let is_hunk = |p: &BStr| blended.contains_key(p.as_bytes());
        let done = git::with_repo(|repo| -> Result<Option<git::lens::Extraction>> {
            let chain = match git::lens::chain(repo, l, head, 100)? {
                Some(chain) => chain,
                // An amend of L: a commit on L's first parent instead.
                None if first_parent(repo, head)? == first_parent(repo, l)? => vec![head],
                None => return Ok(None),
            };
            let lens = git::lens::Lens {
                seals: &view.hunk_seals,
                subject: &subject,
                is_hunk: &is_hunk,
                owners: &owners,
            };
            Ok(Some(git::lens::extract(repo, &lens, l, tip.id, &chain)?))
        })?;
        let Some(done) = done else {
            return self.surrender("HEAD left the lens commit's line");
        };
        if let Some(blocked) = &done.blocked {
            let paths: Vec<String> = (blocked.conflicts.iter())
                .map(|c| c.path.to_str_lossy().into_owned())
                .collect();
            log::warn!(
                "{branch}: commit {} changes layers' lines ({}): it waits",
                blocked.commit,
                paths.join(", ")
            );
        }
        if let Some(new_l) = done.lens {
            git::lens::advance(admin, branch, (tip.id, done.tip), (head, new_l))?;
            let mut layers = self.layers.write();
            layers.lens_head = Some(new_l);
            layers.applied.head = Some(new_l);
            layers.applied.write(&self.state)?;
            log::info!(
                "{branch}: {} commit(s) made on the lens extracted",
                done.extracted.len()
            );
        }
        Ok(())
    }

    /// Turns git off in it, saying why, and tells the kernel.
    fn surrender(&self, why: &str) -> Result<()> {
        let stored = Lens::parse(&self.layers.read().applied.lens)?;
        let content = surrendered(&stored, why);
        {
            let mut gitfile = self.gitfile.write();
            if **gitfile == content {
                return Ok(());
            }
            log::warn!("{}: git off: {why}", self.state.branch);
            crate::fsutil::write_atomic(&self.state.dir.join(GIT_OFF), why.as_bytes())?;
            *gitfile = Arc::new(content);
        }
        self.invalidate(&Stale {
            inodes: vec![GITFILE_LOCAL],
            entries: Vec::new(),
        });
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
        changes: &[lens::Change],
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
        for c in changes {
            if self.shadowed(layers, &c.path) {
                continue;
            }
            // The highest path that appeared, vanished or changed type is an
            // entry change in its directory; below it, nothing was cached.
            let changed_at = ancestors(&c.path)
                .chain(std::iter::once(c.path.as_slice()))
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
                    layers.times.set(&c.path, Some(stamp));
                    if let Some(local) = nodes.held(&c.path) {
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

    /// Whether `path` is a layer's whole file (or its deletion).
    pub(super) fn is_mixed(layers: &Layers, path: &[u8]) -> bool {
        !layers.mixed.is_empty() && layers.mixed.contains_key(path)
    }

    /// Whether any path below directory `path` is a layer's: a whole file or
    /// deletion, or a blended file (whose upper copy holds the layers' lines:
    /// not to be moved).
    pub(super) fn has_mixed_below(layers: &Layers, path: &[u8]) -> bool {
        if layers.mixed.is_empty() && layers.blended.is_empty() {
            return false;
        }
        let prefix = [path, b"/"].concat();
        path.is_empty()
            || (layers.mixed.keys().chain(layers.blended.keys())).any(|p| p.starts_with(&prefix))
    }
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
