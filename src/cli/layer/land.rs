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
//! per commit touching the parent's paths. The key includes what was
//! recorded about the base (`record`), which tells a fast-forward apart.

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
        .find(|b| {
            matches!(
                stack::rev(common_dir, &format!("refs/heads/{b}")),
                Ok(Some(_))
            )
        })
        .map(str::to_string)
}

/// The trunk as users know it: its local branch's name (`main` for
/// `origin/main`), if there is a trunk.
pub fn trunk_name(common_dir: &Path, local: &Local) -> Option<String> {
    let trunk = trunk(common_dir, local)?;
    Some(local_of(common_dir, &trunk).unwrap_or(trunk))
}

/// The main branch as commands given to users name it: `trunk_name`, else
/// `main`.
pub fn main_name(common_dir: &Path, local: &Local) -> String {
    trunk_name(common_dir, local).unwrap_or_else(|| "main".into())
}

/// The commit to move onto for the trunk named `name` (`main`, or as
/// configured, `origin/main`): the newer of the local branch and the
/// configured trunk, the configured one if they diverged, whichever exists
/// if only one does. A local `main` may be stale, or missing, when the trunk
/// is `origin/main`.
pub fn trunk_tip(
    common_dir: &Path,
    local: &Local,
    name: &str,
) -> Result<Option<(String, ObjectId)>> {
    let named = resolve(common_dir, name)?.map(|(_, tip)| (name.to_string(), tip));
    let configured = match trunk(common_dir, local) {
        Some(trunk) if trunk != name => resolve(common_dir, &trunk)?.map(|(_, tip)| (trunk, tip)),
        _ => None,
    };
    Ok(match (named, configured) {
        (Some(named), Some(configured)) => {
            match stack::is_ancestor(common_dir, configured.1, named.1)? {
                true => Some(named),
                false => Some(configured),
            }
        }
        (named, configured) => named.or(configured),
    })
}

/// Whether `a` and `b` name the same target: equal, or both the trunk
/// (`main` and `origin/main`).
pub fn same(common_dir: &Path, local: &Local, a: &str, b: &str) -> bool {
    a == b || (is_trunk(common_dir, local, a) && is_trunk(common_dir, local, b))
}

/// Whether `name` is the trunk: a parent never restacked onto at a turn
/// boundary (following the trunk is `layer base`'s, on request).
/// A local branch tracking it counts too (`main` for `origin/main`): a
/// branch stacked on it would otherwise be restacked on every pull, and its
/// own commits taken for landed once rebased onto the trunk.
pub fn is_trunk(common_dir: &Path, local: &Local, name: &str) -> bool {
    let Some(trunk) = trunk(common_dir, local) else {
        return false;
    };
    trunk == name || local_of(common_dir, &trunk).as_deref() == Some(name)
}

