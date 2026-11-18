//! Conflict notices: when a branch gets new commits in its workspace, those
//! whose work they now conflict with are told, in their inboxes
//! (`cli::inbox::deliver`, `from: gitfold`). A step a workspace's sync thread
//! saw (`Workspace::with_commits`) is checked here, on a thread of its own:
//!
//! 1. its own paths: what its commits change, not those of where they start
//!    (`follow::own_base`: a rebase onto its parent, a merge of it);
//! 2. who is related (`related`): the workspaces in the view of the branch it
//!    lands in (`Tree::lands_in`: its parent, or the root for a branch
//!    without one);
//! 3. a cheap check for each: its stored uncommitted changes
//!    (`StateDir::may_change`) or its commits since the merge base touch one
//!    of those paths;
//! 4. git's merges (`Private::conflicts`) of the new tip and of the tip
//!    before with the workspace as it is (its live tree, or its tip if its
//!    work can't meet the step's): conflicts only after are new;
//! 5. one message each to both sides and the workspace of the branch they
//!    land in (`told`), unless their branch says not.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::{Weak, mpsc};
use std::thread;

use anyhow::{Result, anyhow};
use gix::ObjectId;
use log::{info, warn};

use super::members::{self, Bases, Tip};
use super::merge::Private;
use super::serve::Views;
use crate::git;
use crate::settings::Local;
use crate::state::{self, PathTree, StateDir};
use crate::store::Store;
use crate::tree::{self, Tree};

/// New commits of `branch`, seen in its workspace: its tip went from `from`
/// to `to`.
pub struct Step {
    pub branch: String,
    pub from: ObjectId,
    pub to: ObjectId,
}

/// Starts the thread checking the commit steps sent to what it returns
/// (`Workspaces::set_commits`), while `views` exist. Steps queued meanwhile
/// are one per branch: from its first to its last.
pub fn spawn(views: Weak<Views>) -> mpsc::Sender<Step> {
    let (send, steps) = mpsc::channel::<Step>();
    let spawned = thread::Builder::new()
        .name("notices".into())
        .spawn(move || {
            while let Ok(first) = steps.recv() {
                let mut queued = vec![first];
                for step in steps.try_iter() {
                    match queued.iter_mut().find(|q| q.branch == step.branch) {
                        Some(q) => q.to = step.to,
                        None => queued.push(step),
                    }
                }
                let Some(views) = views.upgrade() else { return };
                for step in queued {
                    if let Err(e) = check(&views, &step) {
                        warn!("conflict notices: {} at {}: {e:#}", step.branch, step.to);
                    }
                }
            }
        });
    if let Err(e) = spawned {
        warn!("conflict notices: can't start their thread: {e}");
    }
    send
}

/// Paths, ordered as git orders them.
type Paths = BTreeSet<Vec<u8>>;

/// The new conflicts of a step with one workspace: on paths its uncommitted
/// work changes, and on paths only its committed work does.
#[derive(Debug, PartialEq)]
struct Conflicts {
    uncommitted: Paths,
    committed: Paths,
}

/// Where two branches land together.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Lands {
    /// In the committer: the other is built on it.
    InCommitter,
    /// Where the committer lands (`Related::lands`).
    WithCommitter,
}

/// Which of two branches is built on the other and follows it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Follower {
    Neither,
    Other,
    Committer,
}

/// A workspace whose work meets the committer's.
#[derive(Debug, PartialEq)]
struct Other {
    branch: String,
    lands: Lands,
    follower: Follower,
}

/// Whose work meets the committer's (`related`).
#[derive(Debug, PartialEq)]
struct Related {
    others: Vec<Other>,
    /// Where the committer lands, as shown, and that branch's workspace (if
    /// it has one); none for the root.
    lands: Option<(String, Option<String>)>,
}

