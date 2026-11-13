//! `gitfold remove` (a workspace, or a snapshot's registration) and `gitfold
//! prune`, unmounted under the repository lock or by the daemon.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::daemon::control;
use crate::fs::workspace::catch_up;
use crate::git;
use crate::git::worktree;
use crate::refusal::Refusal;
use crate::state::retire::{
    check_name, check_workspace, delete_workspace, removable_workspace, remove_dir,
    remove_snapshot_registration,
};
use crate::store::Store;

/// Takes the repository's daemon lock exclusively, failing if a gitfold daemon
/// holds it (mounted, or still exiting after an unmount): the daemon keeps
/// workspace state in memory and owns the files below `.git/gitfold/`. Held
/// until the returned file is dropped, so no mount starts meanwhile.
fn exclusive(common_dir: &Path) -> Result<crate::lock::Lock> {
    crate::lock::lock(common_dir)?.with_context(|| {
        format!(
            "{} is in use by a gitfold daemon; unmount it first",
            common_dir.display()
        )
    })
}

/// `gitfold remove`: deletes a workspace (its registration, state and base
/// pin; its commits stay on the branch), or a snapshot registration
/// (`@<rev>`). While mounted, the daemon does it (see `daemon::control`). With
/// `--check`, only checks what would be refused. Retries for up to `--wait`
/// seconds while it is in use.
pub fn remove(repo: &Path, args: RemoveArgs) -> Result<()> {
    let RemoveArgs {
        name,
        force,
        check,
        wait,
    } = args;
    let (name, wait) = (name.as_str(), Duration::from_secs(wait));
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
        let ignored = check_workspace(&store, common_dir, name, force, &catch_up)?;
        Ok(match check {
            true => removable_workspace(name, ignored),
            false => delete_workspace(common_dir, name, ignored)?,
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

/// The arguments of `gitfold remove`.
#[derive(clap::Args)]
pub struct RemoveArgs {
    /// A workspace's branch, or @<rev> for a snapshot.
    #[arg(value_name = "BRANCH|@REV")]
    pub name: String,
    /// Discard uncommitted changes.
    #[arg(long)]
    pub force: bool,
    /// Only check whether it would be removed; remove nothing.
    #[arg(long)]
    pub check: bool,
    /// While in use, retry for up to this many seconds.
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    pub wait: u64,
}
