//! The commands run from the shell, not by the daemon.
//!
//! - `inbox`: `watch`, `inbox`, `send`: messages and watching on task folders.
//! - `branch`: `branch` (and its forks), `prewarm`, `track-reads`.
//! - `list`: `list`, `changes`.
//! - `remove`: `remove`, `prune`.
//! - `snapshot`: `snapshot`: workspaces' work, uncommitted included, merged.
//! - `layer`: `layer` (`add`, `remove`, `base`): other branches' work shown
//!   in a workspace, and its base.
//! - `proposal`: `propose`, `proposals`, `consent`, `accept`, `reject`:
//!   changes to a layer proposed from a workspace that shows it.
//! - `layer::moves`: `layer base` and `branch --base`: branches built on another
//!   branch (stacked), moved onto its new commits.
//! - `mount`: `mount` in the background, `unmount`, `version`.
//! - `overlaps`: what a workspace read or changed that changed elsewhere since.
//! - `claude`: Claude Code's hooks: its worktrees as workspaces (`branch` and
//!   `remove` with `--claude-hook`), the files it reads (`track-reads
//!   --claude-hook`).

pub mod branch;
pub mod claude;
pub mod hook;
pub mod inbox;
pub mod layer;
pub mod list;
pub mod mount;
pub mod overlaps;
pub mod proposal;
pub mod remove;
pub mod snapshot;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// The repository commands work on: the one the current directory is in (its
/// git directory). In a workspace showing layers, whose `.git` turns git away
/// (see `fs::workspace::layers`), its registration, found from the mount.
pub fn repo() -> Result<PathBuf> {
    // The mount first: discovery could skip the `.git` of a workspace with git off
    // and find a repository the mount point is in.
    let cwd = std::env::current_dir()?;
    if let Some((state, _)) = workspace_at(&cwd) {
        return Ok(state.admin);
    }
    // The mount point itself (or a snapshot under it): its repository.
    let mounted = (crate::mount_table::mounts().unwrap_or_default().into_iter())
        .filter(|(mount, _)| cwd.starts_with(mount))
        .max_by_key(|(mount, _)| mount.as_os_str().len());
    if let Some((_, common_dir)) = mounted {
        return Ok(common_dir);
    }
    crate::git::discover(Path::new("."))
}

/// Exits with a usage error (exit code 2), as clap does for a bad argument:
/// `message`, then where to find help.
pub fn usage(message: impl std::fmt::Display) -> ! {
    clap::Error::raw(
        clap::error::ErrorKind::ArgumentConflict,
        format!("{message}\n"),
    )
    .exit()
}

/// `n` and the singular or plural noun: `1 commit`, `2 commits`.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The workspace `path` (absolute) is in, below a gitfold mount, and the path
/// in it: the longest leading path below the mount point that is a workspace
/// (branches may hold a `/`).
pub fn workspace_at(path: &Path) -> Option<(crate::state::StateDir, String)> {
    let (mount, common_dir) = (crate::mount_table::mounts().ok()?)
        .into_iter()
        .filter(|(mount, _)| path.starts_with(mount))
        .max_by_key(|(mount, _)| mount.as_os_str().len())?;
    let rest = path.strip_prefix(&mount).ok()?.to_str()?;
    let mut found = None;
    let mut at = 0;
    loop {
        let end = rest[at..].find('/').map_or(rest.len(), |i| at + i);
        let state = crate::state::StateDir::of(&common_dir, &rest[..end]);
        if state.opened() {
            let inside = rest.get(end + 1..).unwrap_or_default().to_string();
            found = Some((state, inside));
        }
        if end == rest.len() {
            return found;
        }
        at = end + 1;
    }
}

/// `branch_or_current` for a command naming the workspace with `-w BRANCH`,
/// saying so if it is run outside every workspace without it.
pub fn workspace_or_current(workspace: Option<String>, repo: &Path) -> Result<String> {
    current_or(workspace, repo, "pass -w BRANCH")
}

/// The workspace of branch `name`, which must have opened: else refused, as
/// no such branch or no workspace yet.
pub fn opened_workspace(common_dir: &Path, name: &str) -> Result<crate::state::StateDir> {
    let state = crate::state::StateDir::of(common_dir, name);
    if !state.opened() {
        crate::git::require_branch(name)?;
        return Err(crate::refusal::Refusal::NotFound.error(format!("{name} has no workspace yet")));
    }
    Ok(state)
}

/// `branch_or_current`, saying `how` to name the branch if it is run
/// outside every workspace without one.
pub fn current_or(branch: Option<String>, repo: &Path, how: &str) -> Result<String> {
    branch_or_current(branch, repo)
        .map_err(|_| anyhow::anyhow!("not in a workspace: run it inside one, or {how}"))
}

/// `branch` if given, else the branch of the workspace the current directory is
/// in: `repo`, the git directory found from it (`git::discover`), is then the
/// workspace's registration, `<common>/worktrees/gitfold-ws-<id>`.
pub fn branch_or_current(branch: Option<String>, repo: &Path) -> Result<String> {
    if let Some(branch) = branch {
        return Ok(branch);
    }
    let in_worktrees = repo.parent().and_then(Path::file_name) == Some("worktrees".as_ref());
    let id = repo
        .file_name()
        .and_then(|n| n.to_str()?.strip_prefix(crate::state::ADMIN_PREFIX));
    match id {
        Some(id) if in_worktrees => Ok(crate::state::workspace_branch(id)),
        _ => bail!("not in a workspace: run it inside one, or name the branch"),
    }
}

/// Runs `local` under the repository lock if no daemon holds it (unmounted),
/// else sends `request` (named `what` in errors) to the daemon, which does the
/// same while mounted. See ARCHITECTURE.md, "Forks: commands on a workspace's
/// stored state".
pub fn local_or_daemon(
    common_dir: &Path,
    request: &crate::daemon::control::Request,
    what: &str,
    local: impl FnOnce() -> Result<String>,
) -> Result<String> {
    match crate::lock::lock(common_dir)? {
        Some(_lock) => local(),
        None => match crate::daemon::control::request(common_dir, request, None)? {
            Some(done) => Ok(done),
            None => Err(crate::refusal::Refusal::InUse.error(format!(
                "{} is locked by another gitfold command, or by a gitfold daemon that doesn't \
                 take {what} requests (an older gitfold, or one starting or exiting); retry, \
                 or remount it",
                common_dir.display()
            ))),
        },
    }
}

/// `path` as git prints it with `core.quotePath` on (the default): as is,
/// unless it has a control character, a double quote, a backslash or a byte
/// above 0x7f; then in double quotes, with C escapes and octal for the rest.
pub fn quote(path: &[u8]) -> Vec<u8> {
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

/// Writes `path` ending a line of output: quoted as git quotes paths, then a
/// newline; or as it is, then a NUL (`nul`, git's `-z`).
pub fn write_path(out: &mut impl std::io::Write, path: &[u8], nul: bool) -> std::io::Result<()> {
    match nul {
        true => {
            out.write_all(path)?;
            out.write_all(b"\0")
        }
        false => {
            out.write_all(&quote(path))?;
            out.write_all(b"\n")
        }
    }
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
}
