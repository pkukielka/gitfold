//! A workspace's live tree: its files as they are now, uncommitted work
//! included and ignored files left out, as `git add -A` would stage them, as
//! a tree in the views' private repository, which `@live` views merge as the
//! workspace's branch.
//!
//! git makes it, in the workspace (through the mount: the `.gitignore` files,
//! modes and symlinks it shows), on a copy of the workspace's index, with its
//! objects going to the private repository. Only paths given to git are
//! looked at, so git never walks the tree, and every command costs about one
//! read and one write of the index (pathspecs would cost each path times the
//! index): those gitfold knows differ from the base
//! (`state::status::unfiltered`), and those where the index differs from the
//! base (`git diff-index --cached`: staged, unstaged with `git rm --cached`,
//! unmerged). Of these, git says which it ignores (`check-ignore`); those
//! the index tracks stay, as for `git add -A`. git hashes the rest
//! (`update-index`, with the repository's filters), removals first: a base
//! file now a directory leaves the index before what is below it comes in.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use gix::ObjectId;

use super::merge::Private;
use crate::git;
use crate::state::upper_path;

/// A workspace, as its live tree is made from it.
pub struct Workspace<'a> {
    /// Its worktree (through the mount), its index, its upper directory.
    pub dir: &'a Path,
    pub index: &'a Path,
    pub upper: &'a Path,
    /// Its base, its upper files and symlinks that differ from it, and the
    /// base files deleted (`state::status::unfiltered`).
    pub base: ObjectId,
    pub changed: &'a [Vec<u8>],
    pub deleted: &'a [Vec<u8>],
}

/// A workspace's live tree, and the commit of it the views merge.
pub struct Live {
    pub tree: ObjectId,
    pub commit: ObjectId,
}

impl Private {
    /// The live tree of workspace `ws`, of `branch`. The commit is kept
    /// (`refs/gitfold/live/<branch>`) until the next.
    pub fn live(&self, branch: &str, ws: &Workspace) -> Result<Live> {
        let _lock = self.lock.lock();
        let copy = self.live_index(branch)?;
        let index = ws.index;
        std::fs::copy(index, &copy).with_context(|| format!("copying {}", index.display()))?;
        let git = || self.in_workspace(ws.dir, &copy);
        let base = ws.base.to_string();
        let args = ["diff-index", "--cached", "--name-only", "-z", &base];
        let out = git::cmd::output_of(&mut git()?, &args, None)?;
        let staged: Vec<&[u8]> = nul_fields(&out).collect();
        let changed = || ws.changed.iter().map(Vec::as_slice);
        // Removals first (`deleted`, `staged`): a path git would refuse to
        // remove, a directory the index has nothing at (it adds what is
        // below), is left out.
        let removals: Vec<&[u8]> = (ws.deleted.iter().map(Vec::as_slice))
            .chain(staged.iter().copied())
            .collect();
        let asked: Vec<&[u8]> = changed().chain(staged.iter().copied()).collect();
        let ignored = ignored(&git, &asked)?;
        let dirs = removals.iter().copied().filter(|p| {
            std::fs::symlink_metadata(upper_path(ws.upper, p)).is_ok_and(|m| m.is_dir())
        });
        let asked: Vec<&[u8]> = ignored.into_iter().chain(dirs).collect();
        let untracked = untracked(&git, &asked)?;
        let mut seen = HashSet::new();
        let update = (removals.into_iter().chain(changed()))
            .filter(|p| !untracked.contains(p) && seen.insert(*p));
        let update = nul_list(b"", update);
        if !update.is_empty() {
            let args = [
                "update-index",
                "-z",
                "--add",
                "--remove",
                "--replace",
                "--stdin",
            ];
            git::cmd::output_of(&mut git()?, &args, Some(update))?;
        }
        let args = ["write-tree", "--missing-ok"];
        let tree = super::merge::oid(&git::cmd::output_of(&mut git()?, &args, None)?)?;
        let commit = self.commit(&tree, branch)?;
        self.refs(format!("update refs/gitfold/live/{branch} {commit}\n"))?;
        Ok(Live { tree, commit })
    }

