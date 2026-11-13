//! Commands that manage a repository's workspaces and registrations, mounted
//! or not: `branch`, `list`, `changes`, `remove`, `prune`, `prewarm`,
//! `track-reads`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, ensure};

use crate::daemon::control;
use crate::daemon::prewarm::Prewarm;
use crate::fs::workspace::catch_up;
use crate::git;
use crate::git::worktree;
use crate::refusal::Refusal;
use crate::settings;
use crate::state::retire::{
    base_note, check_name, check_workspace, removable_workspace, remove_dir,
    remove_snapshot_registration, remove_workspace,
};
use crate::state::task::TaskDir;
use crate::state::{self, StateDir};
use crate::state::{fork, status};
use crate::store::Store;

/// Takes the repository's daemon lock exclusively, failing if a gitfold daemon
/// holds it (mounted, or still exiting after an unmount): the daemon keeps
/// workspace state in memory and owns the files below `.git/gitfold/`. Held
/// until the returned file is dropped, so no mount starts meanwhile.
fn exclusive(common_dir: &Path) -> Result<crate::git::lock::Lock> {
    crate::git::lock::lock(common_dir)?.with_context(|| {
        format!(
            "{} is in use by a gitfold daemon; unmount it first",
            common_dir.display()
        )
    })
}

/// `gitfold branch`: creates a branch to open as a workspace.
pub fn branch(
    repo: &Path,
    name: &str,
    from: Option<&str>,
    prewarm: Option<Prewarm>,
    track_reads: Option<bool>,
    with_changes: bool,
) -> Result<()> {
    // Mount names starting with `@` are snapshots; such a branch would be
    // unreachable as a workspace.
    ensure!(
        !name.starts_with('@'),
        "branch names starting with @ can't be opened as workspaces (@<rev> is a snapshot)"
    );
    // A workspace left from a deleted branch of that name would be resumed.
    let common_dir = git::init(repo)?;
    ensure!(
        git::branch_exists(name) || !StateDir::of(&common_dir, name).dir.exists(),
        "a workspace of an earlier branch {name} is still stored; `gitfold remove` it first"
    );
    if with_changes {
        // `git branch` checks names itself; `update-ref` would take `HEAD`, and
        // refuse other bad names only after the copy.
        let valid = git::cmd::output(&common_dir, &["check-ref-format", "--branch", name]);
        ensure!(valid.is_ok(), "{name:?} is not a valid branch name");
        // A workspace to copy: `from`, or the one we are in.
        let source = super::branch_or_current(from.map(str::to_string), repo).map_err(|_| {
            anyhow::anyhow!("not in a gitfold workspace: name the source with --from")
        })?;
        check_name(&source)?;
        if !git::branch_exists(&source) {
            let message = format!("--with-changes copies a branch's workspace: no branch {source}");
            return Err(Refusal::NotFound.error(message));
        }
        let request = control::Request::Fork {
            from: source.clone(),
            to: name.to_string(),
        };
        let done = super::local_or_daemon(&common_dir, &request, "branch --with-changes", || {
            fork::fork(&common_dir, &source, name, None, |dst| {
                fork::copy_stored(&common_dir, &source, dst)
            })
        })?;
        set_branch_settings(repo, name, prewarm, track_reads)?;
        println!("{done}; open it as <mountpoint>/{name}");
        return Ok(());
    }
    let mut args = vec!["branch", "--", name];
    args.extend(from);
    git::cmd::run(repo, &args)?;
    set_branch_settings(repo, name, prewarm, track_reads)?;
    println!("created branch {name}; open it as <mountpoint>/{name}");
    Ok(())
}

/// `gitfold prewarm`: sets the prewarm mode of workspace `name` (as `branch
/// --prewarm`) and, while mounted, has the daemon apply it at once.
pub fn prewarm(repo: &Path, name: &str, mode: Prewarm) -> Result<()> {
    let common_dir = git::init(repo)?;
    git::require_branch(name)?;
    set_prewarm(repo, name, mode)?;
    let request = control::Request::Prewarm {
        branch: name.to_string(),
    };
    match control::request(&common_dir, &request, None) {
        Ok(Some(done)) => println!("{done}"),
        Ok(None) if mode == Prewarm::None => println!("set; {name} isn't walked"),
        Ok(None) => println!("set; {name} is walked when opened"),
        Err(e) if control::is_older_daemon(&e) => {
            println!("set; the mounted gitfold is older: {name} is walked when it opens next")
        }
        Err(e) => return Err(e.context("set, but not applied to the mount")),
    }
    Ok(())
}

