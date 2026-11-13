//! The task folder: a directory every workspace links to (`.task` by
//! default) for what belongs to the task but not to its commits (a spec,
//! drafts, handoffs, notes, messages, a PR description), kept after the
//! workspace, and its branch, are gone.
//!
//! Its files live in `.git/gitfold/.tasks/<id>/` (or `<gitfold.tasksDir>/<id>/`),
//! which `remove` never touches; the workspace root shows a virtual symlink
//! to it, as it shows the virtual `.git` file. So the files are plain files
//! outside the mount: written at disk speed, seen at once whoever writes them
//! (the kernel's caching of the workspace doesn't apply), watched with inotify
//! like any directory. Git doesn't follow symlinks, so it never sees or cleans
//! what is inside (`git clean -fdx` fails to delete the link and leaves it),
//! and an `info/exclude` entry every worktree shares keeps the link itself out
//! of `git status`. The name is `gitfold.taskDir` in the repository's config;
//! a workspace whose base tracks a path of that name, or that has one among
//! its changes, has no task folder.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::warn;

use crate::git::worktree::{self, workspace_id};

/// The folder's name unless `gitfold.taskDir` says otherwise.
pub const DEFAULT_NAME: &str = ".task";

/// The file whose first line `gitfold list` shows.
pub const STATUS: &str = "status";

/// Where a workspace's task folder is: its name in the workspace root, and the
/// directory holding its files.
#[derive(Clone)]
pub struct TaskDir {
    pub name: Vec<u8>,
    pub dir: PathBuf,
}

impl TaskDir {
    /// The task folder of `branch` in repository `common_dir`, as configured;
    /// a setting that can't be used is warned about and the default used.
    pub fn of(common_dir: &Path, branch: &str) -> TaskDir {
        let config = config(common_dir);
        let get = |key: &str| {
            config
                .as_ref()
                .and_then(|c| c.string_by("gitfold", None, key))
                .map(|v| v.to_string().trim().to_string())
                .filter(|v| !v.is_empty())
        };
        // One name in the workspace root, not one of its special names.
        let name = match get("taskDir") {
            Some(name) if !name.contains('/') && ![".", "..", ".git"].contains(&name.as_str()) => {
                name
            }
            Some(name) => {
                warn!("gitfold.taskDir {name:?} isn't a single name: using {DEFAULT_NAME}");
                DEFAULT_NAME.to_string()
            }
            None => DEFAULT_NAME.to_string(),
        };
        let default = || worktree::state_root(common_dir).join(".tasks");
        // Relative paths would depend on each process's current directory.
        let root = match get("tasksDir").map(|d| expand_home(&d)) {
            Some(dir) if dir.is_absolute() => dir,
            Some(dir) => {
                warn!(
                    "gitfold.tasksDir {} isn't absolute: using the default",
                    dir.display()
                );
                default()
            }
            None => default(),
        };
        TaskDir {
            name: name.into_bytes(),
            dir: root.join(workspace_id(branch)),
        }
    }

    /// The first line of the folder's `status` file, if any.
    pub fn status(&self) -> Option<String> {
        let text = fs::read_to_string(self.dir.join(STATUS)).ok()?;
        let line = text.lines().next()?.trim();
        (!line.is_empty()).then(|| line.to_string())
    }

    /// The newest modification time below the folder (its files and
    /// directories; not the folder itself, made when the workspace opens).
    pub fn newest(&self) -> Option<std::time::SystemTime> {
        fn walk(dir: &Path, newest: &mut Option<std::time::SystemTime>) {
            let Ok(meta) = fs::symlink_metadata(dir) else {
                return;
            };
            *newest = (*newest).max(meta.modified().ok());
            if meta.is_dir()
                && let Ok(entries) = fs::read_dir(dir)
            {
                for entry in entries.flatten() {
                    walk(&entry.path(), newest);
                }
            }
        }
        let mut newest = None;
        for entry in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            walk(&entry.path(), &mut newest);
        }
        newest
    }

    /// Creates the folder's directory and keeps git from seeing the link.
    pub fn prepare(&self, common_dir: &Path) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        exclude(common_dir, &self.name)
    }

    /// Copies the folder of another branch into this one's (`branch -w`), unless
    /// this one has a folder already (a branch of the same name had one).
    /// Returns whether it copied.
    /// Its inbox (`inbox`, not copied: messages are for the source's agent)
    /// and names starting with a dot (files being written or taken) stay
    /// behind.
    pub fn copy_from(&self, from: &TaskDir, inbox: &str) -> Result<bool> {
        if self.dir.exists() || !from.dir.exists() {
            return Ok(false);
        }
        let parent = self.dir.parent().context("task folder")?;
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".copy-{}",
            self.dir.file_name().unwrap_or_default().to_string_lossy()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let copied =
            copy_tree(&from.dir, &tmp, Some(inbox)).and_then(|()| Ok(fs::rename(&tmp, &self.dir)?));
        if copied.is_err() {
            let _ = fs::remove_dir_all(&tmp);
        }
        copied.map(|()| true)
    }
}