/// A workspace the step may conflict with: its tip, its merge bases with the
/// step's tip and with the tip before, and whether its uncommitted work may
/// meet the step's.
struct Candidate<'a> {
    other: &'a Other,
    tip: Tip,
    base: ObjectId,
    before: Option<ObjectId>,
    uncommitted: bool,
}

/// The conflicts of a step with one workspace, and how they read.
struct Pair<'a> {
    other: &'a Other,
    conflicts: Conflicts,
}

/// Checks `step` (see the module doc).
fn check(views: &Views, step: &Step) -> Result<()> {
    let common_dir = &views.common_dir;
    let local = Local::read_checked(common_dir)?;
    let tree = Tree::of_config(common_dir, &local);
    let committer = step.branch.as_str();
    let workspaces: HashSet<String> = (state::list(common_dir)?.into_iter())
        .filter(|s| s.state.opened())
        .map(|s| s.branch)
        .collect();
    let related = related(&tree, committer, &workspaces);
    let lands_ws = related.lands.as_ref().and_then(|(_, ws)| ws.as_deref());
    let mut involved = (related.others.iter().map(|o| o.branch.as_str()))
        .chain(lands_ws)
        .chain([committer]);
    if related.others.is_empty() || !involved.any(|b| local.conflict_notices(b)) {
        return Ok(());
    }
    let Some((paths, took)) = own_paths(common_dir, &tree, step)? else {
        return Ok(());
    };
    let mut bases = Bases::default();
    let candidates: Vec<Candidate> = (related.others.iter())
        .filter_map(|other| {
            candidate(common_dir, step, other, &paths, &mut bases)
                .inspect_err(|e| warn!("conflict notices: {committer} and {}: {e:#}", other.branch))
                .ok()
                .flatten()
        })
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    let private = views.private().map_err(|e| anyhow!("{e:?}"))?;
    let (from, to) = (git::commit(step.from)?, git::commit(step.to)?);
    let step_tips = ((from.id, from.tree), (to.id, to.tree));
    // The live trees of those whose uncommitted work may meet the step's,
    // side by side.
    let asked: Vec<(&str, ObjectId)> = (candidates.iter())
        .filter(|c| c.uncommitted)
        .map(|c| (c.other.branch.as_str(), c.tip.0))
        .collect();
    let mut lives = views.lives.tips(&private, &asked, true).into_iter();
    let only = took.then_some(&paths);
    let mut pairs = Vec::new();
    for c in &candidates {
        let branch = &c.other.branch;
        // A live tree that can't be made: its tip, as the views take it
        // (its committed conflicts still told).
        let live = match c.uncommitted {
            false => c.tip,
            true => match lives.next().expect("one per asked") {
                Ok(live) => live.unwrap_or(c.tip),
                Err(e) => {
                    warn!("conflict notices: the workspace of {branch}: {e:#}; its tip instead");
                    c.tip
                }
            },
        };
        match new_conflicts(&private, &views.store, step_tips, c, live, only) {
            Ok(Some(conflicts)) => pairs.push(Pair {
                other: c.other,
                conflicts,
            }),
            Ok(None) => {}
            Err(e) => warn!("conflict notices: {committer} and {branch}: {e:#}"),
        }
    }
    if pairs.is_empty() {
        return Ok(());
    }
    let short = git::with_repo(|repo| {
        gix::prelude::ObjectIdExt::attach(step.to, repo)
            .shorten_or_id()
            .to_string()
    });
    let on = |b: &str| local.conflict_notices(b);
    for (to, (subject, body)) in told(committer, &short, related.lands.as_ref(), &pairs, on) {
        match crate::cli::inbox::deliver(common_dir, &to, "gitfold", Some(&subject), &body) {
            Ok(()) => info!("conflict notices: {committer} {short}: told {to}"),
            Err(e) => warn!("conflict notices: telling {to} of {committer} {short}: {e:#}"),
        }
    }
    Ok(())
}