/// Stores the workspace settings `gitfold branch` was given for `name`.
fn set_branch_settings(
    repo: &Path,
    name: &str,
    prewarm: Option<Prewarm>,
    track_reads: Option<bool>,
) -> Result<()> {
    if let Some(mode) = prewarm {
        set_prewarm(repo, name, mode)?;
    }
    if let Some(on) = track_reads {
        set_track_reads(repo, name, on)?;
    }
    Ok(())
}

/// `gitfold track-reads`: stores whether branch `name`'s workspace records
/// reads, and applies it at once if mounted.
pub fn track_reads(repo: &Path, name: &str, on: bool) -> Result<()> {
    let common_dir = git::init(repo)?;
    git::require_branch(name)?;
    set_track_reads(repo, name, on)?;
    let request = control::Request::TrackReads {
        branch: name.to_string(),
    };
    match control::request(&common_dir, &request, None) {
        Ok(Some(done)) => println!("{done}"),
        Ok(None) => println!("set; applied when {name} opens"),
        Err(e) if control::is_older_daemon(&e) => {
            println!("set; the mounted gitfold is older: applied when {name} opens next")
        }
        Err(e) => return Err(e.context("set, but not applied to the mount")),
    }
    Ok(())
}

/// Stores whether branch `name`'s workspace records reads, in git config.
fn set_track_reads(repo: &Path, name: &str, on: bool) -> Result<()> {
    let value = &on.to_string();
    settings::set_branch(repo, name, settings::TRACK_READS, value)
}

/// Stores `mode` as branch `name`'s prewarm mode, in git config.
fn set_prewarm(repo: &Path, name: &str, mode: Prewarm) -> Result<()> {
    settings::set_branch(repo, name, settings::PREWARM, &mode.to_string())
}

