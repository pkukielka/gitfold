//! Running git itself, where gitoxide doesn't do something (writing refs and
//! config as git does, `diff`, `merge-base` style queries) or where git's own
//! answer is the reference (`core.excludesFile` with git's includes).
//!
//! Every call names the git directory (`--git-dir`): a repository's, or a
//! worktree's admin directory for what is per worktree (its HEAD, index and
//! config).

use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Result, anyhow, bail};

/// `git --git-dir <git_dir>`, to add arguments to.
pub fn command(git_dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("--git-dir").arg(git_dir);
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
    let out = command(git_dir).args(args).output()?;
    if !out.status.success() {
        return Err(failed(args, &out));
    }
    Ok(out.stdout)
}

/// `output`, with exit code 1 an answer too (git's "no" of `merge-base`, a
/// conflict of `merge-tree`): whether git exited 0, and what it printed.
pub fn output_or_no(git_dir: &Path, args: &[&str]) -> Result<(bool, Vec<u8>)> {
    let out = command(git_dir).args(args).output()?;
    match out.status.code() {
        Some(0) => Ok((true, out.stdout)),
        Some(1) => Ok((false, out.stdout)),
        _ => Err(failed(args, &out)),
    }
}

/// `output`, with `input` on git's stdin.
pub fn output_with_input(git_dir: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    checked(command(git_dir), args, input)
}

/// `exec`, failing if git does.
pub fn checked(git: Command, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let out = exec(git, args, input)?;
    if !out.status.success() {
        return Err(failed(args, &out));
    }
    Ok(out.stdout)
}

/// Runs `git` (from `command`, to which it adds `args`) with `input` on its
/// stdin, whatever its exit code; git reads it whole before it prints.
pub fn exec(mut git: Command, args: &[&str], input: &[u8]) -> Result<Output> {
    let mut child = (git.args(args))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    match child.stdin.take().expect("piped").write_all(input) {
        // git exited without reading it all: its own error says why.
        Err(e) if e.kind() == ErrorKind::BrokenPipe => {}
        written => written?,
    }
    Ok(child.wait_with_output()?)
}

/// The error of git run with `args` that ended as `out` says: with what it said.
pub fn failed(args: &[&str], out: &Output) -> anyhow::Error {
    anyhow!(
        "git {} failed ({}): {}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )
}
