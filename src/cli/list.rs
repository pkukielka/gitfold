//! `gitfold list` (the workspaces, their changes and activity) and `gitfold
//! changes` (one workspace's changed paths; with `-- PATH`, who is changing
//! those paths).

use std::cell::OnceCell;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

use super::write_path;
use crate::git;
use crate::settings;
use crate::state::retire::{base_note, check_name};
use crate::state::status;
use crate::state::task::TaskDir;
use crate::state::{self, StateDir};
use crate::store::Store;

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
    let note = base_note(name, &s.base);
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
    let store = Store::default();
    let mut workspaces = Vec::new();
    for w in state::list(&common_dir)? {
        if !w.state.opened() {
            continue;
        }
        match status::status(&store, &w.state) {
            Ok(s) => {
                let note = base_note(&w.branch, &s.base);
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
    let update: Vec<&str> = (PLAIN.into_iter())
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

/// The options of git on a throwaway index: a plain index, whatever the
/// repository's are like, so that nothing of it (a split index's shared
/// part, a sparse index's trees) is written into the repository and no hook
/// hears of it; paths taken literally.
const PLAIN: [&str; 11] = [
    "-c",
    "core.splitIndex=false",
    "-c",
    "index.sparse=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.hooksPath=/dev/null",
    "--literal-pathspecs",
];

/// `git ls-files` of the index at `index` (none there: an empty one), from
/// the current directory: its paths matching `typed`, NUL-terminated.
fn ls_files(index: &Path, typed: &[String]) -> Result<Vec<u8>> {
    let args: Vec<&str> = (PLAIN.into_iter())
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

/// `gitfold list`: the repository's workspaces and their uncommitted changes.
pub fn list(repo: &Path, args: ListArgs) -> Result<()> {
    let ListArgs { stat } = args;
    let common_dir = git::init(repo)?;
    let list = state::list(&common_dir)?;
    if list.is_empty() {
        println!("no workspaces");
        return Ok(());
    }
    let store = Store::default();
    let local = settings::Local::read(&common_dir);
    let lines = match stat {
        true => format!(" {:>8} {:>8}", "+lines", "-lines"),
        false => String::new(),
    };
    println!(
        "{:<40} {:>8} {:>8} {:>8}{lines}  {:<12} {:>6}  status",
        "branch", "changed", "deleted", "ignored", "base", "last"
    );
    for w in list {
        let s = match status::status(&store, &w.state) {
            Ok(s) => s,
            Err(e) => {
                println!("{:<40} unreadable ({e:#}); id {}", w.branch, w.state.id);
                continue;
            }
        };
        let lines = match stat {
            true => {
                let (mut added, mut removed) = (0, 0);
                for (change, path) in s.changes(&store) {
                    if let Some((a, r)) = s.line_counts(&store, &w.state, change, path)? {
                        (added, removed) = (added + a, removed + r);
                    }
                }
                format!(" {:>8} {:>8}", format!("+{added}"), format!("-{removed}"))
            }
            false => String::new(),
        };
        let task = TaskDir::with(&local, &common_dir, &w.branch);
        let last = last_change(&w.state, &s, &task).map_or("-".into(), age);
        // What the agent says it is doing, then what gitfold notes.
        let status = match (task.status(), base_note(&w.branch, &s.base)) {
            (Some(status), "") => status,
            (Some(status), note) => format!("{status} ({note})"),
            (None, "") => String::new(),
            (None, note) => format!("({note})"),
        };
        println!(
            "{:<40} {:>8} {:>8} {:>8}{lines}  {:<12} {:>6}  {}",
            w.branch,
            s.changed.len(),
            s.deleted.len(),
            s.ignored,
            s.base.to_hex_with_len(12).to_string(),
            last,
            status
        );
    }
    Ok(())
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
