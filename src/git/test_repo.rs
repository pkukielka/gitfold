//! A scratch repository for unit tests, driven with the git CLI in an
//! isolated configuration (`layers` and `stack` tests).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gix::ObjectId;

pub struct Repo {
    pub dir: PathBuf,
    /// Commits get one fixed date, so the same commit made twice (by gitfold
    /// and by `git rebase`) has the same id.
    fixed_dates: bool,
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Repo {
    /// A new repository with branch `main`, in a directory named after the
    /// test (`name`, unique across the tests of one run).
    pub fn new(name: &str) -> Repo {
        Repo::create(name, false)
    }

    /// `new`, every commit with the same date.
    pub fn with_fixed_dates(name: &str) -> Repo {
        Repo::create(name, true)
    }

    fn create(name: &str, fixed_dates: bool) -> Repo {
        let dir = std::env::temp_dir().join(format!("gitfold-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let repo = Repo { dir, fixed_dates };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    pub fn work(&self) -> PathBuf {
        self.dir.join("repo")
    }

    /// The git common directory.
    pub fn common(&self) -> PathBuf {
        self.work().join(".git")
    }

    pub fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.dir)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Agent")
            .env("GIT_AUTHOR_EMAIL", "agent@example.com")
            .env("GIT_COMMITTER_NAME", "Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com");
        if self.fixed_dates {
            let date = "2026-01-01T00:00:00Z";
            command
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date);
        }
        command.output().unwrap()
    }

    /// Runs git in `dir`, which must succeed; its output, trimmed.
    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run_in(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .trim_end()
            .to_string()
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.work(), args)
    }

    pub fn id(&self, spec: &str) -> ObjectId {
        ObjectId::from_hex(self.git(&["rev-parse", spec]).as_bytes()).unwrap()
    }

    pub fn write(&self, path: &str, content: &str) {
        let path = self.work().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Commits everything on the current branch; the commit.
    pub fn commit(&self, message: &str) -> ObjectId {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.id("HEAD")
    }

    pub fn open(&self) -> gix::Repository {
        gix::open_opts(self.work(), gix::open::Options::isolated()).unwrap()
    }

    /// `git cat-file -p <object>:<path>`, trimmed.
    pub fn show(&self, object: ObjectId, path: &str) -> String {
        self.git(&["cat-file", "-p", &format!("{object}:{path}")])
    }

    /// `git diff --name-status` between two trees or commits, renames not
    /// followed.
    pub fn diff(&self, from: ObjectId, to: ObjectId) -> String {
        let (from, to) = (from.to_string(), to.to_string());
        self.git(&["diff", "--no-renames", "--name-status", &from, &to])
    }

    /// `git merge-tree --write-tree` of three trees or commits: the tree, or
    /// `None` on conflicts.
    pub fn merge_tree(&self, base: ObjectId, ours: ObjectId, theirs: ObjectId) -> Option<ObjectId> {
        let base = format!("--merge-base={base}");
        let (ours, theirs) = (ours.to_string(), theirs.to_string());
        let args = ["merge-tree", "--write-tree", &base, &ours, &theirs];
        let out = self.run_in(&self.work(), &args);
        let first = String::from_utf8(out.stdout).unwrap();
        let first = first.lines().next().unwrap_or_default().to_string();
        match out.status.code() {
            Some(0) => Some(ObjectId::from_hex(first.as_bytes()).unwrap()),
            Some(1) => None,
            _ => panic!("merge-tree: {}", String::from_utf8_lossy(&out.stderr)),
        }
    }
}
