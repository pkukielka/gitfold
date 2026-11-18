//! Conflict notices: when a branch gets new commits in its workspace, the
//! workspaces whose work they now conflict with are told, in their inboxes
//! (`cli::inbox::deliver`, `from: gitfold`).
//!
//! - **The trigger** is a commit step a workspace's sync thread saw
//!   (`Workspace::take_commit`: its own commits, not gitfold's moves of it),
//!   handed to one thread per mount (`spawn`), which does the checks: they
//!   read other workspaces through the mount, which takes seconds.
//! - **Who is checked:** the workspaces of the branches that meet the
//!   committer's work where it lands: those in the view of its parent
//!   (`members`, the parent included). A branch without a parent (made
//!   from a start point, not built on anything) is taken as built on the
//!   root: the branches without one are siblings there, with those built on
//!   the root; the root itself checks them all.
//! - **What is new:** for each, git merges (`git merge-tree`, pairwise, from
//!   their merge base, as a view merges) the committer's tip with the
//!   workspace as it is (`Lives`: its commits and its uncommitted work), and
//!   so its tip before the step; conflicts only after are new. A conflict
//!   seen before isn't told again; one that went and came back is. Only
//!   the step's commits that aren't its parent's count (a rebase onto it, a
//!   merge of it, brings the parent's changes, not the committer's).
//!   Before any merge, a cheap check: a path the step's commits change that
//!   the workspace changes too, uncommitted (its upper files and markers) or
//!   committed (since the merge base).
//! - **Who is told** (`told`): everyone involved, the same words: both
//!   sides, and the workspace of the branch they land in (the committer's
//!   parent, or the committer if the other is built on it); each unless its
//!   branch says not (`gitfoldConflictNotices`, `gitfold.conflictNotices`).
//!   One message per workspace and step. It informs, and hints where a
//!   change would be cheapest if one is needed: the side whose change is
//!   uncommitted (still open); else, of a branch and the one following it,
//!   the child (where following's pause says it gets resolved); else the
//!   committer (committed last). gitfold decides nothing: whether it
//!   matters is for the agents or their lead.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::{Weak, mpsc};
use std::thread;

use anyhow::Result;
use gix::ObjectId;
use log::{info, warn};

use super::lives::Reach;
use super::members::{self, Bases};
use super::merge::Private;
use super::serve::Views;
use crate::git;
use crate::settings::Local;
use crate::state::markers::{Markers, hidden};
use crate::state::{self, StateDir, ancestors, upper_path};
use crate::tree::{self, Tree};

/// New commits of `branch`, seen in its workspace: its tip went from `from`
/// to `to`.
pub struct Step {
    pub branch: String,
    pub from: ObjectId,
    pub to: ObjectId,
}

/// Starts the thread checking the commit steps sent to what it returns
/// (`Workspaces::set_commits`), while `views` exist.
pub fn spawn(views: Weak<Views>) -> mpsc::Sender<Step> {
    let (send, steps) = mpsc::channel::<Step>();
    let spawned = thread::Builder::new()
        .name("notices".into())
        .spawn(move || {
            for step in steps {
                let Some(views) = views.upgrade() else { return };
                if let Err(e) = check(&views, &step) {
                    let to = &step.to;
                    warn!("conflict notices: {} at {to}: {e:#}", step.branch);
                }
            }
        });
    if let Err(e) = spawned {
        warn!("conflict notices: can't start their thread: {e}");
    }
    send
}

/// A path set, as git names paths.
type Paths = BTreeSet<Vec<u8>>;

/// The new conflicts of a step with one workspace: on paths its
/// uncommitted work changes, and on paths only committed work does.
#[derive(Debug, Default, PartialEq)]
pub struct Conflicts {
    pub uncommitted: Paths,
    pub committed: Paths,
}

/// One workspace's conflicts with a step.
struct Pair {
    other: String,
    found: Conflicts,
    /// Where the two land: the committer if the other is built on it, else
    /// the committer's parent (as shown); and that branch's workspace, if
    /// it has one.
    land: (String, Option<String>),
    /// The one of the two built on the other and following it, if one is
    /// (its following stops on these).
    follower: Option<String>,
}

