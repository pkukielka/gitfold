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
    checked(args, command(git_dir).args(args).output()?)
}

/// `output`, with `input` on git's standard input.
pub fn output_with_input(git_dir: &Path, args: &[&str], input: Vec<u8>) -> Result<Vec<u8>> {
    let mut child = command(git_dir)
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

fn checked(args: &[&str], out: Output) -> Result<Vec<u8>> {
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