/// Whose work meets `committer`'s where it lands (`Tree::lands_in`), among
/// the branches with a workspace (`workspaces`): those in the view of that
/// branch (a local branch, or origin's root as the root), and the branch
/// itself; of a remote-tracking branch, what is built on it. The root's view
/// has the branches without a parent too (they land there, as siblings),
/// with what is built on them.
fn related(tree: &Tree, committer: &str, workspaces: &HashSet<String>) -> Related {
    let has_workspace = |b: &str| workspaces.contains(b);
    let root = tree.root();
    let lands = tree.lands_in(committer);
    let view_of = match &lands {
        None => Some(committer.to_string()),
        Some(r) => (r.strip_prefix("refs/heads/").map(String::from)).or_else(|| {
            let root = root?;
            (members::origin_root(tree, root).as_ref() == Some(r)).then(|| root.to_string())
        }),
    };
    let mut names: Vec<String> = match (&view_of, &lands) {
        (Some(view), _) => {
            let origin = members::origin_root(tree, view);
            let members = members::members(tree, view, origin.as_deref(), has_workspace);
            let members = members.into_iter().map(|m| m.branch);
            std::iter::once(view.clone()).chain(members).collect()
        }
        (None, Some(r)) => (tree.built_on(std::slice::from_ref(r)).into_iter())
            .map(String::from)
            .collect(),
        (None, None) => Vec::new(),
    };
    if view_of.is_some() && view_of.as_deref() == root {
        let mut parentless: Vec<&String> = (workspaces.iter())
            .filter(|b| tree.parent(b).is_none() && !tree.is_root(b))
            .collect();
        parentless.sort();
        for b in parentless {
            names.push(b.clone());
            names.extend(tree.subtree(b).into_iter().map(String::from));
        }
    }
    let lands = lands.map(|r| {
        let ws = view_of.filter(|v| v != committer && has_workspace(v));
        (tree::short(&r).to_string(), ws)
    });
    let subtree: HashSet<&str> = tree.subtree(committer).into_iter().collect();
    let mut seen = HashSet::new();
    let others = (names.into_iter())
        .filter(|b| b != committer && has_workspace(b) && seen.insert(b.clone()))
        .map(|branch| Other {
            lands: match subtree.contains(branch.as_str()) || lands.is_none() {
                true => Lands::InCommitter,
                false => Lands::WithCommitter,
            },
            follower: if tree.follows_parent(&branch, committer) {
                Follower::Other
            } else if tree.follows_parent(committer, &branch) {
                Follower::Committer
            } else {
                Follower::Neither
            },
            branch,
        })
        .collect();
    Related { others, lands }
}

/// The paths the step's own commits change (`git log`, renames as a
/// deletion and an addition, merges by themselves none): those after where
/// its own commits start (`follow::own_base`; the root's are all its own);
/// and whether the step took commits from there too (a rebase onto its
/// parent, a merge of it: then a conflict counts only on these paths).
/// `None` if there are none.
fn own_paths(common_dir: &Path, tree: &Tree, step: &Step) -> Result<Option<(PathTree, bool)>> {
    let branch = &step.branch;
    let start = match tree.is_root(branch) {
        true => None,
        false => {
            let parent = tree.parent(branch);
            Some(crate::follow::own_base(
                common_dir,
                branch,
                parent,
                &tree.root_refs(),
                step.to,
            )?)
        }
    };
    // Not commits already on a remote: a pull, a fast-forward to upstream,
    // is no one's work in a workspace. A branch's own remote branch's are
    // its own, though (pushed right after committing, before this check),
    // but the root's (where pulls come from). A remote ref that can't be read
    // is left out (`--ignore-missing`), as the views leave it.
    let mut args: Vec<String> = [
        "log",
        "--ignore-missing",
        "--no-renames",
        "--name-only",
        "-z",
        "--format=",
    ]
    .map(String::from)
    .into();
    args.push(format!("{}..{}", step.from, step.to));
    args.extend(start.map(|s| format!("^{s}")));
    args.push("--not".into());
    if start.is_some() {
        args.push(format!("--exclude=*/{branch}"));
    }
    args.push("--remotes".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git::cmd::output(common_dir, &args)?;
    // Each commit's paths after a newline (its empty format).
    let paths = PathTree::new(
        (out.split(|b| *b == 0)
            .map(|p| p.strip_prefix(b"\n").unwrap_or(p)))
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec),
    );
    if paths.is_empty() {
        return Ok(None);
    }
    let took = match start {
        Some(start) => !crate::follow::is_ancestor(common_dir, start, step.from)?,
        None => false,
    };
    Ok(Some((paths, took)))
}

