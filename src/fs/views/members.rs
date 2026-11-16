//! Which branches a view merges in, and what each brings to the committed
//! view: its tip, against its merge point with its parent.

use anyhow::{Context, Result};
use gix::ObjectId;

use super::build::{Changes, Files, Member};
use crate::git;
use crate::store::{EMPTY_TREE, Store};
use crate::tree::Tree;

/// A branch a view merges in, and its parent (a full ref).
#[derive(Debug, PartialEq)]
pub struct BuiltOn {
    pub branch: String,
    pub parent: String,
}

/// The branches the view of local branch `branch` merges in, every branch
/// before those built on it:
/// - for the root, those built on it (on its local branch, or on its
///   remote-tracking branch `remote_root`) and on them, but only those opened
///   in gitfold (`opened`: with a workspace). Every branch of the repository
///   would be a mess (decided 2026-11-16);
/// - for any other branch, everything built on it.
pub fn members(
    tree: &Tree,
    branch: &str,
    remote_root: Option<&str>,
    opened: impl Fn(&str) -> bool,
) -> Vec<BuiltOn> {
    let is_root = tree.root() == Some(branch);
    let names = if is_root {
        let mut refs = vec![format!("refs/heads/{branch}")];
        refs.extend(remote_root.map(String::from));
        tree.built_on(&refs)
    } else {
        tree.subtree(branch)
    };
    (names.into_iter())
        .filter(|name| !is_root || opened(name))
        .filter_map(|name| {
            Some(BuiltOn {
                branch: name.to_string(),
                parent: tree.parent(name)?.to_string(),
            })
        })
        .collect()
}

/// The committed view's inputs: the tip of `branch`, and each member's tip
/// with its merge point (the merge base of its tip and its parent's; none
/// for unrelated histories: everything is its change).
pub fn committed(store: &Store, branch: &str, members: &[BuiltOn]) -> Result<(Files, Vec<Member>)> {
    let tip = |name: &str| git::branch_tip(name).with_context(|| format!("branch {name}"));
    let files = |tree: u32| Files {
        tree,
        uncommitted: Changes::new(),
    };
    let own = files(store.load_tree(tip(branch)?.tree)?);
    let mut out = Vec::with_capacity(members.len());
    for m in members {
        let at = tip(&m.branch)?;
        let parent = git::resolve(&m.parent).with_context(|| format!("parent {}", m.parent))?;
        let merge_point = match merge_base_tree(at.id, parent.id)? {
            Some(tree) => store.load_tree(tree)?,
            None => EMPTY_TREE,
        };
        out.push(Member {
            branch: m.branch.clone(),
            merge_point,
            files: files(store.load_tree(at.tree)?),
        });
    }
    Ok((own, out))
}

/// The tree of the best merge base of commits `a` and `b`, as `git
/// merge-base` finds it.
fn merge_base_tree(a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    git::with_repo(|repo| {
        let Some(base) = repo.merge_base(a, b)? else {
            return Ok(None);
        };
        Ok(Some(base.object()?.peel_to_commit()?.tree_id()?.detach()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pairs: &[(&str, &str)]) -> Tree {
        let stored = (pairs.iter())
            .map(|(c, p)| (c.to_string(), p.to_string()))
            .collect();
        Tree::of(stored, Some("main".into()), |_| true)
    }

    fn names(members: &[BuiltOn]) -> Vec<&str> {
        members.iter().map(|m| m.branch.as_str()).collect()
    }

    #[test]
    fn who_is_in_a_view() {
        let t = tree(&[
            ("api", "refs/heads/main"),
            ("ui", "refs/heads/api"),
            ("css", "refs/heads/ui"),
            ("cli", "refs/remotes/origin/main"),
            ("old", "refs/heads/main"),
        ]);
        let all = |_: &str| true;
        assert_eq!(names(&members(&t, "api", None, all)), ["ui", "css"]);
        assert_eq!(
            members(&t, "ui", None, all),
            [BuiltOn {
                branch: "css".into(),
                parent: "refs/heads/ui".into()
            }]
        );
        // Other branches' views ignore whether members are opened.
        assert_eq!(
            names(&members(&t, "api", None, |b| b != "css")),
            ["ui", "css"]
        );
        let opened = |b: &str| b != "old" && b != "ui";
        let root = members(&t, "main", Some("refs/remotes/origin/main"), opened);
        assert_eq!(names(&root), ["api", "css", "cli"], "only those opened");
        assert_eq!(root[2].parent, "refs/remotes/origin/main");
        let local_only = members(&t, "main", None, opened);
        assert_eq!(names(&local_only), ["api", "css"]);
    }
}
