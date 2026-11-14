//! The commands run from the shell, not by the daemon.
//!
//! - `inbox`: `watch`, `inbox`, `send`: messages and watching on task folders.
//! - `branch`: `branch` (and its forks), `prewarm`, `track-reads`.
//! - `list`: `list`, `changes`.
//! - `remove`: `remove`, `prune`.
//! - `mount`: `mount` in the background, `unmount`, `version`.
//! - `overlaps`: what a workspace read or changed that changed elsewhere since.
//! - `claude`: Claude Code's worktrees as workspaces (`branch` and `remove`
//!   with `--claude-hook`).

pub mod branch;
pub mod claude;
pub mod inbox;
pub mod list;
pub mod mount;
pub mod overlaps;
pub mod remove;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// The repository commands work on: the one the current directory is in (its
/// git directory).
pub fn repo() -> Result<PathBuf> {
    crate::git::discover(Path::new("."))
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
        _ => bail!("not in a gitfold workspace: name the branch"),
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
