//! Merging revisions as `git merge` would on a branch at the first one, without
//! a worktree (`gitfold branch NEW --from A --from B …`): the sources are
//! reduced as git reduces them (one contained in another is dropped, the
//! first one too), a single one left is a fast-forward (as `merge.ff`
//! allows) or merged by `git merge-tree` (git's default strategy, ort), and
//! several are merged as git's octopus strategy (git-merge-octopus.sh) merges
//! them: one after the other, each by `git read-tree --aggressive` in an
//! index of its own and git-merge-one-file's rules for what that leaves.
//! Only the last may conflict. The commit is `git commit-tree`'s, with `git
//! fmt-merge-msg`'s message.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};

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
    /// At new merge commit `id`, whose parents are the sources at `parents`.
    Merge { id: String, parents: Vec<usize> },
    /// The last source conflicts: a merge in progress (not built yet).
    Conflict,
}

/// The commit `rev` stands for.
pub fn source(rev: &str) -> Result<Source> {
    let commit = super::resolve(rev).map_err(|_| anyhow!("{rev} - not something we can merge"))?;
    Ok(Source {
        name: rev.to_string(),
        id: commit.id.to_string(),
    })
}

/// `merge.ff`, as git reads it.
#[derive(PartialEq)]
enum FastForward {
    Allow,
    No,
    Only,
}

/// Merges `sources` (at least two; the first is where the branch starts) for
/// new branch `into` (named in the message), as `git merge` run on a branch
/// at the first would.
pub fn merge(common_dir: &Path, into: &str, sources: &[Source]) -> Result<Merged> {
    // Repeated sources count once, as for git (its list of heads is a set).
    let mut heads: Vec<usize> = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        if !heads.iter().any(|&h| sources[h].id == s.id) {
            heads.push(i);
        }
    }
    let head = &sources[0];
    // Those that no other contains (git's `reduce_heads`), in the order given.
    let mut args = vec!["merge-base", "--independent"];
    args.extend(heads.iter().map(|&h| sources[h].id.as_str()));
    let independent = lines(&cmd::output(common_dir, &args)?);
    let head_subsumed = !independent.contains(&head.id);
    let remotes: Vec<usize> = (heads[1..].iter().copied())
        .filter(|&h| independent.contains(&sources[h].id))
        .collect();
    if remotes.is_empty() {
        return Ok(Merged::UpToDate);
    }
    let all: Vec<&str> = std::iter::once(0)
        .chain(remotes.iter().copied())
        .map(|i| sources[i].id.as_str())
        .collect();
    ensure!(
        !merge_bases(common_dir, "--octopus", &all)?.is_empty(),
        "refusing to merge unrelated histories"
    );
    let ff = fast_forward(common_dir)?;
    if let [remote] = remotes[..]
        && head_subsumed
        && ff != FastForward::No
    {
        return Ok(Merged::FastForward(remote));
    }
    ensure!(
        ff != FastForward::Only,
        "Not possible to fast-forward, aborting."
    );
    let tree = match remotes[..] {
        [remote] => merge_tree(common_dir, &head.id, &sources[remote].id)?,
        _ => octopus(common_dir, head, &remotes, sources)?,
    };
    let Some(tree) = tree else {
        return Ok(Merged::Conflict);
    };
    let mut parents = Vec::new();
    if !head_subsumed || ff == FastForward::No {
        parents.push(0);
    }
    parents.extend(&remotes);
    let ids: Vec<String> = parents.iter().map(|&p| sources[p].id.clone()).collect();
    let remotes: Vec<&Source> = remotes.iter().map(|&r| &sources[r]).collect();
    let message = message(common_dir, into, &remotes)?;
    let id = commit_tree(common_dir, &tree, &ids, &message)?;
    Ok(Merged::Merge { id, parents })
}

/// `merge.ff` (default: allowed); a value git doesn't know is ignored, as
/// git ignores it.
fn fast_forward(common_dir: &Path) -> Result<FastForward> {
    let args = ["config", "--type=bool-or-str", "--get", "merge.ff"];
    // Exit 1: not set.
    let (_, out) = cmd::output_or_no(common_dir, &args)?;
    Ok(match first_line(&out).as_deref() {
        Some("false") => FastForward::No,
        Some("only") => FastForward::Only,
        _ => FastForward::Allow,
    })
}