    /// Where the copy of `branch`'s workspace index is, without a lock a
    /// killed git may have left.
    fn live_index(&self, branch: &str) -> Result<PathBuf> {
        let dir = self.dir.join("live");
        std::fs::create_dir_all(&dir)?;
        let index = dir.join(format!("{}.index", crate::state::workspace_id(branch)));
        let mut lock = index.clone().into_os_string();
        lock.push(".lock");
        if let Err(e) = std::fs::remove_file(&lock)
            && !crate::fsutil::not_found(&e)
        {
            return Err(e.into());
        }
        Ok(index)
    }

    /// git in the workspace at `dir`, on index `index`, its objects going to
    /// the private repository: a plain index, whatever the repository's are
    /// like, unhashed (nothing reads it but these commands), and no fsmonitor
    /// or hook hearing of it. Paths aren't pathspecs here but for
    /// `check-ignore`, which takes no `--literal-pathspecs` (see
    /// `ignored`).
    fn in_workspace(&self, dir: &Path, index: &Path) -> Result<Command> {
        let mut git = git::cmd::in_workspace(dir)?;
        let plain = (git::cmd::PLAIN_INDEX.iter()).filter(|a| **a != "--literal-pathspecs");
        git.args(plain)
            .args(["-c", "index.skipHash=true"])
            .env("GIT_INDEX_FILE", index);
        self.to_private(&mut git);
        Ok(git)
    }
}

/// Of `paths`, those git ignores (tracked or not).
fn ignored<'a>(git: &dyn Fn() -> Result<Command>, paths: &[&'a [u8]]) -> Result<Vec<&'a [u8]>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // `./`: a path is taken as a pathspec there, which `:` would start.
    // `--no-index`: the index is asked apart (`untracked`; git would match
    // each path against the whole index).
    let input = nul_list(b"./", paths.iter().copied());
    let args = ["check-ignore", "--no-index", "-z", "--stdin"];
    // 1: none ignored.
    let out = git::cmd::output_allowing(&mut git()?, &args, Some(input), &[1])?;
    let ignored: HashSet<&[u8]> = nul_fields(&out)
        .map(|p| p.strip_prefix(b"./").unwrap_or(p))
        .collect();
    Ok(paths
        .iter()
        .copied()
        .filter(|p| ignored.contains(p))
        .collect())
}

/// The fields of git's NUL-terminated output (`-z`), up to an empty one.
fn nul_fields(out: &[u8]) -> impl Iterator<Item = &[u8]> {
    out.split(|b| *b == 0).take_while(|p| !p.is_empty())
}

/// Of `paths`, those the index has no entry at (unmerged ones aside).
fn untracked<'a>(
    git: &dyn Fn() -> Result<Command>,
    paths: &[&'a [u8]],
) -> Result<HashSet<&'a [u8]>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    // `:0:<path>`: the index's entry at the path, if it has one.
    let input = nul_list(b":0:", paths.iter().copied());
    let args = ["cat-file", "-Z", "--batch-check=%(objectname)"];
    let answers = git::cmd::output_of(&mut git()?, &args, Some(input))?;
    Ok((paths.iter().zip(answers.split(|b| *b == 0)))
        .filter(|(_, a)| a.ends_with(b" missing"))
        .map(|(p, _)| *p)
        .collect())
}

