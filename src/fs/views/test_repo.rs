//! A git repository for the views' tests: one per test process (`git::init`
//! opens one), shared by the tests, each using its own branch names. Commits
//! are written with git's plumbing and a private index each, so tests run in
//! parallel.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

static REPO: OnceLock<PathBuf> = OnceLock::new();
static INDEXES: AtomicUsize = AtomicUsize::new(0);

fn repo() -> &'static Path {
    REPO.get_or_init(|| {
        let tmp = std::env::temp_dir();
        // Nothing removes a test process's repository when it ends: the next
        // run removes those of processes gone.
        for entry in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let pid = name.to_str().and_then(|n| n.strip_prefix("gitfold-views-"));
            if pid.is_some_and(|pid| !Path::new("/proc").join(pid).exists()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let dir = tmp.join(format!("gitfold-views-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"], None, None);
        crate::git::init(&dir).unwrap();
        dir
    })
}

/// Runs git in `dir` (with index `index`, input `stdin`); its output.
fn git(dir: &Path, args: &[&str], index: Option<&Path>, stdin: Option<&[u8]>) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    for (var, value) in [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@t"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@t"),
        // Not the user's settings (`commit.gpgSign` would sign, or fail).
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ] {
        cmd.env(var, value);
    }
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write;
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.unwrap_or_default()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A commit on `parent` (none: a root commit) with `files` set (`link:T`: a
/// symlink to T; `exec:D`: an executable file; `gitlink:C`: a submodule at
/// commit C) or deleted (`None`).
pub fn commit(parent: Option<&str>, files: &[(&str, Option<&str>)]) -> String {
    let dir = repo();
    let n = INDEXES.fetch_add(1, Ordering::Relaxed);
    let index = dir.join(format!("test-index-{n}"));
    let run = |args: &[&str], stdin: Option<&[u8]>| git(dir, args, Some(&index), stdin);
    if let Some(parent) = parent {
        run(&["read-tree", parent], None);
    }
    for (path, content) in files {
        match content {
            Some(content) => {
                let hash =
                    |data: &str| run(&["hash-object", "-w", "--stdin"], Some(data.as_bytes()));
                let (mode, oid) = if let Some(target) = content.strip_prefix("link:") {
                    ("120000", hash(target))
                } else if let Some(data) = content.strip_prefix("exec:") {
                    ("100755", hash(data))
                } else if let Some(commit) = content.strip_prefix("gitlink:") {
                    ("160000", commit.to_string())
                } else {
                    ("100644", hash(content))
                };
                run(
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("{mode},{oid},{path}"),
                    ],
                    None,
                );
            }
            None => {
                run(&["update-index", "--force-remove", path], None);
            }
        }
    }
    let tree = run(&["write-tree"], None);
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "test"];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    let commit = run(&args, None);
    let _ = std::fs::remove_file(&index);
    commit
}

/// Points branch `name` at `commit`; returns `commit`.
pub fn branch(name: &str, commit: &str) -> String {
    git(
        repo(),
        &["update-ref", &format!("refs/heads/{name}"), commit],
        None,
        None,
    );
    commit.to_string()
}

/// Points remote-tracking branch `name` (`origin/x`) at `commit`.
pub fn remote_branch(name: &str, commit: &str) {
    git(
        repo(),
        &["update-ref", &format!("refs/remotes/{name}"), commit],
        None,
        None,
    );
}

/// Writes ref `full` as a file pointing at `oid`, which git's commands
/// would refuse if the object is missing.
pub fn write_ref(full: &str, oid: &str) {
    let path = git_dir().join(full);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("{oid}\n")).unwrap();
}

/// The repository's git directory.
pub fn git_dir() -> PathBuf {
    repo().join(".git")
}

/// The views' private repository of the test repository (one per process,
/// as `git::open_private` allows).
pub fn private() -> &'static super::merge::Private {
    static PRIVATE: OnceLock<super::merge::Private> = OnceLock::new();
    PRIVATE.get_or_init(|| super::merge::Private::create(&git_dir()).unwrap())
}

/// Runs git in the test repository; its output.
pub fn run(args: &[&str]) -> String {
    git(repo(), args, None, None)
}

/// The commit `commit` (and its tree).
pub fn tip(commit: &str) -> crate::git::Commit {
    crate::git::resolve(commit).unwrap()
}

/// Runs git in `dir` (a worktree of the test repository, with input
/// `stdin`); its output.
pub fn run_in(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    git(dir, args, None, stdin)
}

/// A new worktree of the test repository at `commit` (detached), named
/// `name`; its directory.
pub fn worktree(name: &str, commit: &str) -> PathBuf {
    let dir = repo().join("worktrees").join(name);
    let path = dir.to_str().unwrap();
    git(
        repo(),
        &["worktree", "add", "-q", "--detach", path, commit],
        None,
        None,
    );
    dir
}
