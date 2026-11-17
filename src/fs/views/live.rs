//! A workspace's live tree: its files as they are now, uncommitted work
//! included and ignored files left out, as a tree in the views' private
//! repository, which `@live` views merge as the workspace's branch.
//!
//! git makes it, in the workspace (through the mount: the `.gitignore` files,
//! modes and symlinks it shows), on a copy of the workspace's index, with its
//! objects going to the private repository. Only the paths gitfold knows
//! differ from the base are given to git (`state::status::unfiltered`): git
//! never walks the tree, and every command costs about one read and one write
//! of the index (pathspecs would cost each path times the index). For upper
//! files, git says which it ignores (`check-ignore`); those the index tracks
//! stay, as for `git add -A`. git hashes the rest (`update-index`, with the
//! repository's filters), deletions first: a base file now a directory leaves
//! the index before what is below it comes in.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use gix::ObjectId;

use super::merge::Private;
use crate::git;

/// A workspace's live tree, and the commit of it the views merge.
pub struct Live {
    pub tree: ObjectId,
    pub commit: ObjectId,
}

impl Private {
    /// The live tree of the workspace of `branch`: its worktree `dir`, its
    /// index `index`, its upper files and symlinks that differ from its base
    /// `changed` and its deleted base files `deleted` (`state::status::
    /// unfiltered`). The commit is kept (`refs/gitfold/live/<branch>`) until
    /// the next.
    pub fn live(
        &self,
        branch: &str,
        dir: &Path,
        index: &Path,
        changed: &[Vec<u8>],
        deleted: &[Vec<u8>],
    ) -> Result<Live> {
        let _lock = self.lock.lock();
        let copy = self.live_index(branch)?;
        std::fs::copy(index, &copy).with_context(|| format!("copying {}", index.display()))?;
        let git = || in_workspace(dir, &copy, &self.dir.join("objects"));
        let untracked = untracked_ignored(&git, changed)?;
        let kept = changed.iter().filter(|p| !untracked.contains(*p));
        let update = nul_list(deleted.iter().chain(kept).cloned());
        let args = [
            "update-index",
            "-z",
            "--add",
            "--remove",
            "--replace",
            "--stdin",
        ];
        git::cmd::output_of(&mut git()?, &args, Some(update))?;
        let tree = write_tree(&git)?;
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
}

/// git in the workspace at `dir`, on index `index`, its objects going to
/// `objects`: a plain index, whatever the repository's are like, unhashed
/// (nothing reads it but these commands), and no fsmonitor or hook hearing
/// of it. Paths aren't pathspecs here but for `check-ignore`, which takes no
/// `--literal-pathspecs` (see `untracked_ignored`).
fn in_workspace(dir: &Path, index: &Path, objects: &Path) -> Result<Command> {
    let mut git = git::cmd::in_workspace(dir)?;
    let plain = git::cmd::PLAIN_INDEX
        .iter()
        .filter(|a| **a != "--literal-pathspecs");
    git.args(plain)
        .args(["-c", "index.skipHash=true"])
        .env("GIT_INDEX_FILE", index)
        .env("GIT_OBJECT_DIRECTORY", objects)
        .env("GIT_NO_LAZY_FETCH", "1");
    Ok(git)
}

/// Of `paths`, those git ignores and the index doesn't track.
fn untracked_ignored(
    git: &dyn Fn() -> Result<Command>,
    paths: &[Vec<u8>],
) -> Result<HashSet<Vec<u8>>> {
    // `./`: a path is taken as a pathspec there, which `:` would start.
    // `--no-index`: the index is asked below (git would match each path
    // against the whole index).
    let input = nul_list(paths.iter().map(|p| [b"./", p.as_slice()].concat()));
    let args = ["check-ignore", "--no-index", "-z", "--stdin"];
    // 1: none ignored.
    let out = git::cmd::output_allowing(&mut git()?, &args, Some(input), &[1])?;
    let ignored: Vec<Vec<u8>> = (git::cmd::nul_paths(&out).into_iter())
        .map(|p| p.strip_prefix(b"./").map(<[u8]>::to_vec).unwrap_or(p))
        .collect();
    if ignored.is_empty() {
        return Ok(HashSet::new());
    }
    // `:0:<path>`: the index's entry at the path, if it has one.
    let input = nul_list(ignored.iter().map(|p| [b":0:", p.as_slice()].concat()));
    let args = ["cat-file", "-Z", "--batch-check=%(objectname)"];
    let out = git::cmd::output_of(&mut git()?, &args, Some(input))?;
    let answers: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    Ok((ignored.into_iter().zip(answers))
        .filter(|(_, a)| a.ends_with(b" missing"))
        .map(|(p, _)| p)
        .collect())
}

/// The tree of the index. Paths the index has unmerged (a merge in
/// progress) that gitfold doesn't know changed are taken as the worktree has
/// them first, as `git add` of them would. Objects aren't checked: they are
/// the repository's (a partial clone's may be missing) or hashed just now.
fn write_tree(git: &dyn Fn() -> Result<Command>) -> Result<ObjectId> {
    let write = || git::cmd::output_of(&mut git()?, &["write-tree", "--missing-ok"], None);
    let out = match write() {
        Ok(out) => out,
        Err(e) => {
            let args = ["ls-files", "-z", "--unmerged"];
            let unmerged = git::cmd::output_of(&mut git()?, &args, None)?;
            // Each: a mode, an id and a stage, a tab, the path.
            let mut paths: Vec<&[u8]> = (unmerged.split(|b| *b == 0))
                .filter_map(|e| Some(&e[e.iter().position(|b| *b == b'\t')? + 1..]))
                .collect();
            paths.dedup();
            if paths.is_empty() {
                return Err(e);
            }
            let input = nul_list(paths.into_iter().map(<[u8]>::to_vec));
            let args = ["update-index", "-z", "--add", "--remove", "--stdin"];
            git::cmd::output_of(&mut git()?, &args, Some(input))?;
            write()?
        }
    };
    Ok(ObjectId::from_hex(
        String::from_utf8_lossy(&out).trim().as_bytes(),
    )?)
}

/// `paths`, each ended by a NUL.
fn nul_list(paths: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
    paths
        .flat_map(|mut p| {
            p.push(0);
            p
        })
        .collect()
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
        PathBuf::from(run_in(dir, &args))
    }

