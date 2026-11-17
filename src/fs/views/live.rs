//! A workspace's live tree: its files as they are now, uncommitted work
//! included and ignored files left out, as `git add -A` stages them, as a
//! tree in the views' private repository, which `@live` views merge as the
//! workspace's branch.
//!
//! git makes it with `git add -A` and `git write-tree`, in the workspace
//! (through the mount), on a private index of its own (a copy of the
//! workspace's, made again when that one changes), with its objects going to
//! the private repository. So every rule of `git add -A` is git's: ignored
//! files, submodules and nested repositories, symlinks, conflicts.
//!
//! git learns what changed from gitfold, as from a file system monitor
//! (`core.fsmonitor`, a hook of version 2 answering "what changed since
//! token T"): the paths that differ from the workspace's base now, and those
//! that did at the refresh before (changed back since). git checks only
//! those, and for a token it wasn't given, everything (as after a copy).
//! git still reads every directory for new files: the untracked cache serves
//! `git status` only.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use gix::ObjectId;

use super::merge::{Private, oid};
use crate::git;

/// The hook git asks what changed, given the directory `GITFOLD_LIVE`: the
/// token git should hold, the new one, and the paths changed between them
/// (each ended by a NUL), in its files `since`, `token`, `paths`.
const HOOK: &str = r#"#!/bin/sh
# git's file system monitor (core.fsmonitor, hook version 2) for gitfold's
# views: what changed since token $2, as gitfold wrote it; all of it ("/")
# for another token.
printf '%s\0' "$(cat "$GITFOLD_LIVE/token")"
if [ "$2" = "$(cat "$GITFOLD_LIVE/since")" ]; then
    exec cat "$GITFOLD_LIVE/paths"
fi
printf '/\0'
"#;

/// Writes the hook into the private repository at `dir` (`live/fsmonitor`).
pub(super) fn install(dir: &Path) -> Result<()> {
    let hook = hook(dir);
    std::fs::create_dir_all(hook.parent().expect("in live/"))?;
    std::fs::write(&hook, HOOK)?;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Where the hook is in the private repository at `dir`.
fn hook(dir: &Path) -> PathBuf {
    dir.join("live").join("fsmonitor")
}

/// A workspace, as its live tree is made from it.
pub struct Workspace<'a> {
    /// Its worktree (through the mount), its index.
    pub dir: &'a Path,
    pub index: &'a Path,
    /// The paths that differ from its base (`state::status::unfiltered`):
    /// upper files and symlinks, deleted base files.
    pub changed: &'a [Vec<u8>],
    pub deleted: &'a [Vec<u8>],
}

/// A workspace's live tree, and the commit of it the views merge.
pub struct Live {
    pub tree: ObjectId,
    pub commit: ObjectId,
}

/// What git was told of each workspace, by branch.
#[derive(Default)]
pub struct Told(HashMap<String, Member>);

/// What git was told of a workspace.
struct Member {
    /// The workspace's index when the private one was copied from it.
    copied: Stamp,
    /// The token the private index holds, and the paths that differed from
    /// the base then.
    token: u64,
    paths: Vec<Vec<u8>>,
}

/// A file's identity and modification time.
type Stamp = (u64, i64, i64, u64);

fn stamp(path: &Path) -> Result<Stamp> {
    let m = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    Ok((m.ino(), m.mtime(), m.mtime_nsec(), m.len()))
}

impl Private {
    /// The live tree of workspace `ws`, of `branch`. The commit is kept
    /// (`refs/gitfold/live/<branch>`) until the next.
    pub fn live(&self, branch: &str, ws: &Workspace) -> Result<Live> {
        let mut told = self.lock.lock();
        // Its private index and the hook's files.
        let dir = self
            .dir
            .join("live")
            .join(crate::state::workspace_id(branch));
        let index = dir.join("index");
        let copied = stamp(ws.index)?;
        // The private index made again from the workspace's when that one
        // changed (or there was none): git checks it all (no token).
        let member = match told.0.remove(branch) {
            Some(m) if m.copied == copied => m,
            _ => {
                std::fs::create_dir_all(&dir)?;
                std::fs::copy(ws.index, &index)
                    .with_context(|| format!("copying {}", ws.index.display()))?;
                Member {
                    copied,
                    token: 0,
                    paths: Vec::new(),
                }
            }
        };
        let token = member.token + 1;
        let now: Vec<Vec<u8>> = ws.changed.iter().chain(ws.deleted).cloned().collect();
        let paths: Vec<u8> = (member.paths.iter().chain(&now))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .flat_map(|p| p.iter().copied().chain([0]))
            .collect();
        std::fs::write(dir.join("since"), since(member.token))?;
        std::fs::write(dir.join("token"), since(token))?;
        std::fs::write(dir.join("paths"), paths)?;
        let git = || self.in_workspace(ws.dir, &dir);
        git::cmd::output_of(&mut git()?, &["add", "-A"], None)?;
        // The private index holds the new token: nothing changed since, for
        // `write-tree` (which asks too).
        std::fs::write(dir.join("since"), since(token))?;
        std::fs::write(dir.join("paths"), b"")?;
        let member = Member {
            copied,
            token,
            paths: now,
        };
        told.0.insert(branch.to_string(), member);
        let args = ["write-tree", "--missing-ok"];
        let tree = oid(&git::cmd::output_of(&mut git()?, &args, None)?)?;
        let commit = self.commit(&tree, branch)?;
        self.refs(format!("update refs/gitfold/live/{branch} {commit}\n"))?;
        Ok(Live { tree, commit })
    }

