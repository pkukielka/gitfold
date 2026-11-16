//! Branches built on branches: each branch's parent and whether it follows
//! it (settings: `branch.<branch>.gitfoldParent`, `gitfoldFollow`), and the
//! tree the parents form, rooted at the main branch (`git::root_branch`).
//! The tree owns its rules: only branches that exist, the root has no
//! parent (and is never stored as one), no branch is its own parent, no
//! cycles (`Tree::check`, `Tree::check_new`, which every command applies).
//! Writing parents and following comes with `gitfold parent` (stream 4). Shared by `gitfold parent`,
//! `branch --parent`, `list`, `changes -- PATH` and the views; see
//! layers-design/15-tree-spec.md in the blog notes for the design.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::git;
use crate::settings::Local;

/// The repository's branches built on branches, as its config has them.
#[derive(Debug, Default)]
pub struct Tree {
    /// Branch to parent: only branches that exist, never the root.
    parents: BTreeMap<String, String>,
    /// Parent to its children, by name.
    children: BTreeMap<String, Vec<String>>,
    /// The root (`git::root_branch`), if known.
    root: Option<String>,
}

/// Why a branch can't get a parent.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// No such branch (either side).
    Unknown(String),
    /// No root is known: no main branch to build the tree on.
    NoRoot,
    /// The branch is the root, which has no parent.
    Root,
    /// The branch itself.
    Own,
    /// A branch built on it.
    Cycle,
}

/// How `refused` reads for `parent` as the parent of `branch`.
pub fn refusal(branch: &str, parent: &str, refused: &Refused) -> String {
    match refused {
        Refused::Unknown(name) => format!("no branch {name}"),
        Refused::NoRoot => "no main branch found (no origin/HEAD, and no branch main, master, \
             origin/main or origin/master): the tree of branches has no root to build on"
            .to_string(),
        Refused::Root => format!("{branch} is the root of the tree; it can't have a parent"),
        Refused::Own => "a branch can't be its own parent".to_string(),
        Refused::Cycle => format!("{parent} is built on {branch}: that would be a cycle"),
    }
}

impl Tree {
    /// The tree of the repository of `common_dir` (`git::init` done).
    pub fn read(common_dir: &Path) -> Tree {
        Tree::of(
            Local::read(common_dir).parents(),
            git::root_branch(common_dir),
            git::branch_exists,
        )
    }

    /// The tree of `parents` (branch to parent) rooted at `root`, of the
    /// branches `exists` says exist: a deleted branch's settings may outlive
    /// it, and a parent stored on the root (by hand, or an older build)
    /// doesn't count.
    fn of(
        mut parents: BTreeMap<String, String>,
        root: Option<String>,
        exists: impl Fn(&str) -> bool,
    ) -> Tree {
        parents.retain(|branch, _| exists(branch) && Some(branch) != root.as_ref());
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (child, parent) in &parents {
            children
                .entry(parent.clone())
                .or_default()
                .push(child.clone());
        }
        Tree {
            parents,
            children,
            root,
        }
    }

    /// The parent of `branch`, if it has one.
    pub fn parent(&self, branch: &str) -> Option<&str> {
        self.parents.get(branch).map(String::as_str)
    }

    /// The branches whose parent is `branch`, by name.
    pub fn children(&self, branch: &str) -> Vec<&str> {
        (self.children.get(branch).into_iter().flatten())
            .map(String::as_str)
            .collect()
    }

    /// Everything built on `branch`: its children, theirs, and so on, each
    /// once, every branch before its children (`branch` itself not
    /// included; safe on cycles a hand-edited config may hold).
    #[allow(dead_code)] // used as the tree's streams land (the views)
    pub fn subtree(&self, branch: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let mut seen = std::collections::HashSet::from([branch]);
        let mut stack: Vec<&str> = self.children(branch).into_iter().rev().collect();
        while let Some(next) = stack.pop() {
            if seen.insert(next) {
                out.push(next);
                stack.extend(self.children(next).into_iter().rev());
            }
        }
        out
    }