/// Checks `step` (see the module doc).
fn check(views: &Views, step: &Step) -> Result<()> {
    let common_dir = &views.common_dir;
    let local = Local::read(common_dir);
    let tree = Tree::read(common_dir)?;
    let committer = step.branch.as_str();
    let opened: HashSet<String> = (state::list(common_dir)?.into_iter())
        .filter(|s| s.state.opened())
        .map(|s| s.branch)
        .collect();
    let Related { others, land } = related(&tree, committer, &opened);
    let land_ws = land.as_ref().and_then(|(_, ws)| ws.as_ref());
    let mut everyone = (others.iter().chain(land_ws)).chain(std::iter::once(&step.branch));
    if !everyone.any(|b| local.conflict_notices(b)) {
        return Ok(());
    }
    // A branch without a parent: built on the root, as for `related`.
    let parents = match tree.parent(committer) {
        Some(p) => vec![p.to_string()],
        None if !tree.is_root(committer) => tree.root_refs(),
        None => Vec::new(),
    };
    let (paths, filter) = own_paths(common_dir, step, &parents)?;
    if paths.is_empty() {
        return Ok(());
    }
    let private = views.private().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let reach = Reach {
        store: &views.store,
        names: &views.names,
        workspaces: &views.workspaces,
        private: &private,
    };
    let mut bases = Bases::default();
    let mut pairs = Vec::new();
    for other in &others {
        let found = (|| -> Result<Option<Conflicts>> {
            let tip = git::branch_tip(other)?.id;
            let Some(base) = bases.get(step.to, tip)? else {
                return Ok(None);
            };
            let state = StateDir::of(common_dir, other);
            if !touched(&state, &paths)? && !meets(&changed(common_dir, base, tip)?, &paths) {
                return Ok(None);
            }
            for commit in [step.to, step.from, tip, base] {
                views
                    .store
                    .load_tree(git::resolve(&commit.to_string())?.tree)?;
            }
            let Some(live) = views.lives.tip(&reach, other, tip, true)? else {
                return Ok(None);
            };
            let before = bases.get(step.from, tip)?;
            new_conflicts(
                &private,
                step,
                (base, before),
                tip,
                live.0,
                filter.then_some(&paths),
            )
        })();
        match found {
            Ok(Some(found)) => {
                let built_on = tree.subtree(committer).contains(&other.as_str());
                pairs.push(Pair {
                    other: other.clone(),
                    found,
                    land: match (built_on, &land) {
                        (false, Some(land)) => land.clone(),
                        _ => (committer.to_string(), Some(committer.to_string())),
                    },
                    follower: [(other.as_str(), committer), (committer, other.as_str())]
                        .into_iter()
                        .find(|(child, parent)| {
                            tree.parent(child) == Some(format!("refs/heads/{parent}").as_str())
                                && tree.follows(child)
                        })
                        .map(|(child, _)| child.to_string()),
                });
            }
            Ok(None) => {}
            Err(e) => warn!("conflict notices: {committer} and {other}: {e:#}"),
        }
    }
    if pairs.is_empty() {
        return Ok(());
    }
    let short = git::cmd::output(common_dir, &["rev-parse", "--short", &step.to.to_string()])?;
    let short = String::from_utf8_lossy(&short).trim().to_string();
    let on = |b: &str| local.conflict_notices(b);
    for (to, (subject, body)) in told(committer, &short, &pairs, on) {
        match crate::cli::inbox::deliver(common_dir, &to, "gitfold", Some(&subject), &body) {
            Ok(()) => info!("conflict notices: {committer} {short}: told {to}"),
            Err(e) => warn!("conflict notices: telling {to} of {committer} {short}: {e:#}"),
        }
    }
    Ok(())
}

/// Whose work meets `committer`'s (see the module doc).
struct Related {
    /// The branches with a workspace whose work meets it (`committer` left
    /// out).
    others: Vec<String>,
    /// Where it lands with those not built on it, as shown, and that
    /// branch's workspace if it has one: its parent; for a branch without
    /// one, the root. None for the root itself.
    land: Option<(String, Option<String>)>,
}

