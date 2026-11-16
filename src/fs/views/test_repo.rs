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
        let dir = std::env::temp_dir().join(format!("gitfold-views-{}", std::process::id()));
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
/// symlink to T) or deleted (`None`).
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
                let (mode, data) = match content.strip_prefix("link:") {
                    Some(target) => ("120000", target),
                    None => ("100644", *content),
                };
                let oid = run(&["hash-object", "-w", "--stdin"], Some(data.as_bytes()));
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

/// The repository's git directory.
pub fn git_dir() -> PathBuf {
    repo().join(".git")
}

/// Sets `key` to `value` in the repository's config.
pub fn config(key: &str, value: &str) {
    git(repo(), &["config", key, value], None, None);
}

/// The commit `commit` (and its tree).
pub fn tip(commit: &str) -> crate::git::Commit {
    crate::git::resolve(commit).unwrap()
}
