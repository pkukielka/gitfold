//! A stacked branch's parent that landed: where its work lands (its
//! target), whether the branch's parent base landed there
//! (`git::stack::landing`, cached), and the trunk.
//!
//! A parent's target is its own parent if it is stacked (p1 merged into
//! p0, a stacked pull request's base), else the trunk: `gitfold.trunk`, else
//! the branch `origin/HEAD` names (`origin/main`), else a local `main` or
//! `master`. The trunk may be a remote-tracking branch, so a parent is
//! resolved as a local branch first, then a remote-tracking one.
//!
//! The answer for a (parent base, target tip) pair never changes, so it is
//! kept in `<common>/gitfold/.stack-landings`, one line per pair: `list`
//! and the hook ask on every call, and a walk of a busy trunk costs a merge
//! per commit touching the parent's paths.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use gix::ObjectId;

use crate::git::stack::{self, How, Landing};
use crate::git::{self, cmd};
use crate::settings::Local;

/// How many answers the cache keeps (the newest).
const CACHED: usize = 512;

/// Where a parent's work lands.
pub struct Target {
    /// As `gitfoldParent` names it: a branch, or the trunk as configured.
    pub name: String,
    pub tip: ObjectId,
    /// The branch a pull request onto it has as its base (`main` for
    /// `origin/main`).
    pub pr_base: String,
}

/// The trunk's name, if there is one (see the module doc).
pub fn trunk(common_dir: &Path, local: &Local) -> Option<String> {
    if let Some(trunk) = local.trunk() {
        return Some(trunk);
    }
    let out = cmd::command(common_dir)
        .args(["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"])
        .output()
        .ok()?;
    let origin = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !origin.is_empty() {
        return Some(origin);
    }
    ["main", "master"]
        .into_iter()
        .find(|b| git::branch_exists(b))
        .map(str::to_string)
}

/// Whether `name` is the trunk: a parent never restacked onto at a turn
/// boundary (following the trunk is `stack sync`'s, on request).
pub fn is_trunk(common_dir: &Path, local: &Local, name: &str) -> bool {
    trunk(common_dir, local).as_deref() == Some(name)
}

/// The full ref name and commit of parent `name`: a local branch, else a
/// remote-tracking branch (a trunk such as `origin/main`).
pub fn resolve(common_dir: &Path, name: &str) -> Result<Option<(String, ObjectId)>> {
    for full in [format!("refs/heads/{name}"), format!("refs/remotes/{name}")] {
        if let Some(tip) = stack::rev(common_dir, &full)? {
            return Ok(Some((full, tip)));
        }
    }
    Ok(None)
}

/// Where the work of `parent` lands: its own parent, else the trunk (`None`
/// if there is none, or `parent` is it).
pub fn target(common_dir: &Path, local: &Local, parent: &str) -> Result<Option<Target>> {
    let name = match local.parent(parent) {
        Some(above) => above,
        None => match trunk(common_dir, local) {
            Some(trunk) if trunk != parent => trunk,
            _ => return Ok(None),
        },
    };
    let Some((full, tip)) = resolve(common_dir, &name)? else {
        return Ok(None);
    };
    // A remote-tracking branch's name there: `refs/remotes/<remote>/<b>`.
    let pr_base = match full.strip_prefix("refs/remotes/") {
        Some(rest) => rest.split_once('/').map_or(rest, |(_, b)| b).to_string(),
        None => name.clone(),
    };
    Ok(Some(Target { name, tip, pr_base }))
}

/// Whether parent base `base` landed on target tip `target`, and where
/// (`git::stack::landing`), remembered.
pub fn landed(common_dir: &Path, base: ObjectId, target: ObjectId) -> Result<Option<Landing>> {
    let path = cache(common_dir);
    let text = fs::read_to_string(&path).unwrap_or_default();
    let key = format!("{base} {target} ");
    if let Some(line) = text.lines().find(|l| l.starts_with(&key))
        && let Some(known) = parse(&line[key.len()..])
    {
        return Ok(known);
    }
    let found = git::with_repo(|repo| stack::landing(repo, base, target))?;
    let answer = match found {
        None => "-".to_string(),
        Some(l) => format!("{} {}", l.at, if l.how == How::Merged { "m" } else { "s" }),
    };
    let mut lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(CACHED - 1);
    lines.drain(..skip);
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out += &format!("{key}{answer}\n");
    // Only a cache: a failed write costs a walk next time.
    let written = (path.parent().map_or(Ok(()), fs::create_dir_all))
        .and_then(|()| crate::fsutil::write_atomic(&path, out.as_bytes()));
    if let Err(e) = written {
        log::debug!("{}: {e}", path.display());
    }
    Ok(found)
}

fn cache(common_dir: &Path) -> PathBuf {
    crate::state::state_root(common_dir).join(".stack-landings")
}

/// A cached answer: `-`, or `<at> m|s`; `None` if unreadable.
fn parse(answer: &str) -> Option<Option<Landing>> {
    if answer == "-" {
        return Some(None);
    }
    let (at, how) = answer.split_once(' ')?;
    let how = match how {
        "m" => How::Merged,
        "s" => How::Squashed,
        _ => return None,
    };
    Some(Some(Landing {
        at: ObjectId::from_hex(at.as_bytes()).ok()?,
        how,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_answers_read_back() {
        let at = ObjectId::from_bytes_or_panic(&[7; 20]);
        assert_eq!(parse("-"), Some(None));
        assert_eq!(
            parse(&format!("{at} s")),
            Some(Some(Landing {
                at,
                how: How::Squashed
            }))
        );
        assert_eq!(parse("junk"), None);
    }
}