    /// `tree` as `git ls-tree -r` lists it (read from the private
    /// repository).
    fn listing(tree: &ObjectId) -> String {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&private().dir)
            .args(["ls-tree", "-r", &tree.to_string()])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
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
        run_in(&wt, &["add", "st"]);
        let index = index_of(&wt);
        let before = fs::read(&index).unwrap();
        let changed = paths(&[
            "tracked.o",
            "new.o",
            "keep.o",
            "build/x",
            "d",
            "f/c/z",
            "link",
            "exe",
            "st",
        ]);
        let deleted = paths(&["d/b", "d/e/a", "f/c"]);
        let live = private()
            .live("l1", &wt, &index, &changed, &deleted)
            .unwrap();
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
        let expected: String = (expected.iter())
            .map(|(mode, path, data)| format!("{mode} blob {}\t{path}\n", blob(data)))
            .collect();
        assert_eq!(listing(&live.tree), expected);
        assert_eq!(
            fs::read(&index).unwrap(),
            before,
            "the workspace's index untouched"
        );
        let kept = Command::new("git")
            .arg("--git-dir")
            .arg(&private().dir)
            .args([
                "rev-parse",
                "refs/gitfold/live/l1",
                &format!("{}^{{tree}}", live.commit),
            ])
            .output()
            .unwrap();
        let kept = String::from_utf8(kept.stdout).unwrap();
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
        let changed = paths(&[":x", "1:y", "a b"]);
        let live = private()
            .live("l2", &wt, &index_of(&wt), &changed, &[])
            .unwrap();
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
        let index = index_of(&wt);
        let mut git = Command::new("git");
        git.current_dir(&wt)
            .args(["update-index", "--index-info"])
            .stdin(std::process::Stdio::piped());
        let mut child = git.spawn().unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), info.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
        let live = private().live("l3", &wt, &index, &[], &[]).unwrap();
        let expected = format!(
            "100644 blob {}\tm\n100644 blob {}\tn\n",
            blob("base\n"),
            blob("n\n")
        );
        assert_eq!(listing(&live.tree), expected);
    }

    #[test]
    fn changes_from_the_state_ignored_included() {
        let base = commit(
            None,
            &[
                (".gitignore", Some("*.o\n")),
                ("a/x", Some("x\n")),
                ("a/y", Some("y\n")),
                ("b", Some("b\n")),
                ("s", Some("s\n")),
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
        fs::write(state.markers(), "Wa\0").unwrap();
        let s = crate::state::status::unfiltered(&crate::store::Store::default(), &state).unwrap();
        let mut changed = s.changed.clone();
        changed.sort();
        assert_eq!(changed, paths(&["b", "c", "n.o"]), "s is as in the base");
        assert_eq!(s.deleted, paths(&["a/x", "a/y"]));
        assert_eq!(s.ignored, 0);
    }
}
