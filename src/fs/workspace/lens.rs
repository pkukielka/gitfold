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
//! Where that can't be done (sparse checkout, a split or sparse index, HEAD
//! taken off L), git is turned off in the workspace instead (surrendered):
//! its `.git` names a directory that doesn't exist, whose name says why, so
//! every git command fails with it.
//!
//! Meanwhile the base doesn't follow the branch (`rebase`): moving it would
//! copy the layers' files into the workspace's own. Commits still work; the
//! base catches up once no layer shows.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::{BStr, ByteSlice};

use super::times::Stamp;
use super::{GITFILE_LOCAL, Layers, Workspace};
use crate::git::index_edit::{self, Edit};
use crate::git::{self, worktree, worktree_config};
use crate::refusal::Refusal;
use crate::state::lens::{self, Lens};
use crate::state::{StateDir, ancestors};
use crate::store::{Lower, Store};

/// A file in the state directory whose presence means git is surrendered
/// (see the module doc): its content is why.
const GIT_OFF: &str = "lens-git-off";

/// What the kernel must be told after a switch (local ids of the workspace).
#[derive(Default)]
pub struct Stale {
    /// Nodes whose attributes and pages changed (directories: their listing).
    pub inodes: Vec<u64>,
    /// Names added, removed or changed in type, by their directory's node.
    pub entries: Vec<(u64, Vec<u8>)>,
    /// What the agent should hear about the view (`state::lens::View`).
    pub notes: Vec<String>,
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

/// The commit a detached HEAD in the registration at `admin` is at.
fn detached_head(admin: &std::path::Path) -> Option<ObjectId> {
    let head = fs::read_to_string(admin.join("HEAD")).ok()?;
    ObjectId::from_hex(head.trim().as_bytes()).ok()
}

/// Sets up the view of the layers stored for the workspace in `state`, if any
/// is shown, when it opens; returns the `.git` content if git is surrendered.
pub(super) fn open_view(
    store: &Store,
    state: &StateDir,
    layers: &mut Layers,
) -> Result<Option<Vec<u8>>> {
    let stored = Lens::read(state)?;
    let Some(view) = lens::view(&state.common_dir, &layers.base, &stored)? else {
        return Ok(None);
    };
    layers.view_tree = store.load_tree(view.tree)?;
    layers.view = view.tree;
    layers.hunk = view.hunk_tree;
    layers.mixed = view.mixed;
    layers.blended = view.blended;
    layers.lens_on = true;
    // The lens commit HEAD is at, or that commits made since were made on.
    layers.lens_head = detached_head(&state.admin).and_then(|head| {
        git::with_repo(|repo| {
            let mut at = head;
            for _ in 0..100 {
                if git::lens::is_lens(repo, at).ok()? {
                    return Some(at);
                }
                at = repo.find_commit(at).ok()?.parent_ids().next()?.detach();
            }
            None
        })
    });
    // The index, excludes and HEAD were set when the layers were switched on.
    Ok(fs::read_to_string(state.dir.join(GIT_OFF))
        .ok()
        .map(|why| surrendered(&stored, why.trim())))
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
    match out.status.code() {
        Some(0) => Ok(Some(out.stdout)),
        Some(n) if n > 0 => Ok(None),
        _ => anyhow::bail!(
            "git merge-file: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

impl Workspace {
    /// Whether layers are shown.
    pub fn lensed(&self) -> bool {
        self.layers.read().lens_on
    }

    /// Shows the layers stored for it (`state::lens`), as they are now:
    /// what the kernel must be told is returned. Refused while git is busy
    /// in it (the switch changes what its index describes), while commits
    /// made on L can't be extracted yet, or if its own lines in a blended
    /// file depend on lines that would go.
    pub fn switch(&self, store: &Store) -> Result<Stale> {
        let branch = &self.state.branch;
        if let Some(busy) = worktree::git_running(&self.state.common_dir, &self.state.admin, branch)
        {
            return Err(Refusal::Changes.error(format!(
                "{branch}: git is busy ({busy}); switch its layers when it is done"
            )));
        }
        if self.layers.read().lens_head.is_some() {
            self.sync_lens()?;
            if let Some(blocked) = self.extraction_pending() {
                return Err(Refusal::Changes.error(format!(
                    "{branch}: {blocked}; switch its layers once it can go onto {branch}"
                )));
            }
        } else if let Some(busy) = self.state.git_busy() {
            return Err(Refusal::Changes.error(format!(
                "{branch}: git is busy ({busy}); switch its layers when it is done"
            )));
        }
        let stored = Lens::read(&self.state)?;
        let base = self.layers.read().base;
        let base_tree = git::resolve(&base.to_string())?.tree;
        let view = lens::view(&self.state.common_dir, &base, &stored)?;
        let (new_view, new_hunk, mixed, blended) = match &view {
            Some(v) => (v.tree, v.hunk_tree, v.mixed.clone(), v.blended.clone()),
            None => (base_tree, base_tree, Default::default(), Default::default()),
        };
        // The workspace's own lines in blended files, carried over: merged
        // before anything changes, so a conflict refuses the whole switch.
        let carried = {
            let layers = self.layers.read();
            self.carry_over(&layers, layers.hunk, new_hunk, &blended)?
        };
        let new_tree = store.load_tree(new_view)?;
        let mut layers = self.layers.write();
        let changes = lens::changes(&self.state.common_dir, &layers.view, &new_view)
            .context("comparing the views")?;
        let old_tree = layers.view_tree;
        let old_mixed = std::mem::replace(&mut layers.mixed, mixed);
        let old_blended = std::mem::replace(&mut layers.blended, blended);
        layers.view_tree = new_tree;
        layers.view = new_view;
        layers.hunk = new_hunk;
        layers.lens_on = stored.is_on();
        let mut stale = self.stale(store, &mut layers, old_tree, new_tree, &changes)?;
        let nodes = self.nodes.lock();
        for (path, data) in &carried {
            self.rewrite_upper(path, data)?;
            if let Some(local) = nodes.held(path) {
                stale.inodes.push(local);
            }
        }
        drop(nodes);
        stale.notes = view.as_ref().map(|v| v.notes.clone()).unwrap_or_default();
        layers.times.save()?;
        self.changed();
        let git = self
            .tell_git(
                store,
                &mut layers,
                &stored,
                view.as_ref(),
                &old_mixed,
                &old_blended,
            )
            .err()
            .map(|e| format!("{e:#}"));
        let off = self.state.dir.join(GIT_OFF);
        *self.gitfile.write() = Arc::new(match (&git, layers.lens_on) {
            (Some(why), true) => {
                log::warn!("{branch}: git surrendered while layers show: {why}");
                crate::fsutil::write_atomic(&off, why.as_bytes())?;
                surrendered(&stored, why)
            }
            (git, _) => {
                if let Some(why) = git {
                    log::warn!("{branch}: giving git back after the layers: {why}");
                }
                let _ = fs::remove_file(&off);
                registered(&self.state)
            }
        });
        Ok(stale)
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

    /// Replaces upper file `path`'s content, keeping its mode.
    fn rewrite_upper(&self, path: &[u8], data: &[u8]) -> Result<()> {
        let target = self.upper_path(path);
        let mode = fs::metadata(&target)?.permissions().mode();
        let tmp = self.tmp.join(format!("carry-{}", std::process::id()));
        fs::write(&tmp, data)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, &target)?;
        Ok(())
    }

    /// Has git see only the workspace's own changes with the layers in
    /// `view` shown (or none): the layers' tracked whole files skip-worktree,
    /// the files they add excluded, and with blended files, HEAD at a lens
    /// commit and the blends in the index. Paths only `old_mixed` and
    /// `old_blended` (shown before) had are given back.
    fn tell_git(
        &self,
        store: &Store,
        layers: &mut Layers,
        stored: &Lens,
        view: Option<&lens::View>,
        old_mixed: &HashMap<Vec<u8>, usize>,
        old_blended: &HashMap<Vec<u8>, Vec<usize>>,
    ) -> Result<()> {
        let admin = &self.state.admin;
        let common_dir = &self.state.common_dir;
        let branch = &self.state.branch;
        if layers.lens_on {
            anyhow::ensure!(
                !index_edit::sparse_checkout(admin)?,
                "sparse checkout is on"
            );
        }
        // What the branch tip has: the base, or past it by commits made with
        // layers shown (the base waits for the layers to go).
        let tip = git::branch_tip(branch)?;
        let mut edits: HashMap<Vec<u8>, Edit> = HashMap::new();
        let mut added = Vec::new();
        for path in old_mixed.keys().chain(layers.mixed.keys()) {
            let shown = layers.mixed.contains_key(path) && !self.shadowed(layers, path);
            let tracked = matches!(store.entry_at(layers.base_tree, path), Some(Lower::File(_)));
            if shown && !tracked {
                added.push(path.clone());
            }
            if tracked {
                let edit = Edit {
                    skip_worktree: Some(shown),
                    stat: None,
                    oid: None,
                };
                edits.insert(path.clone(), edit);
            }
        }
        // Blended files: the index has the blend over the tip (what L has),
        // or the tip's version again.
        let hunk = match (view, layers.blended.is_empty()) {
            (Some(v), false) if tip.id != layers.base => {
                lens::view(common_dir, &tip.id, stored)?.map_or(v.hunk_tree, |t| t.hunk_tree)
            }
            (Some(v), false) => v.hunk_tree,
            _ => tip.tree,
        };
        for path in old_blended.keys().chain(layers.blended.keys()) {
            let tree = match layers.blended.contains_key(path) {
                true => hunk,
                false => tip.tree,
            };
            let item = git::with_repo(|repo| git::lens::lookup(repo, tree, BStr::new(path)))?;
            if let Some(item) = item {
                // The size git compares first is the blob's; zero times make
                // it read the file once to be sure (a recorded size that
                // differs would be taken as a change unread).
                let size = git::object_size(item.oid)?.unwrap_or_default() as u32;
                let stat = index_edit::Stat {
                    size,
                    ..Default::default()
                };
                let edit = Edit {
                    skip_worktree: Some(false),
                    stat: Some(stat),
                    oid: Some(item.oid),
                };
                edits.insert(path.clone(), edit);
            }
        }
        index_edit::edit(&self.state.index(), &edits)?;
        added.sort_unstable();
        added.dedup();
        match layers.lens_on {
            true => worktree_config::set_excludes(admin, &added)?,
            false => worktree_config::remove_excludes(admin)?,
        }
        // HEAD: at a lens commit while files are blended, else on the branch.
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
                let msg = "gitfold: lens";
                git::cmd::run(
                    admin,
                    &[
                        "update-ref",
                        "--no-deref",
                        "-m",
                        msg,
                        "HEAD",
                        &l.to_string(),
                    ],
                )?;
                worktree_config::install_push_guard(admin)?;
                layers.lens_head = Some(l);
            }
            _ => {
                if layers.lens_head.take().is_some() {
                    let full = format!("refs/heads/{branch}");
                    let msg = "gitfold: lens off";
                    git::cmd::run(admin, &["symbolic-ref", "-m", msg, "HEAD", &full])?;
                    worktree_config::remove_push_guard(admin)?;
                    git::cmd::run(common_dir, &["update-ref", "-d", &lens_ref(&self.state.id)])?;
                }
            }
        }
        Ok(())
    }

    /// Why commits made on the lens commit can't all go onto the branch yet,
    /// if they can't: HEAD isn't L after `sync_lens`.
    fn extraction_pending(&self) -> Option<String> {
        let l = self.layers.read().lens_head?;
        let head = detached_head(&self.state.admin);
        (head != Some(l)).then(|| match head {
            Some(head) => format!(
                "commit {} can't go onto the branch alone (it changes lines of a layer's)",
                &head.to_string()[..12]
            ),
            None => "HEAD left the lens commit (git switch, reset or rebase)".to_string(),
        })
    }

    /// The sync thread's part while layers show: commits made on the lens
    /// commit are extracted onto the branch (`git::lens::extract`), and HEAD
    /// moves to the new lens commit (the tree of the last one, so nothing git
    /// sees changes). Git must be idle (but for HEAD being detached). If HEAD
    /// left L's line (git switch, reset), git is surrendered: the index holds
    /// the blends, which mustn't be committed on the branch.
    pub(super) fn sync_lens(&self) -> Result<()> {
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
        let stored = Lens::read(&self.state)?;
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
            let Some(chain) = git::lens::chain(repo, l, head, 100)? else {
                return Ok(None);
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
            self.layers.write().lens_head = Some(new_l);
            log::info!(
                "{branch}: {} commit(s) made on the lens extracted",
                done.extracted.len()
            );
        }
        Ok(())
    }

    /// Turns git off in it, saying why (see the module doc).
    fn surrender(&self, why: &str) -> Result<()> {
        let stored = Lens::read(&self.state)?;
        let content = surrendered(&stored, why);
        let mut gitfile = self.gitfile.write();
        if **gitfile != content {
            log::warn!("{}: git surrendered: {why}", self.state.branch);
            crate::fsutil::write_atomic(&self.state.dir.join(GIT_OFF), why.as_bytes())?;
            *gitfile = Arc::new(content);
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
        changes: &[lens::Change],
    ) -> Result<Stale> {
        let stamp = {
            let now = SystemTime::now();
            Stamp {
                mtime: now,
                ctime: now,
            }
        };
        let kind = |tree: u32, path: &[u8]| match store.entry_at(tree, path) {
            None => 0,
            Some(Lower::Tree(_) | Lower::Gitlink) => 1,
            Some(Lower::File(_)) => 2,
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
                Some(at) => {
                    if kind(new, at) == 2 {
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
        Ok(stale)
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

    /// Whether any path below directory `path` shows a layer's whole file.
    pub(super) fn has_mixed_below(layers: &Layers, path: &[u8]) -> bool {
        if layers.mixed.is_empty() {
            return false;
        }
        let prefix = [path, b"/"].concat();
        path.is_empty() || layers.mixed.keys().any(|p| p.starts_with(&prefix))
    }
}

/// The line-set rule (`git::lens`) for carrying the workspace's own lines
/// over in a blended file whose merge conflicts (lines appended next to a
/// layer's): `new`'s lines less those `ours` removed from `old`, then the
/// lines `ours` added, in its order. `None` for anything but text.
fn line_set(old: &[u8], ours: &[u8], new: &[u8]) -> Option<Vec<u8>> {
    if [old, ours, new].iter().any(|d| d.contains(&0)) {
        return None;
    }
    let lines = |d: &[u8]| -> Vec<Vec<u8>> { d.lines().map(<[u8]>::to_vec).collect() };
    let (old, ours, new) = (lines(old), lines(ours), lines(new));
    let removed: HashSet<&Vec<u8>> = old.iter().filter(|l| !ours.contains(l)).collect();
    let mut out = Vec::new();
    for line in new.iter().filter(|l| !removed.contains(l)) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    for line in ours.iter().filter(|l| !old.contains(l) && !new.contains(l)) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    Some(out)
}

/// (directory, name) of `path`.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (b"", path),
    }
}
