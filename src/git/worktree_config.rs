//! A linked worktree's own git configuration, for layers: excludes for the
//! files other workspaces' layers add, and the push guard against publishing
//! a layers commit. Both are per-worktree config (`<admin>/config.worktree`,
//! with `extensions.worktreeConfig`), so the main worktree and the others are
//! unaffected; the files they name live in the admin directory too.
//!
//! Each setting *replaces* the user's for that worktree (`core.excludesFile`
//! the global ignore file, `core.hooksPath` the hooks), so what gitfold puts
//! there includes or runs the user's.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::fsutil;
use crate::git::{self, cmd};

/// The worktree's excludes file, in its admin directory.
const EXCLUDE_FILE: &str = "gitfold-exclude";
/// Between the user's global ignore patterns and gitfold's in it.
const MARKER: &[u8] = b"# gitfold layers\n";
/// The worktree's hooks directory, in its admin directory.
const HOOKS_DIR: &str = "gitfold-hooks";
/// What the hooks share; not a hook name, so git never runs it.
const HOOKS_LIB: &str = "gitfold-lib.sh";

/// Hooks that run the user's of the same name (`githooks(5)`), except
/// `pre-push` (gitfold's own), `push-to-checkout` (whose mere presence
/// changes what a push into this worktree does) and `fsmonitor-watchman`
/// (run only when `core.fsmonitor` names it).
const SHIMMED_HOOKS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
];

/// Makes the worktree with admin directory `admin` ignore `paths` (relative
/// to its top, as bytes) on top of what it ignores anyway: points its
/// `core.excludesFile` at `<admin>/gitfold-exclude`, written as the user's
/// global ignore file followed by one anchored pattern per path. Replaces
/// the paths a previous call gave.
pub fn set_excludes<P: AsRef<[u8]>>(admin: &Path, paths: &[P]) -> Result<()> {
    let mut ours = Vec::new();
    for path in paths {
        ours.extend_from_slice(&exclude_pattern(path.as_ref())?);
        ours.push(b'\n');
    }
    // The file before the config naming it: git never looks for a missing one.
    write_excludes(admin, &ours)?;
    set(admin, "core.excludesFile", &excludes_path(admin))
}

/// Undoes `set_excludes`: the worktree ignores what the others do again.
pub fn remove_excludes(admin: &Path) -> Result<()> {
    unset(admin, "core.excludesFile")?;
    remove_file(&excludes_path(admin))
}

/// Installs the push guard in the worktree with admin directory `admin`: its
/// `core.hooksPath` becomes `<admin>/gitfold-hooks`, where `pre-push`
/// refuses to push a commit with a `gitfold-layers` header that the remotes
/// don't have, then runs the user's own `pre-push`; every other hook runs
/// the user's (as git would without this). The hooks find the user's when
/// they run, so later changes to them apply.
pub fn install_push_guard(admin: &Path) -> Result<()> {
    let dir = hooks_path(admin);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    install_file(&dir.join(HOOKS_LIB), include_bytes!("hooks/lib.sh"), 0o644)?;
    install_file(
        &dir.join("pre-push"),
        include_bytes!("hooks/pre-push"),
        0o755,
    )?;
    for name in SHIMMED_HOOKS {
        let shim = format!(
            "#!/bin/sh\n\
             # gitfold: runs your own {name} hook (this directory replaces them).\n\
             . \"${{0%/*}}/{HOOKS_LIB}\"\n\
             gitfold_user_hook {name} \"$@\"\n"
        );
        install_file(&dir.join(name), shim.as_bytes(), 0o755)?;
    }
    set(admin, "core.hooksPath", &dir)
}

