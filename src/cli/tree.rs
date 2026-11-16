//! The tree of branches: a branch built on another (its parent) takes the
//! parent's new commits. The command-line contract of the design (`gitfold
//! parent`, `branch --parent`, `branch` with several `--from`, `changes --
//! PATH`) and the hints for commands people guess; what isn't built yet
//! parses its arguments as specified and says so (`not_implemented`, exit 1).

use std::path::Path;

use anyhow::Result;

use crate::git;
use crate::refusal::Refusal;
use crate::settings;
use crate::state::retire::check_name;
use crate::tree::{Refused, Tree};

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
/// exit code: 3 not found, 1 no root, 2 the command as typed is invalid.
fn refused(what: &str, branch: &str, parent: &str, refused: Refused) -> anyhow::Error {
    let message = format!("{what}: {}", crate::tree::refusal(branch, parent, &refused));
    match refused {
        Refused::Unknown(_) | Refused::NotBranch { .. } | Refused::OnlyRemote { .. } => {
            Refusal::NotFound.error(message)
        }
        Refused::NoRoot => anyhow::anyhow!(message),
        Refused::Root | Refused::Own | Refused::Cycle | Refused::NoParent => Usage(message).into(),
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

/// `gitfold parent`: the checks of the rules table (S1-S6, F6), then shows
/// the parent (`show`), forgets it (`--none`, F5) or turns following on or
/// off; `--set` and `--update` are not implemented yet.
pub fn parent(repo: &Path, args: ParentArgs) -> Result<()> {
    let given = super::branch_or_current(args.branch, repo)?;
    let common_dir = git::init(repo)?;
    check_name(&given)?;
    // S1 before reading the tree: an unknown branch is "not found" even when
    // the config can't be read (T5).
    let Some(name) = crate::tree::local_branch(&given) else {
        let r = Refused::Unknown(given.clone());
        return Err(refused("gitfold parent", &given, "", r));
    };
    let refuse = |parent: &str, r| refused("gitfold parent", &name, parent, r);
    let tree = Tree::read(&common_dir)?;
    if let Some(parent) = &args.set {
        tree.check(&name, parent).map_err(|r| refuse(parent, r))?;
    } else if args.follow || args.no_follow {
        tree.check_follow(&name).map_err(|r| refuse("", r))?;
    }
    if args.set.is_some() || args.update {
        return Err(not_implemented("gitfold parent"));
    }
    let parent = tree.parent(&name).map(crate::tree::short);
    match parent {
        // The rules table's F5; a parent stored on the root or a deleted
        // branch (T1, T2) goes too.
        _ if args.none => {
            settings::forget_parent(&common_dir, &name)?;
            match parent {
                Some(parent) => println!("{name} no longer has a parent (it was {parent})"),
                None => println!("{name} has no parent"),
            }
        }
        // `check_follow` let only a branch with a parent through (F6).
        Some(parent) if args.follow || args.no_follow => {
            settings::set_follow(&common_dir, &name, args.follow)?;
            match args.follow {
                true => println!("{name} follows {parent}"),
                false => println!(
                    "{name} no longer follows {parent}: `gitfold parent {name} --update` takes \
                     its new commits"
                ),
            }
        }
        _ => println!("{}", show(&common_dir, &tree, &name)?),
    }
    Ok(())
}

/// What `gitfold parent` says of local branch `name`: its parent, whether
/// it follows it, and whether the parent is gone (T3).
fn show(common_dir: &Path, tree: &Tree, name: &str) -> Result<String> {
    let Some(parent) = tree.parent(name) else {
        return Ok(match tree.is_root(name) {
            true => format!("{name} is the root of the tree; it has no parent"),
            false => format!("{name} has no parent"),
        });
    };
    let follows = settings::Local::read_checked(common_dir)?.follow(name);
    let short = crate::tree::short(parent);
    let gone = match git::resolve(parent) {
        Ok(_) => "",
        Err(_) => " (parent gone)",
    };
    Ok(match follows {
        true => format!("{name} is built on {short}{gone}, following it"),
        false => format!("{name} is built on {short}{gone}, not following it"),
    })
}

/// Checks `gitfold branch NEW --parent P` (in the repository of `repo`):
/// NEW doesn't exist yet (N1, refused as `branch` refuses an existing
/// branch), P resolves (N2), and the tree's rules for a new branch (N3, N4):
/// P's full ref.
pub fn check_new_parent(repo: &Path, branch: &str, parent: &str) -> Result<String> {
    let common_dir = git::init(repo)?;
    super::branch::check_new(&common_dir, branch)?;
    anyhow::ensure!(
        !git::branch_exists(branch),
        "a branch named '{branch}' already exists"
    );
    let tree = Tree::read(&common_dir)?;
    tree.check_new(branch, parent)
        .map_err(|r| refused("gitfold branch", branch, parent, r))
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