/// git-merge-octopus.sh: merges the sources at `remotes` one after the other
/// into `head`; the tree, or none if the last conflicts. One conflicting
/// earlier is refused, as the octopus refuses it.
fn octopus(
    common_dir: &Path,
    head: &Source,
    remotes: &[usize],
    sources: &[Source],
) -> Result<Option<String>> {
    let index = TempIndex::new(common_dir);
    // The commits merged so far (`MRC`) and the tree of their merge (`MRT`).
    let mut merged = vec![head.id.as_str()];
    let mut tree = format!("{}^{{tree}}", head.id);
    let mut non_ff = false;
    for (i, &r) in remotes.iter().enumerate() {
        let remote = &sources[r];
        let mut args = vec![remote.id.as_str()];
        args.extend(&merged);
        let common = merge_bases(common_dir, "--all", &args)?;
        if !non_ff && common == merged {
            // The first head merged was a fast-forward: its tree is the result
            // so far, and it still counts among the parents.
            merged = vec![remote.id.as_str()];
            tree = format!("{}^{{tree}}", remote.id);
            continue;
        }
        non_ff = true;
        match index.merge(&common, &tree, &remote.id)? {
            Some(next) => tree = next,
            None if i + 1 == remotes.len() => return Ok(None),
            None => bail!(
                "merging {}: Automated merge did not work. Should not be doing an octopus \
                 (only the last --from may conflict).",
                remote.name
            ),
        }
        merged.push(remote.id.as_str());
    }
    Ok(Some(tree))
}

/// An index entry: its mode and object id, as git prints them.
type Entry = (String, String);

/// An index file of its own for the octopus's steps, deleted when dropped.
struct TempIndex {
    common_dir: PathBuf,
    path: PathBuf,
}

impl TempIndex {
    /// In the git directory itself, as git's own `index.lock`: nothing else
    /// (gitfold's state directory) is made for it.
    fn new(common_dir: &Path) -> TempIndex {
        let name = format!("gitfold-merge-index-{}", std::process::id());
        TempIndex {
            common_dir: common_dir.to_path_buf(),
            path: common_dir.join(name),
        }
    }

    /// git with this index, to run.
    fn command(&self) -> std::process::Command {
        let mut git = cmd::command(&self.common_dir);
        git.env("GIT_INDEX_FILE", &self.path);
        git
    }

    /// Runs git with `args` on this index; what it printed.
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        cmd::checked(self.command(), args, b"")
    }

    /// One step of the octopus: `ours` and `theirs` (trees) merged with
    /// merge bases `bases`; the tree, or none on a conflict.
    fn merge(&self, bases: &[String], ours: &str, theirs: &str) -> Result<Option<String>> {
        let _ = fs::remove_file(&self.path);
        let mut args = vec!["read-tree", "-m", "--aggressive"];
        args.extend(bases.iter().map(String::as_str));
        args.extend([ours, theirs]);
        self.git(&args)?;
        // What read-tree left unmerged, by path: stages 1 (base), 2 (ours) and
        // 3 (theirs), as (mode, id).
        let mut unmerged: BTreeMap<Vec<u8>, [Option<Entry>; 3]> = BTreeMap::new();
        for entry in self.git(&["ls-files", "-u", "-z"])?.split(|&b| b == 0) {
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&entry[..tab]);
            let fields: Vec<&str> = meta.split(' ').collect();
            let [mode, id, stage] = fields[..] else {
                bail!("git ls-files -u: unexpected entry {meta}");
            };
            let stage: usize = stage.parse()?;
            ensure!((1..=3).contains(&stage), "git ls-files -u: stage {stage}");
            let stages = unmerged.entry(entry[tab + 1..].to_vec()).or_default();
            stages[stage - 1] = Some((mode.to_string(), id.to_string()));
        }
        // In index order, each as merge-one-file updates it: a path that is a
        // file in one source and a directory in another is refused there, a
        // conflict.
        for (path, stages) in &unmerged {
            let Some(resolved) = merge_one_file(&self.common_dir, stages)? else {
                return Ok(None);
            };
            let mut git = self.command();
            git.arg("update-index");
            match resolved {
                Some((mode, id)) => git.args(["--add", "--cacheinfo", &mode, &id]),
                None => git.args(["--force-remove", "--"]),
            };
            let out = git.arg(OsStr::from_bytes(path)).output()?;
            if !out.status.success() {
                return Ok(None);
            }
        }
        let tree = first_line(&self.git(&["write-tree"])?);
        Ok(Some(tree.context("git write-tree printed no tree")?))
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// git-merge-one-file's resolution of a path read-tree left unmerged, with
/// `stages` 1 (base), 2 (ours) and 3 (theirs): the entry it keeps (none:
/// removed), or none if git leaves a conflict (or refuses the case).
fn merge_one_file(common_dir: &Path, stages: &[Option<Entry>; 3]) -> Result<Option<Option<Entry>>> {
    fn mode(s: &Option<Entry>) -> &str {
        s.as_ref().map_or("", |(m, _)| m.as_str())
    }
    fn id(s: &Option<Entry>) -> Option<&str> {
        s.as_ref().map(|(_, i)| i.as_str())
    }
    let [base, ours, theirs] = stages;
    // Deleted in both, or deleted in one and unchanged in the other: refused
    // if the mode changed.
    let deleted = base.is_some()
        && ((ours.is_none() && (theirs.is_none() || id(theirs) == id(base)))
            || (theirs.is_none() && id(ours) == id(base)));
    if deleted {
        let mode_changed = (ours.is_none() && mode(base) != mode(theirs))
            || (theirs.is_none() && mode(base) != mode(ours));
        return Ok((!mode_changed).then_some(None));
    }
    match (base, ours, theirs) {
        // Added in one.
        (None, Some(added), None) | (None, None, Some(added)) => Ok(Some(Some(added.clone()))),
        // Added in both, identically (and with the same mode).
        (None, Some(o), Some(t)) if o.1 == t.1 => Ok((o.0 == t.0).then(|| Some(o.clone()))),
        // Modified in both, but differently: merged by content, if regular
        // files; added in both differently is a conflict.
        (Some(b), Some(o), Some(t)) => {
            let special = |m: &str| m == "120000" || m == "160000";
            if special(&o.0) || special(&t.0) || o.0 != t.0 {
                return Ok(None);
            }
            // Any failure is a conflict, as for merge-one-file (binary files).
            let args = ["merge-file", "--object-id", &o.1, &b.1, &t.1];
            let out = cmd::command(common_dir).args(args).output()?;
            if !out.status.success() {
                return Ok(None);
            }
            let merged = first_line(&out.stdout).context("git merge-file printed no id")?;
            Ok(Some(Some((o.0.clone(), merged))))
        }
        _ => Ok(None),
    }
}

