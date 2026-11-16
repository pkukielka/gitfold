//! Branches built on branches: each branch's parent (setting
//! `branch.<branch>.gitfoldParent`, a full ref: a local or a
//! remote-tracking branch) and the tree the parents form, rooted at the main
//! branch (`git::root_branch`). The rules are the case table in the blog
//! notes, layers-design/16-root-rules.md (row names in comments: R, P, S, N,
//! F, T); the tree owns them: only existing branches, the root has no
//! parent, no branch is its own parent, no cycles (`Tree::check`,
//! `Tree::check_new`), and parent names resolve one way (`resolve_parent`).
//! Writing parents and following comes with `gitfold parent` (stream 4).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::git;
use crate::settings::Local;

/// The repository's branches built on branches, as its config has them.
#[derive(Debug, Default)]
pub struct Tree {
    /// Local branch to its parent's full ref: only branches that exist,
    /// never the root.
    parents: BTreeMap<String, String>,
    /// A parent's full ref to its children (local branch names), by name.
    children: BTreeMap<String, Vec<String>>,
    /// The root (`git::root_branch`), if known.
    root: Option<String>,
}

/// Why a parent can't be given or set.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// No such branch (P6, S1).
    Unknown(String),
    /// A name git takes for a ref that isn't a branch: a tag, `stash` (P6).
    NotBranch { name: String, full: String },
    /// No branch of that name, but a remote-tracking one of a configured
    /// remote (P5).
    OnlyRemote { name: String, remote: String },
    /// No root known: no main branch to build the tree on (S3, N3).
    NoRoot,
    /// The branch is the root, which has no parent (S4, N4).
    Root,
    /// The branch itself (S5).
    Own,
    /// A branch built on it (S6).
    Cycle,
    /// `--follow`/`--no-follow` on a branch without a parent (F6).
    NoParent,
}

/// How `refused` reads for `parent` as the parent of `branch`.
pub fn refusal(branch: &str, parent: &str, refused: &Refused) -> String {
    match refused {
        Refused::Unknown(name) => format!("no branch {name}"),
        Refused::NotBranch { name, full } => format!("no branch {name}: git takes it for {full}"),
        Refused::OnlyRemote { name, remote } => format!(
            "no branch {name} (only {remote} exists). Either: `git branch {name} {remote}` (a \
             local {name}), then retry; or retry with {remote} (build on the remote-tracking \
             branch)"
        ),
        Refused::NoRoot => "no main branch found (no origin/HEAD, and no branch main, master, \
             origin/main or origin/master): the tree of branches has no root to build on"
            .to_string(),
        Refused::Root => format!("{branch} is the root of the tree; it can't have a parent"),
        Refused::Own => "a branch can't be its own parent".to_string(),
        Refused::Cycle => format!("{parent} is built on {branch}: that would be a cycle"),
        Refused::NoParent => {
            format!("{branch} has no parent to follow: `gitfold parent {branch} --set P` first")
        }
    }
}

/// The full ref of local branch `name`.
fn local(name: &str) -> String {
    format!("refs/heads/{name}")
}

/// A full ref as shown: `api` for `refs/heads/api`, `origin/main` for
/// `refs/remotes/origin/main`.
pub fn short(full: &str) -> &str {
    (full.strip_prefix("refs/heads/"))
        .or_else(|| full.strip_prefix("refs/remotes/"))
        .unwrap_or(full)
}

/// The full ref a parent named `name` is (P1-P6): what git takes the name
/// for (`git::full_ref`), if that is a local or a remote-tracking branch.
fn resolve_parent(name: &str) -> Result<String, Refused> {
    match git::full_ref(name) {
        Some(full) if full.starts_with("refs/heads/") || full.starts_with("refs/remotes/") => {
            Ok(full)
        }
        Some(full) => Err(Refused::NotBranch {
            name: name.to_string(),
            full,
        }),
        None => match git::remote_tracking(name) {
            Some(remote) => Err(Refused::OnlyRemote {
                name: name.to_string(),
                remote: short(&remote).to_string(),
            }),
            None => Err(Refused::Unknown(name.to_string())),
        },
    }
}

/// The local branch named `name`, as git takes it: itself, or the branch
/// it is an alias of (`git symbolic-ref`), if it exists.
pub fn local_branch(name: &str) -> Option<String> {
    let full = git::full_ref(&local(name))?;
    full.strip_prefix("refs/heads/").map(String::from)
}

