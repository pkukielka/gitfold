//! Stacked branches on the git side. Branch b is built on a parent branch a
//! (`settings`: `branch.<b>.gitfoldParent`); its **parent base** B is the
//! commit of a that b's own commits sit on, kept by `refs/gitfold-stack/<b>`.
//! b's own commits are `B..b`, whatever happens to a later (amends, force
//! pushes): with only `merge-base(a, b)`, a rewritten parent would have its
//! old commits replayed too.
//!
//! Restacking b onto a's tip A copies each commit C of `B..b`, oldest first,
//! onto the copy of its parent: `merge_trees(base = C^, ours = the copy so
//! far, theirs = C)`, with C's author, committer and message
//! (`lens::copy_commit`). These are the commits `git rebase --onto A B b`
//! makes when it keeps the committer. A commit that becomes empty (a already
//! made the same change) is dropped, as `git rebase` drops it; a merge in
//! `B..b` is refused. Nothing moves here but in `move_refs`: b and its stack
//! ref, in one compare-and-swap transaction, so a crash can't leave b
//! restacked with B stale.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::BString;

use super::cmd;
use super::lens::{commit_tree, copy_commit, merge_trees};

/// The stack refs: `refs/gitfold-stack/<branch>` keeps a stacked branch's
/// parent base.
pub const STACK_REFS: &str = "refs/gitfold-stack/";

/// No object: in an `update-ref` transaction, "the ref must not exist".
const NO_REF: &str = "0000000000000000000000000000000000000000";

/// The ref keeping the parent base of `branch`.
pub fn stack_ref(branch: &str) -> String {
    format!("{STACK_REFS}{branch}")
}

/// A stacked branch's parent base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base {
    /// B: the commit the branch's own commits sit on.
    pub commit: ObjectId,
    /// What its stack ref holds now (`None`: no ref), for the transaction's
    /// compare-and-swap.
    pub stored: Option<ObjectId>,
    /// The ref was missing or not an ancestor of the branch (a `git branch
    /// -m` doesn't rename it), so B was guessed.
    pub guessed: bool,
}

/// The parent base of `branch` (at `tip`) on `parent` (at `parent_tip`): its
/// stack ref, if that is an ancestor of `tip`; else guessed by git's
/// `--fork-point` rule (the newest entry of the parent's reflog that is an
/// ancestor of the branch), else their merge base.
pub fn parent_base(
    common_dir: &Path,
    branch: &str,
    parent: &str,
    tip: ObjectId,
    parent_tip: ObjectId,
) -> Result<Base> {
    let stored = rev(common_dir, &stack_ref(branch))?;
    if let Some(commit) = stored
        && is_ancestor(common_dir, commit, tip)?
    {
        return Ok(Base {
            commit,
            stored,
            guessed: false,
        });
    }
    let parent_ref = format!("refs/heads/{parent}");
    let tip_hex = tip.to_string();
    let fork_point = cmd::command(common_dir)
        .args(["merge-base", "--fork-point", &parent_ref, &tip_hex])
        .output()?;
    let commit = match fork_point.status.success() {
        true => parse_id(&fork_point.stdout)?,
        false => merge_base_of(common_dir, parent_tip, tip)?,
    };
    Ok(Base {
        commit,
        stored,
        guessed: true,
    })
}

/// `git merge-base a b`.
fn merge_base_of(common_dir: &Path, a: ObjectId, b: ObjectId) -> Result<ObjectId> {
    let out = cmd::output(common_dir, &["merge-base", &a.to_string(), &b.to_string()])
        .with_context(|| format!("{a} and {b} have no common ancestor"))?;
    parse_id(&out)
}

/// The commit `spec` names, if it names one.
pub fn rev(common_dir: &Path, spec: &str) -> Result<Option<ObjectId>> {
    let out = cmd::command(common_dir)
        .args(["rev-parse", "-q", "--verify", &format!("{spec}^{{commit}}")])
        .output()?;
    match out.status.success() {
        true => Ok(Some(parse_id(&out.stdout)?)),
        false => Ok(None),
    }
}

fn parse_id(out: &[u8]) -> Result<ObjectId> {
    Ok(ObjectId::from_hex(
        String::from_utf8_lossy(out).trim().as_bytes(),
    )?)
}

/// Whether commit `a` is an ancestor of `b` (or `b` itself).
pub fn is_ancestor(common_dir: &Path, a: ObjectId, b: ObjectId) -> Result<bool> {
    let status = cmd::command(common_dir)
        .args([
            "merge-base",
            "--is-ancestor",
            &a.to_string(),
            &b.to_string(),
        ])
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base --is-ancestor {a} {b} failed ({status})"),
    }
}

