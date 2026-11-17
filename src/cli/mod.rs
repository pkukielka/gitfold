//! The commands run from the shell, not by the daemon.
//!
//! - `inbox`: `watch`, `inbox`, `send`: messages and watching on task folders.
//! - `branch`: `branch` (and its forks), `prewarm`, `track-reads`.
//! - `list`: `list`, `changes`.
//! - `remove`: `remove`, `prune`.
//! - `mount`: `mount` in the background, `unmount`, `version`.
//! - `overlaps`: what a workspace read or changed that changed elsewhere since.
//! - `tree`: branches built on branches: `parent`, and the hints for
//!   commands people guess.
//! - `claude`: Claude Code's hooks: its worktrees as workspaces (`branch` and
//!   `remove` with `--claude-hook`), the files it reads (`track-reads
//!   --claude-hook`).

pub mod branch;
pub mod claude;
pub mod hook;
pub mod inbox;
pub mod list;
pub mod mount;
pub mod overlaps;
pub mod remove;
pub mod tree;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// The repository commands work on: the one the current directory is in (its
/// git directory), found as git finds it; or, below a gitfold mount where git
/// finds none (its root, a view, a namespace directory), the mounted one
/// (`mounted_here`).
pub fn repo() -> Result<PathBuf> {
    match crate::git::discover(Path::new(".")) {
        Ok(repo) => Ok(repo),
        Err(e) => mounted_here()?.ok_or(e),
    }
}

/// The git common directory of the gitfold mount the current directory is
/// in, if it is in one. There, git run on paths as typed (`git::cmd::here`)
/// takes them from the top of the view the current directory is in
/// (`@live/<branch>`, `@committed/<branch>`); elsewhere in the mount (its
/// root, `@live`, a namespace directory), from the current directory, which
/// stands for the top of the work tree.
fn mounted_here() -> Result<Option<PathBuf>> {
    let Ok(cwd) = std::env::current_dir() else {
        return Ok(None);
    };
    // The innermost, should one be mounted in another.
    let Some((mount, common_dir)) = (crate::mount_table::mounts()?.into_iter())
        .filter(|(mount, _)| cwd.starts_with(mount))
        .max_by_key(|(mount, _)| mount.components().count())
    else {
        return Ok(None);
    };
    crate::git::init(&common_dir)?;
    let names: Vec<_> = (cwd.strip_prefix(&mount)?.components())
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    // A view's branch: the first of the names below `@live` or `@committed`
    // that is one, as the mount resolves them.
    let view = match names.split_first() {
        Some((kind, below)) if kind == "@live" || kind == "@committed" => (1..=below.len())
            .find(|&n| crate::git::branch_exists(&below[..n].join("/")))
            .map(|n| n + 1),
        _ => None,
    };
    let top = match view {
        Some(n) => names[..n].iter().fold(mount, |top, name| top.join(&**name)),
        None => cwd,
    };
    crate::git::cmd::set_here(&common_dir, &top);
    Ok(Some(common_dir))
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
