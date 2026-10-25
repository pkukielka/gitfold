//! The node table of a workspace: which (parent, name) each local id stands
//! for, so a path can be rebuilt from an inode and renames keep inodes.
//!
//! A node exists exactly while the kernel holds it. The kernel counts one
//! reference per entry reply (lookup, readdirplus entry, create, mkdir, symlink)
//! and gives them back with `forget`; `claim` hands out a node and counts the
//! reference in one step, under the table's lock, so a concurrent `forget`
//! can't free a node between the two. Freeing keeps memory bounded and lets a
//! path recreated after deletion get its path-derived id back (which the git
//! index expects) instead of a probed one.

use std::collections::HashMap;

use super::{GITFILE_LOCAL, join};
use crate::inode;

/// Parent of nodes whose path no longer exists.
const ORPHAN: u64 = u64::MAX;

struct Node {
    parent: u64,
    name: Box<[u8]>,
    /// References the kernel holds (entry replies minus forgets).
    nlookup: u64,
}

#[derive(Default)]
pub(super) struct Nodes {
    by_local: HashMap<u64, Node>,
    /// parent → name → local; nested so lookups by `&[u8]` don't allocate.
    by_name: HashMap<u64, HashMap<Box<[u8]>, u64>>,
}

impl Nodes {
    pub(super) fn path(&self, local: u64) -> Option<Vec<u8>> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut cur = local;
        while cur != 0 {
            let node = self.by_local.get(&cur)?;
            if node.parent == ORPHAN {
                return None;
            }
            parts.push(&node.name);
            cur = node.parent;
        }
        parts.reverse();
        Some(parts.join(&b'/'))
    }

    fn existing(&self, parent: u64, name: &[u8]) -> Option<u64> {
        self.by_name.get(&parent)?.get(name).copied()
    }

    /// The node for `name` in `parent` (whose path is `dir`), allocating one on
    /// first sight, with one more kernel reference counted.
    pub(super) fn claim(&mut self, parent: u64, name: &[u8], dir: &[u8]) -> u64 {
        let local = match self.existing(parent, name) {
            Some(local) => local,
            None => {
                let local = self.free_local(&join(dir, name));
                self.insert(local, parent, name, 0);
                local
            }
        };
        self.by_local.get_mut(&local).expect("just ensured").nlookup += 1;
        local
    }

    /// The local id `name` has or would get, without allocating (for plain
    /// readdir, whose entries the kernel doesn't count).
    pub(super) fn peek(&self, parent: u64, name: &[u8], dir: &[u8]) -> u64 {
        self.existing(parent, name)
            .unwrap_or_else(|| self.free_local(&join(dir, name)))
    }

    /// The path's preferred local id, or the next one not taken (probing).
    fn free_local(&self, path: &[u8]) -> u64 {
        let mut local = inode::path_local(path);
        while self.by_local.contains_key(&local) || local == GITFILE_LOCAL {
            local = ((local + 1) & inode::WS_LOCAL_MASK).max(1);
        }
        local
    }

    fn insert(&mut self, local: u64, parent: u64, name: &[u8], nlookup: u64) {
        let name: Box<[u8]> = name.into();
        self.by_name
            .entry(parent)
            .or_default()
            .insert(name.clone(), local);
        self.by_local.insert(
            local,
            Node {
                parent,
                name,
                nlookup,
            },
        );
    }

    fn take(&mut self, parent: u64, name: &[u8]) -> Option<u64> {
        self.by_name.get_mut(&parent)?.remove(name)
    }

    pub(super) fn orphan(&mut self, parent: u64, name: &[u8]) {
        if let Some(local) = self.take(parent, name)
            && let Some(node) = self.by_local.get_mut(&local)
        {
            node.parent = ORPHAN;
        }
    }

    pub(super) fn reparent(&mut self, parent: u64, name: &[u8], new_parent: u64, new_name: &[u8]) {
        self.orphan(new_parent, new_name);
        if let Some(local) = self.take(parent, name) {
            let nlookup = self.by_local.get(&local).map_or(0, |n| n.nlookup);
            self.insert(local, new_parent, new_name, nlookup);
        }
    }

    /// The kernel dropped `n` references to `local`; frees it at zero.
    pub(super) fn forget(&mut self, local: u64, n: u64) {
        let Some(node) = self.by_local.get_mut(&local) else {
            return;
        };
        node.nlookup = node.nlookup.saturating_sub(n);
        if node.nlookup > 0 {
            return;
        }
        let node = self.by_local.remove(&local).expect("present above");
        if node.parent != ORPHAN
            && let Some(names) = self.by_name.get_mut(&node.parent)
            && names.get(&node.name) == Some(&local)
        {
            names.remove(&node.name);
        }
        // A directory is forgotten only after all its children are (each child
        // pins its parent in the kernel), so its name map is empty or stale.
        self.by_name.remove(&local);
    }
}