/// The local branch name of remote-tracking branch `trunk` (`main` for
/// `origin/main`), if it is one.
fn local_of(common_dir: &Path, trunk: &str) -> Option<String> {
    let (_, branch) = trunk.split_once('/')?;
    stack::rev(common_dir, &format!("refs/remotes/{trunk}"))
        .ok()
        .flatten()
        .map(|_| branch.to_string())
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

/// Whether branch `parent` is gone: deleted here, or its upstream deleted
/// on its remote (a forge deletes a merged pull request's branch; seen once
/// fetched with `--prune`). What makes a squash found by content safe to
/// follow unasked.
pub fn gone(common_dir: &Path, local: &Local, parent: &str) -> Result<bool> {
    if resolve(common_dir, parent)?.is_none() {
        return Ok(true);
    }
    let Some((remote, theirs)) = local.upstream(parent) else {
        return Ok(false);
    };
    Ok(stack::rev(common_dir, &format!("refs/remotes/{remote}/{theirs}"))?.is_none())
}

/// Where the work of `parent` lands: its own parent, else the trunk (`None`
/// if there is none, or `parent` is it).
pub fn target(common_dir: &Path, local: &Local, parent: &str) -> Result<Option<Target>> {
    let name = match local.parent(parent) {
        Some(above) => above,
        None => match trunk(common_dir, local) {
            Some(trunk) if !is_trunk(common_dir, local, parent) => trunk,
            _ => return Ok(None),
        },
    };
    named(common_dir, name)
}

/// The target named `name` (a branch, or a remote-tracking one), if it
/// exists.
pub fn named(common_dir: &Path, name: String) -> Result<Option<Target>> {
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

/// Whether parent base `base` landed on `target`, and where
/// (`git::stack::landing`), remembered: a landing while the target still
/// has it, "not landed up to tip T" as a starting point, so after a fetch
/// only the target's new commits are walked.
pub fn landed(
    common_dir: &Path,
    base: ObjectId,
    target: &Target,
    was_on_target: Option<bool>,
) -> Result<Option<Landing>> {
    let path = cache(common_dir);
    let text = fs::read_to_string(&path).unwrap_or_default();
    let recorded = match was_on_target {
        Some(true) => "y",
        Some(false) => "n",
        None => "-",
    };
    let key = format!("{base} {} {recorded} ", target.name);
    let mut since = None;
    if let Some(line) = text.lines().find(|l| l.starts_with(&key))
        && let Some((tip, known)) = parse(&line[key.len()..])
    {
        match known {
            Some(landing) if stack::is_ancestor(common_dir, landing.at, target.tip)? => {
                return Ok(Some(landing));
            }
            None if tip == target.tip => return Ok(None),
            None if stack::is_ancestor(common_dir, tip, target.tip)? => since = Some(tip),
            // The target was rewound or rewritten: from the start.
            _ => {}
        }
    }
    let found =
        git::with_repo(|repo| stack::landing(repo, base, target.tip, since, was_on_target))?;
    let answer = match found {
        None => "-".to_string(),
        Some(l) => format!("{} {}", l.at, how_code(l.how)),
    };
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.starts_with(&key)).collect();
    let skip = lines.len().saturating_sub(CACHED - 1);
    lines.drain(..skip);
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out += &format!("{key}{} {answer}\n", target.tip);
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

/// How a landing is written in the cache (`parse` reads it back).
fn how_code(how: How) -> &'static str {
    match how {
        How::Merged => "m",
        How::FastForward => "f",
        How::Squashed => "s",
    }
}

/// Whether the parent base `base` of `branch` was in `target` (named) when
/// it was recorded, if that is what was recorded (`gitfoldBaseOnTarget`).
pub fn was_on_target(local: &Local, branch: &str, base: ObjectId, target: &str) -> Option<bool> {
    let record = local.base_on_target(branch)?;
    let mut words = record.split(' ');
    let (Some(at), Some(on), Some(answer)) = (words.next(), words.next(), words.next()) else {
        return None;
    };
    if ObjectId::from_hex(at.as_bytes()).ok() != Some(base) || on != target {
        return None;
    }
    match answer {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// Records whether parent base `base` of `branch`, stacked on `parent`, is
/// in the parent's target now (`gitfoldBaseOnTarget`): a later fast-forward
/// of the parent into it is then told apart from a fresh parent, whose base
/// was there all along. Forgotten where the parent has no target.
pub fn record(common_dir: &Path, branch: &str, parent: &str, base: ObjectId) -> Result<()> {
    let local = Local::read(common_dir);
    let record = match target(common_dir, &local, parent)? {
        Some(target) => {
            let on = stack::is_ancestor(common_dir, base, target.tip)?;
            Some(format!(
                "{base} {} {}",
                target.name,
                if on { "yes" } else { "no" }
            ))
        }
        None => None,
    };
    crate::settings::set_base_on_target(common_dir, branch, record.as_deref())
}

/// A cached answer, after its key: the target tip it was found at, then
/// `-` (not landed up to it) or `<at> m|s`; `None` if unreadable.
fn parse(answer: &str) -> Option<(ObjectId, Option<Landing>)> {
    let (tip, rest) = answer.split_once(' ')?;
    let tip = ObjectId::from_hex(tip.as_bytes()).ok()?;
    if rest == "-" {
        return Some((tip, None));
    }
    let (at, how) = rest.split_once(' ')?;
    let how = match how {
        "m" => How::Merged,
        "s" => How::Squashed,
        "f" => How::FastForward,
        _ => return None,
    };
    let at = ObjectId::from_hex(at.as_bytes()).ok()?;
    Some((tip, Some(Landing { at, how })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_answers_read_back() {
        let (tip, at) = (
            ObjectId::from_bytes_or_panic(&[6; 20]),
            ObjectId::from_bytes_or_panic(&[7; 20]),
        );
        assert_eq!(parse(&format!("{tip} -")), Some((tip, None)));
        assert_eq!(
            parse(&format!("{tip} {at} s")),
            Some((
                tip,
                Some(Landing {
                    at,
                    how: How::Squashed
                })
            ))
        );
        assert_eq!(parse("junk"), None);
    }
}
