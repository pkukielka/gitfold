//! Running git itself, where gitoxide doesn't do something (writing refs and
//! config as git does, `diff`, `merge-base` style queries) or where git's own
//! answer is the reference (`core.excludesFile` with git's includes).
//!
//! Every call names the git directory (`--git-dir`): a repository's, or a
//! worktree's admin directory for what is per worktree (its HEAD, index and
//! config).

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

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
    checked(command(git_dir).args(args), args)
}

/// Runs `git` (with `args` added), returning what it printed; a failure's
/// error holds what git said.
pub fn checked(git: &mut Command, args: &[&str]) -> Result<Vec<u8>> {
    let out = git.output()?;
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

/// `git`, run in another worktree or repository than the one gitfold was
/// started in (a new workspace): without the variables that tie git to a
/// repository, which a caller (a git hook, a script run by git) may have
/// set for its own, as `git rev-parse --local-env-vars` lists them (git
/// itself drops them for a submodule); but for the configuration given in
/// the environment (`git -c`, `GIT_CONFIG_COUNT`), which applies anywhere.
/// The rule for every such git call gitfold makes.
pub fn in_other_worktree(git: &mut Command) -> Result<&mut Command> {
    static LOCAL: OnceLock<Vec<String>> = OnceLock::new();
    let local = match LOCAL.get() {
        Some(local) => local,
        None => {
            let out = checked(
                Command::new("git").args(["rev-parse", "--local-env-vars"]),
                &["rev-parse", "--local-env-vars"],
            )?;
            let vars = String::from_utf8_lossy(&out)
                .lines()
                .map(String::from)
                .collect();
            LOCAL.get_or_init(|| vars)
        }
    };
    let config = |var: &str| {
        matches!(var, "GIT_CONFIG_PARAMETERS" | "GIT_CONFIG_COUNT")
            || var.starts_with("GIT_CONFIG_KEY_")
            || var.starts_with("GIT_CONFIG_VALUE_")
    };
    for var in local.iter().filter(|v| !config(v)) {
        git.env_remove(var);
    }
    Ok(git)
}

/// `command`, for a worktree or repository other than gitfold's own (see
/// `in_other_worktree`).
pub fn command_in_other(git_dir: &Path) -> Result<Command> {
    let mut git = command(git_dir);
    in_other_worktree(&mut git)?;
    Ok(git)
}