/// `other`, if the step may conflict with its work: its uncommitted changes
/// (as stored) or its commits since its merge base with the step's tip touch
/// one of the step's `paths`.
fn candidate<'a>(
    common_dir: &Path,
    step: &Step,
    other: &'a Other,
    paths: &PathTree,
    bases: &mut Bases,
) -> Result<Option<Candidate<'a>>> {
    let tip = git::branch_tip(&other.branch)?;
    let Some(base) = bases.get(step.to, tip.id)? else {
        return Ok(None);
    };
    let uncommitted = StateDir::of(common_dir, &other.branch).may_change(paths.iter())?;
    if !uncommitted {
        let committed = git::cmd::changed_paths(common_dir, &format!("{base}..{}", tip.id))?;
        if !committed.iter().any(|p| paths.touches(p)) {
            return Ok(None);
        }
    }
    Ok(Some(Candidate {
        other,
        tip: (tip.id, tip.tree),
        base,
        before: bases.get(step.from, tip.id)?,
        uncommitted,
    }))
}

/// The new conflicts of the step (its tips before and after, `step`) with
/// candidate `c`, its workspace as it is `live`: git's merge of `live` into
/// the step's tip, but on paths that conflicted in that into the tip before
/// (in whichever way), and with `only`, on paths it touches only. Those where the merge of its tip conflicts too are with
/// its committed work (all of them, if its uncommitted work can't meet the
/// step's).
fn new_conflicts(
    private: &Private,
    store: &Store,
    (from, to): (Tip, Tip),
    c: &Candidate,
    live: Tip,
    only: Option<&PathTree>,
) -> Result<Option<Conflicts>> {
    let after = private.conflicts(store, c.base, to, live)?;
    if after.is_empty() {
        return Ok(None);
    }
    // By path: one that conflicted before, in another way, isn't new.
    let before: HashSet<Vec<u8>> = match c.before {
        Some(base) => (private.conflicts(store, base, from, live)?.into_iter())
            .map(|(_, path)| path)
            .collect(),
        None => HashSet::new(),
    };
    let new: Paths = (after.into_iter())
        .map(|(_, path)| path)
        .filter(|path| !before.contains(path))
        .filter(|path| only.is_none_or(|only| only.touches(path)))
        .collect();
    if new.is_empty() {
        return Ok(None);
    }
    if !c.uncommitted {
        return Ok(Some(Conflicts {
            uncommitted: Paths::new(),
            committed: new,
        }));
    }
    let committed: HashSet<Vec<u8>> = (private.conflicts(store, c.base, to, c.tip)?.into_iter())
        .map(|(_, path)| path)
        .collect();
    let (committed, uncommitted) = new.into_iter().partition(|p| committed.contains(p));
    Ok(Some(Conflicts {
        uncommitted,
        committed,
    }))
}

/// A message: its subject and its text.
type Message = (String, String);

