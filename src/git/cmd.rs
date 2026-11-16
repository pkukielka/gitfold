//! Running git itself, where gitoxide doesn't do something (writing refs and
//! config as git does, `diff`, `merge-base` style queries) or where git's own
//! answer is the reference (`core.excludesFile` with git's includes).
//!
//! Every call names the git directory (`--git-dir`): a repository's, or a
//! worktree's admin directory for what is per worktree (its HEAD, index and
//! config). Except where git must take paths as typed in the current
//! directory (pathspecs): then git finds the repository there itself
//! (`here`).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

/// `git --git-dir <git_dir>`, to add arguments to.
pub fn command(git_dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("--git-dir").arg(git_dir);
    command
}

/// `git`, run in the current directory: it finds the repository there, and
/// takes pathspecs relative to it, as when typed.
pub fn here() -> Command {
    Command::new("git")
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
    output_of(command(git_dir).args(args), args, None)
}

/// Runs `command` (git, as `command` or `here` made it, `args` added), with
/// `input` on its stdin if given; what it printed. A failure's error holds
/// `args` and what git said.
pub fn output_of(command: &mut Command, args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let out = match input {
        None => command.stdin(Stdio::null()).output()?,
        Some(input) => {
            let mut child = (command.stdin(Stdio::piped()))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut stdin = child.stdin.take().expect("piped");
            // A thread of its own: git may print before it has read it all.
            let writer = std::thread::spawn(move || stdin.write_all(&input));
            let out = child.wait_with_output()?;
            writer.join().expect("the stdin writer")?;
            out
        }
    };
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
