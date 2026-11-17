//! `gitfold list` (the workspaces, their changes and activity) and `gitfold
//! changes` (one workspace's changed paths; with `-- PATH`, who is changing
//! those paths).

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

use super::tree::{landed_said, paused};
use super::write_path;
use crate::follow;
use crate::git;
use crate::mount_table;
use crate::settings;
use crate::state::retire::{base_note, check_name};
use crate::state::status;
use crate::state::task::TaskDir;
use crate::state::{self, StateDir};
use crate::store::Store;
use crate::tree::Tree;

/// `gitfold changes`: the uncommitted changes of a workspace relative to
/// its base (normally its branch's tip), untracked files included and ignored
/// ones not, whatever is staged: what `git add -A` and `git diff --cached
/// --name-status <base>` would print, paths quoted as git quotes them, or
/// NUL-terminated and unquoted (`-z`). With `--stat`, each line also has the
/// lines added and removed (`-` for binary files), as `git diff --numstat`,
/// and a summary line follows, as `git diff --shortstat`. Read from its stored
/// state, without listing the tree; mounted or not. A branch never opened has
/// none.
pub fn changes(repo: &Path, args: ChangesArgs) -> Result<()> {
    let ChangesArgs {
        branch,
        nul,
        stat,
        paths: _,
    } = args;
    let name = &super::branch_or_current(branch, repo)?;
    check_name(name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, name);
    if !state.opened() {
        git::require_branch(name)?;
        return Ok(());
    }
    let store = Store::default();
    let s = status::status(&store, &state)?;
    let note = base_note(name, &s.base, mount_table::mounted(&common_dir));
    if !note.is_empty() {
        eprintln!("note: {name}: {note}; changes are against the stored base");
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let changes = s.changes(&store);
    let mut total = (0, 0);
    for &(change, path) in &changes {
        write!(out, "{}\t", change.letter())?;
        if stat {
            match s.line_counts(&store, &state, change, path)? {
                Some((added, removed)) => {
                    write!(out, "{added}\t{removed}\t")?;
                    total = (total.0 + added, total.1 + removed);
                }
                None => write!(out, "-\t-\t")?,
            }
        }
        write_path(&mut out, path, nul)?;
    }
    if stat && !nul && !changes.is_empty() {
        writeln!(out, "{}", shortstat(changes.len(), total))?;
    }
    out.flush()?;
    Ok(())
}

/// `gitfold changes -- PATH…`: who is changing `paths`, one line per branch
/// and path, `BRANCH\tcommitted|uncommitted\tPATH`, sorted by path. git
/// itself matches the paths, as typed (pathspecs taken literally, relative
/// to the current directory), and the paths printed are from the
/// repository's root, as `git diff --name-only` prints them; paths git
/// refuses are refused with what git says (exit 1). Committed: the branch's
/// own commits (since it diverged from its parent, if that still exists,
/// else from main: `git::default_compare_target`) on branches not merged
/// into main; with no main branch, only uncommitted. Uncommitted: what
/// `gitfold changes` says of each workspace. Its exit code.
pub fn who_changes(repo: &Path, args: ChangesArgs) -> Result<i32> {
    let common_dir = git::init(repo)?;
    // git checks the paths first, once: listing an empty index (one that
    // doesn't exist). If it fails, what git says is all that is said.
    let dir = TempDir::new()?;
    if let Err(e) = ls_files(&dir.0.join("none"), &args.paths) {
        let failed = e.downcast::<git::cmd::Failed>()?;
        match failed.stderr.trim() {
            "" => eprintln!("{failed}"),
            said => eprintln!("{said}"),
        }
        return Ok(1);
    }
    // Each workspace's uncommitted paths, and its note if any.
    let mounted = mount_table::mounted(&common_dir);
    let store = Store::default();
    let mut workspaces = Vec::new();
    for w in state::list(&common_dir)? {
        if !w.state.opened() {
            continue;
        }
        match status::status(&store, &w.state) {
            Ok(s) => {
                let note = base_note(&w.branch, &s.base, mounted);
                let paths: Vec<Vec<u8>> = s.changed.into_iter().chain(s.deleted).collect();
                workspaces.push((w.branch, paths, note));
            }
            Err(e) => eprintln!("skipping workspace {}: {e:#}", w.branch),
        }
    }
    let all = workspaces.iter().flat_map(|(_, paths, _)| paths);
    let matching = matching(all, &args.paths, &dir)?;
    let mut lines = BTreeSet::new();
    match git::default_compare_target(&common_dir)? {
        Some(main) => committed(&common_dir, &main, &args.paths, &mut lines)?,
        None => eprintln!("note: no main branch to compare with: uncommitted changes only"),
    }
    for (branch, paths, note) in workspaces {
        let mut found = false;
        for path in paths.into_iter().filter(|p| matching.contains(p)) {
            lines.insert((path, branch.clone(), "uncommitted"));
            found = true;
        }
        if found && !note.is_empty() {
            eprintln!("note: {branch}: {note}; changes are against the stored base");
        }
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for (path, branch, kind) in &lines {
        write!(out, "{branch}\t{kind}\t")?;
        write_path(&mut out, path, args.nul)?;
    }
    out.flush()?;
    Ok(0)
}

/// Adds to `lines` the paths matching `typed` that each branch not merged
/// into `main` changed in its own commits: since it diverged from its
/// parent, if that still exists, else from `main`. A branch with no history
/// shared with that (as `gh-pages` often has) changes nothing there.
fn committed(
    common_dir: &Path,
    main: &str,
    typed: &[String],
    lines: &mut BTreeSet<(Vec<u8>, String, &str)>,
) -> Result<()> {
    let tree = crate::tree::Tree::read(common_dir)?;
    // Whether the repository's history is whole (not a shallow clone).
    let whole = OnceCell::new();
    let no_merged = format!("--no-merged={main}");
    let args = [
        "for-each-ref",
        &no_merged,
        "--format=%(refname:lstrip=2)",
        "refs/heads",
    ];
    let branches = git::cmd::output(common_dir, &args)?;
    for branch in branches.split(|b| *b == b'\n').filter(|r| !r.is_empty()) {
        let branch = String::from_utf8_lossy(branch);
        let from = match tree.parent(&branch) {
            Some(parent) if !tree.parent_gone(&branch) => parent,
            _ => main,
        };
        let to = format!("refs/heads/{branch}");
        let range = format!("{from}...{to}");
        let diff = [
            "--literal-pathspecs",
            "diff",
            "--name-only",
            "--no-relative",
            "-z",
            "--no-renames",
            &range,
            "--",
        ];
        let args: Vec<&str> = (diff.into_iter())
            .chain(typed.iter().map(String::as_str))
            .collect();
        // One branch git can't diff doesn't hide the others.
        match git::cmd::output_of(&mut git::cmd::here(), &args, None) {
            Ok(out) => {
                for path in out.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                    lines.insert((path.to_vec(), branch.to_string(), "committed"));
                }
            }
            Err(_) if unrelated(common_dir, from, &to, &whole) => {}
            Err(e) => eprintln!("skipping branch {branch}: {e:#}"),
        }
    }
    Ok(())
}

/// Whether `a` and `b` have no history in common (`git merge-base` exits 1),
/// in a repository whose history is whole: in a shallow clone the common
/// part may just be cut off (asked of git once, kept in `whole`). False if
/// git can't tell.
fn unrelated(common_dir: &Path, a: &str, b: &str, whole: &OnceCell<bool>) -> bool {
    let quiet = |args: &[&str]| {
        (git::cmd::command(common_dir).args(args))
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
    };
    let no_merge_base = quiet(&["merge-base", a, b]).is_some_and(|o| o.status.code() == Some(1));
    no_merge_base
        && *whole.get_or_init(|| {
            quiet(&["rev-parse", "--is-shallow-repository"])
                .is_some_and(|o| o.status.success() && o.stdout.trim_ascii() == b"false")
        })
}

/// Which of `paths` (from the root) match `typed`, as git matches them: they
/// go into throwaway indexes in `dir` (no objects written), which `git
/// ls-files` lists.
fn matching<'a>(
    paths: impl Iterator<Item = &'a Vec<u8>>,
    typed: &[String],
    dir: &TempDir,
) -> Result<HashSet<Vec<u8>>> {
    let empty = git::with_repo(|repo| gix::ObjectId::empty_blob(repo.object_hash()));
    let entry = format!("100644 {empty} 0\t");
    let update: Vec<&str> = (git::cmd::PLAIN_INDEX.into_iter())
        .chain(["update-index", "-z", "--index-info"])
        .collect();
    let mut matching = HashSet::new();
    for (n, index) in without_clashes(paths).iter().enumerate() {
        let file = dir.0.join(format!("index{n}"));
        let mut info = Vec::new();
        for path in index {
            info.extend(entry.as_bytes());
            info.extend(*path);
            info.push(0);
        }
        let mut command = git::cmd::here();
        command.env("GIT_INDEX_FILE", &file);
        git::cmd::output_of(&mut command, &update, Some(info))?;
        let out = ls_files(&file, typed)?;
        matching.extend(
            out.split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(<[u8]>::to_vec),
        );
    }
    Ok(matching)
}

