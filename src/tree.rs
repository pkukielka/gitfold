//! Branches built on branches: each branch's parent and whether it follows
//! it (settings: `branch.<branch>.gitfoldParent`, `gitfoldFollow`), and the
//! tree the parents form. Shared by `gitfold parent`, `branch --parent`,
//! `list`, `changes -- PATH` and the views; see layers-design/15-tree-spec.md
//! in the blog notes for the design. The main branch, the root, is
//! `git::main_branch`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::settings::{self, Local};

/// The parents of the repository's branches, as its config has them.
#[derive(Debug, Default)]
pub struct Tree {
    /// Branch to parent.
    parents: BTreeMap<String, String>,
}

/// Why a branch can't get a parent.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The branch itself.
    Own,
    /// A branch built on it.
    Cycle,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Own => write!(f, "a branch can't be its own parent"),
            Refused::Cycle => write!(f, "that would be a cycle"),
        }
    }
}

impl Tree {
    /// The tree in `local`.
    pub fn read(local: &Local) -> Tree {
        Tree {
            parents: local.parents(),
        }
    }

    /// The parent of `branch`, if it has one.
    pub fn parent(&self, branch: &str) -> Option<&str> {
        self.parents.get(branch).map(String::as_str)
    }

    /// The branches whose parent is `branch`, by name.
    pub fn children(&self, branch: &str) -> Vec<&str> {
        self.parents
            .iter()
            .filter(|(_, parent)| parent.as_str() == branch)
            .map(|(child, _)| child.as_str())
            .collect()
    }

    /// Everything built on `branch`: its children, theirs, and so on, each
    /// once, every branch before its children (`branch` itself not
    /// included; safe on cycles a hand-edited config may hold).
    #[allow(dead_code)] // used as the tree's streams land (the views)
    pub fn subtree(&self, branch: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let mut stack: Vec<&str> = self.children(branch).into_iter().rev().collect();
        while let Some(next) = stack.pop() {
            if next != branch && !out.contains(&next) {
                out.push(next);
                stack.extend(self.children(next).into_iter().rev());
            }
        }
        out
    }

    /// Whether `parent` may become the parent of `branch`: not `branch`
    /// itself, nor built on it (a cycle).
    pub fn check_parent(&self, branch: &str, parent: &str) -> Result<(), Refused> {
        if parent == branch {
            return Err(Refused::Own);
        }
        // Bounded: a cycle already in a hand-edited config ends the walk.
        let ancestors = std::iter::successors(Some(parent), |name| self.parent(name));
        match ancestors
            .take(self.parents.len() + 1)
            .any(|name| name == branch)
        {
            true => Err(Refused::Cycle),
            false => Ok(()),
        }
    }
}

/// Makes `parent` the parent of `branch`, or with `None` forgets it,
/// refusing a parent `Tree::check_parent` refuses: the one way to set it.
#[allow(dead_code)] // used as the tree's streams land (`gitfold parent`)
pub fn set_parent(common_dir: &Path, branch: &str, parent: Option<&str>) -> Result<()> {
    if let Some(parent) = parent {
        Tree::read(&Local::read(common_dir))
            .check_parent(branch, parent)
            .map_err(|refused| anyhow::anyhow!("{parent} as the parent of {branch}: {refused}"))?;
    }
    settings::write_parent(common_dir, branch, parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pairs: &[(&str, &str)]) -> Tree {
        Tree {
            parents: pairs
                .iter()
                .map(|(c, p)| (c.to_string(), p.to_string()))
                .collect(),
        }
    }

    #[test]
    fn parents_children_subtree() {
        let t = tree(&[
            ("api", "main"),
            ("ui", "api"),
            ("web", "api"),
            ("css", "ui"),
        ]);
        assert_eq!(t.parent("ui"), Some("api"));
        assert_eq!(t.parent("main"), None);
        assert_eq!(t.children("api"), ["ui", "web"]);
        assert_eq!(t.subtree("api"), ["ui", "css", "web"]);
        assert_eq!(t.subtree("main"), ["api", "ui", "css", "web"]);
        assert!(t.subtree("css").is_empty());
    }

    #[test]
    fn parents_refused() {
        let t = tree(&[("api", "main"), ("ui", "api")]);
        assert_eq!(t.check_parent("api", "api"), Err(Refused::Own));
        assert_eq!(t.check_parent("api", "ui"), Err(Refused::Cycle));
        assert_eq!(t.check_parent("ui", "main"), Ok(()));
        assert_eq!(t.check_parent("web", "ui"), Ok(()));
        // A cycle already in the config (hand-edited): no endless loop.
        let looped = tree(&[("a", "b"), ("b", "a")]);
        assert_eq!(looped.check_parent("c", "a"), Ok(()));
        assert_eq!(looped.subtree("a"), ["b"]);
    }
}
