//! The tree of branches: a branch built on another (its parent) takes the
//! parent's new commits. The command-line contract of the design (`gitfold
//! parent`, `branch --parent`, `branch` with several `--from`, `changes --
//! PATH`) and the hints for commands people guess; what isn't built yet
//! parses its arguments as specified and says so (`not_implemented`, exit 1).

use std::path::Path;

use anyhow::Result;

use crate::git;
use crate::refusal::Refusal;
use crate::state::retire::check_name;

/// The error of a command whose arguments parse but that isn't built yet.
pub fn not_implemented(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{what}: not implemented yet")
}

/// The arguments of `gitfold parent`.
#[derive(clap::Args)]
pub struct ParentArgs {
    /// The branch (default: the workspace you are in).
    pub branch: Option<String>,
    /// Make P its parent: its own commits move onto P's tip, and from then
    /// on onto P's new commits (never creates a branch).
    #[arg(long, value_name = "P", conflicts_with = "none")]
    pub set: Option<String>,
    /// Forget its parent; its history stays as it is (the parent's commits
    /// count as its own from then on).
    #[arg(long, conflicts_with_all = ["follow", "no_follow"])]
    pub none: bool,
    /// Take the parent's new commits by itself (at its agent's turn start,
    /// or once its workspace is idle).
    #[arg(long, conflicts_with = "no_follow")]
    pub follow: bool,
    /// Take them only when asked (`--update`).
    #[arg(long)]
    pub no_follow: bool,
    /// Take the parent's new commits now, once.
    #[arg(long, conflicts_with_all = ["set", "none"])]
    pub update: bool,
    /// With --update: move commits that are pushed already.
    #[arg(long, requires = "update")]
    pub yes: bool,
}

/// `gitfold parent`.
pub fn parent(repo: &Path, args: ParentArgs) -> Result<()> {
    let name = super::branch_or_current(args.branch, repo)?;
    git::init(repo)?;
    for branch in std::iter::once(&name).chain(&args.set) {
        check_name(branch)?;
        if !git::branch_exists(branch) {
            return Err(Refusal::NotFound.error(format!("no branch {branch}")));
        }
    }
    Err(not_implemented("gitfold parent"))
}

/// The arguments of a command people guess (`gitfold sync`, `pull`,
/// `restack`, `rebase`): any, ignored.
#[derive(clap::Args)]
pub struct GuessArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    pub rest: Vec<String>,
}

/// What to say to a guessed command `name`: its right form.
pub fn guessed(name: &str) -> String {
    format!(
        "gitfold {name}: no such command. To take your parent's new commits: gitfold parent --update"
    )
}

/// The hint for `gitfold branch` flags people guess (`--on`, `--merge`), if
/// one was given.
pub fn branch_hint(on: &[String], merge: &[String]) -> Option<String> {
    if !on.is_empty() {
        return Some("gitfold branch: no --on; to build on a branch: --parent P".into());
    }
    if !merge.is_empty() {
        let froms: Vec<String> = merge.iter().map(|m| format!("--from {m}")).collect();
        return Some(format!(
            "gitfold branch: no --merge; to start at several branches merged: {}",
            froms.join(" ")
        ));
    }
    None
}
