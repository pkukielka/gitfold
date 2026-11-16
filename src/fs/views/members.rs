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

/// The remote-tracking branch of the root `branch` on `origin`, if `branch`
/// is the tree's root and it exists: branches built on it are in the
/// root's view too (only origin's: decided 2026-11-16).
pub fn origin_root(tree: &Tree, branch: &str) -> Option<String> {
    if tree.root() != Some(branch) {
        return None;
    }
    let full = format!("refs/remotes/origin/{branch}");
    (git::full_ref(&full).as_ref() == Some(&full)).then_some(full)
}

/// The branches the view of local branch `branch` merges in, every branch
/// before those built on it:
/// - for the root, those built on it (on its local branch, or on its
///   remote-tracking branch `origin_root`) and on them, but only those
///   opened in gitfold (`opened`: with a workspace), and those an opened one
///   is built on, so its work isn't missing. Every branch of the repository
///   would be a mess (decided 2026-11-16);
/// - for any other branch, everything built on it.
pub fn members(
    tree: &Tree,
    branch: &str,
    origin_root: Option<&str>,
    opened: impl Fn(&str) -> bool,
) -> Vec<BuiltOn> {
    let is_root = tree.root() == Some(branch);
    let names = if is_root {
        let mut refs = vec![format!("refs/heads/{branch}")];
        refs.extend(origin_root.map(String::from));
        tree.built_on(&refs)
    } else {
        tree.subtree(branch)
    };
    let shown = |name: &str| opened(name) || tree.subtree(name).into_iter().any(&opened);
    (names.into_iter())
        .filter(|name| !is_root || shown(name))
        .filter_map(|name| {
            Some(BuiltOn {
                branch: name.to_string(),
                parent: tree.parent(name)?.to_string(),
            })
        })
        .collect()
}

/// The committed view's inputs.
pub struct Committed {
    pub files: Files,
    pub members: Vec<Member>,
    /// Members whose tip or parent doesn't resolve (deleted, pruned), with
    /// why: left out of the view.
    pub skipped: Vec<(String, String)>,
}

/// The committed view's inputs: the files of `branch`, and each member's tip
/// with its merge point (the merge base of its tip and its parent's; none
/// for unrelated histories: everything is its change). The files of the
/// root are its local tip, or `origin_root`'s if the local one is only
/// behind it (an ancestor).
pub fn committed(
    store: &Store,
    branch: &str,
    origin_root: Option<&str>,
    members: &[BuiltOn],
) -> Result<Committed> {
    let files = |tree: u32| Files {
        tree,
        uncommitted: Changes::new(),
    };
    let mut own = git::branch_tip(branch).with_context(|| format!("branch {branch}"))?;
    if let Some(remote) = origin_root.and_then(|r| git::resolve(r).ok())
        && merge_base(own.id, remote.id)? == Some(own.id)
    {
        own = remote;
    }
    let mut out = Committed {
        files: files(store.load_tree(own.tree)?),
        members: Vec::with_capacity(members.len()),
        skipped: Vec::new(),
    };
    for m in members {
        let refs = git::branch_tip(&m.branch)
            .with_context(|| format!("branch {}", m.branch))
            .and_then(|at| {
                let parent = (git::resolve(&m.parent))
                    .with_context(|| format!("its parent {}", m.parent))?;
                Ok((at, parent))
            });
        let (at, parent) = match refs {
            Ok(refs) => refs,
            Err(e) => {
                out.skipped.push((m.branch.clone(), format!("{e:#}")));
                continue;
            }
        };
        let merge_point = match merge_base(at.id, parent.id)? {
            Some(base) => store.load_tree(git::with_repo(|repo| commit_tree(repo, base))?)?,
            None => EMPTY_TREE,
        };
        out.members.push(Member {
            branch: m.branch.clone(),
            merge_point,
            files: files(store.load_tree(at.tree)?),
        });
    }
    Ok(out)
}

/// The best merge base of commits `a` and `b`, as `git merge-base` finds it.
fn merge_base(a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    git::with_repo(|repo| Ok(repo.merge_base(a, b)?.map(|id| id.detach())))
}

fn commit_tree(repo: &gix::Repository, commit: ObjectId) -> Result<ObjectId> {
    Ok(repo.find_commit(commit)?.tree_id()?.detach())
}

#[cfg(test)]
mod tests {
    use super::super::test_repo::{branch, commit, remote_branch};
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
        let opened = |b: &str| ["api", "css", "cli"].contains(&b);
        let root = members(&t, "main", Some("refs/remotes/origin/main"), opened);
        assert_eq!(
            names(&root),
            ["api", "ui", "css", "cli"],
            "those opened, and ui: css is built on it"
        );
        assert_eq!(root[3].parent, "refs/remotes/origin/main");
        let local_only = members(&t, "main", None, opened);
        assert_eq!(names(&local_only), ["api", "ui", "css"]);
        let none = members(&t, "main", None, |b| b == "api");
        assert_eq!(names(&none), ["api"], "ui only for an opened branch on it");
    }

    #[test]
    fn root_files_and_skipped_members() {
        let store = Store::default();
        let local = commit(None, &[("a", Some("1\n"))]);
        let ahead = commit(Some(&local), &[("a", Some("2\n"))]);
        branch("m1-main", &local);
        remote_branch("origin/m1-main", &ahead);
        let api = BuiltOn {
            branch: "m1-api".into(),
            parent: "refs/heads/m1-main".into(),
        };
        branch("m1-api", &local);
        let gone = BuiltOn {
            branch: "m1-gone".into(),
            parent: "refs/heads/m1-main".into(),
        };
        let orphan = BuiltOn {
            branch: "m1-api".into(),
            parent: "refs/heads/m1-nosuch".into(),
        };
        let remote = Some("refs/remotes/origin/m1-main");
        let tree_of = |c: &str| store.load_tree(git::resolve(c).unwrap().tree).unwrap();
        let c = committed(&store, "m1-main", remote, &[api, gone, orphan]).unwrap();
        assert_eq!(
            c.files.tree,
            tree_of(&ahead),
            "local main only behind: origin's"
        );
        assert_eq!(names_of(&c), ["m1-api"]);
        let skipped: Vec<&str> = c.skipped.iter().map(|(b, _)| b.as_str()).collect();
        assert_eq!(skipped, ["m1-gone", "m1-api"]);
        assert!(c.skipped[1].1.contains("m1-nosuch"), "{}", c.skipped[1].1);

        // Local main with its own commit: its own files.
        let own = commit(Some(&local), &[("b", Some("b\n"))]);
        branch("m1-main", &own);
        let c = committed(&store, "m1-main", remote, &[]).unwrap();
        assert_eq!(c.files.tree, tree_of(&own));
    }

    #[test]
    fn origins_root() {
        let c = commit(None, &[("a", Some("1\n"))]);
        let t = Tree::of(Default::default(), Some("m2-main".into()), |_| true);
        assert_eq!(origin_root(&t, "m2-main"), None, "no origin/m2-main yet");
        remote_branch("origin/m2-main", &c);
        remote_branch("upstream/m2-main", &c);
        let full = Some("refs/remotes/origin/m2-main".to_string());
        assert_eq!(origin_root(&t, "m2-main"), full);
        assert_eq!(origin_root(&t, "m2-api"), None, "not the root");
    }

    fn names_of(c: &Committed) -> Vec<&str> {
        c.members.iter().map(|m| m.branch.as_str()).collect()
    }
}