/// `git ls-files` of the index at `index` (none there: an empty one), from
/// the current directory: its paths matching `typed`, NUL-terminated.
fn ls_files(index: &Path, typed: &[String]) -> Result<Vec<u8>> {
    let args: Vec<&str> = (git::cmd::PLAIN_INDEX.into_iter())
        .chain(["ls-files", "-z", "--full-name", "--"])
        .chain(typed.iter().map(String::as_str))
        .collect();
    let mut command = git::cmd::here();
    command.env("GIT_INDEX_FILE", index);
    git::cmd::output_of(&mut command, &args, None)
}

/// `paths` in sets that an index can hold: none with both a path and one
/// below it (`d` and `d/x`, as when a workspace replaced a directory with a
/// file); normally one set.
fn without_clashes<'a>(paths: impl Iterator<Item = &'a Vec<u8>>) -> Vec<BTreeSet<&'a [u8]>> {
    let mut sets: Vec<BTreeSet<&[u8]>> = Vec::new();
    for path in paths {
        let below = [path.as_slice(), b"/"].concat();
        let fits = |set: &BTreeSet<&[u8]>| {
            !state::ancestors(path).any(|a| set.contains(a))
                && (set.range(below.as_slice()..).next()).is_none_or(|p| !p.starts_with(&below))
        };
        match sets.iter_mut().find(|set| fits(set)) {
            Some(set) => {
                set.insert(path);
            }
            None => sets.push(BTreeSet::from([path.as_slice()])),
        }
    }
    sets
}

