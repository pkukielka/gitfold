//! The safety net behind the guard (`git::guard`): whenever a workspace's
//! branch moves (a commit, a reset, anything), the daemon checks the new
//! commits with the guard's rule. The guard's hooks refuse what git does,
//! but libgit2-based tools (GUI clients, some IDE integrations) run no hooks
//! and ignore the filter, so a commit of theirs can land on the branch with
//! another layer's uncommitted lines. Such a commit can't be refused after
//! the fact; it is said, loudly: in the listing, the turn's hook notes and
//! the daemon's log, naming the commit and the lines.
//!
//! Kept in the workspace's state directory: `guard-checked`, the branch tip
//! last checked (the first check only records it), and `guard-alarms`, one
//! line per commit and file found (`<commit> TAB <text>`), dropped once the
//! commit is no longer in the branch (reset away, amended).

use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use gix::ObjectId;

use super::StateDir;
use crate::fsutil::{not_found, write_atomic};
use crate::git;

const CHECKED: &str = "guard-checked";
const ALARMS: &str = "guard-alarms";

/// A commit's guarded lines in one file: the commit, whose lines, the lines.
type Found = (ObjectId, String, Vec<String>);

/// The alarms recorded for the workspace in `state` (see the module doc),
/// as said.
pub fn read(state: &StateDir) -> Vec<String> {
    let text = fs::read_to_string(state.dir.join(ALARMS)).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split_once('\t').map(|(_, said)| said.to_string()))
        .collect()
}

/// Records where the branch of the workspace in `state` is as checked, if
/// nothing is recorded yet: when it opens, so its first check sees every
/// commit made since (one kept from an earlier mount sees those made while
/// it was closed).
pub fn start(state: &StateDir) -> Result<()> {
    let checked = state.dir.join(CHECKED);
    if checked.exists() {
        return Ok(());
    }
    match git::branch_tip(&state.branch) {
        Ok(tip) => Ok(write_atomic(&checked, tip.id.to_string().as_bytes())?),
        Err(_) => Ok(()),
    }
}

/// Checks the commits the branch of the workspace in `state` got since the
/// last check (see the module doc). Returns the new alarms' texts.
pub fn check(state: &StateDir) -> Result<Vec<String>> {
    let Ok(tip) = git::branch_tip(&state.branch) else {
        return Ok(Vec::new());
    };
    let checked_file = state.dir.join(CHECKED);
    let last = match fs::read_to_string(&checked_file) {
        Ok(text) => ObjectId::from_hex(text.trim().as_bytes()).ok(),
        Err(e) if not_found(&e) => None,
        Err(e) => return Err(e.into()),
    };
    if last == Some(tip.id) {
        return Ok(Vec::new());
    }
    let common = &state.common_dir;
    // Alarms whose commit is gone from the branch go.
    let mut alarms: Vec<(String, String)> = Vec::new();
    let text = fs::read_to_string(state.dir.join(ALARMS)).unwrap_or_default();
    for line in text.lines() {
        let Some((commit, said)) = line.split_once('\t') else {
            continue;
        };
        let Ok(id) = ObjectId::from_hex(commit.as_bytes()) else {
            continue;
        };
        if git::stack::is_ancestor(common, id, tip.id).unwrap_or(false) {
            alarms.push((commit.to_string(), said.to_string()));
        }
    }
    let mut new = Vec::new();
    let guard = git::guard::Guard::read(&state.admin)?;
    if let (Some(last), Some(guard)) = (last, guard.filter(|g| !g.is_empty())) {
        let known = git::with_repo(|repo| repo.has_object(last));
        let mut args = vec![tip.id.to_string()];
        if known {
            args.push(format!("^{last}"));
        }
        // A layer's own commits (the branch moved onto them) are its.
        let layers = super::layers::ShownLayers::read(state)?.layers()?;
        for layer in &layers.layers {
            if git::branch_exists(&layer.branch) {
                args.push(format!("^refs/heads/{}", layer.branch));
            }
        }
        let commits = git::guard::rev_list(common, &args)?;
        let found = git::with_repo(|repo| guard.violations(repo, &commits))?;
        // By commit and path, in the order found.
        let mut by: BTreeMap<(usize, Vec<u8>), Found> = BTreeMap::new();
        for v in found {
            let order = commits.iter().position(|c| *c == v.commit).unwrap_or(0);
            let entry = by
                .entry((order, v.path.clone()))
                .or_insert_with(|| (v.commit, v.layer.clone(), Vec::new()));
            entry
                .2
                .push(format!("{:?}", String::from_utf8_lossy(&v.line).trim()));
        }
        for ((_, path), (commit, layer, lines)) in by {
            let said = alarm(state, commit, &layer, &path, &lines);
            let hex = commit.to_string();
            if !alarms.iter().any(|(c, s)| *c == hex && *s == said) {
                log::warn!("{}: {said}", state.branch);
                new.push(said.clone());
                alarms.push((hex, said));
            }
        }
    }
    let text: String = (alarms.iter())
        .map(|(c, s)| format!("{c}\t{s}\n"))
        .collect();
    match text.is_empty() {
        true => match fs::remove_file(state.dir.join(ALARMS)) {
            Err(e) if !not_found(&e) => return Err(e.into()),
            _ => {}
        },
        false => write_atomic(&state.dir.join(ALARMS), text.as_bytes())?,
    }
    write_atomic(&checked_file, tip.id.to_string().as_bytes())?;
    Ok(new)
}

/// What an alarm says: commit `commit` of the branch of `state` has
/// `layer`'s uncommitted `lines` in `path`.
fn alarm(state: &StateDir, commit: ObjectId, layer: &str, path: &[u8], lines: &[String]) -> String {
    let short = &commit.to_string()[..12];
    let subject = git::cmd::output(
        &state.common_dir,
        &["log", "-1", "--format=%s", &commit.to_string()],
    )
    .map(|s| String::from_utf8_lossy(&s).trim().to_string())
    .unwrap_or_default();
    let shown: Vec<&str> = lines.iter().take(3).map(String::as_str).collect();
    let more = match lines.len() > 3 {
        true => format!(" and {} more", lines.len() - 3),
        false => String::new(),
    };
    format!(
        "commit {short} ({subject}) on {branch} has {layer}'s uncommitted lines in {path}: \
         {lines}{more}. It was made without gitfold's filter (a tool built on libgit2 runs no \
         hooks), so {layer}'s unfinished work is in your branch. Take it out before pushing: \
         `git reset --soft {short}^` and commit again with git (git push refuses it meanwhile)",
        branch = state.branch,
        path = String::from_utf8_lossy(path),
        lines = shown.join(", "),
    )
}