/// `paths`, each with `prefix` and ended by a NUL.
fn nul_list<'a>(prefix: &[u8], paths: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut list = Vec::new();
    for path in paths {
        list.extend_from_slice(prefix);
        list.extend_from_slice(path);
        list.push(0);
    }
    list
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::super::test_repo::{commit, private, run_in, worktree};
    use super::*;

    fn paths(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    /// The worktree's index.
    fn index_of(dir: &Path) -> PathBuf {
        let args = ["rev-parse", "--path-format=absolute", "--git-path", "index"];
        PathBuf::from(run_in(dir, &args, None))
    }

    /// `tree` as `git ls-tree -r` lists it (read from the private
    /// repository).
    fn listing(tree: &ObjectId) -> String {
        in_private(&["ls-tree", "-r", &tree.to_string()])
    }

    /// git's output in the private repository.
    fn in_private(args: &[&str]) -> String {
        let out = git::cmd::output(&private().dir, args).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The live tree of worktree `wt` (its own upper directory) at `base`.
    fn live(name: &str, wt: &Path, base: &str, changed: &[&str], deleted: &[&str]) -> Live {
        let (changed, deleted) = (paths(changed), paths(deleted));
        let index = index_of(wt);
        let ws = Workspace {
            dir: wt,
            index: &index,
            upper: wt,
            base: ObjectId::from_hex(base.as_bytes()).unwrap(),
            changed: &changed,
            deleted: &deleted,
        };
        private().live(name, &ws).unwrap()
    }

    /// `files` (mode, path, content) as `git ls-tree -r` lists them.
    fn tree_of(files: &[(&str, &str, &str)]) -> String {
        (files.iter())
            .map(|(mode, path, data)| format!("{mode} blob {}\t{path}\n", blob(data)))
            .collect()
    }

    /// The blob id of `data`, as git would hash it.
    fn blob(data: &str) -> String {
        let id = gix::objs::compute_hash(
            gix::hash::Kind::Sha1,
            gix::objs::Kind::Blob,
            data.as_bytes(),
        );
        id.unwrap().to_string()
    }

    #[test]
    fn workspace_as_git_add_sees_it() {
        let base = commit(
            None,
            &[
                (".gitignore", Some("*.o\nbuild/\n!keep.o\n")),
                ("d/e/a", Some("a\n")),
                ("d/b", Some("b\n")),
                ("f/c", Some("c\n")),
                ("tracked.o", Some("t\n")),
                ("same", Some("s\n")),
            ],
        );
        let wt = worktree("l1", &base);
        let w = |p: &str, data: &str| fs::write(wt.join(p), data).unwrap();
        w("tracked.o", "t2\n");
        w("new.o", "ignored\n");
        w("keep.o", "k\n");
        fs::create_dir(wt.join("build")).unwrap();
        w("build/x", "ignored\n");
        fs::remove_dir_all(wt.join("d")).unwrap();
        w("d", "now a file\n");
        fs::remove_file(wt.join("f/c")).unwrap();
        fs::create_dir(wt.join("f/c")).unwrap();
        w("f/c/z", "z\n");
        symlink("d", wt.join("link")).unwrap();
        w("exe", "x\n");
        fs::set_permissions(wt.join("exe"), fs::Permissions::from_mode(0o755)).unwrap();
        w("st", "staged\n");
        run_in(&wt, &["add", "st"], None);
        let index = index_of(&wt);
        let before = fs::read(&index).unwrap();
        let changed = [
            "tracked.o",
            "new.o",
            "keep.o",
            "build/x",
            "d",
            "f/c/z",
            "link",
            "exe",
            "st",
        ];
        let live = live("l1", &wt, &base, &changed, &["d/b", "d/e/a", "f/c"]);
        let expected = [
            ("100644", ".gitignore", "*.o\nbuild/\n!keep.o\n"),
            ("100644", "d", "now a file\n"),
            ("100755", "exe", "x\n"),
            ("100644", "f/c/z", "z\n"),
            ("100644", "keep.o", "k\n"),
            ("120000", "link", "d"),
            ("100644", "same", "s\n"),
            ("100644", "st", "staged\n"),
            ("100644", "tracked.o", "t2\n"),
        ];
        assert_eq!(listing(&live.tree), tree_of(&expected));
        assert_eq!(
            fs::read(&index).unwrap(),
            before,
            "the workspace's index untouched"
        );
        let tree = format!("{}^{{tree}}", live.commit);
        let kept = in_private(&["rev-parse", "refs/gitfold/live/l1", &tree]);
        assert_eq!(kept, format!("{}\n{}\n", live.commit, live.tree));
    }

    #[test]
    fn paths_as_written() {
        let ignore = "x*\n1:*\n";
        let base = commit(None, &[(".gitignore", Some(ignore)), ("1:y", Some("1\n"))]);
        let wt = worktree("l2", &base);
        // `:x` isn't the pathspec `x` (ignored); `1:y`, ignored but tracked,
        // isn't stage 1 of `y`.
        for p in [":x", "1:y", "a b"] {
            fs::write(wt.join(p), "p\n").unwrap();
        }
        let live = live("l2", &wt, &base, &[":x", "1:y", "a b"], &[]);
        let expected: String = [".gitignore", "1:y", ":x", "a b"]
            .iter()
            .map(|p| {
                let data = if *p == ".gitignore" { ignore } else { "p\n" };
                format!("100644 blob {}\t{p}\n", blob(data))
            })
            .collect();
        assert_eq!(listing(&live.tree), expected);
    }

    #[test]
    fn a_merge_in_progress() {
        let base = commit(None, &[("m", Some("base\n")), ("n", Some("n\n"))]);
        let wt = worktree("l3", &base);
        // `m` unmerged in the index, as the worktree has it: not a change.
        let (one, two) = (blob("base\n"), blob("ours\n"));
        let info = format!(
            "0 {zero}\tm\n100644 {one} 1\tm\n100644 {two} 2\tm\n",
            zero = "0".repeat(40)
        );
        run_in(
            &wt,
            &["update-index", "--index-info"],
            Some(info.as_bytes()),
        );
        let live = live("l3", &wt, &base, &[], &[]);
        let expected = format!(
            "100644 blob {}\tm\n100644 blob {}\tn\n",
            blob("base\n"),
            blob("n\n")
        );
        assert_eq!(listing(&live.tree), expected);
    }

    #[test]
    fn the_index_follows_the_worktree() {
        let files = [
            ("same", Some("s\n")),
            ("f", Some("f\n")),
            ("g", Some("g\n")),
            ("h", Some("h\n")),
        ];
        let base = commit(None, &files);
        let wt = worktree("l5", &base);
        let w = |p: &str, data: &str| fs::write(wt.join(p), data).unwrap();
        // None of these differs from the base in the worktree: the index does.
        run_in(&wt, &["rm", "-q", "--cached", "same"], None);
        w("new", "x\n");
        run_in(&wt, &["add", "new"], None);
        fs::remove_file(wt.join("new")).unwrap();
        w("f", "f2\n");
        run_in(&wt, &["add", "f"], None);
        w("f", "f\n");
        // Base files now empty directories, removed from the index (`g`) or
        // not (`h`).
        run_in(&wt, &["rm", "-q", "g"], None);
        fs::remove_file(wt.join("h")).unwrap();
        for d in ["g", "h"] {
            fs::create_dir(wt.join(d)).unwrap();
        }
        let live = live("l5", &wt, &base, &[], &["g", "h"]);
        let expected = [("100644", "f", "f\n"), ("100644", "same", "s\n")];
        assert_eq!(listing(&live.tree), tree_of(&expected));
    }

    #[test]
    fn a_submodule_as_its_commit() {
        let old = commit(None, &[("a", Some("a\n"))]);
        let base = commit(None, &[("sub", Some(&format!("gitlink:{old}")))]);
        let wt = worktree("l6", &base);
        let sub = wt.join("sub");
        run_in(&wt, &["init", "-q", "sub"], None);
        fs::write(sub.join("f"), "f\n").unwrap();
        run_in(&sub, &["add", "f"], None);
        run_in(&sub, &["commit", "-q", "-m", "s"], None);
        let head = run_in(&sub, &["rev-parse", "HEAD"], None);
        let live = live("l6", &wt, &base, &["sub"], &[]);
        assert_eq!(listing(&live.tree), format!("160000 commit {head}\tsub\n"));
    }

    #[test]
    fn changes_from_the_state_ignored_included() {
        let old = commit(None, &[("a", Some("a\n"))]);
        let base = commit(
            None,
            &[
                (".gitignore", Some("*.o\n")),
                ("a/x", Some("x\n")),
                ("a/y", Some("y\n")),
                ("b", Some("b\n")),
                ("s", Some("s\n")),
                ("sub", Some(&format!("gitlink:{old}"))),
            ],
        );
        let state = crate::state::StateDir::of(&super::super::test_repo::git_dir(), "l4");
        let upper = state.upper();
        fs::create_dir_all(&upper).unwrap();
        state
            .write_base(&ObjectId::from_hex(base.as_bytes()).unwrap())
            .unwrap();
        for (p, data) in [("b", "b2\n"), ("n.o", "o\n"), ("c", "c\n"), ("s", "s\n")] {
            fs::write(upper.join(p), data).unwrap();
        }
        // A submodule checked out: its commit is git's to tell.
        fs::create_dir_all(upper.join("sub")).unwrap();
        fs::write(upper.join("sub/.git"), "gitdir: x\n").unwrap();
        fs::write(upper.join("sub/f"), "f\n").unwrap();
        fs::write(state.markers(), "Wa\0").unwrap();
        let s = crate::state::status::unfiltered(&crate::store::Store::default(), &state).unwrap();
        let mut changed = s.changed.clone();
        changed.sort();
        assert_eq!(
            changed,
            paths(&["b", "c", "n.o", "sub"]),
            "s is as in the base"
        );
        assert_eq!(s.deleted, paths(&["a/x", "a/y"]));
        assert_eq!(s.ignored, 0);
    }
}