impl Tree {
    /// The tree of the repository of `common_dir` (`git::init` done); an
    /// unreadable config fails (T5).
    pub fn read(common_dir: &Path) -> Result<Tree> {
        let parents = Local::read_checked(common_dir)?.parents();
        Ok(Tree::of(
            parents,
            git::root_branch(common_dir),
            git::branch_exists,
        ))
    }

    /// The tree of `stored` (branch to the parent as stored) rooted at
    /// `root`, of the local branches `exists` says exist: a deleted branch's
    /// settings may outlive it (T2), and a parent stored on the root doesn't
    /// count (T1). A stored short value is a local branch.
    pub(crate) fn of(
        stored: BTreeMap<String, String>,
        root: Option<String>,
        exists: impl Fn(&str) -> bool,
    ) -> Tree {
        let parents: BTreeMap<String, String> = stored
            .into_iter()
            .filter(|(branch, _)| exists(branch) && Some(branch) != root.as_ref())
            .map(|(branch, parent)| match parent.starts_with("refs/") {
                true => (branch, parent),
                false => (branch, local(&parent)),
            })
            .collect();
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

    /// The parent of local branch `branch`, as a full ref, if it has one.
    pub fn parent(&self, branch: &str) -> Option<&str> {
        self.parents.get(branch).map(String::as_str)
    }

    /// The root (`git::root_branch`), if known.
    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    /// The local branches whose parent is local branch `branch`, by name.
    pub fn children(&self, branch: &str) -> Vec<&str> {
        self.children_of(&local(branch))
    }

    /// The local branches whose parent is `full` (the full ref of a local or
    /// a remote-tracking branch), by name.
    fn children_of(&self, full: &str) -> Vec<&str> {
        (self.children.get(full))
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// Everything built on local branch `branch`: its children, theirs, and
    /// so on, each once, every branch before its children (`branch` itself
    /// not included; safe on cycles a hand-edited config may hold, T4).
    pub fn subtree(&self, branch: &str) -> Vec<&str> {
        self.built_on(&[local(branch)])
    }

    /// `subtree` of several branches at once, given by full ref (local or
    /// remote-tracking), in their order: each branch once, those among
    /// `refs` not included.
    pub fn built_on(&self, refs: &[String]) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = (refs.iter())
            .filter_map(|r| r.strip_prefix("refs/heads/"))
            .collect();
        let mut stack: Vec<&str> = (refs.iter().rev())
            .flat_map(|r| self.children_of(r).into_iter().rev())
            .collect();
        while let Some(next) = stack.pop() {
            if seen.insert(next) {
                out.push(next);
                stack.extend(self.children(next).into_iter().rev());
            }
        }
        out
    }

    /// The full ref of the parent named `parent`, if the parent of local
    /// branch `branch` may become it: the name resolves (S2), a root is known
    /// and `branch` isn't it, `parent` isn't `branch`, nor built on it
    /// (S3-S6).
    pub fn check(&self, branch: &str, parent: &str) -> Result<String, Refused> {
        let full = resolve_parent(parent)?;
        self.check_ancestry(branch, &full)?;
        Ok(full)
    }

    fn check_ancestry(&self, branch: &str, parent: &str) -> Result<(), Refused> {
        self.check_root(branch)?;
        let own = local(branch);
        if parent == own {
            return Err(Refused::Own);
        }
        // Up the stored parents; a remote-tracking parent ends the walk, and
        // so does the bound, should a hand-edited config hold a cycle (T4).
        let ancestors = std::iter::successors(Some(parent), |at| {
            self.parent(at.strip_prefix("refs/heads/")?)
        });
        match ancestors.take(self.parents.len() + 1).any(|at| at == own) {
            true => Err(Refused::Cycle),
            false => Ok(()),
        }
    }

    /// The full ref of the parent named `parent`, if a new local branch
    /// `branch` may be created built on it (N2-N4): the name resolves, a root
    /// is known and `branch` isn't its name. Nothing else can go wrong for a
    /// branch that doesn't exist yet: it has no children.
    pub fn check_new(&self, branch: &str, parent: &str) -> Result<String, Refused> {
        let full = resolve_parent(parent)?;
        self.check_root(branch)?;
        Ok(full)
    }

    /// Whether local branch `branch` has a parent to follow (F6): it isn't
    /// the root (S4), and it has a parent. (Following needs no root.)
    pub fn check_follow(&self, branch: &str) -> Result<(), Refused> {
        if self.root.as_deref() == Some(branch) {
            return Err(Refused::Root);
        }
        match self.parent(branch) {
            Some(_) => Ok(()),
            None => Err(Refused::NoParent),
        }
    }

    fn check_root(&self, branch: &str) -> Result<(), Refused> {
        match &self.root {
            None => Err(Refused::NoRoot),
            Some(root) if root == branch => Err(Refused::Root),
            Some(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(c, p)| (c.to_string(), p.to_string()))
            .collect()
    }

    fn tree(pairs: &[(&str, &str)]) -> Tree {
        Tree::of(stored(pairs), Some("main".to_string()), |_| true)
    }

    #[test]
    fn parents_children_subtree() {
        let t = tree(&[
            ("api", "refs/heads/main"),
            ("ui", "refs/heads/api"),
            ("web", "api"),
            ("css", "refs/heads/ui"),
            ("cli", "refs/remotes/origin/main"),
        ]);
        assert_eq!(t.parent("ui"), Some("refs/heads/api"));
        assert_eq!(
            t.parent("web"),
            Some("refs/heads/api"),
            "a short value is local"
        );
        assert_eq!(t.parent("main"), None);
        assert_eq!(t.children("api"), ["ui", "web"]);
        assert_eq!(t.subtree("api"), ["ui", "css", "web"]);
        assert_eq!(t.subtree("main"), ["api", "ui", "css", "web"]);
        assert!(t.subtree("css").is_empty());
        let main = ["refs/heads/main", "refs/remotes/origin/main"].map(String::from);
        assert_eq!(t.built_on(&main), ["api", "ui", "css", "web", "cli"]);
        assert_eq!(short("refs/remotes/origin/main"), "origin/main");
    }

    #[test]
    fn stored_rules() {
        // T1: a parent stored on the root; T2: a deleted branch's leftover.
        let t = Tree::of(
            stored(&[
                ("main", "refs/heads/api"),
                ("api", "refs/heads/main"),
                ("gone", "refs/heads/api"),
            ]),
            Some("main".to_string()),
            |b| b != "gone",
        );
        assert_eq!(t.parent("main"), None);
        assert!(t.children("api").is_empty());
        assert_eq!(t.subtree("main"), ["api"]);
    }

    #[test]
    fn checks() {
        let t = tree(&[("api", "refs/heads/main"), ("ui", "refs/heads/api")]);
        assert_eq!(
            t.check_ancestry("main", "refs/heads/api"),
            Err(Refused::Root)
        );
        assert_eq!(t.check_ancestry("api", "refs/heads/api"), Err(Refused::Own));
        assert_eq!(
            t.check_ancestry("api", "refs/heads/ui"),
            Err(Refused::Cycle)
        );
        assert_eq!(t.check_ancestry("ui", "refs/heads/main"), Ok(()));
        assert_eq!(t.check_ancestry("ui", "refs/remotes/origin/main"), Ok(()));
        assert_eq!(t.check_root("main"), Err(Refused::Root));
        assert_eq!(t.check_root("web"), Ok(()));
        assert_eq!(t.check_follow("ui"), Ok(()));
        assert_eq!(t.check_follow("api"), Ok(()));
        assert_eq!(t.check_follow("main"), Err(Refused::Root));
        assert_eq!(
            Tree::of(stored(&[("ui", "api")]), None, |_| true).check_follow("ui"),
            Ok(())
        );
        let rootless = Tree::of(BTreeMap::new(), None, |_| true);
        assert_eq!(
            rootless.check_ancestry("ui", "refs/heads/api"),
            Err(Refused::NoRoot)
        );
        assert_eq!(rootless.check_root("web"), Err(Refused::NoRoot));
        // T4: a cycle already in the config: no endless loop.
        let looped = Tree::of(
            stored(&[("a", "refs/heads/b"), ("b", "refs/heads/a")]),
            Some("main".to_string()),
            |_| true,
        );
        assert_eq!(looped.check_ancestry("c", "refs/heads/a"), Ok(()));
        assert_eq!(looped.subtree("a"), ["b"]);
    }
}
