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

/// A command whose arguments parse but that isn't built yet: printed as
/// `<command>: not implemented yet` (no `Error:` prefix), exit 1.
#[derive(Debug)]
pub struct NotImplemented(String);

impl std::fmt::Display for NotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: not implemented yet", self.0)
    }
}

impl std::error::Error for NotImplemented {}

/// A refusal of the arguments as given: printed as is (no `Error:` prefix),
/// exit 2, like clap's own usage errors.
#[derive(Debug)]
pub struct Usage(pub String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

/// The tree's refusal of `parent` for `branch` in command `what`, with its
/// exit code (`Refused::exit_code`).
fn refused(
    what: &str,
    branch: &str,
    parent: &str,
    refused: &crate::tree::Refused,
) -> anyhow::Error {
    let message = format!("{what}: {}", crate::tree::refusal(branch, parent, refused));
    match refused.exit_code() {
        3 => Refusal::NotFound.error(message),
        2 => Usage(message).into(),
        _ => anyhow::anyhow!(message),
    }
}

/// The error of a command whose arguments parse but that isn't built yet.
pub fn not_implemented(what: &str) -> anyhow::Error {
    NotImplemented(what.to_string()).into()
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

/// `gitfold parent`: the checks of the rules table (S1-S6, F6), then not
/// implemented yet.
pub fn parent(repo: &Path, args: ParentArgs) -> Result<()> {
    use crate::tree::{Refused, Tree, resolve_parent};
    let name = super::branch_or_current(args.branch, repo)?;
    let common_dir = git::init(repo)?;
    check_name(&name)?;
    let refuse = |parent: &str, r: Refused| refused("gitfold parent", &name, parent, &r);
    if !git::branch_exists(&name) {
        return Err(refuse("", Refused::Unknown(name.clone())));
    }
    let tree = Tree::read(&common_dir)?;
    if let Some(parent) = &args.set {
        let full = resolve_parent(parent).map_err(|r| refuse(parent, r))?;
        tree.check(&name, &full).map_err(|r| refuse(parent, r))?;
    } else if (args.follow || args.no_follow) && tree.parent(&name).is_none() {
        return Err(refuse("", Refused::NoParent));
    }
    Err(not_implemented("gitfold parent"))
}

/// Checks `gitfold branch NEW --parent P` (in the repository of `repo`):
/// NEW doesn't exist yet (N1, refused as `branch` refuses an existing
/// branch), P resolves (N2), and the tree's rules for a new branch (N3, N4).
pub fn check_new_parent(repo: &Path, branch: &str, parent: &str) -> Result<()> {
    use crate::tree::{Tree, resolve_parent};
    let common_dir = git::init(repo)?;
    super::branch::check_new(&common_dir, branch)?;
    anyhow::ensure!(
        !git::branch_exists(branch),
        "a branch named '{branch}' already exists"
    );
    let refuse = |r| refused("gitfold branch", branch, parent, &r);
    resolve_parent(parent).map_err(refuse)?;
    Tree::read(&common_dir)?.check_new(branch).map_err(refuse)
}

// The arguments of a command people guess (`gitfold sync`, `pull`, `restack`,
// `rebase`): any, ignored (`//`, not `///`: no help text of its own).
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

/// The hint for `gitfold changes BRANCH -- PATH`: with paths it lists every
/// branch.
pub fn changes_hint(branch: bool) -> Option<String> {
    branch.then(|| "gitfold changes: with -- PATH it lists every branch; drop BRANCH".into())
}

/// The hint for `gitfold branch` flags people guess (`--on`, `--merge`), if
/// one was given.
pub fn branch_hint(on: Option<&[String]>, merge: Option<&[String]>) -> Option<String> {
    if on.is_some() {
        return Some("gitfold branch: no --on; to build on a branch: --parent P".into());
    }
    if let Some(merge) = merge {
        let froms: Vec<String> = match merge {
            [] => vec!["--from A".into(), "--from B".into()],
            _ => merge.iter().map(|m| format!("--from {m}")).collect(),
        };
        return Some(format!(
            "gitfold branch: no --merge; to start at several branches merged: {}",
            froms.join(" ")
        ));
    }
    None
}
