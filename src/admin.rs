//! Commands that manage a repository's workspaces and registrations without
//! mounting it: `branch`, `list`, `remove`, `prune`.

use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::control;
use crate::fs::workspace::{Workspace, status};
use crate::git;
use crate::store::Store;
use crate::worktree::{self, workspace_branch, workspace_id};

/// How many paths an error message lists before "and N more".
const SHOWN_PATHS: usize = 10;

/// The source a mount of `common_dir` shows in the mount table (`FSName`): the
/// path, with `%`, `,` (mount options are comma-separated), `\` (fusermount
/// strips it) and non-ASCII bytes percent-escaped, so it reads back exactly.
/// `gitfold unmount` reads it back to wait for the daemon.
pub fn fs_name(common_dir: &Path) -> String {
    let mut name = String::new();
    for &b in common_dir.as_os_str().as_bytes() {
        if matches!(b, b'%' | b',' | b'\\') || !b.is_ascii() {
            name.push_str(&format!("%{b:02X}"));
        } else {
            name.push(b as char);
        }
    }
    name
}

/// The git common directory of a mount with source `source` (see `fs_name`).
pub fn common_dir_of_source(source: &str) -> PathBuf {
    let s = source.as_bytes();
    let mut bytes = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let hex = source
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) if s[i] == b'%' => {
                bytes.push(b);
                i += 3;
            }
            _ => {
                bytes.push(s[i]);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// Holds the repository's daemon lock for the life of a mount: two daemons
/// would share, and corrupt, the stored workspace state, and `remove` and
/// `prune` must not run meanwhile.
pub fn mount_lock(common_dir: &Path) -> Result<worktree::Lock> {
    worktree::lock(common_dir)?.with_context(|| {
        format!(
            "{} is already mounted (or a `gitfold remove`/`prune` of it is running); \
             a repository can be mounted only once at a time",
            common_dir.display()
        )
    })
}

fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()?;
    ensure!(status.success(), "git {} failed ({status})", args.join(" "));
    Ok(())
}

/// Takes the repository's daemon lock exclusively, failing if a gitfold daemon
/// holds it (mounted, or still exiting after an unmount): the daemon keeps
/// workspace state in memory and owns the files below `.git/gitfold/`. Held
/// until the returned file is dropped, so no mount starts meanwhile.
fn exclusive(common_dir: &Path) -> Result<worktree::Lock> {
    worktree::lock(common_dir)?.with_context(|| {
        format!(
            "{} is in use by a gitfold daemon; unmount it first",
            common_dir.display()
        )
    })
}

fn shown(paths: &[Vec<u8>]) -> String {
    let mut s: Vec<String> = (paths.iter().take(SHOWN_PATHS))
        .map(|p| format!("  {}", String::from_utf8_lossy(p)))
        .collect();
    if paths.len() > SHOWN_PATHS {
        s.push(format!("  ... and {} more", paths.len() - SHOWN_PATHS));
    }
    s.join("\n")
}

/// `gitfold branch`: creates a branch to open as a workspace.
pub fn branch(repo: &Path, name: &str, from: Option<&str>) -> Result<()> {
    // Mount names starting with `@` are snapshots; such a branch would be
    // unreachable as a workspace.
    ensure!(
        !name.starts_with('@'),
        "branch names starting with @ can't be opened as workspaces (@<rev> is a snapshot)"
    );
    // A workspace left from a deleted branch of that name would be resumed.
    let common_dir = git::init(repo)?;
    let state_dir = worktree::state_root(&common_dir).join(workspace_id(name));
    ensure!(
        git::branch_exists(name) || !state_dir.exists(),
        "a workspace of an earlier branch {name} is still stored; `gitfold remove` it first"
    );
    let mut args = vec!["branch", "--", name];
    args.extend(from);
    git(repo, &args)?;
    println!("created branch {name}; open it as <mountpoint>/{name}");
    Ok(())
}

struct Listed {
    id: String,
    branch: String,
    state_dir: PathBuf,
}

fn workspaces(common_dir: &Path) -> Result<Vec<Listed>> {
    let dir = worktree::state_root(common_dir);
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
        let branch = fs::read_to_string(entry.path().join("branch"))
            .map(|b| b.trim_end_matches('\n').to_string())
            .unwrap_or_else(|_| workspace_branch(&id));
        list.push(Listed {
            id,
            branch,
            state_dir: entry.path(),
        });
    }
    list.sort_by(|a, b| a.branch.cmp(&b.branch));
    Ok(list)
}