    /// git in the workspace at `dir`, on the private index in `told` and
    /// told what changed by the hook (`HOOK`, its files in `told` too), its
    /// objects going to the private repository: safe for the repository
    /// (`git::cmd::REPOSITORY_SAFE`), and unhashed (nothing reads it but
    /// these commands).
    fn in_workspace(&self, dir: &Path, told: &Path) -> Result<Command> {
        let mut git = git::cmd::in_workspace(dir)?;
        // git runs it with the shell, which reads the path from the
        // environment, whatever its bytes.
        let monitor = "core.fsmonitor=\"$GITFOLD_FSMONITOR\"";
        git.args(git::cmd::REPOSITORY_SAFE)
            .args([
                "-c",
                "index.skipHash=true",
                "-c",
                "core.untrackedCache=true",
            ])
            .args(["-c", monitor, "-c", "core.fsmonitorHookVersion=2"])
            .env("GIT_INDEX_FILE", told.join("index"))
            .env("GITFOLD_FSMONITOR", hook(&self.dir))
            .env("GITFOLD_LIVE", told);
        self.to_private(&mut git);
        Ok(git)
    }
}

/// The token of a refresh: none (0) is no token git holds.
fn since(token: u64) -> String {
    match token {
        0 => String::new(),
        t => format!("gitfold:{t}"),
    }
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

    /// The live tree of worktree `wt`, with paths `changed` and `deleted`.
    fn live(name: &str, wt: &Path, changed: &[&str], deleted: &[&str]) -> Live {
        let (changed, deleted) = (paths(changed), paths(deleted));
        let index = index_of(wt);
        let ws = Workspace {
            dir: wt,
            index: &index,
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
        let live = live("l1", &wt, &changed, &["d/b", "d/e/a", "f/c"]);
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
        let live = live("l3", &wt, &[], &[]);
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
        let live = live("l5", &wt, &[], &["g", "h"]);
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
        let live = live("l6", &wt, &["sub"], &[]);
        assert_eq!(listing(&live.tree), format!("160000 commit {head}\tsub\n"));
    }

    #[test]
    fn told_what_changed() {
        let files = [
            (".gitignore", Some("*.o\n")),
            ("f", Some("f\n")),
            ("g", Some("g\n")),
        ];
        let base = commit(None, &files);
        let wt = worktree("l7", &base);
        // Written an hour ago: not racily clean, which git would check.
        let w = |p: &str, data: &str| {
            fs::write(wt.join(p), data).unwrap();
            let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            let file = fs::File::options().write(true).open(wt.join(p)).unwrap();
            file.set_modified(past).unwrap();
        };
        let ignore = ("100644", ".gitignore", "*.o\n");
        // All checked, then the entries git found unchanged trusted.
        live("l7", &wt, &[], &[]);
        live("l7", &wt, &[], &[]);
        w("f", "f2\n");
        w("n", "n\n");
        w("x.o", "x\n");
        let second = live("l7", &wt, &["f", "n", "x.o"], &[]);
        // Told again, unchanged: trusted from then on.
        live("l7", &wt, &["f", "n", "x.o"], &[]);
        let expected = [
            ignore,
            ("100644", "f", "f2\n"),
            ("100644", "g", "g\n"),
            ("100644", "n", "n\n"),
        ];
        assert_eq!(listing(&second.tree), tree_of(&expected));
        // `f` as in the base again and `n` gone: told as changed the time
        // before.
        w("f", "f\n");
        fs::remove_file(wt.join("n")).unwrap();
        let third = live("l7", &wt, &[], &[]);
        let expected = [ignore, ("100644", "f", "f\n"), ("100644", "g", "g\n")];
        assert_eq!(listing(&third.tree), tree_of(&expected));
        // The workspace's index changed: copied again, all of it checked.
        w("g", "g2\n");
        run_in(&wt, &["add", "-f", "x.o"], None);
        let fourth = live("l7", &wt, &[], &[]);
        let expected = [
            ignore,
            ("100644", "f", "f\n"),
            ("100644", "g", "g2\n"),
            ("100644", "x.o", "x\n"),
        ];
        assert_eq!(listing(&fourth.tree), tree_of(&expected));
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
