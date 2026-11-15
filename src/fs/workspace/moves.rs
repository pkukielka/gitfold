//! A move of a workspace's branch (`cli::layer::moves`: its own commits
//! onto an editable layer's new commits, or off one), in one path, mounted
//! (the daemon, asked by the command) or not (the command itself, under the
//! repository lock), layers shown or not: a layer switch that also moves
//! the base (`Workspace::switch_with`). Unlike a rebase, which keeps what
//! the workspace shows, the move's changes are meant to show: the view is
//! built over the new tip, with the layers the move leaves (an editable
//! layer added or gone), and the files it changes get a fresh time, so
//! builds see them. Recorded first as an intent (`state::moves`), so a
//! crash half-way is finished or undone.
//!
//! The workspace's own uncommitted work stays where it is. Work on a path
//! the move changes is in its way: the move waits (as `git read-tree -m`
//! would refuse). A copy of a committed file there (left in upper by a
//! commit made while layers showed: the base waits then) isn't work: it
//! goes, so the move's version shows. Union files' own lines are carried
//! over, as by any switch.

use anyhow::Result;
use gix::bstr::BString;

use super::layers::{MoveStep, Switched};
use super::{Layers, Workspace};
use crate::git;
use crate::state::markers::Kind;
use crate::state::moves::{self, Intent, Recovery};
use crate::state::{ancestors, upper_path};
use crate::store::Store;

/// How a move of a workspace's branch ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    /// Moved: the paths it changed.
    Done(Vec<BString>),
    /// Another move got there first.
    Already,
    /// Not now: why.
    Waits(String),
}

impl Moved {
    /// As the daemon answers it (`decode` reads it back).
    pub fn encode(&self) -> String {
        match self {
            Moved::Done(paths) => {
                let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
                format!("moved\n{}", paths.join("\0"))
            }
            Moved::Already => "already".into(),
            Moved::Waits(why) => format!("waits\n{why}"),
        }
    }

    pub fn decode(text: &str) -> Result<Moved> {
        Ok(match text.split_once('\n') {
            Some(("moved", paths)) => Moved::Done(
                paths
                    .split('\0')
                    .filter(|p| !p.is_empty())
                    .map(BString::from)
                    .collect(),
            ),
            Some(("waits", why)) => Moved::Waits(why.to_string()),
            _ if text == "already" => Moved::Already,
            _ => anyhow::bail!("not a move's answer: {text:?}"),
        })
    }
}

impl Workspace {
    /// Moves its branch as `intent` says, showing the layers `layers` (a
    /// layers file's content) over its new tip.
    pub fn move_branch(&self, store: &Store, intent: Intent, layers: Vec<u8>) -> Result<Moved> {
        if let Some(moved) = self.moved_meanwhile(&intent)? {
            return Ok(moved);
        }
        let paths = moves::changed_paths(&intent)?;
        match self.switch_with(store, Some(&MoveStep { intent, layers }))? {
            Switched::Done(done) => {
                log::info!(
                    "{}: moved from {} to {}; {done}",
                    self.state.branch,
                    intent.from,
                    intent.to
                );
                Ok(Moved::Done(paths))
            }
            Switched::Waits(why) => match self.moved_meanwhile(&intent)? {
                Some(moved) => Ok(moved),
                None => Ok(Moved::Waits(why)),
            },
        }
    }

    /// Whether the branch and its own ref are still where `intent` starts
    /// from (`None`), else how it ended: a move computed before another one
    /// finished would otherwise move the files against the other's result.
    pub(super) fn moved_meanwhile(&self, intent: &Intent) -> Result<Option<Moved>> {
        let (common_dir, branch) = (&self.state.common_dir, &self.state.branch);
        let tip = git::stack::rev(common_dir, &format!("refs/heads/{branch}"))?;
        let own = git::stack::rev(common_dir, &git::stack::own_ref(branch))?;
        Ok(if tip == Some(intent.from) && own == intent.own_from {
            None
        } else if tip == Some(intent.to) {
            Some(Moved::Already)
        } else {
            Some(Moved::Waits(
                "the branch moved meanwhile; retried next time".into(),
            ))
        })
    }