/// Whether the workspace's base is the branch tip, as a note for the user.
fn base_note(branch: &str, base: &gix::ObjectId) -> &'static str {
    match git::resolve(&format!("refs/heads/{branch}")) {
        Err(_) => "branch deleted",
        Ok(tip) if tip.id != *base => "base behind the branch tip (mount to update)",
        Ok(_) => "",
    }
}

/// `gitfold list`: the repository's workspaces and their uncommitted changes.
pub fn list(repo: &Path) -> Result<()> {
    let common_dir = git::init(repo)?;
    let store = Store::default();
    let list = workspaces(&common_dir)?;
    if list.is_empty() {
        println!("no workspaces");
        return Ok(());
    }
    println!(
        "{:<40} {:>8} {:>8} {:>8}  {:<12} note",
        "branch", "changed", "deleted", "ignored", "base"
    );
    for w in list {
        match status::status(&store, &common_dir, &w.state_dir) {
            Ok(s) => println!(
                "{:<40} {:>8} {:>8} {:>8}  {:<12} {}",
                w.branch,
                s.changed.len(),
                s.deleted.len(),
                s.ignored,
                s.base.to_hex_with_len(12).to_string(),
                base_note(&w.branch, &s.base)
            ),
            Err(e) => println!("{:<40} unreadable ({e:#}); id {}", w.branch, w.id),
        }
    }
    Ok(())
}

/// `gitfold remove`: deletes a workspace (its registration, state and base
/// pin; its commits stay on the branch), or a snapshot registration
/// (`@<rev>`). While mounted, the daemon does it (see `control`).
pub fn remove(repo: &Path, name: &str, force: bool) -> Result<()> {
    check_name(name)?;
    let common_dir = git::init(repo)?;
    let lock = worktree::lock(&common_dir)?;
    let done = match (lock, name.strip_prefix('@')) {
        (None, _) => {
            let request = control::Request::Remove {
                name: name.to_string(),
                force,
            };
            control::request(&common_dir, &request)?.with_context(|| {
                format!(
                    "{} is locked by a gitfold daemon that doesn't take removal requests (one \
                     starting or exiting, or an older gitfold), or by a `gitfold remove` or \
                     `prune` running; retry, or unmount it first",
                    common_dir.display()
                )
            })?
        }
        (Some(_), Some(rev)) => {
            let commit = git::resolve(rev).with_context(|| format!("no snapshot @{rev}"))?;
            remove_snapshot_registration(&common_dir, rev, &commit.id, false)?
        }
        (Some(_), None) => remove_workspace(&Store::default(), &common_dir, name, force)?,
    };
    println!("{done}");
    Ok(())
}

/// Refuses what can name neither a workspace (a valid branch name) nor a
/// snapshot (`@<rev>`): a name like `..` would point the removal elsewhere.
pub fn check_name(name: &str) -> Result<()> {
    let valid = match name.strip_prefix('@') {
        Some(rev) => !rev.is_empty(),
        None => {
            gix::validate::reference::name(format!("refs/heads/{name}").as_str().into()).is_ok()
        }
    };
    ensure!(valid, "{name:?} is neither a branch name nor @<rev>");
    Ok(())
}

/// Deletes the registration of the snapshot of `commit` (named `@<rev>`)
/// unless `in_use`; returns what was done.
pub fn remove_snapshot_registration(
    common_dir: &Path,
    rev: &str,
    commit: &gix::ObjectId,
    in_use: bool,
) -> Result<String> {
    let admin = worktree::snapshot_admin_dir(common_dir, commit);
    Ok(if in_use {
        format!("the registration of @{rev} stays: another open snapshot shows {commit}")
    } else if admin.exists() {
        remove_dir(&admin)?;
        format!("removed the registration of snapshot @{rev}")
    } else {
        format!("no registration of snapshot @{rev} ({commit}) to remove")
    })
}

/// Deletes the workspace of `branch` (registration, state and base pin)
/// unless it has uncommitted changes or a git operation in progress (`force`
/// skips the checks). Returns what was done.
pub fn remove_workspace(
    store: &Store,
    common_dir: &Path,
    branch: &str,
    force: bool,
) -> Result<String> {
    let ignored = check_workspace(store, common_dir, branch, force, None)?;
    delete_workspace(common_dir, branch, ignored)
}

