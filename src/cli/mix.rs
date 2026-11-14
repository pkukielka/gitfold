//! `gitfold seal` (a workspace's uncommitted work as a commit) and `gitfold
//! mix` (several workspaces' work, uncommitted included, merged into one
//! commit to open as a snapshot): the read-only form of layers. Nothing in a
//! workspace, its index or its branch changes.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use gix::ObjectId;

use crate::git;
use crate::mount_table;
use crate::refusal::Refusal;
use crate::state::StateDir;
use crate::state::retire::check_name;
use crate::state::seal::{self, IDENTITY};
use crate::store::Store;

#[derive(Args)]
pub struct SealArgs {
    /// The workspace's branch (default: the workspace you are in).
    pub branch: Option<String>,
    /// As Claude Code's `Stop` hook (`stop`): seal the workspace the session
    /// is in if another workspace shows it as a layer; print nothing.
    #[arg(long, value_name = "EVENT", value_parser = ["stop"])]
    pub claude_hook: Option<String>,
}

#[derive(Args)]
pub struct MixArgs {
    /// Start from this revision instead of the first one named (merging
    /// every one named onto it).
    #[arg(long, value_name = "REV")]
    pub onto: Option<String>,
    /// Print the commit only, not the snapshot's path.
    #[arg(long)]
    pub commit: bool,
    /// Branches (their workspaces' work, uncommitted included, else their
    /// tips) or revisions, merged in this order.
    #[arg(required = true, value_name = "BRANCH|REV")]
    pub what: Vec<String>,
}

/// `gitfold seal --claude-hook stop`: when a session stops, seals its
/// workspace if another workspace shows it as a layer, so layers move to the
/// end of a turn, not into the middle of one (`gitfold layer --claude-hook`).
/// Does nothing otherwise; errors are only logged.
pub fn seal_hook() -> Result<()> {
    if let Err(e) = (|| -> Result<()> {
        let repo = super::repo()?;
        let name = super::branch_or_current(None, &repo)?;
        let common_dir = git::init(&repo)?;
        if !crate::state::lens::shown_anywhere(&common_dir, &name)? {
            return Ok(());
        }
        seal::seal(&Store::default(), &StateDir::of(&common_dir, &name))?;
        Ok(())
    })() {
        log::debug!("gitfold seal --claude-hook: {e:#}");
    }
    Ok(())
}

/// `gitfold seal`: seals the workspace and prints the commit.
pub fn seal(repo: &Path, args: SealArgs) -> Result<()> {
    let name = &super::branch_or_current(args.branch, repo)?;
    check_name(name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, name);
    if !state.opened() {
        git::require_branch(name)?;
        return Err(
            Refusal::NotFound.error(format!("{name} has no workspace yet: nothing to seal"))
        );
    }
    let s = seal::seal(&Store::default(), &state)?;
    if s.same {
        eprintln!("unchanged since its last seal");
    }
    println!("{}", s.commit);
    Ok(())
}

/// `gitfold mix`: merges the work of each named workspace (sealed), or each
/// revision, in order, onto the first (or `--onto`), with `git merge-tree`;
/// prints the merge as a snapshot path (`<mountpoint>/@<sha>`, or `@<sha>`
/// unmounted). Conflicts: exit 4, the paths listed. The same work gives the
/// same commit, so a repeated mix reuses its snapshot.
pub fn mix(repo: &Path, args: MixArgs) -> Result<()> {
    let common_dir = git::init(repo)?;
    let store = Store::default();
    let mut commits = Vec::new();
    for what in &args.what {
        commits.push(work_of(&store, &common_dir, what)?);
    }
    let (mut acc, rest) = match &args.onto {
        Some(rev) => (git::resolve(rev)?.id, &commits[..]),
        None => (commits[0].1, &commits[1..]),
    };
    for (what, commit) in rest {
        acc = merge(&common_dir, &acc, commit, what)?;
    }
    match args.commit {
        true => println!("{acc}"),
        false => match mount_table::mount_of(&common_dir)? {
            Some(mount) => println!("{}", mount.join(format!("@{acc}")).display()),
            None => println!("@{acc}"),
        },
    }
    Ok(())
}

/// What `what` contributes to a mix: its workspace's seal if it is a branch
/// with one, else the revision.
fn work_of(store: &Store, common_dir: &Path, what: &str) -> Result<(String, ObjectId)> {
    if check_name(what).is_ok() && !what.starts_with('@') {
        let state = StateDir::of(common_dir, what);
        if state.opened() {
            return Ok((what.to_string(), seal::seal(store, &state)?.commit));
        }
    }
    let rev = what.strip_prefix('@').unwrap_or(what);
    let commit =
        git::resolve(rev).with_context(|| format!("{what}: neither a workspace nor a revision"))?;
    Ok((what.to_string(), commit.id))
}

/// The merge of `theirs` (named `what`) into `ours`, made with `IDENTITY`.
fn merge(common_dir: &Path, ours: &ObjectId, theirs: &ObjectId, what: &str) -> Result<ObjectId> {
    let (ours_hex, theirs_hex) = (ours.to_string(), theirs.to_string());
    let out = git::cmd::command(common_dir)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            &ours_hex,
            &theirs_hex,
        ])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let tree = lines.next().unwrap_or_default().to_string();
    match out.status.code() {
        Some(0) => {}
        Some(1) => {
            let paths: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
            return Err(Refusal::Changes.error(format!(
                "conflicts merging {what}:\n  {}",
                paths.join("\n  ")
            )));
        }
        _ => anyhow::bail!(
            "git merge-tree {ours_hex} {theirs_hex}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
    let message = format!("gitfold mix: {what}");
    let out = git::cmd::command(common_dir)
        .args([
            "commit-tree",
            &tree,
            "-p",
            &ours_hex,
            "-p",
            &theirs_hex,
            "-m",
            &message,
        ])
        .envs(IDENTITY)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git commit-tree: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(ObjectId::from_hex(
        String::from_utf8_lossy(&out.stdout).trim().as_bytes(),
    )?)
}