/// How many commits `from..to` has.
pub fn count(common_dir: &Path, from: ObjectId, to: ObjectId) -> Result<usize> {
    let range = format!("{from}..{to}");
    let out = cmd::output(common_dir, &["rev-list", "--count", &range])?;
    Ok(String::from_utf8_lossy(&out).trim().parse()?)
}

/// What `restack` computed.
#[derive(Debug, PartialEq, Eq)]
pub enum Restack {
    /// The copies: `tip` is the new branch tip (C'), `copied` each commit
    /// with its copy, `dropped` the commits that became empty.
    Done {
        tip: ObjectId,
        copied: Vec<(ObjectId, ObjectId)>,
        dropped: Vec<ObjectId>,
    },
    /// `commit` conflicts with the parent's changes on `paths`.
    Conflict {
        commit: ObjectId,
        paths: Vec<BString>,
    },
    /// `commit` is a merge, which v1 doesn't restack.
    Merge(ObjectId),
}

/// Copies the commits of `base..tip` onto `onto` (see the module doc).
/// Writes objects only; no ref moves.
pub fn restack(
    repo: &gix::Repository,
    base: ObjectId,
    onto: ObjectId,
    tip: ObjectId,
) -> Result<Restack> {
    let range = format!("{base}..{tip}");
    let args = ["rev-list", "--reverse", "--topo-order", "--parents", &range];
    let out = cmd::output(repo.common_dir(), &args)?;
    let (mut copied, mut dropped) = (Vec::new(), Vec::new());
    let mut at = onto;
    for line in String::from_utf8_lossy(&out).lines() {
        let ids = line
            .split(' ')
            .map(|hex| ObjectId::from_hex(hex.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let (commit, parent) = match ids[..] {
            [commit, parent] => (commit, parent),
            [commit, ..] if ids.len() > 2 => return Ok(Restack::Merge(commit)),
            _ => bail!("{base} isn't an ancestor of {tip}"),
        };
        let (parent_tree, tree) = (commit_tree(repo, parent)?, commit_tree(repo, commit)?);
        let at_tree = commit_tree(repo, at)?;
        let merged = match merge_trees(repo, parent_tree, at_tree, tree)? {
            Ok(merged) => merged,
            Err(paths) => return Ok(Restack::Conflict { commit, paths }),
        };
        // Empty only now: the parent made its change already. A commit
        // that was empty all along is kept, as `git rebase` keeps it.
        if merged == at_tree && parent_tree != tree {
            dropped.push(commit);
            continue;
        }
        let copy = copy_commit(repo, commit, merged, at)?;
        copied.push((commit, copy));
        at = copy;
    }
    Ok(Restack::Done {
        tip: at,
        copied,
        dropped,
    })
}

/// Moves `branch` from `tip.0` to `tip.1` and its stack ref from
/// `stack.0` (`None`: no ref) to `stack.1`, in one `update-ref`
/// transaction: both move, or, if either was moved meanwhile, neither does
/// and this fails. Git moves a branch checked out in a worktree without a
/// word; the caller sees to its files first.
pub fn move_refs(
    common_dir: &Path,
    branch: &str,
    tip: (ObjectId, ObjectId),
    stack: (Option<ObjectId>, ObjectId),
    why: &str,
) -> Result<()> {
    let old_stack = stack.0.map_or(NO_REF.to_string(), |o| o.to_string());
    let mut script = String::new();
    // Unmoved, the branch is still compared: the stack ref moves only with
    // the branch where the caller saw it.
    if tip.0 == tip.1 {
        script += &format!("verify refs/heads/{branch} {}\n", tip.0);
    } else {
        script += &format!("update refs/heads/{branch} {} {}\n", tip.1, tip.0);
    }
    if stack.0 != Some(stack.1) {
        script += &format!("update {} {} {old_stack}\n", stack_ref(branch), stack.1);
    }
    if tip.0 == tip.1 && stack.0 == Some(stack.1) {
        return Ok(());
    }
    let args = ["update-ref", "-m", why, "--stdin"];
    cmd::output_with_input(common_dir, &args, script.into_bytes())?;
    Ok(())
}

/// Points the stack ref of `branch` at `base`, whatever it held.
pub fn set_base(common_dir: &Path, branch: &str, base: ObjectId) -> Result<()> {
    let args = ["update-ref", "-m", "gitfold: stack on"];
    let name = stack_ref(branch);
    let base = base.to_string();
    cmd::output(common_dir, &[&args[..], &[&name, &base]].concat()).map(drop)
}

/// Deletes the stack ref of `branch`, if it has one.
pub fn drop_base(common_dir: &Path, branch: &str) -> Result<()> {
    let name = stack_ref(branch);
    match rev(common_dir, &name)? {
        Some(_) => cmd::output(common_dir, &["update-ref", "-d", &name]).map(drop),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_repo::Repo;

    /// The design's experiments' repository: main with f.txt (1..30) and
    /// m.txt; p1 on main (line 5, p1.txt), p2 on p1 (line 15, p2.txt).
    fn stack(name: &str) -> Repo {
        let r = Repo::with_fixed_dates(name);
        let numbers: String = (1..=30).map(|i| format!("{i}\n")).collect();
        r.write("f.txt", &numbers);
        r.write("m.txt", "m\n");
        r.commit("init");
        r.git(&["switch", "-q", "-c", "p1"]);
        edit(&r, "5", "5 p1");
        r.commit("p1 a");
        r.write("p1.txt", "one\n");
        r.commit("p1 b");
        r.git(&["switch", "-q", "-c", "p2"]);
        edit(&r, "15", "15 p2");
        r.commit("p2 a");
        r.write("p2.txt", "two\n");
        r.commit("p2 b");
        r
    }

    /// Replaces line `from` of f.txt with `to`.
    fn edit(r: &Repo, from: &str, to: &str) {
        let text = std::fs::read_to_string(r.work().join("f.txt")).unwrap();
        let text: String = text
            .lines()
            .map(|l| {
                if l == from {
                    format!("{to}\n")
                } else {
                    format!("{l}\n")
                }
            })
            .collect();
        r.write("f.txt", &text);
    }

    /// A commit on p1 (checked out) changing line `line` of f.txt.
    fn fix_p1(r: &Repo, line: &str, to: &str) {
        r.git(&["switch", "-q", "p1"]);
        edit(r, line, to);
        r.commit("p1 review fix");
    }

    /// What `git rebase --onto <onto> <base>` makes of a copy of `branch`.
    fn git_rebase(r: &Repo, onto: &str, base: ObjectId, branch: &str) -> ObjectId {
        let copy = format!("{branch}-by-git");
        r.git(&["switch", "-q", "-c", &copy, branch]);
        r.git(&["rebase", "-q", "--onto", onto, &base.to_string()]);
        r.id(&copy)
    }

    #[test]
    fn restack_as_git_rebase() {
        let r = stack("stack-rebase");
        let old_p1 = r.id("p1");
        fix_p1(&r, "6", "6 fix");
        r.write("p1b.txt", "more\n");
        r.commit("p1 adds a file");
        let repo = r.open();
        let (p1, p2) = (r.id("p1"), r.id("p2"));
        let Restack::Done {
            tip,
            copied,
            dropped,
        } = restack(&repo, old_p1, p1, p2).unwrap()
        else {
            panic!("a conflict")
        };
        assert_eq!(copied.len(), 2);
        assert!(dropped.is_empty());
        assert_eq!(tip, git_rebase(&r, "p1", old_p1, "p2"), "the same commits");
        assert_eq!(r.id("p2"), p2, "nothing moved");
        assert_eq!(
            r.git(&["log", "--format=%an %cn %s", "-2", &tip.to_string()]),
            "Agent Committer p2 b\nAgent Committer p2 a"
        );
    }

    #[test]
    fn commits_made_empty_are_dropped() {
        let r = stack("stack-empty");
        let old_p1 = r.id("p1");
        // p1 takes p2's first change as is.
        fix_p1(&r, "15", "15 p2");
        let repo = r.open();
        let Restack::Done { tip, dropped, .. } =
            restack(&repo, old_p1, r.id("p1"), r.id("p2")).unwrap()
        else {
            panic!("a conflict")
        };
        assert_eq!(dropped, vec![r.id("p2~1")]);
        assert_eq!(tip, git_rebase(&r, "p1", old_p1, "p2"), "git drops it too");
        assert_eq!(r.git(&["rev-list", "--count", &format!("p1..{tip}")]), "1");
    }

    #[test]
    fn conflicts_name_the_commit_and_paths() {
        let r = stack("stack-conflict");
        let old_p1 = r.id("p1");
        fix_p1(&r, "15", "15 p1 too");
        let repo = r.open();
        let got = restack(&repo, old_p1, r.id("p1"), r.id("p2")).unwrap();
        assert_eq!(
            got,
            Restack::Conflict {
                commit: r.id("p2~1"),
                paths: vec![BString::from("f.txt")],
            }
        );
        let by_git = r.run_in(
            &r.work(),
            &["rebase", "--onto", "p1", &old_p1.to_string(), "p2"],
        );
        assert!(!by_git.status.success(), "git conflicts too");
        r.git(&["rebase", "--abort"]);
    }

    #[test]
    fn merges_are_refused() {
        let r = stack("stack-merge");
        let old_p1 = r.id("p1");
        r.git(&["switch", "-q", "-c", "side", "p1"]);
        r.write("side.txt", "s\n");
        r.commit("side");
        r.git(&["switch", "-q", "p2"]);
        r.git(&["merge", "-q", "--no-edit", "side"]);
        fix_p1(&r, "6", "6 fix");
        let repo = r.open();
        let got = restack(&repo, old_p1, r.id("p1"), r.id("p2")).unwrap();
        assert_eq!(got, Restack::Merge(r.id("p2")));
    }

    #[test]
    fn refs_move_together_or_not_at_all() {
        let r = stack("stack-refs");
        let common = r.common();
        let old_p1 = r.id("p1");
        // Both checked out in linked worktrees, as in gitfold.
        r.git(&["switch", "-q", "main"]);
        for b in ["p1", "p2"] {
            let wt = r.dir.join(format!("wt-{b}"));
            r.git(&["worktree", "add", "-q", wt.to_str().unwrap(), b]);
        }
        let wt1 = r.dir.join("wt-p1");
        edit_in(&r, &wt1);
        let (p1, p2) = (r.id("p1"), r.id("p2"));
        let Restack::Done { tip, .. } = restack(&r.open(), old_p1, p1, p2).unwrap() else {
            panic!("a conflict")
        };
        // A stale compare-and-swap value for the stack ref: nothing moves.
        let wrong = Some(p2);
        assert!(move_refs(&common, "p2", (p2, tip), (wrong, p1), "test").is_err());
        assert_eq!(
            (r.id("p2"), rev(&common, &stack_ref("p2")).unwrap()),
            (p2, None)
        );
        move_refs(&common, "p2", (p2, tip), (None, p1), "test").unwrap();
        assert_eq!(r.id("p2"), tip);
        assert_eq!(rev(&common, &stack_ref("p2")).unwrap(), Some(p1));
        // Again from the old tip: refused.
        assert!(move_refs(&common, "p2", (p2, tip), (Some(p1), p1), "test").is_err());
        // The stack ref alone, with the branch not where the caller saw it:
        // refused too.
        assert!(move_refs(&common, "p2", (p2, p2), (Some(p1), old_p1), "test").is_err());
        assert_eq!(rev(&common, &stack_ref("p2")).unwrap(), Some(p1));
        move_refs(&common, "p2", (tip, tip), (Some(p1), old_p1), "test").unwrap();
        assert_eq!(rev(&common, &stack_ref("p2")).unwrap(), Some(old_p1));
        let wt2 = r.dir.join("wt-p2");
        assert_eq!(
            r.git_in(&wt2, &["status", "--short"]),
            "M  f.txt\nD  p1b.txt",
            "the worktree's files didn't move (the caller's job)"
        );
    }

    /// A review fix on p1 in its worktree `wt`: line 6, and a new file.
    fn edit_in(r: &Repo, wt: &Path) {
        let text = std::fs::read_to_string(wt.join("f.txt")).unwrap();
        std::fs::write(wt.join("f.txt"), text.replace("\n6\n", "\n6 fix\n")).unwrap();
        std::fs::write(wt.join("p1b.txt"), "more\n").unwrap();
        r.git_in(wt, &["add", "-A"]);
        r.git_in(wt, &["commit", "-q", "-m", "p1 review fix"]);
    }

    #[test]
    fn the_parent_base_is_guessed_without_its_ref() {
        let r = stack("stack-base");
        let common = r.common();
        let old_p1 = r.id("p1");
        let base = |r: &Repo| parent_base(&common, "p2", "p1", r.id("p2"), r.id("p1")).unwrap();
        let guessed = base(&r);
        assert_eq!((guessed.commit, guessed.guessed), (old_p1, true));
        set_base(&common, "p2", old_p1).unwrap();
        fix_p1(&r, "6", "6 fix");
        let stored = base(&r);
        assert_eq!(
            stored,
            Base {
                commit: old_p1,
                stored: Some(old_p1),
                guessed: false
            }
        );
        // A ref that isn't an ancestor of the branch (a renamed branch's
        // leftover, say): the fork point instead.
        set_base(&common, "p2", r.id("p1")).unwrap();
        let fork_point = base(&r);
        assert_eq!((fork_point.commit, fork_point.guessed), (old_p1, true));
        drop_base(&common, "p2").unwrap();
        assert_eq!(rev(&common, &stack_ref("p2")).unwrap(), None);
        assert_eq!(count(&common, old_p1, r.id("p1")).unwrap(), 1);
    }
}
