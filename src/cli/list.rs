//! `gitfold list` (the workspaces, their changes and activity) and `gitfold
//! changes` (one workspace's changed paths).

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
    let ChangesArgs { branch, nul, stat } = args;
    let name = &super::branch_or_current(branch, repo)?;
    check_name(name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, name);
    if !state.opened() {
        git::require_branch(name)?;
        return Ok(());
    }
    let store = Store::default();
    let s = status::status(&store, &common_dir, &state)?;
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

/// When workspace `branch` (stored in `state`, with status `s`) last
/// changed: its newest upper entry, its deletion log, or its branch's reflog
/// (commits: committed files leave upper).
fn last_change(
    common_dir: &Path,
    branch: &str,
    state: &StateDir,
    s: &status::Status,
) -> Option<SystemTime> {
    let mtime = |p: PathBuf| fs::metadata(p).and_then(|m| m.modified()).ok();
    let reflog = common_dir.join("logs/refs/heads").join(branch);
    [s.newest, mtime(state.markers()), mtime(reflog)]
        .into_iter()
        .flatten()
        .max()
}

/// `gitfold list`: the repository's workspaces and their uncommitted changes.
pub fn list(repo: &Path, args: ListArgs) -> Result<()> {
    let ListArgs { stat } = args;
    let common_dir = git::init(repo)?;
    let store = Store::default();
    let list = state::list(&common_dir)?;
    let local = settings::Local::read(&common_dir);
    if list.is_empty() {
        println!("no workspaces");
        return Ok(());
    }
    let lines = match stat {
        true => format!(" {:>8} {:>8}", "+lines", "-lines"),
        false => String::new(),
    };
    println!(
        "{:<40} {:>8} {:>8} {:>8}{lines}  {:<12} {:>6}  status",
        "branch", "changed", "deleted", "ignored", "base", "last"
    );
    for w in list {
        let s = match status::status(&store, &common_dir, &w.state) {
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
        let last = [
            last_change(&common_dir, &w.branch, &w.state, &s),
            task.newest(),
        ]
        .into_iter()
        .flatten()
        .max()
        .map_or("-".into(), age);
        // What the agent says it is doing, then what gitfold notes.
        let status = match (task.status(), base_note(&w.branch, &s.base)) {
            (Some(status), "") => status,
            (Some(status), note) => format!("{status} ({note})"),
            (None, note) => format!("({note})").replace("()", ""),
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
    /// as `git diff --numstat`, and a summary line (not with -z).
    #[arg(long)]
    pub stat: bool,
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