    /// Whether `parent` may become the parent of `branch`: both exist, a
    /// root is known and `branch` isn't it, `parent` isn't `branch`, nor
    /// built on it (a cycle). The one check every way of setting a parent
    /// applies.
    pub fn check(&self, branch: &str, parent: &str) -> Result<(), Refused> {
        self.check_with(branch, parent, git::branch_exists)
    }

    /// Whether a new branch `branch` may be created built on `parent`
    /// (`gitfold branch NEW --parent P`): `parent` exists, a root is known,
    /// and `branch` isn't the root's name. The rest of `check` follows: a
    /// branch not created yet has no children, so no cycle.
    pub fn check_new(&self, branch: &str, parent: &str) -> Result<(), Refused> {
        self.check_new_with(branch, parent, git::branch_exists)
    }

    fn check_new_with(
        &self,
        branch: &str,
        parent: &str,
        exists: impl Fn(&str) -> bool,
    ) -> Result<(), Refused> {
        if !exists(parent) {
            return Err(Refused::Unknown(parent.to_string()));
        }
        match &self.root {
            None => Err(Refused::NoRoot),
            Some(root) if root == branch => Err(Refused::Root),
            Some(_) => Ok(()),
        }
    }

    fn check_with(
        &self,
        branch: &str,
        parent: &str,
        exists: impl Fn(&str) -> bool,
    ) -> Result<(), Refused> {
        for name in [branch, parent] {
            if !exists(name) {
                return Err(Refused::Unknown(name.to_string()));
            }
        }
        match &self.root {
            None => return Err(Refused::NoRoot),
            Some(root) if root == branch => return Err(Refused::Root),
            Some(_) => {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pairs: &[(&str, &str)]) -> Tree {
        let parents = pairs
            .iter()
            .map(|(c, p)| (c.to_string(), p.to_string()))
            .collect();
        Tree::of(parents, Some("main".to_string()), |_| true)
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
    fn the_tree_keeps_its_rules() {
        // A parent stored on the root, and a deleted branch's leftover setting.
        let parents = [("main", "api"), ("api", "main"), ("gone", "api")]
            .iter()
            .map(|(c, p)| (c.to_string(), p.to_string()))
            .collect();
        let t = Tree::of(parents, Some("main".to_string()), |b| b != "gone");
        assert_eq!(t.parent("main"), None);
        assert_eq!(t.children("api"), Vec::<&str>::new());
        assert_eq!(t.subtree("main"), ["api"]);
    }

    #[test]
    fn parents_refused() {
        let all = |_: &str| true;
        let t = tree(&[("api", "main"), ("ui", "api")]);
        assert_eq!(t.check_with("main", "api", all), Err(Refused::Root));
        assert_eq!(t.check_with("api", "api", all), Err(Refused::Own));
        assert_eq!(t.check_with("api", "ui", all), Err(Refused::Cycle));
        assert_eq!(t.check_with("ui", "main", all), Ok(()));
        assert_eq!(
            t.check_with("ui", "typo", |b| b != "typo"),
            Err(Refused::Unknown("typo".to_string()))
        );
        let rootless = Tree::of(BTreeMap::new(), None, all);
        assert_eq!(rootless.check_with("ui", "api", all), Err(Refused::NoRoot));
        // A new branch: its parent must exist, a root be known, and it can't be the root.
        assert_eq!(t.check_new_with("web", "api", all), Ok(()));
        assert_eq!(t.check_new_with("web", "main", all), Ok(()));
        assert_eq!(t.check_new_with("main", "api", all), Err(Refused::Root));
        assert_eq!(
            t.check_new_with("web", "typo", |b| b != "typo"),
            Err(Refused::Unknown("typo".to_string()))
        );
        assert_eq!(
            rootless.check_new_with("web", "api", all),
            Err(Refused::NoRoot)
        );
        // A cycle already in the config (hand-edited): no endless loop.
        let looped = Tree::of(
            [("a", "b"), ("b", "a")]
                .iter()
                .map(|(c, p)| (c.to_string(), p.to_string()))
                .collect(),
            Some("main".to_string()),
            all,
        );
        assert_eq!(looped.check_with("c", "a", all), Ok(()));
        assert_eq!(looped.subtree("a"), ["b"]);
    }
}
