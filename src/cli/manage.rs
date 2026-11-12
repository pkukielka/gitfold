//! Commands that manage a repository's workspaces and registrations without
//! mounting it: `branch`, `list`, `remove`, `prune`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::ValueEnum;

use crate::daemon::control;
use crate::daemon::prewarm::Prewarm;
use crate::fs::workspace::retire::{
    base_note, check_name, check_workspace, removable_workspace, remove_dir,
    remove_snapshot_registration, remove_workspace,
};
use crate::fs::workspace::status;
use crate::git;
use crate::git::worktree::{self, workspace_branch, workspace_id};
use crate::refusal::Refusal;
use crate::store::Store;

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
fn exclusive(common_dir: &Path) -> Result<crate::git::lock::Lock> {
    crate::git::lock::lock(common_dir)?.with_context(|| {
        format!(
            "{} is in use by a gitfold daemon; unmount it first",
            common_dir.display()
        )
    })
}

/// `gitfold branch`: creates a branch to open as a workspace.
pub fn branch(repo: &Path, name: &str, from: Option<&str>, prewarm: Option<Prewarm>) -> Result<()> {
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
    if let Some(mode) = prewarm {
        set_prewarm(repo, name, mode)?;
    }
    println!("created branch {name}; open it as <mountpoint>/{name}");
    Ok(())
}

/// `gitfold prewarm`: sets the prewarm mode of workspace `name` (as `branch
/// --prewarm`) and, while mounted, has the daemon apply it at once.
pub fn prewarm(repo: &Path, name: &str, mode: Prewarm) -> Result<()> {
    let common_dir = git::init(repo)?;
    ensure!(git::branch_exists(name), "no branch {name}");
    set_prewarm(repo, name, mode)?;
    let request = control::Request::Prewarm {
        branch: name.to_string(),
    };
    match control::request(&common_dir, &request, None) {
        Ok(Some(done)) => println!("{done}"),
        Ok(None) => println!("set; {name} is walked when opened"),
        Err(e) if control::is_older_daemon(&e) => {
            println!("set; the mounted gitfold is older: {name} is walked when it opens next")
        }
        Err(e) => return Err(e.context("set, but not applied to the mount")),
    }
    Ok(())
}

/// Stores `mode` as branch `name`'s prewarm mode, in git config.
fn set_prewarm(repo: &Path, name: &str, mode: Prewarm) -> Result<()> {
    let key = format!("branch.{name}.{}", crate::daemon::prewarm::BRANCH_KEY);
    let value = ValueEnum::to_possible_value(&mode).expect("not skipped");
    git(repo, &["config", &key, value.get_name()])
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

fn remove_once(common_dir: &Path, name: &str, force: bool, check: bool) -> Result<String> {
    let lock = crate::git::lock::lock(common_dir)?;
    Ok(match (lock, name.strip_prefix('@')) {
        (None, _) => {
            let request = control::Request::Remove {
                name: name.to_string(),
                force,
                check,
            };
            control::request(common_dir, &request, None)?.with_context(|| {
                format!(
                    "{} is locked by a gitfold daemon that doesn't take removal requests (one \
                     starting or exiting, or an older gitfold), or by a `gitfold remove` or \
                     `prune` running; retry, or unmount it first",
                    common_dir.display()
                )
            })?
        }
        (Some(_), Some(rev)) => {
            let commit = git::resolve(rev)
                .map_err(|e| Refusal::NotFound.error(format!("no snapshot @{rev}: {e:#}")))?;
            remove_snapshot_registration(common_dir, rev, &commit.id, false, check)?
        }
        (Some(_), None) => {
            let store = Store::default();
            match check {
                true => {
                    let ignored = check_workspace(&store, common_dir, name, force, None)?;
                    removable_workspace(name, ignored)
                }
                false => remove_workspace(&store, common_dir, name, force)?,
            }
        }
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
