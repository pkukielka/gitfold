//! Which branches a view merges in (`members`), and the refs it is made of
//! now (`refs`, compared between refreshes): each member's tip and its
//! parent's, whose merge base (`Bases`) is where it left it.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use gix::ObjectId;

use crate::git;
use crate::tree::Tree;

/// A branch a view merges in, and its parent (a full ref).
#[derive(Clone, Debug, PartialEq)]
pub struct BuiltOn {
    pub branch: String,
    pub parent: String,
}

/// The remote-tracking branch of the root `branch` on `origin`, if `branch`
/// is the tree's root and it exists: branches built on it are in the
/// root's view too (only origin's: decided 2026-11-16).
pub fn origin_root(tree: &Tree, branch: &str) -> Option<String> {
    // Not `git::remote_tracking`: it goes by the configured remotes, which
    // the mount's repository handle has as they were at mount time.
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
    // Children come after their parents: from the end, a branch is kept if
    // it is opened or a child is kept.
    let mut kept: HashSet<&str> = HashSet::new();
    for &name in names.iter().rev() {
        if !is_root || opened(name) || tree.children(name).iter().any(|c| kept.contains(c)) {
            kept.insert(name);
        }
    }
    (names.into_iter())
        .filter(|name| kept.contains(name))
        .filter_map(|name| {
            Some(BuiltOn {
                branch: name.to_string(),
                parent: tree.parent(name)?.to_string(),
            })
        })
        .collect()
}

/// A commit and its tree.
pub type Tip = (ObjectId, ObjectId);

/// Something a view leaves out or can't show as asked, said in its summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    /// `skipped` (a member left out) or `unread` (a ref that exists but
    /// can't be read).
    pub kind: &'static str,
    /// The branch or ref.
    pub what: String,
    pub why: String,
}

/// The refs a committed view is made of, resolved once: compared between
/// refreshes to see whether it changed, and merged (`merge`).
#[derive(Clone, Debug, PartialEq)]
pub struct Refs {
    /// The view's own files: the branch's tip, or its origin root's.
    pub own: Tip,
    /// The branch's tip.
    pub local: Tip,
    /// Each member, its tip, and its merge point with its parent (where it
    /// left it); skipped members aren't here (`notes`).
    pub members: Vec<(String, Tip, ObjectId)>,
    pub notes: Vec<Note>,
}

/// Merge bases already found, by the two commits: kept between refreshes,
/// for those still asked (`next_round`).
#[derive(Default)]
pub struct Bases {
    now: HashMap<(ObjectId, ObjectId), Option<ObjectId>>,
    before: HashMap<(ObjectId, ObjectId), Option<ObjectId>>,
}

impl Bases {
    /// The best merge base of commits `a` and `b`, as `git merge-base` finds
    /// it.
    pub fn get(&mut self, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
        if let Some(&base) = self.now.get(&(a, b)) {
            return Ok(base);
        }
        let base = match self.before.remove(&(a, b)) {
            Some(base) => base,
            None => git::with_repo(|repo| -> Result<Option<ObjectId>> {
                Ok(repo.merge_base(a, b)?.map(|id| id.detach()))
            })?,
        };
        self.now.insert((a, b), base);
        Ok(base)
    }

    /// Forgets those not asked since the last call.
    pub fn next_round(&mut self) {
        self.before = std::mem::take(&mut self.now);
    }
}

