//! Moves of a branch's own commits, on the git side. Branch b builds on its
//! editable layers (`state::layers`); the commit its own commits sit on,
//! B, is kept by `refs/gitfold-own/<b>`: one layer's commit, or with
//! several, a merge of theirs gitfold makes (`merge_parts`). b's own
//! commits are `B..b`, whatever happens to a layer's branch later (amends,
//! force pushes): with only `merge-base(a, b)`, a rewritten branch would
//! have its old commits replayed too.
//!
//! Moving b onto A (a layer's new tip, or a new merge) copies each commit C
//! of `B..b`, oldest first,
//! onto the copy of its parent: `merge_trees(base = C^, ours = the copy so
//! far, theirs = C)`, with C's author, committer and message
//! (`layers::copy_commit`). These are the commits `git rebase --onto A B b`
//! makes when it keeps the committer. A commit that becomes empty (a already
//! made the same change) is dropped, as `git rebase` drops it; a merge in
//! `B..b` is refused. Nothing moves here but in `move_refs`: b and its stack
//! ref, in one compare-and-swap transaction, so a crash can't leave b
//! moved with B stale.

use std::path::Path;

use anyhow::{Result, bail};
use gix::ObjectId;
use gix::bstr::BString;

use super::cmd;
use super::layers::{commit_tree, copy_commit, merge_trees};

/// The own refs: `refs/gitfold-own/<branch>` keeps where a branch.s own
/// commits start.
pub const OWN_REFS: &str = "refs/gitfold-own/";

/// No object: in an `update-ref` transaction, "the ref must not exist".
const NO_REF: &str = "0000000000000000000000000000000000000000";