/// The messages of a step of `committer` (its tip `short`, landing as
/// `lands` says) with conflicts `pairs`, by workspace, for those told
/// (`on`): each conflict said once for all involved, to each of them; the
/// subject names the paths. The hint: the side whose change is uncommitted
/// (still open, the cheapest); else, of a branch and one following it, the
/// child, as following's pause says; else the committer (committed last).
fn told(
    committer: &str,
    short: &str,
    lands: Option<&(String, Option<String>)>,
    pairs: &[Pair],
    on: impl Fn(&str) -> bool,
) -> BTreeMap<String, Message> {
    let mut said: BTreeMap<String, (Paths, Vec<String>)> = BTreeMap::new();
    for pair in pairs {
        let other = pair.other.branch.as_str();
        let (land, land_ws) = match (pair.other.lands, lands) {
            (Lands::WithCommitter, Some((name, ws))) => (name.as_str(), ws.as_deref()),
            _ => (committer, Some(committer)),
        };
        let mut texts = Vec::new();
        for (paths, committed) in [
            (&pair.conflicts.uncommitted, false),
            (&pair.conflicts.committed, true),
        ] {
            if paths.is_empty() {
                continue;
            }
            let (kind, tail, hint) = match committed {
                false => (
                    "uncommitted",
                    String::new(),
                    format!(
                        "{other}'s change is the one still open, so that's the cheapest place \
                         to adjust"
                    ),
                ),
                true => (
                    "committed",
                    format!(", so the two will conflict when they land in {land}"),
                    match pair.other.follower {
                        Follower::Other => built_on(other, committer),
                        Follower::Committer => built_on(committer, other),
                        Follower::Neither => format!("{committer} committed last"),
                    },
                ),
            };
            let follows = match (pair.other.follower, committed) {
                (Follower::Other, false) => format!(
                    "\n{other} follows {committer}: it takes the commit once they don't \
                     conflict (until then following says nothing moved)."
                ),
                _ => String::new(),
            };
            let paths = crate::follow::quoted(paths).join(", ");
            texts.push(format!(
                "{committer} committed {short}; it conflicts with {other}'s {kind} \
                 {paths}{tail}.\nIf it needs resolving, {hint}.{follows}"
            ));
        }
        let all = (pair.conflicts.uncommitted.iter()).chain(&pair.conflicts.committed);
        let involved: BTreeSet<&str> = [Some(committer), Some(other), land_ws]
            .into_iter()
            .flatten()
            .collect();
        for to in involved.into_iter().filter(|to| on(to)) {
            let (paths, said) = said.entry(to.to_string()).or_default();
            paths.extend(all.clone().cloned());
            said.extend(texts.iter().cloned());
        }
    }
    (said.into_iter())
        .map(|(to, (paths, texts))| {
            let quoted = crate::follow::quoted(&paths);
            let subject = match quoted.len() {
                0..=3 => quoted.join(", "),
                n => format!("{} and {} more", quoted[..2].join(", "), n - 2),
            };
            (
                to,
                (format!("conflict: {subject}"), texts.join("\n\n") + "\n"),
            )
        })
        .collect()
}

