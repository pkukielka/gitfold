//! Running git itself, where gitoxide doesn't do something (writing refs and
//! config as git does, `diff`, `merge-base` style queries) or where git's own
//! answer is the reference (`core.excludesFile` with git's includes).
//!
//! Every call names the git directory (`--git-dir`): a repository's, or a
//! worktree's admin directory for what is per worktree (its HEAD, index and
//! config).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Result, bail};

/// `git --git-dir <git_dir>`, to add arguments to.
pub fn command(git_dir: &Path) -> Command {
    let mut command = git();
    command.arg("--git-dir").arg(git_dir);
    command
}

/// The variables git sets for its hooks and aliases that would point a git
/// run by gitfold at the caller's repository, worktree or index.
const CALLER_ENV: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
];

/// `git` with the caller's repository variables cleared (gitfold may run
/// from a git hook), to add arguments to.
pub fn git() -> Command {
    let mut command = Command::new("git");
    for name in CALLER_ENV {
        command.env_remove(name);
    }
    command
}

/// Runs git with `args`; its messages go to the terminal. Fails if git does.
pub fn run(git_dir: &Path, args: &[&str]) -> Result<()> {
    let status = command(git_dir).args(args).status()?;
    if !status.success() {
        bail!("git {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Runs git with `args`, returning what it printed; a failure's error holds
/// what git said.
pub fn output(git_dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(args, command(git_dir).args(args).output()?)
}

/// `output`, with `input` on git's standard input.
pub fn output_with_input(git_dir: &Path, args: &[&str], input: Vec<u8>) -> Result<Vec<u8>> {
    output_with_env_input(git_dir, args, &[], input)
}

/// `output_with_input`, with variables `env` set for git.
pub fn output_with_env_input(
    git_dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    input: Vec<u8>,
) -> Result<Vec<u8>> {
    let mut child = command(git_dir)
        .envs(env.iter().copied())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // Written meanwhile: git may print before it has read it all. A write
    // error means git exited early; its status says why.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    checked(args, out)
}

pub(super) fn checked(args: &[&str], out: Output) -> Result<Vec<u8>> {
    if !out.status.success() {
        bail!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}
