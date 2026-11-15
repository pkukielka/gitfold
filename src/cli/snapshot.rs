//! `gitfold snapshot`: workspaces' work, uncommitted included, or branches'
//! and revisions' commits, merged into one commit to open as a snapshot: the
//! read-only form of layers. Nothing in a workspace, its index or its branch
//! changes.

use std::path::Path;

use anyhow::Result;
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
pub struct SnapshotArgs {
    /// Branches (their workspaces' work, uncommitted included, else their
    /// tips) or revisions, merged in this order (default: the workspace you
    /// are in).
    #[arg(value_name = "BRANCH|REV")]
    pub what: Vec<String>,
    /// Branches contribute only their commits, not their workspaces'
    /// uncommitted work.
    #[arg(long)]
    pub commits: bool,
    /// Start from this revision instead of the first one named (merging
    /// every one named onto it).
    #[arg(long, value_name = "REV")]
    pub onto: Option<String>,
    /// As Claude Code's `Stop` hook (`stop`): snapshot the workspace the
    /// session is in if another workspace shows it as a layer; print nothing.
    #[arg(long, value_name = "EVENT", value_parser = ["stop"],
          conflicts_with_all = ["what", "commits", "onto"])]
    pub claude_hook: Option<String>,
}

/// `gitfold snapshot --claude-hook stop`: when a session stops, seals its
/// workspace if another workspace shows it as a layer, so layers move to the
/// end of a turn, not into the middle of one (`gitfold layer --claude-hook`).
/// Does nothing otherwise; errors are only logged.
pub fn snapshot_hook() -> Result<()> {
    if let Err(e) = (|| -> Result<()> {
        let repo = super::repo()?;
        let name = super::branch_or_current(None, &repo)?;
        let common_dir = git::init(&repo)?;
        if !crate::state::lens::shown_anywhere(&common_dir, &name) {
            return Ok(());
        }
        seal::seal(&Store::default(), &StateDir::of(&common_dir, &name))?;
        Ok(())
    })() {
        log::debug!("gitfold snapshot --claude-hook: {e:#}");
    }
    Ok(())
}

/// `gitfold snapshot`: merges the work of each named workspace (sealed), or
/// each branch's or revision's commit, in order, onto the first (or
/// `--onto`), with `git merge-tree`; prints the merge as a snapshot path
/// (`<mountpoint>/@<sha>`; unmounted, the commit). One named is its work
/// alone. Conflicts: exit 4, the paths listed. The same work gives the same
/// commit, so a repeated snapshot reuses its folder.
pub fn snapshot(repo: &Path, args: SnapshotArgs) -> Result<()> {
    let what = match args.what.is_empty() {
        true => vec![super::branch_or_current(None, repo)?],
        false => args.what,
    };
    let common_dir = git::init(repo)?;
    let store = Store::default();
    let mut commits = Vec::new();
    for what in &what {
        commits.push(work_of(&store, &common_dir, what, args.commits)?);
    }
    let (mut acc, rest) = match &args.onto {
        Some(rev) => (git::resolve(rev)?.id, &commits[..]),
        None => (commits[0].1, &commits[1..]),
    };
    for (name, commit, _) in rest {
        acc = merge(&common_dir, &acc, commit, name).map_err(|e| {
            if Refusal::of(&e) != Some(Refusal::Changes) {
                return e;
            }
            let flag = if args.commits { " --commits" } else { "" };
            let alone = (what.iter()).map(|w| format!("`gitfold snapshot {w}{flag}`"));
            Refusal::Changes.error(format!(
                "{e:#}\n{} show each alone",
                crate::state::lens::and_list(alone)
            ))
        })?;
    }
    if let [(_, _, true)] = commits[..] {
        eprintln!("unchanged since its last snapshot");
    }
    match mount_table::mount_of(&common_dir)? {
        Some(mount) => println!("{}", mount.join(format!("@{acc}")).display()),
        None => {
            eprintln!("not mounted: printed its commit (mounted, it opens as <MOUNTPOINT>/@{acc})");
            println!("{acc}");
        }
    }
    Ok(())
}

/// What `what` contributes to a snapshot: its workspace's seal if it is a
/// branch with one (and not `commits` only), else the revision; and whether
/// that seal is unchanged since the last.
fn work_of(
    store: &Store,
    common_dir: &Path,
    what: &str,
    commits: bool,
) -> Result<(String, ObjectId, bool)> {
    if !commits && check_name(what).is_ok() && !what.starts_with('@') {
        let state = StateDir::of(common_dir, what);
        if state.opened() {
            let s = seal::seal(store, &state)?;
            return Ok((what.to_string(), s.commit, s.same));
        }
    }
    let rev = what.strip_prefix('@').unwrap_or(what);
    let commit = git::resolve(rev)
        .map_err(|_| Refusal::NotFound.error(format!("no branch or revision {what}")))?;
    Ok((what.to_string(), commit.id, false))
}

/// The merge of `theirs` (named `what`) into `ours`, made with `IDENTITY`.
pub(super) fn merge(
    common_dir: &Path,
    ours: &ObjectId,
    theirs: &ObjectId,
    what: &str,
) -> Result<ObjectId> {
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
    let message = format!("gitfold snapshot: {what}");
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
