//! Merging revisions as `git merge` would on a branch at the first one, without
//! a worktree (`gitfold branch NEW --from A --from B …`): the sources are
//! reduced as git reduces them (one contained in another is dropped, the
//! first one too), a single one left is a fast-forward, and several are
//! merged one after the other as git's octopus strategy does, each step by
//! `git merge-tree`. Only the last step may conflict, as for the octopus.
//! The commit is `git commit-tree`'s, with `git fmt-merge-msg`'s message.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};

use super::cmd;

/// A revision given to merge: as typed, and its commit.
pub struct Source {
    pub name: String,
    pub id: String,
}

/// Where a merge of sources leaves a new branch.
pub enum Merged {
    /// At the first source, which contains the others ("Already up to
    /// date", as git says).
    UpToDate,
    /// At source `.0`, which contains the first (a fast-forward).
    FastForward(usize),
    /// At a new merge commit, `id`.
    Merge { id: String },
    /// The last source conflicts: a merge in progress (not built yet).
    Conflict,
}

/// The commit `rev` stands for, as `git merge` takes it.
pub fn source(common_dir: &Path, rev: &str) -> Result<Source> {
    let spec = format!("{rev}^{{commit}}");
    let out = cmd::output(
        common_dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .ok()
    .and_then(|out| first_line(&out));
    match out {
        Some(id) => Ok(Source {
            name: rev.to_string(),
            id,
        }),
        None => bail!("{rev} - not something we can merge"),
    }
}

/// Merges `sources` (at least two; the first is where the branch starts) for
/// new branch `into` (named in the message), as `git merge` run on a branch
/// at the first would.
pub fn merge(common_dir: &Path, into: &str, sources: &[Source]) -> Result<Merged> {
    // Repeated sources count once, as for git (its list of heads is a set).
    let mut heads: Vec<&Source> = Vec::new();
    for s in sources {
        if !heads.iter().any(|h| h.id == s.id) {
            heads.push(s);
        }
    }
    let head = heads[0];
    // Those that no other contains (git's `reduce_heads`), in the order given.
    let mut args = vec!["merge-base", "--independent"];
    args.extend(heads.iter().map(|h| h.id.as_str()));
    let independent = lines(&cmd::output(common_dir, &args)?);
    let head_subsumed = !independent.contains(&head.id);
    let remotes: Vec<&Source> = heads[1..]
        .iter()
        .copied()
        .filter(|h| independent.contains(&h.id))
        .collect();
    if remotes.is_empty() {
        return Ok(Merged::UpToDate);
    }
    let all: Vec<&str> = std::iter::once(head)
        .chain(remotes.iter().copied())
        .map(|s| s.id.as_str())
        .collect();
    let common = merge_bases(common_dir, "--octopus", &all)?;
    ensure!(!common.is_empty(), "refusing to merge unrelated histories");
    if let [remote] = remotes[..]
        && common == [head.id.as_str()]
    {
        let at = sources.iter().position(|s| s.id == remote.id);
        return Ok(Merged::FastForward(at.expect("a source")));
    }
    // git-merge-octopus.sh (with one remote, the same as a two-way merge): the
    // commits merged so far (`MRC`) and their merge (`ours`, a commit).
    let mut merged = vec![head.id.clone()];
    let mut ours = head.id.clone();
    let mut tree: Option<String> = None;
    let mut non_ff = false;
    let mut failed: Option<&str> = None;
    for remote in &remotes {
        if let Some(name) = failed {
            bail!(
                "merging {name}: automated merge did not work, and more --from follow. \
                 Should not be doing an octopus: only the last --from may conflict"
            );
        }
        let mut args = vec![remote.id.as_str()];
        args.extend(merged.iter().map(String::as_str));
        let common = merge_bases(common_dir, "--all", &args)?;
        if common.contains(&remote.id) {
            continue; // "Already up to date with" it.
        }
        if !non_ff && common == [merged[0].as_str()] {
            // The first head merged was a fast-forward: its tree is the result
            // so far, and it still counts among the parents.
            merged = vec![remote.id.clone()];
            ours = remote.id.clone();
            tree = None;
            continue;
        }
        non_ff = true;
        if let Some(t) = &tree {
            // `merge-tree` finds the merge bases of the commits merged so far
            // and the next itself, from a commit of their merge.
            ours = commit_tree(common_dir, t, &merged, "gitfold: octopus step\n")?;
        }
        match merge_tree(common_dir, &ours, &remote.id)? {
            Some(t) => tree = Some(t),
            None => failed = Some(remote.name.as_str()),
        }
        merged.push(remote.id.clone());
    }
    if failed.is_some() {
        return Ok(Merged::Conflict);
    }
    let tree = match tree {
        Some(tree) => tree,
        None => first_line(&cmd::output(
            common_dir,
            &["rev-parse", "--verify", &format!("{ours}^{{tree}}")],
        )?)
        .context("no tree")?,
    };
    let mut parents: Vec<String> = Vec::new();
    if !head_subsumed {
        parents.push(head.id.clone());
    }
    parents.extend(remotes.iter().map(|r| r.id.clone()));
    let message = message(common_dir, into, &remotes)?;
    let id = commit_tree(common_dir, &tree, &parents, &message)?;
    Ok(Merged::Merge { id })
}

/// `git merge-base <how> <commits>`: none if they have no common ancestor.
fn merge_bases(common_dir: &Path, how: &str, commits: &[&str]) -> Result<Vec<String>> {
    let mut command = cmd::command(common_dir);
    command.args(["merge-base", how]).args(commits);
    let out = command.output()?;
    match out.status.code() {
        Some(0) => Ok(lines(&out.stdout)),
        // No common ancestor.
        Some(1) if out.stdout.is_empty() => Ok(Vec::new()),
        _ => bail!(
            "git merge-base failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The tree `git merge-tree` makes of commits `ours` and `theirs`, if they
/// merge without conflicts.
fn merge_tree(common_dir: &Path, ours: &str, theirs: &str) -> Result<Option<String>> {
    let out = cmd::command(common_dir)
        .args(["merge-tree", "--write-tree", "--no-messages", ours, theirs])
        .output()?;
    match out.status.code() {
        Some(0) => Ok(Some(first_line(&out.stdout).context("no tree")?)),
        Some(1) => Ok(None),
        _ => bail!(
            "git merge-tree failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// A commit of `tree` with `parents` and `message` (`git commit-tree`).
fn commit_tree(common_dir: &Path, tree: &str, parents: &[String], message: &str) -> Result<String> {
    let mut args = vec!["commit-tree", tree];
    for p in parents {
        args.extend(["-p", p.as_str()]);
    }
    let out = with_stdin(common_dir, &args, message.as_bytes())?;
    first_line(&out).context("git commit-tree printed no commit")
}

/// The message `git merge` gives the merge of `remotes` into branch `into`
/// (`git fmt-merge-msg`, from the lines `git merge` writes for it).
///
/// Without `merge.log`'s shortlog: `fmt-merge-msg` takes it against the HEAD
/// of the repository, not the branch made here.
fn message(common_dir: &Path, into: &str, remotes: &[&Source]) -> Result<String> {
    let mut input = String::new();
    for remote in remotes {
        input.push_str(&merge_name(common_dir, remote)?);
    }
    let out = with_stdin(
        common_dir,
        &["fmt-merge-msg", "--no-log", "--into-name", into],
        input.as_bytes(),
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The line `git merge` writes for `fmt-merge-msg` about `source` (builtin/
/// merge.c, `merge_name`): a branch, tag or remote-tracking branch by the
/// ref the name stands for; `<branch>~<n>` or `<branch>^` as the branch's
/// "early part"; any other revision as a commit.
fn merge_name(common_dir: &Path, source: &Source) -> Result<String> {
    let name = &source.name;
    if let Some(full) = super::full_ref(name) {
        let kind = [
            ("refs/heads/", "branch"),
            ("refs/tags/", "tag"),
            ("refs/remotes/", "remote-tracking branch"),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| full.starts_with(prefix).then_some(kind));
        if let Some(kind) = kind {
            // The ref's own object: an annotated tag's, whose message git adds.
            let id = first_line(&cmd::output(common_dir, &["rev-parse", "--verify", &full])?)
                .context("no object")?;
            return Ok(format!("{id}\t\t{kind} '{name}' of .\n"));
        }
    }
    if let Some((branch, early)) = early_part(name)
        && super::branch_exists(branch)
    {
        let early = if early { " (early part)" } else { "" };
        return Ok(format!("{}\t\tbranch '{branch}'{early} of .\n", source.id));
    }
    Ok(format!("{}\t\tcommit '{name}'\n", source.id))
}

/// `name` as `<branch>^…` or `<branch>~<n>`: the branch, and whether it is
/// an early part of it (not for `~0`), as git's `merge_name` reads it.
fn early_part(name: &str) -> Option<(&str, bool)> {
    let carets = name.len() - name.trim_end_matches('^').len();
    if carets > 0 {
        return Some((&name[..name.len() - carets], true));
    }
    let (branch, n) = name.rsplit_once('~')?;
    if !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((branch, n.is_empty() || n.bytes().any(|b| b != b'0')))
}

/// Runs git with `args` and `input` on its stdin; what it printed.
fn with_stdin(common_dir: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = cmd::command(common_dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped").write_all(input)?;
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "git {} failed ({}): {}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

fn lines(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn first_line(out: &[u8]) -> Option<String> {
    lines(out).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::early_part;

    #[test]
    fn early_parts_as_git_reads_them() {
        assert_eq!(early_part("api^"), Some(("api", true)));
        assert_eq!(early_part("api^^"), Some(("api", true)));
        assert_eq!(early_part("api~2"), Some(("api", true)));
        assert_eq!(early_part("api~"), Some(("api", true)));
        assert_eq!(early_part("api~0"), Some(("api", false)));
        assert_eq!(early_part("api~x"), None);
        assert_eq!(early_part("api"), None);
    }
}