/// Undoes `install_push_guard`: the user's hooks run directly again.
pub fn remove_push_guard(admin: &Path) -> Result<()> {
    unset(admin, "core.hooksPath")?;
    let dir = hooks_path(admin);
    match fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

fn excludes_path(admin: &Path) -> PathBuf {
    admin.join(EXCLUDE_FILE)
}

fn hooks_path(admin: &Path) -> PathBuf {
    admin.join(HOOKS_DIR)
}

/// Writes the excludes file: the user's global ignore file, the marker, then
/// `ours`.
fn write_excludes(admin: &Path, ours: &[u8]) -> Result<()> {
    let mut text = user_excludes(admin)?;
    text.extend_from_slice(MARKER);
    text.extend_from_slice(ours);
    let path = excludes_path(admin);
    fsutil::write_atomic(&path, &text).with_context(|| format!("writing {}", path.display()))
}

/// The content of the user's global ignore file, ending in a newline unless
/// empty (none, or missing, is empty, as for git).
fn user_excludes(admin: &Path) -> Result<Vec<u8>> {
    let Some(path) = git::user_excludes_file(admin)? else {
        return Ok(Vec::new());
    };
    let mut text = match fs::read(&path) {
        Ok(text) => text,
        Err(e) if fsutil::not_found(&e) => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if !text.is_empty() && !text.ends_with(b"\n") {
        text.push(b'\n');
    }
    Ok(text)
}

/// The gitignore pattern matching exactly `path` (relative to the top of the
/// worktree): anchored, with glob characters escaped, and the spaces git
/// would trim or that could be taken for the pattern's start.
fn exclude_pattern(path: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !path.is_empty() && !path.contains(&b'\n'),
        "can't exclude {:?}: not expressible as a gitignore pattern",
        String::from_utf8_lossy(path)
    );
    let end = path.len() - path.iter().rev().take_while(|b| **b == b' ').count();
    let start = path.iter().take_while(|b| **b == b' ').count();
    let mut pattern = vec![b'/'];
    for (i, &b) in path.iter().enumerate() {
        let space = b == b' ' && (i < start || i >= end);
        if space || matches!(b, b'*' | b'?' | b'[' | b'\\' | b'!' | b'#') {
            pattern.push(b'\\');
        }
        pattern.push(b);
    }
    Ok(pattern)
}

/// Writes `data` to `path` with `mode`, unless it already holds that: a hook
/// rewritten in place would be missing or not executable for a moment.
fn install_file(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let same = fs::read(path).is_ok_and(|old| old == data)
        && fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == mode);
    if same {
        return Ok(());
    }
    let tmp = path.with_extension("gitfold-tmp");
    fs::write(&tmp, data)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)))
        .and_then(|()| fs::rename(&tmp, path))
        .with_context(|| format!("writing {}", path.display()))
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Sets `key` to `value` in the worktree's own config, enabling per-worktree
/// config first if needed.
fn set(admin: &Path, key: &str, value: &Path) -> Result<()> {
    enable_worktree_config(admin)?;
    let out = cmd::command(admin)
        .args(["config", "--worktree", key])
        .arg(value)
        .output()?;
    check(&out, admin, key)
}

/// Removes `key` from the worktree's own config, if there.
fn unset(admin: &Path, key: &str) -> Result<()> {
    // Without the extension, `--worktree` means the shared config, which
    // gitfold never set this in.
    if !worktree_config_enabled(admin)? {
        return Ok(());
    }
    let out = cmd::command(admin)
        .args(["config", "--worktree", "--unset-all", key])
        .output()?;
    // 5: it wasn't set.
    match out.status.code() {
        Some(5) => Ok(()),
        _ => check(&out, admin, key),
    }
}

/// Turns on `extensions.worktreeConfig` (in the shared config), so worktrees
/// can have config of their own. That alone changes nothing for any of them.
/// `core.repositoryformatversion` stays as it is: git honours this extension
/// at version 0 too, and version 1 would make every git reading the
/// repository refuse extensions it doesn't know.
fn enable_worktree_config(admin: &Path) -> Result<()> {
    if worktree_config_enabled(admin)? {
        return Ok(());
    }
    let out = cmd::command(admin)
        .args(["config", "--local", "extensions.worktreeConfig", "true"])
        .output()?;
    check(&out, admin, "extensions.worktreeConfig")
}

fn worktree_config_enabled(admin: &Path) -> Result<bool> {
    let out = cmd::command(admin)
        .args(["config", "--local", "--type=bool", "--get"])
        .arg("extensions.worktreeConfig")
        .output()?;
    match out.status.code() {
        Some(0) => Ok(out.stdout == b"true\n"),
        Some(1) => Ok(false),
        _ => check(&out, admin, "extensions.worktreeConfig").map(|()| false),
    }
}

