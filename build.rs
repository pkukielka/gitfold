//! Records which build this is (`GITFOLD_VERSION`): the crate version and,
//! when built from a git checkout of gitfold itself, the commit, with
//! `-dirty.<hash of the uncommitted diff>` if there are uncommitted changes. So
//! a mount still running an older binary can be told apart, also between two
//! builds of uncommitted work.

use std::path::Path;
use std::process::Command;

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let commit = checkout().and_then(|()| git(&["rev-parse", "--short=12", "HEAD"]));
    let version = match commit {
        Some(commit) => format!("{version} ({commit}{})", dirty().unwrap_or_default()),
        None => version.to_string(),
    };
    println!("cargo:rustc-env=GITFOLD_VERSION={version}");
    // What changes the answer. Only existing paths: cargo treats a missing one
    // as always changed, and would rebuild every time (a linked worktree, where
    // `.git` is a file, or no git at all).
    let mut watched = vec!["src".to_string(), "Cargo.toml".into(), "Cargo.lock".into()];
    if commit_known(&version) {
        // HEAD's reflog moves with every commit, amend or reset on the branch.
        for path in ["HEAD", "index", "logs/HEAD"] {
            watched.extend(git(&["rev-parse", "--git-path", path]));
        }
    }
    for path in watched.iter().filter(|p| Path::new(p).exists()) {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn commit_known(version: &str) -> bool {
    version.contains('(')
}

/// `Some` if the crate is the top of a git checkout (not, say, a copy of it
/// inside another repository).
fn checkout() -> Option<()> {
    let top = git(&["rev-parse", "--show-toplevel"])?;
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let same = Path::new(&top).canonicalize().ok()? == Path::new(&manifest).canonicalize().ok()?;
    same.then_some(())
}

/// `-dirty.<hash>` if tracked files have uncommitted changes: a hash of the
/// diff, so builds of different uncommitted work differ.
fn dirty() -> Option<String> {
    // No optional locks: `status` would refresh (rewrite) the index, which
    // is watched, and contend with the user's git commands.
    let status = git(&[
        "--no-optional-locks",
        "status",
        "--porcelain",
        "--untracked-files=no",
    ])?;
    if status.is_empty() {
        return None;
    }
    let diff = Command::new("git")
        .args(["--no-optional-locks", "diff", "HEAD"])
        .output()
        .ok()?;
    // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
    let hash = diff.stdout.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    Some(format!("-dirty.{:08x}", hash >> 32))
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