/// The hint where `child` follows `parent`: following's pause says the same.
fn built_on(child: &str, parent: &str) -> String {
    format!(
        "{child} is built on {parent}: that's where it gets resolved (following is paused on it)"
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_repo::{branch, commit, private, run, tip};
    use super::*;

    fn id(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    fn base(a: &str, b: &str) -> ObjectId {
        id(&run(&["merge-base", a, b]))
    }

    fn paths(list: &[&str]) -> Paths {
        list.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    fn tip_of(commit: &str) -> Tip {
        let c = tip(commit);
        (c.id, c.tree)
    }

    fn other(name: &str, follower: Follower) -> Other {
        Other {
            branch: name.into(),
            lands: Lands::WithCommitter,
            follower,
        }
    }

    /// The new conflicts of a step from `from` to `to` with a workspace at
    /// `at`, as it is `live` (its uncommitted work meeting the step's, if
    /// `uncommitted`).
    fn found(
        (from, to): (&str, &str),
        at: &str,
        live: &str,
        uncommitted: bool,
        only: Option<&PathTree>,
    ) -> Option<Conflicts> {
        let ui = other("ui", Follower::Neither);
        let c = Candidate {
            other: &ui,
            tip: tip_of(at),
            base: base(to, at),
            before: Some(base(from, at)),
            uncommitted,
        };
        let step = (tip_of(from), tip_of(to));
        new_conflicts(private(), &Store::default(), step, &c, tip_of(live), only).unwrap()
    }

    fn conflicts(uncommitted: &[&str], committed: &[&str]) -> Option<Conflicts> {
        Some(Conflicts {
            uncommitted: paths(uncommitted),
            committed: paths(committed),
        })
    }

    #[test]
    fn new_conflicts_by_kind() {
        let c0 = commit(None, &[("README", Some("shop\n")), ("x", Some("x\n"))]);
        let api = commit(Some(&c0), &[("README", Some("api\n"))]);
        // ui, built on c0, edits README uncommitted: its live tree.
        let ui_live = commit(Some(&c0), &[("README", Some("ui\n"))]);
        let step = (c0.as_str(), api.as_str());
        assert_eq!(
            found(step, &c0, &ui_live, true, None),
            conflicts(&["README"], &[])
        );
        // Committed: its tip has it; with nothing uncommitted that may meet
        // the step's, all committed.
        assert_eq!(
            found(step, &ui_live, &ui_live, true, None),
            conflicts(&[], &["README"])
        );
        assert_eq!(
            found(step, &ui_live, &ui_live, false, None),
            conflicts(&[], &["README"])
        );
        // Only where the step's own commits changed something.
        let only = |p: &str| PathTree::new([p.as_bytes().to_vec()]);
        assert_eq!(found(step, &c0, &ui_live, true, Some(&only("x"))), None);
        assert!(found(step, &c0, &ui_live, true, Some(&only("README"))).is_some());
        // No conflict: none.
        let ui_other = commit(Some(&c0), &[("x", Some("ui\n"))]);
        assert_eq!(found(step, &c0, &ui_other, true, None), None);
    }

    #[test]
    fn conflicts_seen_before_are_not_new() {
        let c0 = commit(None, &[("README", Some("shop\n")), ("y", Some("y\n"))]);
        let api1 = commit(Some(&c0), &[("README", Some("api\n"))]);
        let ui_live = commit(Some(&c0), &[("README", Some("ui\n"))]);
        let new = |from: &str, to: &str| found((from, to), &c0, &ui_live, true, None);
        // Another commit of api, on README again or elsewhere: told once.
        let api2 = commit(Some(&api1), &[("README", Some("api 2\n"))]);
        assert_eq!(new(&api1, &api2), None);
        let api3 = commit(Some(&api2), &[("y", Some("y3\n"))]);
        assert_eq!(new(&api2, &api3), None);
        // Gone (api took README back), then back: new again.
        let api4 = commit(Some(&api3), &[("README", Some("shop\n"))]);
        assert_eq!(new(&api3, &api4), None);
        let api5 = commit(Some(&api4), &[("README", Some("api 5\n"))]);
        assert!(new(&api4, &api5).is_some());
        // The same path conflicting another way (api deletes what ui edits):
        // not new.
        let api6 = commit(Some(&api5), &[("README", None)]);
        assert_eq!(new(&api5, &api6), None);
    }

    #[test]
    fn own_paths_leave_the_parents_commits_out() {
        let c0 = commit(None, &[("p", Some("p\n")), ("a", Some("a\n"))]);
        let p1 = branch("n1-p", &commit(Some(&c0), &[("p", Some("p1\n"))]));
        let a1 = branch("n1-a", &commit(Some(&c0), &[("a", Some("a1\n"))]));
        let stored = [("n1-a".to_string(), "refs/heads/n1-p".to_string())].into();
        let tree = Tree::of(stored, Some("main".into()), |_| true);
        let own = |from: &str, to: &str| {
            let step = Step {
                branch: "n1-a".into(),
                from: id(from),
                to: id(to),
            };
            let found = own_paths(&super::super::test_repo::git_dir(), &tree, &step).unwrap();
            found.map(|(paths, took)| (paths.iter().map(<[u8]>::to_vec).collect::<Paths>(), took))
        };
        assert_eq!(own(&c0, &a1), Some((paths(&["a"]), false)));
        // Rebased onto p1 by hand: its own commits only, and only their
        // paths.
        let a1_on_p1 = commit(Some(&p1), &[("a", Some("a1\n"))]);
        let a2 = commit(Some(&a1_on_p1), &[("a", Some("a2\n"))]);
        assert_eq!(own(&a1, &a2), Some((paths(&["a"]), true)));
        // A merge of the parent only: nothing of its own.
        let merge = run(&[
            "commit-tree",
            &format!("{a1}^{{tree}}"),
            "-p",
            &a1,
            "-p",
            &p1,
            "-m",
            "merge",
        ]);
        assert_eq!(own(&a1, &merge), None);
        // Its own commits pushed (before the check): still its own.
        super::super::test_repo::remote_branch("origin/n1-a", &a1);
        assert_eq!(own(&c0, &a1), Some((paths(&["a"]), false)));
        // A reset back: nothing.
        assert_eq!(own(&a1, &c0), None);
    }

    #[test]
    fn the_roots_commits_on_a_remote_are_no_ones() {
        let c0 = commit(None, &[("u", Some("u\n")), ("l", Some("l\n"))]);
        let u1 = commit(Some(&c0), &[("u", Some("u1\n"))]);
        super::super::test_repo::remote_branch("origin/n2-root", &u1);
        let l1 = commit(Some(&u1), &[("l", Some("l1\n"))]);
        let tree = Tree::of(Default::default(), Some("n2-root".into()), |_| true);
        let own = |from: &str, to: &str| {
            let step = Step {
                branch: "n2-root".into(),
                from: id(from),
                to: id(to),
            };
            let found = own_paths(&super::super::test_repo::git_dir(), &tree, &step).unwrap();
            found.map(|(paths, _)| paths.iter().map(<[u8]>::to_vec).collect::<Paths>())
        };
        // Fast-forwarded to its upstream (a pull): nothing of its own.
        assert_eq!(own(&c0, &u1), None);
        // Its own commit on top: only that.
        assert_eq!(own(&c0, &l1), Some(paths(&["l"])));
    }

    #[test]
    fn whose_work_meets() {
        super::super::test_repo::git_dir(); // the repository open
        let stored = [("ui", "api"), ("web", "main"), ("css", "web")];
        let stored = (stored.iter())
            .map(|(b, p)| (b.to_string(), format!("refs/heads/{p}")))
            .collect();
        let tree = Tree::of(stored, Some("main".into()), |_| true);
        let workspaces: HashSet<String> = ["api", "lone", "ui", "web", "css", "main"]
            .map(String::from)
            .into();
        let of = |b: &str| {
            let r = related(&tree, b, &workspaces);
            let others: Vec<(String, Lands)> = (r.others.into_iter())
                .map(|o| (o.branch, o.lands))
                .collect();
            (others, r.lands)
        };
        let others = |list: &[(&str, Lands)]| -> Vec<(String, Lands)> {
            list.iter().map(|(b, l)| (b.to_string(), *l)).collect()
        };
        let lands = |name: &str| Some((name.to_string(), Some(name.to_string())));
        use Lands::{InCommitter as In, WithCommitter as With};
        // Without a parent: built on the root, a sibling of the others
        // without one and of those built on the root; its own children land
        // in it.
        assert_eq!(
            of("api"),
            (
                others(&[
                    ("main", With),
                    ("web", With),
                    ("css", With),
                    ("ui", In),
                    ("lone", With)
                ]),
                lands("main")
            )
        );
        assert_eq!(
            of("web"),
            (
                others(&[
                    ("main", With),
                    ("css", In),
                    ("api", With),
                    ("ui", With),
                    ("lone", With)
                ]),
                lands("main")
            )
        );
        // Built on a branch without a parent: its family only.
        assert_eq!(of("ui"), (others(&[("api", With)]), lands("api")));
        assert_eq!(of("css"), (others(&[("web", With)]), lands("web")));
        // The root: everything, landing in itself.
        assert_eq!(
            of("main"),
            (
                others(&[
                    ("web", In),
                    ("css", In),
                    ("api", In),
                    ("ui", In),
                    ("lone", In)
                ]),
                None
            )
        );
    }

    fn pair<'a>(other: &'a Other, uncommitted: &[&str], committed: &[&str]) -> Pair<'a> {
        Pair {
            other,
            conflicts: Conflicts {
                uncommitted: paths(uncommitted),
                committed: paths(committed),
            },
        }
    }

    #[test]
    fn everyone_involved_is_told_the_same() {
        let ui = Other {
            lands: Lands::InCommitter,
            ..other("cart-ui", Follower::Neither)
        };
        let said = told(
            "cart-api",
            "0724067",
            None,
            &[pair(&ui, &["README.md"], &[])],
            |_| true,
        );
        assert_eq!(said.keys().collect::<Vec<_>>(), ["cart-api", "cart-ui"]);
        assert_eq!(
            said["cart-ui"],
            (
                "conflict: README.md".to_string(),
                "cart-api committed 0724067; it conflicts with cart-ui's uncommitted README.md.\n\
                 If it needs resolving, cart-ui's change is the one still open, so that's the \
                 cheapest place to adjust.\n"
                    .to_string()
            )
        );
        assert_eq!(said["cart-api"], said["cart-ui"]);
        // Both committed, landing in their parent, which is told too; one
        // message each, with every conflict it is in; a branch set off
        // isn't told.
        let (api, web) = (
            other("cart-api", Follower::Neither),
            other("cart-web", Follower::Neither),
        );
        let pairs = [
            pair(&api, &[], &["README.md"]),
            pair(&web, &["a b", "c", "d"], &[]),
        ];
        let shop = ("shop".to_string(), Some("shop".to_string()));
        let said = told("cart-ui", "9f3e2a1", Some(&shop), &pairs, |b| {
            b != "cart-web"
        });
        assert_eq!(
            said.keys().collect::<Vec<_>>(),
            ["cart-api", "cart-ui", "shop"]
        );
        assert_eq!(
            said["cart-api"].1,
            "cart-ui committed 9f3e2a1; it conflicts with cart-api's committed README.md, so \
             the two will conflict when they land in shop.\nIf it needs resolving, cart-ui \
             committed last.\n"
        );
        assert!(said["shop"].1.starts_with(said["cart-api"].1.trim_end()));
        assert!(said["shop"].1.contains("cart-web's uncommitted a b, c, d."));
        assert_eq!(said["shop"].0, "conflict: README.md, a b and 2 more");
        assert_eq!(said["shop"], said["cart-ui"]);
    }

    #[test]
    fn a_following_child_is_where_it_gets_resolved() {
        let hint = "If it needs resolving, cart-ui is built on cart-api: that's where it gets \
                    resolved (following is paused on it).\n";
        let ui = Other {
            lands: Lands::InCommitter,
            ..other("cart-ui", Follower::Other)
        };
        let said = told(
            "cart-api",
            "0724067",
            None,
            &[pair(&ui, &[], &["README.md"])],
            |_| true,
        );
        assert_eq!(
            said["cart-ui"].1,
            format!(
                "cart-api committed 0724067; it conflicts with cart-ui's committed README.md, so \
                 the two will conflict when they land in cart-api.\n{hint}"
            )
        );
        // The child committing last: the same.
        let api = other("cart-api", Follower::Committer);
        let lands = ("cart-api".to_string(), Some("cart-api".to_string()));
        let said = told(
            "cart-ui",
            "9f3e2a1",
            Some(&lands),
            &[pair(&api, &[], &["README.md"])],
            |_| true,
        );
        assert!(said["cart-api"].1.ends_with(hint));
    }
}