/// The name of a task folder's inbox: `gitfold.inbox` if it is one plain name
/// (not starting with a dot), else `inbox`.
pub fn inbox_name(common_dir: &Path) -> String {
    let name = config(common_dir)
        .and_then(|c| c.string_by("gitfold", None, "inbox").map(|v| v.to_string()))
        .map(|v| v.trim().to_string());
    match name {
        Some(name) if !name.is_empty() && !name.contains('/') && !name.starts_with('.') => name,
        Some(name) => {
            warn!("gitfold.inbox {name:?} isn't a plain name: using {DEFAULT_INBOX}");
            DEFAULT_INBOX.to_string()
        }
        None => DEFAULT_INBOX.to_string(),
    }
}

/// The inbox's name unless `gitfold.inbox` says otherwise.
const DEFAULT_INBOX: &str = "inbox";

/// The repository's own config file (where `git config` writes), read now.
fn config(common_dir: &Path) -> Option<gix::config::File> {
    let path = common_dir.join("config");
    gix::config::File::from_path_no_includes(path, gix::config::Source::Local).ok()
}

fn expand_home(dir: &str) -> PathBuf {
    let home = || std::env::home_dir().unwrap_or_default();
    match dir.strip_prefix("~") {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => home().join(&rest[1..]),
        _ => PathBuf::from(dir),
    }
}

/// Adds `/<name>` to `info/exclude` (shared by every worktree) if missing, in
/// one append: no temporary file for two processes to share.
fn exclude(common_dir: &Path, name: &[u8]) -> Result<()> {
    let path = common_dir.join("info").join("exclude");
    let line = [b"/", name].concat();
    let text = match fs::read(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    if text.split(|b| *b == b'\n').any(|l| l == line.as_slice()) {
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("info")?)?;
    let mut add = Vec::new();
    if !text.is_empty() && !text.ends_with(b"\n") {
        add.push(b'\n');
    }
    add.extend_from_slice(b"# gitfold's task folder link (see `gitfold.taskDir`)\n");
    add.extend_from_slice(&line);
    add.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(&add)?;
    Ok(())
}

/// Copies directory `src` to new directory `dst`, files, symlinks and
/// directories, with their modes.
/// Leaves out names starting with a dot, and `skip` in `src` itself.
fn copy_tree(src: &Path, dst: &Path, skip: Option<&str>) -> Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_bytes().starts_with(b".") || skip.is_some_and(|s| name == s) {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(&name));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&from, &to, None)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
        } else {
            fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    fs::set_permissions(dst, fs::metadata(src)?.permissions())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch git directory holding `config` (a git config file's text).
    fn git_dir(name: &str, config: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gitfold-task-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config"), config).unwrap();
        dir
    }

    #[test]
    fn defaults() {
        let dir = git_dir("defaults", "");
        let t = TaskDir::of(&dir, "agent/x");
        assert_eq!(t.name, b".task");
        assert_eq!(t.dir, dir.join("gitfold/.tasks/agent%2Fx"));
        assert_eq!(inbox_name(&dir), "inbox");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings() {
        let dir = git_dir(
            "settings",
            "[gitfold]\n\ttaskDir = .notes\n\ttasksDir = /srv/tasks\n\tinbox = mail\n",
        );
        let t = TaskDir::of(&dir, "b");
        assert_eq!(
            (t.name.as_slice(), t.dir.as_path()),
            (&b".notes"[..], Path::new("/srv/tasks/b"))
        );
        assert_eq!(inbox_name(&dir), "mail");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_settings_fall_back() {
        for (name, value) in [
            ("slash", "a/b"),
            ("dot", "."),
            ("dotdot", ".."),
            ("git", ".git"),
        ] {
            let dir = git_dir(
                name,
                &format!(
                    "[gitfold]\n\ttaskDir = {value}\n\ttasksDir = rel/dir\n\tinbox = .hidden\n"
                ),
            );
            let t = TaskDir::of(&dir, "b");
            assert_eq!(t.name, b".task", "{value}");
            assert_eq!(t.dir, dir.join("gitfold/.tasks/b"), "a relative tasksDir");
            assert_eq!(inbox_name(&dir), "inbox", "a dot name");
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn copy_leaves_inbox_and_dot_files() {
        let root = std::env::temp_dir().join(format!("gitfold-task-{}-copy", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (from, to) = (root.join("from"), root.join("to"));
        for p in ["notes", "inbox"] {
            fs::create_dir_all(from.join(p)).unwrap();
        }
        for (p, text) in [
            ("spec.md", "s"),
            ("notes/a", "n"),
            ("inbox/1-x", "m"),
            (".waiting", ""),
            ("notes/.new-1", ""),
        ] {
            fs::write(from.join(p), text).unwrap();
        }
        let src = TaskDir {
            name: b".task".to_vec(),
            dir: from,
        };
        let dst = TaskDir {
            name: b".task".to_vec(),
            dir: to.clone(),
        };
        assert!(dst.copy_from(&src, "inbox").unwrap());
        let mut got: Vec<_> = walk(&to);
        got.sort();
        assert_eq!(got, ["notes", "notes/a", "spec.md"]);
        assert!(
            !dst.copy_from(&src, "inbox").unwrap(),
            "an existing folder is kept"
        );
        fs::remove_dir_all(root).unwrap();

        fn walk(dir: &Path) -> Vec<String> {
            let mut out = Vec::new();
            for e in fs::read_dir(dir).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().unwrap().is_dir() {
                    out.extend(walk(&e.path()).into_iter().map(|p| format!("{name}/{p}")));
                }
                out.push(name);
            }
            out
        }
    }
}