/// A directory of its own in the system's temporary directory, removed
/// when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir> {
        // Not the pid alone: one left by a killed run could be in the way.
        let pid = std::process::id();
        let nanos = (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH))
            .map_or(0, |d| d.subsec_nanos());
        // Absolute: git opens GIT_INDEX_FILE from the work tree's top.
        let dir = std::path::absolute(std::env::temp_dir())?
            .join(format!("gitfold-changes-{pid}-{nanos}"));
        fs::create_dir(&dir)?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The summary `git diff --shortstat` prints: ` 2 files changed, 3
/// insertions(+), 1 deletion(-)`, leaving out zero counts unless both are.
fn shortstat(files: usize, (added, removed): (u32, u32)) -> String {
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut out = format!(" {} changed", plural(files as u64, "file", "files"));
    if added > 0 {
        out += &format!(", {}(+)", plural(added.into(), "insertion", "insertions"));
    }
    if removed > 0 {
        out += &format!(", {}(-)", plural(removed.into(), "deletion", "deletions"));
    }
    out
}

/// How long ago `time` was, briefly: `40s`, `12m`, `3h`, `5d`.
fn age(time: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(time)
        .map_or(0, |d| d.as_secs());
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// When the workspace stored in `state` (with status `s`, task folder `task`)
/// last changed: its newest upper entry, its deletion log, its branch's reflog
/// (commits: committed files leave upper), or its task folder.
fn last_change(state: &StateDir, s: &status::Status, task: &TaskDir) -> Option<SystemTime> {
    let mtime = |p: PathBuf| fs::metadata(p).and_then(|m| m.modified()).ok();
    let reflog = state.common_dir.join("logs/refs/heads").join(&state.branch);
    [
        s.newest,
        mtime(state.markers()),
        mtime(reflog),
        task.newest(),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// `gitfold list`: the repository's workspaces and their uncommitted changes,
/// with the tree of branches (spec section 8): each branch under its parent,
/// whether it follows it, and a note when something needs doing.
pub fn list(repo: &Path, args: ListArgs) -> Result<()> {
    let ListArgs { stat } = args;
    let common_dir = git::init(repo)?;
    // The tree is extra: a config it can't read doesn't hide the workspaces.
    let tree = Tree::read(&common_dir).unwrap_or_else(|e| {
        log::warn!("the tree of branches: {e:#}");
        Tree::default()
    });
    let rows = rows(&tree, state::list(&common_dir)?);
    if rows.is_empty() {
        println!("no workspaces");
        return Ok(());
    }
    let store = Store::default();
    let local = settings::Local::read(&common_dir);
    let mounted = mount_table::mounted(&common_dir);
    let lines = match stat {
        true => format!(" {:>8} {:>8}", "+lines", "-lines"),
        false => String::new(),
    };
    println!(
        "{:<40} {:>8} {:>8} {:>8}{lines}  {:<12} {:>6}  {:<6}  status",
        "branch", "changed", "deleted", "ignored", "base", "last", "follow"
    );
    for (depth, branch, workspace) in rows {
        let name = match depth {
            0 => branch.clone(),
            _ => format!("{}└─ {branch}", "   ".repeat(depth - 1)),
        };
        let follow = match tree.parent(&branch) {
            Some(_) if tree.follows(&branch) => "on",
            Some(_) => "off",
            None => "",
        };
        let mut notes = tree_notes(&common_dir, &tree, &local, &branch);
        // A branch without a workspace: its tree columns only.
        let (mut counts, mut base, mut last, mut task) = (
            ["-".to_string(), "-".into(), "-".into()],
            "-".to_string(),
            "-".to_string(),
            None,
        );
        let mut lines = match stat {
            true => format!(" {:>8} {:>8}", "-", "-"),
            false => String::new(),
        };
        if let Some(w) = workspace {
            let s = match status::status(&store, &w.state) {
                Ok(s) => s,
                Err(e) => {
                    // Its tree notes all the same.
                    let notes = notes.iter().map(|n| format!("; {n}")).collect::<String>();
                    println!("{name:<40} unreadable ({e:#}); id {}{notes}", w.state.id);
                    continue;
                }
            };
            if stat {
                let (mut added, mut removed) = (0, 0);
                for (change, path) in s.changes(&store) {
                    if let Some((a, r)) = s.line_counts(&store, &w.state, change, path)? {
                        (added, removed) = (added + a, removed + r);
                    }
                }
                lines = format!(" {:>8} {:>8}", format!("+{added}"), format!("-{removed}"));
            }
            let dir = TaskDir::with(&local, &common_dir, &w.branch);
            last = last_change(&w.state, &s, &dir).map_or("-".into(), age);
            task = dir.status();
            counts = [s.changed.len(), s.deleted.len(), s.ignored].map(|n| n.to_string());
            base = s.base.to_hex_with_len(12).to_string();
            let note = base_note(&w.branch, &s.base, mounted);
            if !note.is_empty() {
                notes.insert(0, note.to_string());
            }
        }
        // What the agent says it is doing, then what gitfold notes.
        let notes = notes.join("; ");
        let status = match (task, notes.as_str()) {
            (Some(status), "") => status,
            (Some(status), notes) => format!("{status} ({notes})"),
            (None, "") => String::new(),
            (None, notes) => format!("({notes})"),
        };
        let [changed, deleted, ignored] = counts;
        let line = format!(
            "{name:<40} {changed:>8} {deleted:>8} {ignored:>8}{lines}  {base:<12} {last:>6}  \
             {follow:<6}  {status}"
        );
        println!("{}", line.trim_end());
    }
    Ok(())
}

/// The branches `gitfold list` shows, in order, each with how deep it sits
/// in the tree and its workspace: the workspaces, and the branches with a
/// parent or children (never every branch). Each sits under its parent;
/// the root's children, and branches whose parent isn't shown or is gone,
/// at the top.
fn rows(
    tree: &Tree,
    workspaces: Vec<state::Stored>,
) -> Vec<(usize, String, Option<state::Stored>)> {
    let mut shown: BTreeMap<String, Option<state::Stored>> = (workspaces.into_iter())
        .map(|w| (w.branch.clone(), Some(w)))
        .collect();
    for branch in tree.with_parents() {
        shown.entry(branch.to_string()).or_default();
        if let Some(parent) = up(tree, branch) {
            shown.entry(parent.to_string()).or_default();
        }
    }
    // The branches under each shown branch, by name; under `None`, the top.
    let mut under: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    for branch in shown.keys() {
        let parent = up(tree, branch).filter(|p| shown.contains_key(*p));
        (under.entry(parent.map(String::from)).or_default()).push(branch.clone());
    }
    let level = |depth, branches: Vec<String>| branches.into_iter().rev().map(move |b| (depth, b));
    let mut stack: Vec<(usize, String)> =
        level(0, under.remove(&None).unwrap_or_default()).collect();
    let mut rows = Vec::new();
    loop {
        let Some((depth, branch)) = stack.pop() else {
            // Left over: a cycle a hand-edited config may hold (T4), at the top.
            match under.pop_first() {
                Some((_, branches)) => stack.extend(level(0, branches)),
                None => break,
            }
            continue;
        };
        if let Some(children) = under.remove(&Some(branch.clone())) {
            stack.extend(level(depth + 1, children));
        }
        let workspace = shown.get_mut(&branch).and_then(Option::take);
        rows.push((depth, branch, workspace));
    }
    rows
}

/// The parent of `branch` it is shown under: a local branch, not the root,
/// that exists.
fn up<'a>(tree: &'a Tree, branch: &str) -> Option<&'a str> {
    let parent = tree.parent(branch)?.strip_prefix("refs/heads/")?;
    (Some(parent) != tree.root() && !tree.parent_gone(branch)).then_some(parent)
}

/// What needs doing about `branch` in the tree, in a few words: its parent
/// landed (as last checked and recorded: `follow::landed_recorded`), is
/// gone, or following it is paused (a conflict recorded, or commits pushed:
/// as `gitfold parent` says, asked of git only when the parent has commits
/// it lacks).
fn tree_notes(
    common_dir: &Path,
    tree: &Tree,
    local: &settings::Local,
    branch: &str,
) -> Vec<String> {
    let Some(parent) = tree.parent(branch) else {
        return Vec::new();
    };
    let gone = tree.parent_gone(branch);
    let mut notes = Vec::new();
    let landed = (follow::landed_recorded(local, branch, !gone))
        .filter(|to| to != parent && !tree.root_refs().iter().any(|r| r == parent));
    match (landed, gone) {
        (Some(to), _) => notes.push(landed_said(parent, &to, gone)),
        (None, true) => notes.push("parent gone".to_string()),
        (None, false) => {}
    }
    if !gone && tree.follows(branch) {
        let behind = || -> Result<bool> {
            let (from, tip) = (git::resolve(parent)?.id, git::branch_tip(branch)?.id);
            Ok(!follow::is_ancestor(common_dir, from, tip)?)
        };
        let paused = behind().and_then(|behind| match behind {
            true => paused(common_dir, tree, branch),
            false => Ok(None),
        });
        match paused {
            Ok(Some((few, _))) => notes.push(few),
            Ok(None) => {}
            Err(e) => log::warn!("{branch}: whether it can take its parent's commits: {e:#}"),
        }
    }
    notes
}

/// The arguments of `gitfold list`.
#[derive(clap::Args)]
pub struct ListArgs {
    /// Also the lines added and removed, in total (reads every changed
    /// file and its base version).
    #[arg(long)]
    pub stat: bool,
}

/// The arguments of `gitfold changes`.
#[derive(clap::Args)]
pub struct ChangesArgs {
    /// The workspace's branch (default: the workspace you are in).
    pub branch: Option<String>,
    /// End each line with NUL instead of a newline, paths unquoted.
    #[arg(short = 'z')]
    pub nul: bool,
    /// Also the lines added and removed per path (`-` for binary files),
    /// as `git diff --numstat`, and a summary line (not with -z; not with
    /// -- PATH).
    #[arg(long, conflicts_with = "paths")]
    pub stat: bool,
    /// Instead: who is changing these paths, on every branch (committed:
    /// its own commits, since its parent if it has one that still exists,
    /// else not yet in main) and in every workspace (uncommitted).
    #[arg(last = true, value_name = "PATH")]
    pub paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn shortstat_as_git() {
        assert_eq!(shortstat(1, (1, 0)), " 1 file changed, 1 insertion(+)");
        assert_eq!(shortstat(2, (0, 3)), " 2 files changed, 3 deletions(-)");
        assert_eq!(
            shortstat(3, (2, 1)),
            " 3 files changed, 2 insertions(+), 1 deletion(-)"
        );
        assert_eq!(shortstat(1, (0, 0)), " 1 file changed");
    }

    #[test]
    fn ages() {
        let ago = |s| SystemTime::now() - Duration::from_secs(s);
        assert_eq!(age(ago(5)), "5s");
        assert_eq!(age(ago(125)), "2m");
        assert_eq!(age(ago(3 * 3600 + 5)), "3h");
        assert_eq!(age(ago(5 * 86400)), "5d");
        assert_eq!(
            age(SystemTime::now() + Duration::from_secs(60)),
            "0s",
            "the future"
        );
    }
}