/// Fails if the workspace of `branch` doesn't exist, or (unless `force`) has
/// uncommitted changes or a git operation in progress; returns how many
/// ignored files it holds. `open`: the workspace, if the daemon has it open
/// (its base kept at the branch tip), else it is opened to catch up if needed.
pub fn check_workspace(
    store: &Store,
    common_dir: &Path,
    branch: &str,
    force: bool,
    open: Option<&Workspace>,
) -> Result<usize> {
    // `..` would point it at the git directory itself.
    check_name(branch)?;
    let id = workspace_id(branch);
    let state_dir = worktree::state_root(common_dir).join(&id);
    let admin = worktree::ws_admin_dir(common_dir, &id);
    ensure!(
        state_dir.exists() || admin.exists(),
        "no workspace for branch {branch}"
    );
    // Commits reachable only from the worktree (a rebase stopped half-way, a
    // detached HEAD) would be lost with its admin directory.
    if !force
        && admin.exists()
        && let Some(why) = worktree::git_busy(common_dir, &admin, branch)
    {
        bail!("workspace {branch}: {why}; finish it, or use --force to discard it");
    }
    // Staged changes live in the worktree's index, which upper may not show (a
    // file staged, then edited back).
    if !force && admin.join("index").exists() && !index_matches_head(&admin)? {
        bail!("workspace {branch} has staged changes; commit them, or use --force to discard them");
    }
    let mut ignored = 0;
    if !force && state_dir.join("base").exists() {
        let mut s = status::status(store, common_dir, &state_dir)?;
        // Changes are measured against the base; behind the tip (the branch moved
        // after the daemon last saw it), a file edited back to the base's content
        // would look unchanged. Catch up as the daemon would have.
        let tip = git::resolve(&format!("refs/heads/{branch}")).ok();
        if tip.is_some_and(|tip| tip.id != s.base) {
            let catch_up = || {
                if let Some(ws) = open {
                    return ws.sync(store);
                }
                let ws = Workspace::open(
                    store,
                    common_dir.to_path_buf(),
                    state_dir.clone(),
                    admin.clone(),
                    branch,
                    None,
                    Vec::new(),
                )?;
                ws.sync(store)
            };
            catch_up().map_err(|e| {
                anyhow::anyhow!(
                    "workspace {branch}: its base is behind the branch tip and catching up \
                     failed ({e:#}); mount it, open it once, or use --force"
                )
            })?;
            s = status::status(store, common_dir, &state_dir)?;
        }
        ignored = s.ignored;
        let note = base_note(branch, &s.base);
        ensure!(
            note.is_empty() || note == "branch deleted",
            "workspace {branch}: {note}, so its changes can't be told; mount it, open \
             it once, or use --force"
        );
        if !s.changed.is_empty() || !s.deleted.is_empty() {
            let mut msg = format!(
                "workspace {branch} has uncommitted changes ({} changed, {} deleted):",
                s.changed.len(),
                s.deleted.len()
            );
            for (what, paths) in [("changed", &s.changed), ("deleted", &s.deleted)] {
                if !paths.is_empty() {
                    msg += &format!("\n{what}:\n{}", shown(paths));
                }
            }
            bail!("{msg}\ncommit them, or use --force to discard them");
        }
    }
    Ok(ignored)
}

/// Deletes the workspace of `branch` (checked by `check_workspace`, which
/// counted `ignored` files in it); returns what was done.
pub fn delete_workspace(common_dir: &Path, branch: &str, ignored: usize) -> Result<String> {
    check_name(branch)?;
    let id = workspace_id(branch);
    let state_dir = worktree::state_root(common_dir).join(&id);
    let admin = worktree::ws_admin_dir(common_dir, &id);
    // The registration first: without it, the state alone is resumed correctly
    // (the registration is recreated); the reverse is not.
    remove_dir(&admin)?;
    remove_dir(&state_dir)?;
    worktree::unpin(common_dir, &worktree::pin_ref(&id))?;
    Ok(match ignored {
        0 => format!("removed workspace {branch}"),
        n => format!("removed workspace {branch} (and {n} ignored files in it)"),
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

/// Whether the index of the worktree with admin directory `admin` matches its
/// HEAD commit.
fn index_matches_head(admin: &Path) -> Result<bool> {
    let status = Command::new("git")
        .arg("--git-dir")
        .arg(admin)
        .args(["diff-index", "--cached", "--quiet", "HEAD", "--"])
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git diff-index in {} failed ({status})", admin.display()),
    }
}

fn remove_dir(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}