    /// Why the move `intent` can't be made now, if it can't: an interrupted
    /// move to finish first, git busy, the branch moved meanwhile, the base
    /// behind the branch with no layer shown (it catches up first), or the
    /// workspace's own work on a path the move changes (or staged there).
    /// `layers_lock` held.
    pub(super) fn move_waits(&self, store: &Store, intent: &Intent) -> Result<Option<String>> {
        match moves::recover(&self.state)? {
            Recovery::Nothing | Recovery::Finished | Recovery::Undone => {}
            Recovery::InProgress => return Ok(Some("another move of it is running".into())),
            Recovery::Stuck(why) => return Ok(Some(why)),
        }
        if let Some(Moved::Waits(why)) = self.moved_meanwhile(intent)? {
            return Ok(Some(why));
        }
        let (shown, base) = {
            let layers = self.layers.read();
            (layers.layers_on, layers.base)
        };
        if !shown && base != intent.from {
            let from = git::resolve(&intent.from.to_string())?;
            self.rebase(store, &from, true)?;
            if self.layers.read().base != intent.from {
                return Ok(Some(format!(
                    "{}: its base is catching up with its branch; retry",
                    self.state.branch
                )));
            }
        }
        let paths = moves::own_paths(&self.state, intent)?;
        let in_the_way = self.in_the_way(store, intent, &paths)?;
        if !in_the_way.is_empty() {
            return Ok(Some(in_the_way_text(&in_the_way)));
        }
        let paths = moves::changed_paths(intent)?;
        if !paths.is_empty() && !moves::index_matches(&self.state, intent.from, &paths)? {
            return Ok(Some(
                "staged changes overlap the move; commit or unstage them".into(),
            ));
        }
        Ok(None)
    }

    /// The paths of `paths` (those the move `intent` changes) where the
    /// workspace has work of its own: a file of its own that isn't a copy
    /// of the branch's (`moves::stale_copies`), a deletion of a file the
    /// branch has, or a file or a deleted directory above.
    fn in_the_way(&self, store: &Store, intent: &Intent, paths: &[BString]) -> Result<Vec<String>> {
        let copies = moves::stale_copies(&self.state, intent)?;
        let from = git::resolve(&intent.from.to_string())?.tree;
        let layers = self.layers.read();
        let mut out = Vec::new();
        for path in paths {
            let p: &[u8] = path.as_ref();
            let own = match std::fs::symlink_metadata(upper_path(&self.upper, p)) {
                Ok(_) => !copies.contains(path) && !self.as_shown(store, &layers, p)?,
                Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => true,
                Err(_) if layers.markers.whiteouts.contains(p) => {
                    git::with_repo(|repo| git::layers::lookup(repo, from, path.as_ref()))?.is_some()
                }
                Err(_) => ancestors(p).any(|dir| layers.markers.opaque.contains(dir)),
            };
            if own {
                out.push(path.to_string());
            }
        }
        Ok(out)
    }

    /// The move's part of step 2 (`state::moves`), under the write lock,
    /// the base moved: the copies of committed files at paths the move
    /// changes, and the whiteouts of files the branch had deleted there,
    /// dropped. The paths dropped.
    pub(super) fn drop_for_move(
        &self,
        store: &Store,
        layers: &mut Layers,
        intent: &Intent,
    ) -> Result<Vec<Vec<u8>>> {
        let mut dropped = Vec::new();
        let mut copies = moves::stale_copies(&self.state, intent)?;
        // And files the same as what shows below them (written back as
        // they were, under the view as it is before the move): no work of
        // its own either.
        for path in moves::own_paths(&self.state, intent)? {
            if !copies.contains(&path) && self.as_shown(store, layers, path.as_ref())? {
                copies.push(path);
            }
        }
        for path in copies {
            let p: &[u8] = path.as_ref();
            match std::fs::remove_file(upper_path(&self.upper, p)) {
                Err(e) if !crate::fsutil::not_found(&e) => return Err(e.into()),
                _ => dropped.push(p.to_vec()),
            }
        }
        let from = git::resolve(&intent.from.to_string())?.tree;
        let mut marked = false;
        for path in moves::own_paths(&self.state, intent)? {
            let p: &[u8] = path.as_ref();
            if layers.markers.whiteouts.contains(p)
                && git::with_repo(|repo| git::layers::lookup(repo, from, path.as_ref()))?.is_none()
            {
                marked |= layers.markers.set(Kind::Whiteout, p, false)?;
                dropped.push(p.to_vec());
            }
        }
        if marked {
            layers.markers.sync()?;
        }
        Ok(dropped)
    }
}

impl Workspace {
    /// Whether upper file `path` is the same as what shows below it (the
    /// view's file): no work of its own.
    fn as_shown(&self, store: &Store, layers: &Layers, path: &[u8]) -> Result<bool> {
        Ok(match store.entry_at(layers.view_tree, path) {
            Some(crate::store::Lower::File(id)) => self.same_as_stored(store, path, id)?.is_some(),
            _ => false,
        })
    }
}

/// Why a move waits for the workspace's own work on `paths`.
pub fn in_the_way_text<S: AsRef<str>>(paths: &[S]) -> String {
    let paths: Vec<&str> = paths.iter().map(AsRef::as_ref).collect();
    let mut shown = paths[..paths.len().min(5)].join(", ");
    if paths.len() > 5 {
        shown += &format!(" and {} more", paths.len() - 5);
    }
    format!("uncommitted changes to {shown} overlap the move; commit or stash them")
}