/// Whose work meets `committer`'s where it lands, among the branches with a
/// workspace (`workspaces`): those of the view of its parent (a local
/// branch, or origin's root as the root), the parent with them; of a
/// remote-tracking parent, what is built on it. A branch without a parent
/// (but the root) is taken as built on the root: with the root's view, the
/// branches without one count (as siblings), with what is built on them,
/// and the root.
fn related(tree: &Tree, committer: &str, workspaces: &HashSet<String>) -> Related {
    let has_workspace = |b: &str| workspaces.contains(b);
    let root = tree.root();
    let parent = tree.parent(committer);
    let (view_of, land) = match parent {
        Some(p) => {
            let view = (p.strip_prefix("refs/heads/").map(String::from)).or_else(|| {
                let root = root?;
                (p == format!("refs/remotes/origin/{root}")).then(|| root.to_string())
            });
            (view, Some(tree::short(p).to_string()))
        }
        None => match root.filter(|r| *r != committer) {
            Some(root) => (Some(root.to_string()), Some(root.to_string())),
            None => (Some(committer.to_string()), None),
        },
    };
    let mut others: Vec<String> = match (&view_of, parent) {
        (Some(view), _) => {
            let origin = members::origin_root(tree, view);
            let members = members::members(tree, view, origin.as_deref(), has_workspace);
            let members = members.into_iter().map(|m| m.branch);
            std::iter::once(view.clone()).chain(members).collect()
        }
        (None, Some(p)) => (tree.built_on(&[p.to_string()]).into_iter())
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
            others.push(b.clone());
            others.extend(tree.subtree(b).into_iter().map(String::from));
        }
    }
    others.retain(|b| b != committer && has_workspace(b));
    let mut seen = HashSet::new();
    others.retain(|b| seen.insert(b.clone()));
    let ws = view_of.filter(|v| v != committer && has_workspace(v));
    Related {
        others,
        land: land.map(|name| (name, ws)),
    }
}

/// The paths the step's own commits change (`git log`, renames as a
/// deletion and an addition, merges by themselves none), not those of its
/// parent's commits among them (`parents`: its parent, or the root's refs for a
/// branch without one; a rebase onto it, a merge of it);
/// and whether there were any of those (then a conflict counts only on
/// these paths).
fn own_paths(common_dir: &Path, step: &Step, parents: &[String]) -> Result<(Paths, bool)> {
    let range = format!("{}..{}", step.from, step.to);
    let parents = parents.iter().filter(|p| git::resolve(p).is_ok());
    let not: Vec<&str> = match parents.map(String::as_str).collect::<Vec<_>>() {
        found if found.is_empty() => Vec::new(),
        found => [vec!["--not"], found].concat(),
    };
    let count = |not: &[&str]| -> Result<usize> {
        let args = [&["rev-list", "--count", &range][..], not].concat();
        Ok(
            String::from_utf8_lossy(&git::cmd::output(common_dir, &args)?)
                .trim()
                .parse()?,
        )
    };
    let own = count(&not)?;
    if own == 0 {
        return Ok((Paths::new(), false));
    }
    let filter = !not.is_empty() && count(&[])? != own;
    let args = [
        "log",
        "--no-renames",
        "--name-only",
        "-z",
        "--format=",
        &range,
    ];
    let out = git::cmd::output(common_dir, &[&args[..], &not].concat())?;
    let paths = (out
        .split(|b| *b == 0)
        .map(|p| p.strip_prefix(b"\n").unwrap_or(p)))
    .filter(|p| !p.is_empty())
    .map(<[u8]>::to_vec)
    .collect();
    Ok((paths, filter))
}

/// The paths changed from `base` to `tip` (renames as a deletion and an
/// addition).
fn changed(common_dir: &Path, base: ObjectId, tip: ObjectId) -> Result<Paths> {
    let (base, tip) = (base.to_string(), tip.to_string());
    let args = ["diff", "--no-renames", "--name-only", "-z", &base, &tip];
    let out = git::cmd::output(common_dir, &args)?;
    Ok(git::cmd::nul_paths(&out).into_iter().collect())
}

/// Whether `path` is in `set`, or below or above a path there.
fn meets_path(set: &Paths, path: &[u8]) -> bool {
    let below = [path, b"/"].concat();
    set.contains(path)
        || ancestors(path).any(|a| set.contains(a))
        || set
            .range(below.clone()..)
            .next()
            .is_some_and(|p| p.starts_with(&below))
}