/// The refs of the view of `branch` with `members` now. The root's files are
/// its local tip, or `origin_root`'s if the local one is only behind it (an
/// ancestor). Missing refs leave members out (and what is built on them,
/// whose work is measured against them), noted; refs that exist but can't
/// be read fail the view, except `origin_root`'s, noted (the local tip
/// stays).
pub fn refs(
    branch: &str,
    origin_root: Option<&str>,
    members: &[BuiltOn],
    bases: &mut Bases,
) -> Result<Refs> {
    let exists = |full: &str| git::full_ref(full).is_some();
    let tip = |full: &str| -> Result<Tip> {
        let c = git::resolve(full).with_context(|| format!("reading {full}"))?;
        Ok((c.id, c.tree))
    };
    let mut own = tip(&format!("refs/heads/{branch}"))?;
    let local = own;
    let mut notes = Vec::new();
    // The origin root, if it exists but can't be read: noted, not failing.
    let mut unread: Option<&str> = None;
    if let Some(remote) = origin_root.filter(|r| exists(r)) {
        let behind = tip(remote).and_then(|r| Ok((r, bases.get(own.0, r.0)?)));
        match behind {
            Ok((remote, base)) if base == Some(own.0) => own = remote,
            Ok(_) => {}
            Err(e) => {
                unread = Some(remote);
                notes.push(Note {
                    kind: "unread",
                    what: remote.to_string(),
                    why: format!("{e:#}; the view shows {branch}'s own files"),
                });
            }
        }
    }
    let mut out = Vec::with_capacity(members.len());
    let mut skipped: Vec<&str> = Vec::new();
    // `members` has every branch before those built on it (`members()`).
    for m in members {
        let full = format!("refs/heads/{}", m.branch);
        let on_skipped =
            (m.parent.strip_prefix("refs/heads/")).and_then(|p| skipped.iter().find(|s| **s == p));
        let mut why = match on_skipped {
            Some(parent) => Some(format!("built on {parent}, left out")),
            None if !exists(&full) => Some(format!("no branch {}", m.branch)),
            None if !exists(&m.parent) => Some(format!("its parent {} is gone", m.parent)),
            None if unread == Some(m.parent.as_str()) => {
                Some(format!("its parent {} can't be read", m.parent))
            }
            None => None,
        };
        if why.is_none() {
            match tip(&full).and_then(|t| Ok((t, tip(&m.parent)?.0))) {
                // Unrelated histories: nothing to tell its work by.
                Ok((at, parent)) => match bases.get(at.0, parent)? {
                    Some(base) => out.push((m.branch.clone(), at, base)),
                    None => why = Some(format!("no history in common with {}", m.parent)),
                },
                // Deleted meanwhile: as if it had been gone.
                Err(_) if !exists(&full) => why = Some(format!("no branch {}", m.branch)),
                Err(_) if !exists(&m.parent) => {
                    why = Some(format!("its parent {} is gone", m.parent))
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(why) = why {
            skipped.push(&m.branch);
            notes.push(Note {
                kind: "skipped",
                what: m.branch.clone(),
                why,
            });
        }
    }
    Ok(Refs {
        own,
        local,
        members: out,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_repo::{branch, commit, remote_branch, write_ref};
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

    fn built_on(branch: &str, parent: &str) -> BuiltOn {
        BuiltOn {
            branch: branch.into(),
            parent: format!("refs/heads/{parent}"),
        }
    }

    #[test]
    fn root_files_and_skipped_members() {
        let mut bases = Bases::default();
        let local = commit(None, &[("a", Some("1\n"))]);
        let ahead = commit(Some(&local), &[("a", Some("2\n"))]);
        branch("m1-main", &local);
        remote_branch("origin/m1-main", &ahead);
        branch("m1-api", &local);
        branch("m1-ui", &local);
        branch("m1-orphan", &local);
        let members = [
            built_on("m1-api", "m1-main"),
            built_on("m1-gone", "m1-main"),
            built_on("m1-ui", "m1-gone"),
            built_on("m1-orphan", "m1-nosuch"),
        ];
        let remote = Some("refs/remotes/origin/m1-main");
        let tree_of = |c: &str| git::resolve(c).unwrap().tree;
        let r = refs("m1-main", remote, &members, &mut bases).unwrap();
        assert_eq!(r.own.1, tree_of(&ahead), "local main only behind: origin's");
        let kept: Vec<&str> = r.members.iter().map(|m| m.0.as_str()).collect();
        assert_eq!(kept, ["m1-api"]);
        let notes: Vec<(&str, &str)> = (r.notes.iter())
            .map(|n| (n.what.as_str(), n.why.as_str()))
            .collect();
        assert_eq!(
            notes,
            [
                ("m1-gone", "no branch m1-gone"),
                ("m1-ui", "built on m1-gone, left out"),
                ("m1-orphan", "its parent refs/heads/m1-nosuch is gone"),
            ]
        );
        assert!(r.notes.iter().all(|n| n.kind == "skipped"));

        // The same commit, or local main ahead: its own files.
        branch("m1-main", &ahead);
        let own = |bases: &mut Bases| refs("m1-main", remote, &[], bases).unwrap().own.1;
        assert_eq!(own(&mut bases), tree_of(&ahead));
        let further = commit(Some(&ahead), &[("c", Some("c\n"))]);
        branch("m1-main", &further);
        assert_eq!(own(&mut bases), tree_of(&further));
        // Local main with its own commit, origin with others: its own files.
        let mine = commit(Some(&local), &[("b", Some("b\n"))]);
        branch("m1-main", &mine);
        assert_eq!(own(&mut bases), tree_of(&mine));
    }

    #[test]
    fn unreadable_refs() {
        let mut bases = Bases::default();
        let c = commit(None, &[("a", Some("1\n"))]);
        branch("m3-main", &c);
        branch("m3-api", &c);
        // A ref to an object that isn't there: it exists, but can't be read.
        let missing = "1234567890123456789012345678901234567890";
        write_ref("refs/remotes/origin/m3-main", missing);
        let remote = Some("refs/remotes/origin/m3-main");
        let r = refs("m3-main", remote, &[], &mut bases).unwrap();
        assert_eq!(r.own.0.to_string(), c, "the local tip");
        assert_eq!(r.notes.len(), 1);
        assert_eq!(r.notes[0].kind, "unread");
        assert_eq!(r.notes[0].what, "refs/remotes/origin/m3-main");
        // A branch built on it is left out, noted: the view still shows.
        branch("m3-cli", &c);
        let on_origin = BuiltOn {
            branch: "m3-cli".into(),
            parent: "refs/remotes/origin/m3-main".into(),
        };
        let r = refs("m3-main", remote, &[on_origin], &mut bases).unwrap();
        assert!(r.members.is_empty());
        assert_eq!(r.notes[1].what, "m3-cli");
        assert!(
            r.notes[1].why.contains("can't be read"),
            "{}",
            r.notes[1].why
        );
        write_ref("refs/heads/m3-api", missing);
        let failed = refs(
            "m3-main",
            None,
            &[built_on("m3-api", "m3-main")],
            &mut bases,
        );
        assert!(
            failed.is_err(),
            "a member that can't be read fails the view"
        );
    }

    #[test]
    fn origins_root() {
        let c = commit(None, &[("a", Some("1\n"))]);
        let t = Tree::of(Default::default(), Some("m2-main".into()), |_| true);
        remote_branch("upstream/m2-main", &c);
        assert_eq!(origin_root(&t, "m2-main"), None, "only origin's counts");
        remote_branch("origin/m2-main", &c);
        let full = Some("refs/remotes/origin/m2-main".to_string());
        assert_eq!(origin_root(&t, "m2-main"), full);
        assert_eq!(origin_root(&t, "m2-api"), None, "not the root");
    }
}
