//! A workspace's live tree: its files as they are now, uncommitted work
//! included and ignored files left out, as `git add -A` stages them, as a
//! tree in the views' private repository, which `@live` views merge as the
//! workspace's branch.
//!
//! git makes it, each time anew: `git add -A` and `git write-tree` in the
//! workspace (through the mount), on a copy of the workspace's index, with
//! its objects going to the private repository. Every rule of `git add -A`
//! is git's: ignored files, submodules and nested repositories, symlinks,
//! conflicts. git reads every directory and checks every file's attributes
//! (cheap while the kernel holds them; workspaces' are cached long).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use gix::ObjectId;

use super::merge::{Private, oid};
use crate::git;

/// A workspace's live tree, and the commit of it the views merge.
pub struct Live {
    pub tree: ObjectId,
    pub commit: ObjectId,
}

impl Private {
    /// The live tree of the workspace of `branch`: its worktree `dir`
    /// (through the mount), its index `index`. The commit is kept
    /// (`refs/gitfold/live/<workspace id>`) until the next.
    pub fn live(&self, branch: &str, dir: &Path, index: &Path) -> Result<Live> {
        let _writing = self.lock.read();
        let id = crate::state::workspace_id(branch);
        // A copy of its own (two views may ask at once), removed after.
        static COPIES: AtomicU64 = AtomicU64::new(0);
        let n = COPIES.fetch_add(1, Ordering::Relaxed);
        let copy = self.dir.join("live").join(format!("{id}.{n}"));
        std::fs::create_dir_all(copy.parent().expect("in live/"))?;
        let tree = copy_index(index, &copy).and_then(|()| {
            let git = || in_workspace(self, dir, &copy);
            git::cmd::output_of(&mut git()?, &["add", "-A"], None)?;
            let args = ["write-tree", "--missing-ok"];
            oid(&git::cmd::output_of(&mut git()?, &args, None)?)
        });
        // Left if it fails (and a lock a killed git left): the private
        // repository is made anew at each mount.
        let mut lock = copy.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(&copy);
        let _ = std::fs::remove_file(lock);
        let tree = tree?;
        let commit = self.commit(&tree, branch)?;
        // By id: branch names nest (`a`, `a/b`), refs can't.
        self.refs(format!("update refs/gitfold/live/{id} {commit}\n"))?;
        Ok(Live { tree, commit })
    }
}

/// Copies the workspace's index `index` to `copy`, with its modification
/// time: git tells racily clean entries by it (a file changed within the
/// index's timestamp is checked). No index: git starts from an empty one.
fn copy_index(index: &Path, copy: &Path) -> Result<()> {
    let mut from = match std::fs::File::open(index) {
        Ok(file) => file,
        Err(e) if crate::fsutil::not_found(&e) => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", index.display())),
    };
    let mut copying = || -> std::io::Result<()> {
        let modified = from.metadata()?.modified()?;
        let mut to = std::fs::File::create(copy)?;
        std::io::copy(&mut from, &mut to)?;
        to.set_modified(modified)
    };
    copying().with_context(|| format!("copying {} to {}", index.display(), copy.display()))
}

/// git in the workspace at `dir`, on index `index`, its objects going to the
/// private repository of `private`: safe for the repository
/// (`git::cmd::REPOSITORY_SAFE`), without a file system monitor (the copy
/// isn't the workspace's index it knows), and unhashed (nothing reads the
/// copy but these commands).
fn in_workspace(private: &Private, dir: &Path, index: &Path) -> Result<Command> {
    let mut git = git::cmd::in_workspace(dir)?;
    git.args(git::cmd::REPOSITORY_SAFE)
        .args(["-c", "core.fsmonitor=false", "-c", "index.skipHash=true"])
        .env("GIT_INDEX_FILE", index);
    private.to_private(&mut git);
    Ok(git)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::super::test_repo::{commit, private, run_in, worktree};
    use super::*;

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

    /// The live tree of worktree `wt`.
    fn live(name: &str, wt: &Path) -> Live {
        private().live(name, wt, &index_of(wt)).unwrap()
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
        let live = live("l1", &wt);
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
        let live = live("l3", &wt);
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
        let live = live("l5", &wt);
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
        let live = live("l6", &wt);
        assert_eq!(listing(&live.tree), format!("160000 commit {head}\tsub\n"));
    }

    #[test]
    fn no_index_yet() {
        let base = commit(None, &[("a", Some("a\n"))]);
        let wt = worktree("l8", &base);
        fs::remove_file(index_of(&wt)).unwrap();
        fs::write(wt.join("b"), "b\n").unwrap();
        let live = live("l8", &wt);
        let expected = [("100644", "a", "a\n"), ("100644", "b", "b\n")];
        assert_eq!(listing(&live.tree), tree_of(&expected));
    }

    #[test]
    fn each_time_anew() {
        let base = commit(None, &[("a", Some("a\n"))]);
        let wt = worktree("l7", &base);
        fs::write(wt.join("build.log"), "log\n").unwrap();
        let first = live("l7", &wt);
        let expected = [("100644", "a", "a\n"), ("100644", "build.log", "log\n")];
        assert_eq!(listing(&first.tree), tree_of(&expected));
        // Ignored now: gone, as from the workspace's index.
        fs::write(wt.join(".gitignore"), "*.log\n").unwrap();
        let second = live("l7", &wt);
        let expected = [("100644", ".gitignore", "*.log\n"), ("100644", "a", "a\n")];
        assert_eq!(listing(&second.tree), tree_of(&expected));
        // Unless the workspace's index tracks it.
        run_in(&wt, &["add", "-f", "build.log"], None);
        let third = live("l7", &wt);
        let expected = [
            ("100644", ".gitignore", "*.log\n"),
            ("100644", "a", "a\n"),
            ("100644", "build.log", "log\n"),
        ];
        assert_eq!(listing(&third.tree), tree_of(&expected));
    }
}