/// `git merge-base <how> <commits>`: none if they have no common ancestor.
fn merge_bases(common_dir: &Path, how: &str, commits: &[&str]) -> Result<Vec<String>> {
    let mut args = vec!["merge-base", how];
    args.extend(commits);
    // Exit 1: no common ancestor.
    let (_, out) = cmd::output_or_no(common_dir, &args)?;
    Ok(lines(&out))
}

/// The tree `git merge-tree` makes of commits `ours` and `theirs`, if they
/// merge without conflicts.
fn merge_tree(common_dir: &Path, ours: &str, theirs: &str) -> Result<Option<String>> {
    let args = ["merge-tree", "--write-tree", "--no-messages", ours, theirs];
    match cmd::output_or_no(common_dir, &args)? {
        (true, out) => Ok(Some(first_line(&out).context("no tree")?)),
        (false, _) => Ok(None),
    }
}

/// A commit of `tree` with `parents` and `message` (`git commit-tree`).
fn commit_tree(common_dir: &Path, tree: &str, parents: &[String], message: &str) -> Result<String> {
    let mut args = vec!["commit-tree", tree];
    for p in parents {
        args.extend(["-p", p.as_str()]);
    }
    let out = cmd::output_with_input(common_dir, &args, message.as_bytes())?;
    first_line(&out).context("git commit-tree printed no commit")
}

/// The message `git merge` gives the merge of `remotes` into branch `into`
/// (`git fmt-merge-msg`, from the lines `git merge` writes for it).
///
/// Without `merge.log`'s shortlog: `fmt-merge-msg` takes it against the HEAD
/// of the repository, not the branch made here.
fn message(common_dir: &Path, into: &str, remotes: &[&Source]) -> Result<String> {
    let input: String = remotes.iter().map(|r| merge_name(r)).collect();
    let out = cmd::output_with_input(
        common_dir,
        &["fmt-merge-msg", "--no-log", "--into-name", into],
        input.as_bytes(),
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The line `git merge` writes for `fmt-merge-msg` about `source` (builtin/
/// merge.c, `merge_name`; no plumbing writes it): a branch, tag or
/// remote-tracking branch by the ref the name stands for; `<branch>~<n>` or
/// `<branch>^` as the branch's "early part"; a tag object by its id as a
/// tag; any other revision as a commit.
fn merge_name(source: &Source) -> String {
    let name = &source.name;
    let kinds = [
        ("refs/heads/", "branch"),
        ("refs/tags/", "tag"),
        ("refs/remotes/", "remote-tracking branch"),
    ];
    if let Some(full) = super::full_ref(name)
        && let Some((_, kind)) = kinds.iter().find(|(prefix, _)| full.starts_with(prefix))
        // The ref's own object: an annotated tag's, whose message git adds.
        && let Some(id) = ref_target(&full)
    {
        return format!("{id}\t\t{kind} '{name}' of .\n");
    }
    if let Some((branch, early)) = early_part(name)
        && super::branch_exists(branch)
    {
        let early = if early { " (early part)" } else { "" };
        return format!("{}\t\tbranch '{branch}'{early} of .\n", source.id);
    }
    if let Some(tag) = tag_object(name) {
        return format!("{tag}\t\ttag '{name}'\n");
    }
    format!("{}\t\tcommit '{name}'\n", source.id)
}

/// The object ref `full` points at, not peeled.
fn ref_target(full: &str) -> Option<String> {
    super::with_repo(|repo| {
        let reference = repo.find_reference(full).ok()?;
        Some(reference.target().try_id()?.to_string())
    })
}

/// The tag object `rev` names itself (not through a ref), if it names one.
fn tag_object(rev: &str) -> Option<String> {
    super::with_repo(|repo| {
        let object = repo.rev_parse_single(rev).ok()?.object().ok()?;
        (object.kind == gix::object::Kind::Tag).then(|| object.id.to_string())
    })
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