/// The ref keeping where the own commits of `branch` start.
pub fn own_ref(branch: &str) -> String {
    format!("{OWN_REFS}{branch}")
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

/// What `compute_move` computed.
#[derive(Debug, PartialEq, Eq)]
pub enum Move {
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
    /// `commit` is a merge, which v1 doesn't move.
    Merge(ObjectId),
}

/// Copies the commits of `base..tip` onto `onto` (see the module doc).
/// Writes objects only; no ref moves.
pub fn compute_move(
    repo: &gix::Repository,
    base: ObjectId,
    onto: ObjectId,
    tip: ObjectId,
) -> Result<Move> {
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
            [commit, ..] if ids.len() > 2 => return Ok(Move::Merge(commit)),
            _ => bail!("{base} isn't an ancestor of {tip}"),
        };
        let (parent_tree, tree) = (commit_tree(repo, parent)?, commit_tree(repo, commit)?);
        let at_tree = commit_tree(repo, at)?;
        let merged = match merge_trees(repo, parent_tree, at_tree, tree)? {
            Ok(merged) => merged,
            Err(paths) => return Ok(Move::Conflict { commit, paths }),
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
    Ok(Move::Done {
        tip: at,
        copied,
        dropped,
    })
}

/// The own commits of `base..tip` that don't apply onto `onto` (each tried
/// onto the copies of those before it that do), with the paths where they
/// conflict: the commits that can't stand without what `base` has and
/// `onto` doesn't (an editable layer's lines). A merge in the range is
/// listed as it is, with no paths.
pub fn dependent(
    repo: &gix::Repository,
    base: ObjectId,
    onto: ObjectId,
    tip: ObjectId,
) -> Result<Vec<(ObjectId, Vec<BString>)>> {
    let range = format!("{base}..{tip}");
    let args = ["rev-list", "--reverse", "--topo-order", "--parents", &range];
    let out = cmd::output(repo.common_dir(), &args)?;
    let mut at = commit_tree(repo, onto)?;
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let ids = line
            .split(' ')
            .map(|hex| ObjectId::from_hex(hex.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let [commit, parent] = ids[..] else {
            found.push((ids[0], Vec::new()));
            continue;
        };
        let (parent_tree, tree) = (commit_tree(repo, parent)?, commit_tree(repo, commit)?);
        match merge_trees(repo, parent_tree, at, tree)? {
            Ok(merged) => at = merged,
            Err(paths) => found.push((commit, paths)),
        }
    }
    Ok(found)
}

/// The subject of the merge commit a branch's own commits sit on when it
/// builds on several editable layers (`merge_parts`).
const MERGE_SUBJECT: &str = "gitfold: built on";

/// Whether `subject` is a merge commit's `merge_parts` made.
pub fn is_merge_subject(subject: &str) -> bool {
    subject.starts_with(MERGE_SUBJECT)
}

/// What a branch with several editable layers builds on: one commit, the
/// merge of `parts` (the layers' names and the commits of theirs it builds
/// on, in order), made with a fixed identity and date (as snapshots), so
/// the same parts make the same commit; a single part is that commit
/// itself. Its message names the parts (`parts_of` reads them back). If
/// they conflict, the paths.
pub fn merge_parts(
    repo: &gix::Repository,
    parts: &[(String, ObjectId)],
) -> Result<std::result::Result<ObjectId, Vec<BString>>> {
    let Some((_, first)) = parts.first() else {
        bail!("nothing to merge");
    };
    if parts.len() == 1 {
        return Ok(Ok(*first));
    }
    let common_dir = repo.common_dir();
    let mut tree = commit_tree(repo, *first)?;
    for (i, (_, commit)) in parts.iter().enumerate().skip(1) {
        let mut args = vec!["merge-base".to_string(), commit.to_string()];
        args.extend(parts[..i].iter().map(|(_, c)| c.to_string()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let base = match cmd::output(common_dir, &args) {
            Ok(out) => commit_tree(repo, parse_id(&out)?)?,
            Err(_) => bail!("{} shares no history with the others", parts[i].0),
        };
        match merge_trees(repo, base, tree, commit_tree(repo, *commit)?)? {
            Ok(merged) => tree = merged,
            Err(paths) => return Ok(Err(paths)),
        }
    }
    let me = gix::actor::Signature {
        name: "gitfold".into(),
        email: "gitfold@localhost".into(),
        // 2020-01-01T00:00:00Z, as snapshots.
        time: gix::date::Time {
            seconds: 1_577_836_800,
            offset: 0,
        },
    };
    let names: Vec<&str> = parts.iter().map(|(n, _)| n.as_str()).collect();
    let mut message = format!("{MERGE_SUBJECT} {}\n\n", names.join(" + "));
    for name in &names {
        message += &format!("on {name}\n");
    }
    let commit = gix::objs::Commit {
        tree,
        parents: parts.iter().map(|(_, c)| *c).collect(),
        author: me.clone(),
        committer: me,
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    Ok(Ok(repo.write_object(&commit)?.detach()))
}

/// The parts of `commit` if it is a merge `merge_parts` made: the names and
/// commits it merges.
pub fn parts_of(
    repo: &gix::Repository,
    commit: ObjectId,
) -> Result<Option<Vec<(String, ObjectId)>>> {
    let commit = repo.find_commit(commit)?;
    let parents: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
    let message = commit.message_raw()?.to_string();
    if parents.len() < 2 || !message.starts_with(MERGE_SUBJECT) {
        return Ok(None);
    }
    let names: Vec<String> = (message.lines())
        .filter_map(|l| l.strip_prefix("on "))
        .map(str::to_string)
        .collect();
    if names.len() != parents.len() {
        return Ok(None);
    }
    Ok(Some(names.into_iter().zip(parents).collect()))
}

/// Moves `branch` from `tip.0` to `tip.1` and its own ref from
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
    // Unmoved, the branch is still compared: the own ref moves only with
    // the branch where the caller saw it.
    if tip.0 == tip.1 {
        script += &format!("verify refs/heads/{branch} {}\n", tip.0);
    } else {
        script += &format!("update refs/heads/{branch} {} {}\n", tip.1, tip.0);
    }
    if stack.0 != Some(stack.1) {
        script += &format!("update {} {} {old_stack}\n", own_ref(branch), stack.1);
    }
    if tip.0 == tip.1 && stack.0 == Some(stack.1) {
        return Ok(());
    }
    let args = ["update-ref", "-m", why, "--stdin"];
    cmd::output_with_input(common_dir, &args, script.into_bytes())?;
    Ok(())
}

/// Points the own ref of `branch` at `base`, whatever it held.
pub fn set_base(common_dir: &Path, branch: &str, base: ObjectId) -> Result<()> {
    let args = ["update-ref", "-m", "gitfold: base"];
    let name = own_ref(branch);
    let base = base.to_string();
    cmd::output(common_dir, &[&args[..], &[&name, &base]].concat()).map(drop)
}

/// Deletes the own ref of `branch`, if it has one.
pub fn drop_base(common_dir: &Path, branch: &str) -> Result<()> {
    let name = own_ref(branch);
    match rev(common_dir, &name)? {
        Some(_) => cmd::output(common_dir, &["update-ref", "-d", &name]).map(drop),
        None => Ok(()),
    }
}

/// How the commit of an editable layer a branch sits on landed on its
/// target (main, or the one branch the layer builds on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// In the target's history: a merge commit.
    Merged,
    /// Its content, as other commits: a squash or a rebase merge.
    Squashed,
    /// In the target's first-parent line, the layer's branch gone: a
    /// fast-forward.
    FastForward,
}

/// Where such a commit landed: `at`, the first commit of the target that
/// has all of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landing {
    pub at: ObjectId,
    pub how: How,
}

/// The most target commits `landing` tries a no-op merge on in one call:
/// the newest ones, where a landing not seen by an earlier call is.
const MAX_WALK: usize = 2000;

/// Whether commit `base` (of an editable layer: what a branch's own commits sit on) landed
/// on target tip `target`, and where; `since`, an older target tip it was
/// found not to have landed on, limits the walk to what came after it. What
/// matters is that commit, not the layer: a fix the layer got after its
/// merge is the layer's business.
///
/// - `base` in the target's history, brought in by a merge (of it, or of a
///   later tip of the layer): that merge, the first commit of the target's
///   first-parent line that has it. One on the first-parent line itself
///   landed by a fast-forward if `was_on_target` is `Some(false)` (the
///   layer's branch is gone); otherwise
///   it looks like a fresh branch with no commits of its own, so isn't
///   taken.
/// - Else, if the base has changes of its own since it and the target
///   diverged (MB): the first commit S of `MB..target` touching their paths
///   where merging the base changes nothing (`merge_trees(MB, S, base)` is
///   S's tree): a squash merge, the last commit of a rebase merge, also
///   with a maintainer's tweak elsewhere, or main editing near the lines
///   later. It is a guess: main making the same change on its own looks the
///   same. A per-commit patch id (`git cherry`) misses a squash of several
///   commits; a patch id of the whole range misses context changes.
pub fn landing(
    repo: &gix::Repository,
    base: ObjectId,
    target: ObjectId,
    since: Option<ObjectId>,
    was_on_target: Option<bool>,
) -> Result<Option<Landing>> {
    let common_dir = repo.common_dir();
    if base == target && was_on_target != Some(false) {
        return Ok(None);
    }
    if is_ancestor(common_dir, base, target)? {
        return merged_in(repo, base, target, was_on_target);
    }
    let mb = match cmd::output(
        common_dir,
        &["merge-base", &base.to_string(), &target.to_string()],
    ) {
        Ok(out) => parse_id(&out)?,
        // Unrelated histories.
        Err(_) => return Ok(None),
    };
    let (mb_tree, base_tree) = (commit_tree(repo, mb)?, commit_tree(repo, base)?);
    let paths: Vec<String> = super::layers::delta(repo, mb_tree, base_tree)?
        .into_iter()
        .map(|(path, _)| path.to_string())
        .collect();
    // Nothing of its own: trivially "landed" anywhere.
    if paths.is_empty() {
        return Ok(None);
    }
    let from = since.unwrap_or(mb);
    let commits = touching(common_dir, from, target, &paths)?;
    let walked = commits.len().min(MAX_WALK);
    if walked < commits.len() {
        log::warn!(
            "checking whether {base} landed: only the newest {walked} of {} commits since \
             {from} touching its files; `gitfold layer remove <LAYER>` takes a branch's \
             commits off a layer that landed earlier",
            commits.len()
        );
    }
    for &at in &commits[commits.len() - walked..] {
        let at_tree = commit_tree(repo, at)?;
        if merge_trees(repo, mb_tree, at_tree, base_tree)? == Ok(at_tree) {
            return Ok(Some(Landing {
                at,
                how: How::Squashed,
            }));
        }
    }
    Ok(None)
}

/// `landing` for a base in the target's history: the first commit of the
/// target's first-parent line that has it (found by bisection: once one
/// has it, every later one does), unless that is the base's own child on
/// the line.
fn merged_in(
    repo: &gix::Repository,
    base: ObjectId,
    target: ObjectId,
    was_on_target: Option<bool>,
) -> Result<Option<Landing>> {
    let common_dir = repo.common_dir();
    let range = format!("^{base}");
    let target_hex = target.to_string();
    let args = [
        "rev-list",
        "--first-parent",
        "--reverse",
        &target_hex,
        &range,
    ];
    let line = String::from_utf8_lossy(&cmd::output(common_dir, &args)?)
        .lines()
        .map(|hex| ObjectId::from_hex(hex.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    let (mut lo, mut hi) = (0, line.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        match is_ancestor(common_dir, base, line[mid])? {
            true => hi = mid,
            false => lo = mid + 1,
        }
    }
    if let Some(&first) = line.get(lo)
        && repo
            .find_commit(first)?
            .parent_ids()
            .next()
            .map(|p| p.detach())
            != Some(base)
    {
        return Ok(Some(Landing {
            at: first,
            how: How::Merged,
        }));
    }
    // On the line (or the target's tip): a fast-forward, if it wasn't in
    // the target when recorded.
    Ok((was_on_target == Some(false)).then_some(Landing {
        at: base,
        how: How::FastForward,
    }))
}

/// The commits of `from..target` that descend from `from` and touch `paths`,
/// oldest first. The paths go on standard input: a large change's would
/// overflow the command line.
fn touching(
    common_dir: &Path,
    from: ObjectId,
    target: ObjectId,
    paths: &[String],
) -> Result<Vec<ObjectId>> {
    let mut input = format!("{target}\n^{from}\n--\n");
    for path in paths {
        input += path;
        input.push('\n');
    }
    let args = [
        "rev-list",
        "--reverse",
        "--topo-order",
        "--ancestry-path",
        "--stdin",
    ];
    let out = cmd::output_with_env_input(
        common_dir,
        &args,
        &[("GIT_LITERAL_PATHSPECS", "1")],
        input.into_bytes(),
    )?;
    String::from_utf8_lossy(&out)
        .lines()
        .map(|hex| Ok(ObjectId::from_hex(hex.as_bytes())?))
        .collect()
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
    fn move_as_git_rebase() {
        let r = stack("stack-rebase");
        let old_p1 = r.id("p1");
        fix_p1(&r, "6", "6 fix");
        r.write("p1b.txt", "more\n");
        r.commit("p1 adds a file");
        let repo = r.open();
        let (p1, p2) = (r.id("p1"), r.id("p2"));
        let Move::Done {
            tip,
            copied,
            dropped,
        } = compute_move(&repo, old_p1, p1, p2).unwrap()
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
        let Move::Done { tip, dropped, .. } =
            compute_move(&repo, old_p1, r.id("p1"), r.id("p2")).unwrap()
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
        let got = compute_move(&repo, old_p1, r.id("p1"), r.id("p2")).unwrap();
        assert_eq!(
            got,
            Move::Conflict {
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
        let got = compute_move(&repo, old_p1, r.id("p1"), r.id("p2")).unwrap();
        assert_eq!(got, Move::Merge(r.id("p2")));
    }

    #[test]
    fn refs_move_together_or_not_at_all() {
        let r = stack("own-refs");
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
        let Move::Done { tip, .. } = compute_move(&r.open(), old_p1, p1, p2).unwrap() else {
            panic!("a conflict")
        };
        // A stale compare-and-swap value for the own ref: nothing moves.
        let wrong = Some(p2);
        assert!(move_refs(&common, "p2", (p2, tip), (wrong, p1), "test").is_err());
        assert_eq!(
            (r.id("p2"), rev(&common, &own_ref("p2")).unwrap()),
            (p2, None)
        );
        move_refs(&common, "p2", (p2, tip), (None, p1), "test").unwrap();
        assert_eq!(r.id("p2"), tip);
        assert_eq!(rev(&common, &own_ref("p2")).unwrap(), Some(p1));
        // Again from the old tip: refused.
        assert!(move_refs(&common, "p2", (p2, tip), (Some(p1), p1), "test").is_err());
        // The own ref alone, with the branch not where the caller saw it:
        // refused too.
        assert!(move_refs(&common, "p2", (p2, p2), (Some(p1), old_p1), "test").is_err());
        assert_eq!(rev(&common, &own_ref("p2")).unwrap(), Some(p1));
        move_refs(&common, "p2", (tip, tip), (Some(p1), old_p1), "test").unwrap();
        assert_eq!(rev(&common, &own_ref("p2")).unwrap(), Some(old_p1));
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
    fn several_layers_merge_into_one_commit() {
        let r = stack("stack-parts");
        let common = r.common();
        r.git(&["switch", "-q", "-c", "q", "main"]);
        r.write("q.txt", "q\n");
        r.commit("q");
        let repo = r.open();
        let parts = vec![("p1".to_string(), r.id("p1")), ("q".to_string(), r.id("q"))];
        let merge = merge_parts(&repo, &parts).unwrap().unwrap();
        assert_eq!(
            merge_parts(&repo, &parts).unwrap().unwrap(),
            merge,
            "the same parts, the same commit"
        );
        assert_eq!(parts_of(&repo, merge).unwrap(), Some(parts.clone()));
        assert_eq!(
            r.git(&[
                "show",
                "--format=",
                "--name-only",
                "-m",
                "--first-parent",
                &merge.to_string()
            ]),
            "q.txt"
        );
        assert_eq!(merge_parts(&repo, &parts[..1]).unwrap(), Ok(r.id("p1")));
        assert_eq!(parts_of(&repo, r.id("p1")).unwrap(), None);
        r.git(&["switch", "-q", "-c", "other", "main"]);
        edit(&r, "5", "5 other");
        r.commit("other");
        let both = [
            ("p1".to_string(), r.id("p1")),
            ("other".to_string(), r.id("other")),
        ];
        assert_eq!(
            merge_parts(&r.open(), &both).unwrap(),
            Err(vec![BString::from("f.txt")]),
            "a conflict names the paths"
        );
        set_base(&common, "p2", r.id("p1")).unwrap();
        assert_eq!(rev(&common, &own_ref("p2")).unwrap(), Some(r.id("p1")));
        drop_base(&common, "p2").unwrap();
        assert_eq!(rev(&common, &own_ref("p2")).unwrap(), None);
        assert_eq!(count(&common, r.id("main"), r.id("p1")).unwrap(), 2);
    }

    #[test]
    fn commits_changing_a_layers_lines_are_listed() {
        let r = stack("stack-dependent");
        r.git(&["switch", "-q", "p2"]);
        edit(&r, "5 p1", "5 p1 and p2");
        let dependent_one = r.commit("p2 changes p1's line");
        let found = dependent(&r.open(), r.id("p1"), r.id("main"), r.id("p2")).unwrap();
        assert_eq!(found, vec![(dependent_one, vec![BString::from("f.txt")])]);
    }

    /// `stack` with main moved on (another file), checked out on main.
    fn landing_repo(name: &str) -> Repo {
        let r = stack(name);
        r.git(&["switch", "-q", "main"]);
        r.write("other.txt", "other\n");
        r.commit("main moves on");
        r
    }

    fn landed(r: &Repo, base: &str, target: &str) -> Option<Landing> {
        landing(&r.open(), r.id(base), r.id(target), None, None).unwrap()
    }

    #[test]
    fn a_squash_merge_lands() {
        let r = landing_repo("land-squash");
        r.git(&["merge", "-q", "--squash", "p1"]);
        let squash = r.commit("p1 (#1)");
        // A later main commit that edits p1's line again: found further
        // back, at the squash.
        edit(&r, "5 p1", "5 p1 edited on main");
        r.commit("main edits p1's line");
        let expected = Some(Landing {
            at: squash,
            how: How::Squashed,
        });
        assert_eq!(landed(&r, "p1", "main"), expected);
        // A fix p1 got after the merge is p1's business: its old tip landed.
        let base = r.id("p1");
        fix_p1(&r, "6", "late fix");
        assert_eq!(
            landing(&r.open(), base, r.id("main"), None, None).unwrap(),
            expected
        );
        assert_eq!(landed(&r, "p1", "main"), None, "the fix itself didn't land");
    }

    #[test]
    fn a_squash_with_a_tweak_lands() {
        let r = landing_repo("land-tweak");
        r.git(&["merge", "-q", "--squash", "p1"]);
        r.write("m.txt", "m tweaked by the maintainer\n");
        let squash = r.commit("p1 (#1), tweaked");
        assert_eq!(landed(&r, "p1", "main").map(|l| l.at), Some(squash));
    }

    #[test]
    fn a_rebase_merge_lands_at_its_last_commit() {
        let r = landing_repo("land-rebase");
        r.git(&["cherry-pick", "main..p1"]);
        let last = r.id("main");
        assert_eq!(
            landed(&r, "p1", "main"),
            Some(Landing {
                at: last,
                how: How::Squashed
            })
        );
    }

    #[test]
    fn a_merge_commit_lands() {
        let r = landing_repo("land-merge");
        r.git(&["merge", "-q", "--no-ff", "--no-edit", "p1"]);
        let merge = r.id("main");
        r.write("after.txt", "after\n");
        r.commit("after the merge");
        assert_eq!(
            landed(&r, "p1", "main"),
            Some(Landing {
                at: merge,
                how: How::Merged
            })
        );
    }

    #[test]
    fn a_merge_of_a_later_parent_tip_lands() {
        let r = landing_repo("land-merge-later");
        // p2's base is p1 as it was; p1 got a commit after it.
        let base = r.id("p1");
        fix_p1(&r, "6", "6 later");
        r.git(&["switch", "-q", "main"]);
        r.git(&["merge", "-q", "--no-ff", "--no-edit", "p1"]);
        let merge = r.id("main");
        r.write("after.txt", "after\n");
        r.commit("after the merge");
        let found = landing(&r.open(), base, r.id("main"), None, None).unwrap();
        assert_eq!(
            found,
            Some(Landing {
                at: merge,
                how: How::Merged
            })
        );
    }

    #[test]
    fn a_walk_since_an_older_tip_sees_only_what_came_after() {
        let r = landing_repo("land-since");
        let before = r.id("main");
        r.git(&["merge", "-q", "--squash", "p1"]);
        let squash = r.commit("p1 (#1)");
        r.write("after.txt", "after\n");
        let after = r.commit("after the squash");
        let (repo, base) = (r.open(), r.id("p1"));
        let found = |since| {
            landing(&repo, base, after, since, None)
                .unwrap()
                .map(|l| l.at)
        };
        assert_eq!(found(Some(before)), Some(squash));
        assert_eq!(found(Some(squash)), None, "the squash is before it");
    }

    #[test]
    fn a_fast_forward_lands_if_the_base_was_off_the_target() {
        let r = stack("land-ff");
        r.git(&["switch", "-q", "main"]);
        let fresh = r.id("main");
        r.git(&["merge", "-q", "--ff-only", "p1"]);
        r.write("after.txt", "after\n");
        r.commit("after the merge");
        let (repo, base, main) = (r.open(), r.id("p1"), r.id("main"));
        let ff = Some(Landing {
            at: base,
            how: How::FastForward,
        });
        assert_eq!(landing(&repo, base, main, None, Some(false)).unwrap(), ff);
        assert_eq!(
            landing(&repo, base, base, None, Some(false)).unwrap(),
            ff,
            "the target's tip"
        );
        // The branch still there: not told apart.
        assert_eq!(landing(&repo, base, main, None, None).unwrap(), None);
        // A fresh branch: its commit was on the target already.
        assert_eq!(landing(&repo, fresh, main, None, Some(true)).unwrap(), None);
        // The target moved back behind the base.
        assert_eq!(
            landing(&repo, base, fresh, None, Some(false)).unwrap(),
            None
        );
    }

    #[test]
    fn unmerged_and_fresh_parents_dont_land() {
        let r = landing_repo("land-not");
        assert_eq!(landed(&r, "p1", "main"), None, "unmerged");
        // A parent with nothing of its own yet, at an older main commit.
        assert_eq!(landed(&r, "main~1", "main"), None, "fresh");
        assert_eq!(landed(&r, "main", "main"), None, "fresh, at the tip");
        // p2's base, p1, on p2's own history: p1 is in p2 by its first
        // parent line, as for a fresh parent.
        assert_eq!(landed(&r, "p1", "p2"), None);
    }
}
