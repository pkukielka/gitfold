//! Branches built on branches: each branch's parent and whether it follows
//! it (settings: `branch.<branch>.gitfoldParent`, `gitfoldFollow`), the tree
//! the parents form, and which branch is main. Shared by `gitfold parent`,
//! `branch --parent`, `list`, `changes -- PATH` and the views; see
//! layers-design/15-tree-spec.md in the blog notes for the design.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};

use crate::git;
use crate::settings::Local;

/// The parents of the repository's branches, as its config has them.
#[derive(Debug, Default)]
pub struct Tree {
    /// Branch to parent.
    parents: BTreeMap<String, String>,
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
        let mut children: Vec<&str> = self
            .parents
            .iter()
            .filter(|(_, parent)| parent.as_str() == branch)
            .map(|(child, _)| child.as_str())
            .collect();
        children.sort_unstable();
        children
    }

    /// Everything built on `branch`: its children, theirs, and so on, each
    /// once, parents before their children (`branch` itself not included;
    /// safe on cycles a hand-edited config may hold).
    #[allow(dead_code)] // used as the tree's streams land (the views)
    pub fn subtree(&self, branch: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let mut stack = self.children(branch);
        stack.reverse();
        while let Some(next) = stack.pop() {
            if next == branch || out.contains(&next) {
                continue;
            }
            out.push(next);
            let mut children = self.children(next);
            children.reverse();
            stack.extend(children);
        }
        out
    }

    /// Whether making `parent` the parent of `branch` would close a cycle:
    /// `parent` is `branch`, or is built on it.
    pub fn would_cycle(&self, branch: &str, parent: &str) -> bool {
        let mut at = Some(parent);
        let mut steps = 0;
        while let Some(name) = at {
            if name == branch {
                return true;
            }
            // A cycle already in the config: stop rather than loop.
            steps += 1;
            if steps > self.parents.len() {
                return false;
            }
            at = self.parent(name);
        }
        false
    }
}

/// The repository's main branch, what work lands in: the remote's default
/// branch (`origin/HEAD`, normally `origin/main`), else the local branch
/// `main`, else `master`. Not a branch's upstream: after `git push -u` that
/// is its own remote branch.
pub fn main_branch(common_dir: &Path) -> Result<String> {
    let out = git::cmd::command(common_dir)
        .args(["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .output()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !name.is_empty() && name != "origin/HEAD" {
        return Ok(name);
    }
    for name in ["main", "master"] {
        if git::branch_exists(name) {
            return Ok(name.to_string());
        }
    }
    bail!("no main branch: no origin/HEAD, and no branch main or master")
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
    fn cycles() {
        let t = tree(&[("api", "main"), ("ui", "api")]);
        assert!(t.would_cycle("api", "api"));
        assert!(t.would_cycle("api", "ui"));
        assert!(!t.would_cycle("ui", "main"));
        assert!(!t.would_cycle("web", "ui"));
        // A cycle already in the config (hand-edited): no endless loop.
        let looped = tree(&[("a", "b"), ("b", "a")]);
        assert!(!looped.would_cycle("c", "a"));
        assert_eq!(looped.subtree("a"), ["b"]);
    }
}
