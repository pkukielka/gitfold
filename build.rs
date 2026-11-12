//! Records which build this is.
//!
//! - `GITFOLD_VERSION`: the crate version and a hash of the build's inputs
//!   (everything under `src/`, `Cargo.toml`, `Cargo.lock`, `build.rs`), read
//!   from disk. It is what `gitfold version` compares with a mount's daemon:
//!   the same sources give the same value however they were committed (so a
//!   docs-only commit asks for no remount), and any change to them, committed
//!   or not, tracked or not, a different one.
//! - `GITFOLD_LONG_VERSION`: that and, for people (`--version`), the git
//!   commit built from, when built from a checkout of gitfold itself (`-dirty`
//!   with uncommitted changes to tracked files).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const INPUTS: [&str; 4] = ["src", "Cargo.toml", "Cargo.lock", "build.rs"];

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let mut hash = Fnv::default();
    for input in INPUTS {
        hash_path(&dir, Path::new(input), &mut hash);
    }
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "cargo:rustc-env=GITFOLD_VERSION={version} (sources {:012x})",
        hash.0 >> 16
    );
    // Only existing paths: cargo treats a missing one as always changed, and
    // would rebuild every time.
    for input in INPUTS.iter().filter(|p| dir.join(p).exists()) {
        println!("cargo:rerun-if-changed={input}");
    }
    let commit = commit(&dir);
    if commit.is_some() {
        // HEAD's reflog moves with every commit, amend or reset.
        let reflog = git(
            &dir,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "logs/HEAD",
            ],
        );
        for path in reflog.iter().filter(|p| Path::new(p).exists()) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let sources = format!("{version} (sources {:012x})", hash.0 >> 16);
    let long = match commit {
        Some(commit) => format!("{sources}, commit {commit}"),
        None => sources,
    };
    println!("cargo:rustc-env=GITFOLD_LONG_VERSION={long}");
}

/// Hashes the path and content of `path` (below `dir`), of every file below
/// it if it is a directory, in a fixed order.
fn hash_path(dir: &Path, path: &Path, hash: &mut Fnv) {
    let full = dir.join(path);
    let Ok(meta) = fs::metadata(&full) else {
        return;
    };
    hash.write(path.to_string_lossy().as_bytes());
    hash.write(&[0]);
    if meta.is_dir() {
        let mut names: Vec<_> = fs::read_dir(&full)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        for name in names {
            hash_path(dir, &path.join(name), hash);
        }
    } else {
        hash.write(&fs::read(&full).unwrap_or_default());
        hash.write(&[0]);
    }
}

/// FNV-1a, 64 bits: stable across Rust versions, unlike `DefaultHasher`.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf29ce484222325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x100000001b3);
        }
    }
}

/// The commit `dir` is checked out at, if it is the top of a git checkout
/// (not, say, a copy inside another repository), `-dirty` with uncommitted
/// changes to tracked files.
fn commit(dir: &Path) -> Option<String> {
    let top = git(dir, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&top).canonicalize().ok()? != dir.canonicalize().ok()? {
        return None;
    }
    let commit = git(dir, &["rev-parse", "--short=12", "HEAD"])?;
    // No optional locks: `status` would otherwise refresh (rewrite) the index
    // and contend with the user's git commands.
    let status = git(
        dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ],
    )?;
    Some(match status.is_empty() {
        true => commit,
        false => format!("{commit}-dirty"),
    })
}

/// Runs git in `dir`, ignoring the environment's `GIT_DIR` and friends (a
/// build started from inside another repository's git hook, say).
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
