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
//!
//! A prewarmed Chromium workspace holds ~560k nodes, so they are compact: a
//! 32-byte record in one hash table, names in one shared buffer (compacted when
//! mostly garbage), and a (parent, name) index holding only local ids.

use gix::hashtable::hash_table::HashTable;

use super::{GITFILE_LOCAL, join};
use crate::inode;

/// Parent of nodes whose path no longer exists.
const ORPHAN: u64 = u64::MAX;

struct Node {
    local: u64,
    parent: u64,
    /// References the kernel holds (entry replies minus forgets).
    nlookup: u32,
    name_len: u16,
    /// Offset of the name in `Nodes::names`.
    name_off: u32,
}

/// Hash of a local id (path-derived ids leave the high bits zero, which the
/// table's control bytes come from).
fn local_hash(local: u64) -> u64 {
    local.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// Hash of a (parent, name) key: FNV-1a.
fn key_hash(parent: u64, name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in parent.to_le_bytes().iter().chain(name) {
        h = (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn name_of<'a>(names: &'a [u8], node: &Node) -> &'a [u8] {
    &names[node.name_off as usize..][..node.name_len as usize]
}

#[derive(Default)]
pub(super) struct Nodes {
    nodes: HashTable<Node>,
    /// Local ids of the non-orphaned nodes, by (parent, name).
    by_name: HashTable<u64>,
    names: Vec<u8>,
    /// Bytes of `names` no node refers to any more.
    garbage: usize,
}

impl Nodes {
    fn get(&self, local: u64) -> Option<&Node> {
        self.nodes.find(local_hash(local), |n| n.local == local)
    }

    fn get_mut(&mut self, local: u64) -> Option<&mut Node> {
        self.nodes.find_mut(local_hash(local), |n| n.local == local)
    }

    pub(super) fn path(&self, local: u64) -> Option<Vec<u8>> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut cur = local;
        while cur != 0 {
            let node = self.get(cur)?;
            if node.parent == ORPHAN {
                return None;
            }
            parts.push(name_of(&self.names, node));
            cur = node.parent;
        }
        parts.reverse();
        Some(parts.join(&b'/'))
    }

    fn existing(&self, parent: u64, name: &[u8]) -> Option<u64> {
        let is = |&local: &u64| {
            self.get(local)
                .is_some_and(|n| n.parent == parent && name_of(&self.names, n) == name)
        };
        self.by_name.find(key_hash(parent, name), is).copied()
    }

    /// The node for `name` in `parent` (whose path is `dir`), allocating one on
    /// first sight, with one more kernel reference counted.
    pub(super) fn claim(&mut self, parent: u64, name: &[u8], dir: &[u8]) -> u64 {
        let local = match self.existing(parent, name) {
            Some(local) => local,
            None => {
                let local = self.free_local(&join(dir, name));
                self.insert(local, parent, name);
                local
            }
        };
        let node = self.get_mut(local).expect("just ensured");
        node.nlookup = node.nlookup.saturating_add(1);
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
        while self.get(local).is_some() || local == GITFILE_LOCAL {
            local = ((local + 1) & inode::WS_LOCAL_MASK).max(1);
        }
        local
    }

    /// Stores `name` in the shared buffer; returns its offset.
    fn store_name(&mut self, name: &[u8]) -> u32 {
        let off = self.names.len() as u32;
        self.names.extend_from_slice(name);
        off
    }

    /// Adds node `local` (not present) under (parent, name), which must be free.
    fn insert(&mut self, local: u64, parent: u64, name: &[u8]) {
        let node = Node {
            local,
            parent,
            nlookup: 0,
            name_len: name.len() as u16,
            name_off: self.store_name(name),
        };
        self.nodes
            .insert_unique(local_hash(local), node, |n| local_hash(n.local));
        self.index(local, parent, name);
    }

    fn index(&mut self, local: u64, parent: u64, name: &[u8]) {
        let (nodes, names) = (&self.nodes, &self.names);
        let hash_of = |&l: &u64| {
            let n = nodes
                .find(local_hash(l), |n| n.local == l)
                .expect("indexed nodes exist");
            key_hash(n.parent, name_of(names, n))
        };
        self.by_name
            .insert_unique(key_hash(parent, name), local, hash_of);
    }

    /// Removes node `local` from the (parent, name) index.
    fn unindex(&mut self, local: u64) {
        let Some(node) = self.get(local) else { return };
        if node.parent == ORPHAN {
            return;
        }
        let hash = key_hash(node.parent, name_of(&self.names, node));
        if let Ok(entry) = self.by_name.find_entry(hash, |&l| l == local) {
            entry.remove();
        }
    }

    /// Records that a name of `len` bytes is no longer used; compacts the buffer
    /// once it is mostly garbage.
    fn drop_name(&mut self, len: u16) {
        self.garbage += len as usize;
        if self.garbage > 1 << 20 && self.garbage > self.names.len() / 2 {
            let mut names = Vec::with_capacity(self.names.len() - self.garbage);
            for node in self.nodes.iter_mut() {
                let off = names.len() as u32;
                names.extend_from_slice(name_of(&self.names, node));
                node.name_off = off;
            }
            self.names = names;
            self.garbage = 0;
        }
    }

    /// Removes (parent, name) from the index; returns its node.
    fn take(&mut self, parent: u64, name: &[u8]) -> Option<u64> {
        let (nodes, names) = (&self.nodes, &self.names);
        let is = |&local: &u64| {
            nodes
                .find(local_hash(local), |n| n.local == local)
                .is_some_and(|n| n.parent == parent && name_of(names, n) == name)
        };
        let entry = self.by_name.find_entry(key_hash(parent, name), is).ok()?;
        Some(entry.remove().0)
    }

    pub(super) fn orphan(&mut self, parent: u64, name: &[u8]) {
        if let Some(local) = self.take(parent, name)
            && let Some(node) = self.get_mut(local)
        {
            node.parent = ORPHAN;
        }
    }

    pub(super) fn reparent(&mut self, parent: u64, name: &[u8], new_parent: u64, new_name: &[u8]) {
        self.orphan(new_parent, new_name);
        let Some(local) = self.take(parent, name) else {
            return;
        };
        let off = self.store_name(new_name);
        let node = self.get_mut(local).expect("indexed nodes exist");
        let old_len = node.name_len;
        node.parent = new_parent;
        node.name_len = new_name.len() as u16;
        node.name_off = off;
        self.index(local, new_parent, new_name);
        self.drop_name(old_len);
    }

    /// The kernel dropped `n` references to `local`; frees it at zero.
    pub(super) fn forget(&mut self, local: u64, n: u64) {
        let Some(node) = self.get_mut(local) else {
            return;
        };
        node.nlookup = node.nlookup.saturating_sub(n.min(u32::MAX as u64) as u32);
        if node.nlookup > 0 {
            return;
        }
        self.unindex(local);
        if let Ok(entry) = (self.nodes).find_entry(local_hash(local), |n| n.local == local) {
            let (node, _) = entry.remove();
            self.drop_name(node.name_len);
        }
    }
}