/// Whether a path of `a` is in `b`, or below or above a path there.
fn meets(a: &Paths, b: &Paths) -> bool {
    a.iter().any(|p| meets_path(b, p))
}

/// Whether the workspace of `state` may have uncommitted changes on
/// `paths`, as stored: an upper entry at one of them (or a file above
/// one), or a marker hiding one. More than git would say: a check before
/// git's.
fn touched(state: &StateDir, paths: &Paths) -> Result<bool> {
    let upper = state.upper();
    let (whiteouts, opaque) = Markers::read(&state.markers())?;
    let has = |p: &[u8], file: bool| {
        std::fs::symlink_metadata(upper_path(&upper, p)).is_ok_and(|m| !file || !m.is_dir())
    };
    Ok(paths.iter().any(|p| {
        has(p, false) || ancestors(p).any(|a| has(a, true)) || hidden(&whiteouts, &opaque, p)
    }))
}

/// The conflicts of `step` with the workspace of a branch at `tip`, as it
/// is (`live`), that are new: git's merge (`Private::conflicts`) of `live`
/// into the step's tip, from their merge base (`bases.0`), less those of
/// the merge into the tip before (from `bases.1`), and with `only`, on
/// paths there only. Each is uncommitted unless the merge of `tip` itself
/// conflicts on its path too.
fn new_conflicts(
    private: &Private,
    step: &Step,
    bases: (ObjectId, Option<ObjectId>),
    tip: ObjectId,
    live: ObjectId,
    only: Option<&Paths>,
) -> Result<Option<Conflicts>> {
    let after = private.conflicts(bases.0, step.to, live)?;
    if after.is_empty() {
        return Ok(None);
    }
    let before: HashSet<(String, Vec<u8>)> = match bases.1 {
        Some(base) => private
            .conflicts(base, step.from, live)?
            .into_iter()
            .collect(),
        None => HashSet::new(),
    };
    let new: Paths = (after.into_iter())
        .filter(|c| !before.contains(c))
        .filter(|(_, path)| only.is_none_or(|only| meets_path(only, path)))
        .map(|(_, path)| path)
        .collect();
    if new.is_empty() {
        return Ok(None);
    }
    let committed: Paths = (private.conflicts(bases.0, step.to, tip)?.into_iter())
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

/// The messages of a step of `committer` (its tip `short`) with conflicts
/// `pairs`, by workspace, for those told (`on`): each conflict said once
/// for all involved, to each of them (see the module doc); the subject
/// names the paths.
fn told(
    committer: &str,
    short: &str,
    pairs: &[Pair],
    on: impl Fn(&str) -> bool,
) -> BTreeMap<String, Message> {
    let mut said: BTreeMap<String, (Paths, Vec<String>)> = BTreeMap::new();
    let list = |paths: &Paths| {
        let paths: Vec<Vec<u8>> = paths.iter().cloned().collect();
        crate::follow::quoted(&paths).join(", ")
    };
    for pair in pairs {
        let other = pair.other.as_str();
        let land = &pair.land.0;
        let mut texts = Vec::new();
        if !pair.found.uncommitted.is_empty() {
            let paths = list(&pair.found.uncommitted);
            let follows = match pair.follower.as_deref() == Some(other) {
                true => format!(
                    "\n{other} follows {committer}: it takes the commit once they don't \
                     conflict (until then following says nothing moved)."
                ),
                false => String::new(),
            };
            texts.push(format!(
                "{committer} committed {short}; it conflicts with {other}'s uncommitted \
                 {paths}.\nIf it needs resolving, {other}'s change is the one still open, so \
                 that's the cheapest place to adjust.{follows}"
            ));
        }
        if !pair.found.committed.is_empty() {
            let paths = list(&pair.found.committed);
            // Following's pause says the same: the child rebases.
            let hint = match pair.follower.as_deref() {
                Some(child) => {
                    let parent = if child == other { committer } else { other };
                    format!(
                        "{child} is built on {parent}: that's where it gets resolved \
                         (following is paused on it)"
                    )
                }
                None => format!("{committer} committed last"),
            };
            texts.push(format!(
                "{committer} committed {short}; it conflicts with {other}'s committed \
                 {paths}, so the two will conflict when they land in {land}.\nIf it needs \
                 resolving, {hint}."
            ));
        }
        let paths: Paths = (pair.found.uncommitted.iter())
            .chain(&pair.found.committed)
            .cloned()
            .collect();
        let involved = [Some(committer), Some(other), pair.land.1.as_deref()];
        let mut seen = Vec::new();
        for to in involved.into_iter().flatten() {
            if !seen.contains(&to) && on(to) {
                seen.push(to);
                let (all, said) = said.entry(to.to_string()).or_default();
                all.extend(paths.iter().cloned());
                said.extend(texts.iter().cloned());
            }
        }
    }
    (said.into_iter())
        .map(|(to, (paths, texts))| {
            let quoted = crate::follow::quoted(&paths.into_iter().collect::<Vec<_>>());
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

#[cfg(test)]
mod tests {
    use super::super::test_repo::{branch, commit, git_dir, private, run};
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

    /// The new conflicts of `branch` going from `from` to `to` with a
    /// workspace at `tip`, as it is `live`.
    fn found(
        from: &str,
        to: &str,
        tip: &str,
        live: &str,
        only: Option<&Paths>,
    ) -> Option<Conflicts> {
        let step = Step {
            branch: "x".into(),
            from: id(from),
            to: id(to),
        };
        let bases = (base(to, tip), Some(base(from, tip)));
        new_conflicts(private(), &step, bases, id(tip), id(live), only).unwrap()
    }

    #[test]
    fn new_conflicts_by_kind() {
        let c0 = commit(None, &[("README", Some("shop\n")), ("x", Some("x\n"))]);
        let api = commit(Some(&c0), &[("README", Some("api\n"))]);
        // ui, built on c0, edits README uncommitted: its live tree.
        let ui_live = commit(Some(&c0), &[("README", Some("ui\n"))]);
        assert_eq!(
            found(&c0, &api, &c0, &ui_live, None),
            Some(Conflicts {
                uncommitted: paths(&["README"]),
                committed: Paths::new()
            })
        );
        // Committed: its tip has it.
        assert_eq!(
            found(&c0, &api, &ui_live, &ui_live, None),
            Some(Conflicts {
                uncommitted: Paths::new(),
                committed: paths(&["README"])
            })
        );
        // Only where the step's own commits changed something.
        assert_eq!(found(&c0, &api, &c0, &ui_live, Some(&paths(&["x"]))), None);
        assert!(found(&c0, &api, &c0, &ui_live, Some(&paths(&["README"]))).is_some());
        // No conflict: none.
        let ui_other = commit(Some(&c0), &[("x", Some("ui\n"))]);
        assert_eq!(found(&c0, &api, &c0, &ui_other, None), None);
    }

    #[test]
    fn conflicts_seen_before_are_not_new() {
        let c0 = commit(None, &[("README", Some("shop\n")), ("y", Some("y\n"))]);
        let api1 = commit(Some(&c0), &[("README", Some("api\n"))]);
        let ui_live = commit(Some(&c0), &[("README", Some("ui\n"))]);
        // Another commit of api, on README again or elsewhere: told once.
        let api2 = commit(Some(&api1), &[("README", Some("api 2\n"))]);
        assert_eq!(found(&api1, &api2, &c0, &ui_live, None), None);
        let api3 = commit(Some(&api2), &[("y", Some("y3\n"))]);
        assert_eq!(found(&api2, &api3, &c0, &ui_live, None), None);
        // Gone (api took README back), then back: new again.
        let api4 = commit(Some(&api3), &[("README", Some("shop\n"))]);
        assert_eq!(found(&api3, &api4, &c0, &ui_live, None), None);
        let api5 = commit(Some(&api4), &[("README", Some("api 5\n"))]);
        assert!(found(&api4, &api5, &c0, &ui_live, None).is_some());
    }

    #[test]
    fn own_paths_leave_the_parents_commits_out() {
        let c0 = commit(None, &[("p", Some("p\n")), ("a", Some("a\n"))]);
        let p1 = branch("n1-p", &commit(Some(&c0), &[("p", Some("p1\n"))]));
        let a1 = commit(Some(&c0), &[("a", Some("a1\n"))]);
        let step = |from: &str, to: &str| Step {
            branch: "n1-a".into(),
            from: id(from),
            to: id(to),
        };
        let own =
            |from, to| own_paths(&git_dir(), &step(from, to), &["refs/heads/n1-p".into()]).unwrap();
        assert_eq!(own(&c0, &a1), (paths(&["a"]), false));
        // Rebased onto p1 by hand: its own commit only, and only its paths.
        let a1_on_p1 = commit(Some(&p1), &[("a", Some("a1\n"))]);
        let a2 = commit(Some(&a1_on_p1), &[("a", Some("a2\n"))]);
        assert_eq!(own(&a1, &a2), (paths(&["a"]), true));
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
        assert_eq!(own(&a1, &merge).0, Paths::new());
        // A reset back: nothing.
        assert_eq!(own(&a1, &c0).0, Paths::new());
    }

    #[test]
    fn paths_meet_through_directories() {
        let set = paths(&["d/x", "f"]);
        assert!(meets_path(&set, b"d/x"));
        assert!(meets_path(&set, b"d"));
        assert!(meets_path(&set, b"f/y"));
        assert!(!meets_path(&set, b"dx"));
        assert!(!meets_path(&set, b"e"));
    }

    #[test]
    fn whose_work_meets() {
        git_dir(); // the repository open
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
            (r.others, r.land)
        };
        let names = |list: &[&str]| list.iter().map(|b| b.to_string()).collect::<Vec<_>>();
        let land = |name: &str, ws: Option<&str>| Some((name.to_string(), ws.map(String::from)));
        // Without a parent: built on the root, a sibling of the others
        // without one and of those built on the root.
        let main = land("main", Some("main"));
        assert_eq!(
            of("api"),
            (names(&["main", "web", "css", "ui", "lone"]), main.clone())
        );
        assert_eq!(
            of("web"),
            (names(&["main", "css", "api", "ui", "lone"]), main)
        );
        // Built on a branch without a parent: its family only.
        assert_eq!(of("ui"), (names(&["api"]), land("api", Some("api"))));
        assert_eq!(of("css"), (names(&["web"]), land("web", Some("web"))));
        // The root: everything, landing in itself.
        assert_eq!(
            of("main"),
            (names(&["web", "css", "api", "ui", "lone"]), None)
        );
    }

    #[test]
    fn a_following_child_is_where_it_gets_resolved() {
        let mut p = pair(
            "cart-ui",
            &[],
            &["README.md"],
            ("cart-api", Some("cart-api")),
        );
        p.follower = Some("cart-ui".into());
        let said = told("cart-api", "0724067", &[p], |_| true);
        assert_eq!(
            said["cart-ui"].1,
            "cart-api committed 0724067; it conflicts with cart-ui's committed README.md, so \
             the two will conflict when they land in cart-api.\nIf it needs resolving, cart-ui \
             is built on cart-api: that's where it gets resolved (following is paused on it).\n"
        );
        // The child committing last: the same.
        let mut p = pair(
            "cart-api",
            &[],
            &["README.md"],
            ("cart-api", Some("cart-api")),
        );
        p.follower = Some("cart-ui".into());
        let said = told("cart-ui", "9f3e2a1", &[p], |_| true);
        assert!(said["cart-api"].1.ends_with(
            "If it needs resolving, cart-ui is built on cart-api: that's where it gets resolved \
             (following is paused on it).\n"
        ));
    }

    fn pair(
        other: &str,
        uncommitted: &[&str],
        committed: &[&str],
        land: (&str, Option<&str>),
    ) -> Pair {
        Pair {
            other: other.into(),
            found: Conflicts {
                uncommitted: paths(uncommitted),
                committed: paths(committed),
            },
            land: (land.0.into(), land.1.map(String::from)),
            follower: None,
        }
    }

    #[test]
    fn everyone_involved_is_told_the_same() {
        let pairs = [pair(
            "cart-ui",
            &["README.md"],
            &[],
            ("cart-api", Some("cart-api")),
        )];
        let said = told("cart-api", "0724067", &pairs, |_| true);
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
        let pairs = [
            pair("cart-api", &[], &["README.md"], ("shop", Some("shop"))),
            pair("cart-web", &["a b", "c", "d"], &[], ("shop", Some("shop"))),
        ];
        let said = told("cart-ui", "9f3e2a1", &pairs, |b| b != "cart-web");
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
}