/// `gitfold changes`: the uncommitted changes of workspace `name` relative to
/// its base (normally its branch's tip), untracked files included and ignored
/// ones not, whatever is staged: what `git add -A` and `git diff --cached
/// --name-status <base>` would print, paths quoted as git quotes them, or
/// NUL-terminated and unquoted (`nul`). With `stat`, each line also has the
/// lines added and removed (`-` for binary files), as `git diff --numstat`,
/// and a summary line follows, as `git diff --shortstat`. Read from its stored
/// state, without listing the tree; mounted or not. A branch never opened has
/// none.
pub fn changes(repo: &Path, name: &str, nul: bool, stat: bool) -> Result<()> {
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
        match nul {
            true => {
                out.write_all(path)?;
                out.write_all(b"\0")?;
            }
            false => {
                out.write_all(&quote(path))?;
                out.write_all(b"\n")?;
            }
        }
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

/// `path` as git prints it with `core.quotePath` on (the default): as is,
/// unless it has a control character, a double quote, a backslash or a byte
/// above 0x7f; then in double quotes, with C escapes and octal for the rest.
pub(super) fn quote(path: &[u8]) -> Vec<u8> {
    let plain = |b: u8| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\';
    if path.iter().all(|&b| plain(b)) {
        return path.to_vec();
    }
    let mut out = vec![b'"'];
    for &b in path {
        match b {
            b'\x07' => out.extend(b"\\a"),
            b'\x08' => out.extend(b"\\b"),
            b'\t' => out.extend(b"\\t"),
            b'\n' => out.extend(b"\\n"),
            b'\x0b' => out.extend(b"\\v"),
            b'\x0c' => out.extend(b"\\f"),
            b'\r' => out.extend(b"\\r"),
            b'"' => out.extend(b"\\\""),
            b'\\' => out.extend(b"\\\\"),
            b if plain(b) => out.push(b),
            b => out.extend(format!("\\{b:03o}").bytes()),
        }
    }
    out.push(b'"');
    out
}

pub(super) struct Listed {
    pub(super) branch: String,
    state: StateDir,
}

pub(super) fn workspaces(common_dir: &Path) -> Result<Vec<Listed>> {
    let dir = state::state_root(common_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut list = Vec::new();
    for entry in entries {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        // Not a workspace, e.g. the `.lock` file of earlier builds (branch
        // names, so ids, never start with a dot).
        if id.starts_with('.') {
            continue;
        }
        let branch =
            state::read_branch(&entry.path()).unwrap_or_else(|| state::workspace_branch(&id));
        list.push(Listed {
            branch,
            state: StateDir::of_id(common_dir, &id),
        });
    }
    list.sort_by(|a, b| a.branch.cmp(&b.branch));
    Ok(list)
}

/// `gitfold list`: the repository's workspaces and their uncommitted changes.
pub fn list(repo: &Path, stat: bool) -> Result<()> {
    let common_dir = git::init(repo)?;
    let store = Store::default();
    let list = workspaces(&common_dir)?;
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
        let task = TaskDir::of(&common_dir, &w.branch);
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

/// `gitfold remove`: deletes a workspace (its registration, state and base
/// pin; its commits stay on the branch), or a snapshot registration
/// (`@<rev>`). While mounted, the daemon does it (see `daemon::control`). With
/// `check`, only checks what would be refused. Retries for up to `wait` while
/// it is in use.
pub fn remove(repo: &Path, name: &str, force: bool, check: bool, wait: Duration) -> Result<()> {
    check_name(name)?;
    let common_dir = git::init(repo)?;
    let started = Instant::now();
    let mut told = false;
    let done = loop {
        match remove_once(&common_dir, name, force, check) {
            Err(e) if Refusal::of(&e) == Some(Refusal::InUse) && started.elapsed() < wait => {
                if !told {
                    eprintln!("{e:#}\nwaiting (up to {} s)...", wait.as_secs());
                    told = true;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) if told && Refusal::of(&e) == Some(Refusal::InUse) => {
                let waited = started.elapsed().as_secs();
                return Err(e.context(format!("still in use after {waited} s")));
            }
            done => break done?,
        }
    };
    println!("{done}");
    Ok(())
}

/// One attempt at `remove`: under the repository lock, or by the daemon while
/// mounted; the lock held otherwise (another `remove` or `prune`) is in use, so
/// `--wait` retries.
fn remove_once(common_dir: &Path, name: &str, force: bool, check: bool) -> Result<String> {
    let request = control::Request::Remove {
        name: name.to_string(),
        force,
        check,
    };
    super::local_or_daemon(common_dir, &request, "removal", || {
        if let Some(rev) = name.strip_prefix('@') {
            let commit = git::resolve(rev)
                .map_err(|e| Refusal::NotFound.error(format!("no snapshot @{rev}: {e:#}")))?;
            return remove_snapshot_registration(common_dir, rev, &commit.id, false, check);
        }
        let store = Store::default();
        Ok(match check {
            true => removable_workspace(
                name,
                check_workspace(&store, common_dir, name, force, &|| {
                    catch_up(&store, common_dir, name)
                })?,
            ),
            false => remove_workspace(&store, common_dir, name, force, &|| {
                catch_up(&store, common_dir, name)
            })?,
        })
    })
}

/// `gitfold prune`: deletes the snapshot registrations (recreated on demand).
pub fn prune(repo: &Path) -> Result<()> {
    let common_dir = git::init(repo)?;
    let _lock = exclusive(&common_dir)?;
    let mut removed = 0;
    if let Ok(entries) = fs::read_dir(common_dir.join("worktrees")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if worktree::is_snapshot_registration(&name) {
                remove_dir(&entry.path())?;
                removed += 1;
            }
        }
    }
    println!("removed {removed} snapshot registrations");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_as_git_quotes() {
        assert_eq!(quote(b"plain/path.txt"), b"plain/path.txt");
        assert_eq!(quote(b"t\tb"), b"\"t\\tb\"");
        assert_eq!(quote(b"q\"b\\s"), b"\"q\\\"b\\\\s\"");
        assert_eq!(quote("é".as_bytes()), b"\"\\303\\251\"");
        assert_eq!(quote(b"\x7f"), b"\"\\177\"");
    }

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