fn check(out: &std::process::Output, admin: &Path, key: &str) -> Result<()> {
    if !out.status.success() {
        bail!(
            "git config {key} in {}: {}",
            admin.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Output, Stdio};

    /// A repository `r` with one commit, a bare `remote.git` as its `origin`
    /// and a linked worktree `w`, in a fresh directory. Returns the
    /// directory and `w`'s admin directory.
    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("gitfold-wtconfig-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "--bare", "remote.git"]);
        git(&dir, &["init", "-q", "-b", "main", "r"]);
        let r = dir.join("r");
        git(&r, &["config", "user.name", "T"]);
        git(&r, &["config", "user.email", "t@example.com"]);
        git(&r, &["remote", "add", "origin", "../remote.git"]);
        fs::write(r.join("a.txt"), "a\n").unwrap();
        git(&r, &["add", "a.txt"]);
        git(&r, &["commit", "-q", "-m", "a"]);
        git(&r, &["worktree", "add", "-q", "--detach", "../w"]);
        let admin = r.join(".git/worktrees/w").canonicalize().unwrap();
        (dir, admin)
    }

    /// Git in `dir`, with no system or global config (the hooks it runs see
    /// that too); the test fails if git does.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = try_git(dir, args, b"");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn try_git(dir: &Path, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait_with_output().unwrap()
    }

    fn untracked(dir: &Path) -> String {
        git(dir, &["status", "--porcelain", "--untracked-files=all"])
    }

    /// Whether gitoxide, as gitfold's status uses it (`state/status.rs`),
    /// ignores `path` in the worktree with admin directory `admin`.
    fn gix_ignores(common: &Path, admin: &Path, path: &str) -> bool {
        let mut buf = Vec::new();
        let search = gix::ignore::Search::from_git_dir(
            common,
            git::excludes_file(admin).unwrap(),
            &mut buf,
            Default::default(),
        )
        .unwrap();
        search
            .pattern_matching_relative_path(
                path.into(),
                Some(false),
                gix::glob::pattern::Case::Sensitive,
            )
            .is_some()
    }

    /// Patterns match exactly their path, whatever characters it has, for
    /// git and for gitoxide.
    #[test]
    fn exclude_patterns_escape() {
        assert_eq!(
            exclude_pattern(b"src/new_thing.rs").unwrap(),
            b"/src/new_thing.rs"
        );
        assert_eq!(
            exclude_pattern(b" a*?[b]\\!#c  ").unwrap(),
            b"/\\ a\\*\\?\\[b]\\\\\\!\\#c\\ \\ "
        );
        assert!(exclude_pattern(b"a\nb").is_err());
        let (dir, admin) = fixture("escape");
        let (common, w) = (dir.join("r/.git"), dir.join("w"));
        let odd = [" a*b ", "#c", "!d", "e?[f]", "g\\h"];
        let plain = ["xab", "c", "d", "ef", " a*b"];
        for name in odd.iter().chain(&plain) {
            fs::write(w.join(name), "").unwrap();
        }
        set_excludes(&admin, &odd).unwrap();
        let shown = untracked(&w);
        let mut shown: Vec<&str> = shown.lines().collect();
        shown.sort();
        assert_eq!(shown, ["?? \" a*b\"", "?? c", "?? d", "?? ef", "?? xab"]);
        for name in odd {
            assert!(gix_ignores(&common, &admin, name), "{name:?}");
        }
        for name in plain {
            assert!(!gix_ignores(&common, &admin, name), "{name:?}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The worktree's own excludes keep the user's global ignore file (E2 in
    /// the layers design: an override alone dropped it), for git and for
    /// gitfold's status, follow changes to it, and go away without a trace;
    /// the main worktree never sees them.
    #[test]
    fn excludes_keep_global_ignore() {
        let (dir, admin) = fixture("excludes");
        let (r, w, common) = (dir.join("r"), dir.join("w"), dir.join("r/.git"));
        let global = dir.join("global-ignore");
        fs::write(&global, "*.globalign").unwrap();
        git(
            &r,
            &["config", "core.excludesFile", global.to_str().unwrap()],
        );
        for d in [&r, &w] {
            for name in ["x.globalign", "t.txt", "y.later"] {
                fs::write(d.join(name), "").unwrap();
            }
        }
        assert_eq!(untracked(&w), "?? t.txt\n?? y.later\n");

        set_excludes(&admin, &["t.txt"]).unwrap();
        assert_eq!(untracked(&w), "?? y.later\n");
        assert_eq!(untracked(&r), "?? t.txt\n?? y.later\n");
        assert_eq!(git(&r, &["config", "core.repositoryformatversion"]), "0\n");
        assert_eq!(
            git::excludes_file(&admin).unwrap(),
            Some(admin.join(EXCLUDE_FILE))
        );
        for (path, ignored) in [("x.globalign", true), ("t.txt", true), ("y.later", false)] {
            assert_eq!(gix_ignores(&common, &admin, path), ignored, "{path}");
        }

        // Another call replaces the paths, and takes the user's file as it
        // is now.
        fs::write(&global, "*.globalign\n*.later\n").unwrap();
        set_excludes(&admin, &["nothing"]).unwrap();
        assert_eq!(untracked(&w), "?? t.txt\n");

        remove_excludes(&admin).unwrap();
        remove_excludes(&admin).unwrap();
        assert_eq!(untracked(&w), "?? t.txt\n");
        assert!(!admin.join(EXCLUDE_FILE).exists());
        assert_eq!(git::excludes_file(&admin).unwrap(), Some(global));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Writes a commit on top of HEAD, with HEAD's tree and a
    /// `gitfold-layers` header; returns its id.
    fn layers_commit(w: &Path) -> String {
        let tree = git(w, &["rev-parse", "HEAD^{tree}"]);
        let parent = git(w, &["rev-parse", "HEAD"]);
        let text = format!(
            "tree {}\nparent {}\nauthor T <t@example.com> 0 +0000\n\
             committer T <t@example.com> 0 +0000\ngitfold-layers 1\n\nlayers\n",
            tree.trim(),
            parent.trim()
        );
        let args = ["hash-object", "-t", "commit", "-w", "--stdin"];
        let out = try_git(w, &args, text.as_bytes());
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn hook(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The push guard refuses a layers commit (and what is on top of it), lets
    /// other pushes through, and runs the user's own hooks; the main worktree
    /// has none of it, and removing it restores the worktree.
    #[test]
    fn push_guard() {
        let (dir, admin) = fixture("push");
        let (r, w) = (dir.join("r"), dir.join("w"));
        let ran = dir.join("pre-commit-ran");
        let got = dir.join("pre-push-got");
        let user_hooks = r.join(".git/hooks");
        let pre_commit = format!("echo x >> '{}'", ran.display());
        hook(&user_hooks, "pre-commit", &pre_commit);
        let pre_push = format!("cat > '{0}'; echo \"$1\" >> '{0}'", got.display());
        hook(&user_hooks, "pre-push", &pre_push);

        install_push_guard(&admin).unwrap();
        install_push_guard(&admin).unwrap();
        let unset =
            |d: &Path| try_git(d, &["config", "core.hooksPath"], b"").status.code() == Some(1);
        assert!(unset(&r));

        // The user's pre-commit runs through the shim.
        git(&w, &["commit", "-q", "--allow-empty", "-m", "mine"]);
        assert_eq!(fs::read_to_string(&ran).unwrap(), "x\n");

        let layers = layers_commit(&w);
        git(&w, &["checkout", "-q", "--detach", &layers]);
        git(&w, &["commit", "-q", "--allow-empty", "-m", "on layers"]);
        let out = try_git(&w, &["push", "origin", "HEAD:refs/heads/main"], b"");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{stderr}");
        let refusal = format!(
            "gitfold: refusing to push refs/heads/main: it contains the layers commit {layers}"
        );
        assert!(stderr.contains(&refusal), "{stderr}");
        assert!(!got.exists(), "the user's pre-push ran after a refusal");

        // A push without the layers commit passes, through the user's pre-push.
        git(
            &w,
            &[
                "push",
                "-q",
                "origin",
                &format!("{layers}~1:refs/heads/main"),
            ],
        );
        let pushed = fs::read_to_string(&got).unwrap();
        assert!(pushed.contains(" refs/heads/main 0000000"), "{pushed}");
        assert!(pushed.ends_with("\norigin\n"), "{pushed}");
        // So does a deletion.
        git(&w, &["push", "-q", "origin", ":refs/heads/main"]);

        // The main worktree runs the user's hooks itself, and has no guard.
        git(&r, &["commit", "-q", "--allow-empty", "-m", "main"]);
        assert_eq!(fs::read_to_string(&ran).unwrap(), "x\nx\nx\n");
        git(
            &r,
            &["push", "-q", "origin", &format!("{layers}:refs/heads/main")],
        );

        remove_push_guard(&admin).unwrap();
        remove_push_guard(&admin).unwrap();
        assert!(!admin.join(HOOKS_DIR).exists());
        assert!(unset(&w));
        git(&w, &["commit", "-q", "--allow-empty", "-m", "after"]);
        assert_eq!(fs::read_to_string(&ran).unwrap(), "x\nx\nx\nx\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The shims find the user's hooks through the user's core.hooksPath
    /// (any scope but the worktree's), which removal leaves as it was.
    #[test]
    fn push_guard_user_hooks_path() {
        let (dir, admin) = fixture("hookspath");
        let (r, w) = (dir.join("r"), dir.join("w"));
        let user_hooks = dir.join("my-hooks");
        hook(&user_hooks, "pre-commit", "echo no-commit >&2; exit 1");
        git(
            &r,
            &["config", "core.hooksPath", user_hooks.to_str().unwrap()],
        );
        install_push_guard(&admin).unwrap();
        let out = try_git(&w, &["commit", "--allow-empty", "-m", "x"], b"");
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("no-commit"));
        remove_push_guard(&admin).unwrap();
        let path = git(&w, &["config", "core.hooksPath"]);
        assert_eq!(path.trim(), user_hooks.to_str().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}
