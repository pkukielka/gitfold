//! The guard: no commit holding another layer's uncommitted lines of a union
//! file reaches the workspace's branch, or a remote.
//!
//! The filter (`git::filter`) keeps those lines out of what git commits, but
//! not everything runs it: `git -c filter.<driver>.clean=cat`, `hash-object
//! --no-filters`, `update-index --cacheinfo`, and libgit2-based tools, which
//! ignore custom filters even when required. So the worktree's hooks check
//! the commits themselves (`worktree_config::set_guard`):
//! - `reference-transaction` (state `prepared`, for every ref update git
//!   makes, plumbing included, and not skipped by `--no-verify`) refuses an
//!   update of the workspace's branch to a commit that would bring such lines;
//! - `pre-push`, a second line, refuses pushing one.
//!
//! The lines are recorded by the daemon at each switch, per union path, with
//! the layer whose they are (`Guard`, in `<admin>/gitfold-guard`). A commit
//! brings one when its version of the path has the line and none of its
//! parents' versions does: a merge of the layer's branch, once it committed
//! them, brings them from that parent and passes.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::{BStr, ByteSlice};

use super::layers::{key, lines, lookup};
use crate::fsutil;

/// The guard's file in the worktree's admin directory.
pub const GUARD_FILE: &str = "gitfold-guard";
const HEADER: &str = "gitfold-guard 1\n";

/// The most commits one update or push is checked for.
const MAX_COMMITS: usize = 10_000;

/// What the guard keeps from the workspace's branch: per union path, the
/// layers' uncommitted lines (without their newlines) and whose they are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Guard {
    pub branch: String,
    pub paths: BTreeMap<Vec<u8>, Vec<(String, Vec<u8>)>>,
}

impl Guard {
    pub fn is_empty(&self) -> bool {
        self.paths.values().all(Vec::is_empty)
    }

    /// The file's content: the header, `branch <name>`, then NUL-terminated
    /// records, `P<path>` followed by its lines, `L<layer> TAB <line>`.
    fn encode(&self) -> Vec<u8> {
        let mut out = format!("{HEADER}branch {}\n", self.branch).into_bytes();
        for (path, lines) in &self.paths {
            out.push(b'P');
            out.extend_from_slice(path);
            out.push(0);
            for (layer, line) in lines {
                out.push(b'L');
                out.extend_from_slice(layer.as_bytes());
                out.push(b'\t');
                out.extend_from_slice(line);
                out.push(0);
            }
        }
        out
    }

    fn decode(data: &[u8]) -> Result<Guard> {
        let bad = || anyhow::anyhow!("not a guard file this gitfold knows");
        let rest = data.strip_prefix(HEADER.as_bytes()).ok_or_else(bad)?;
        let rest = rest.strip_prefix(b"branch ").ok_or_else(bad)?;
        let nl = rest.find_byte(b'\n').ok_or_else(bad)?;
        let mut guard = Guard {
            branch: String::from_utf8(rest[..nl].to_vec())?,
            ..Default::default()
        };
        let mut path: Option<Vec<u8>> = None;
        for record in rest[nl + 1..].split(|&b| b == 0).filter(|r| !r.is_empty()) {
            match record.split_first() {
                Some((b'P', p)) => {
                    guard.paths.entry(p.to_vec()).or_default();
                    path = Some(p.to_vec());
                }
                Some((b'L', l)) => {
                    let p = path.as_ref().ok_or_else(bad)?;
                    let tab = l.find_byte(b'\t').ok_or_else(bad)?;
                    let layer = String::from_utf8(l[..tab].to_vec())?;
                    let entry = guard.paths.get_mut(p).ok_or_else(bad)?;
                    entry.push((layer, l[tab + 1..].to_vec()));
                }
                _ => return Err(bad()),
            }
        }
        Ok(guard)
    }

    /// Writes it to the worktree with admin directory `admin`.
    pub fn write(&self, admin: &Path) -> Result<()> {
        let path = admin.join(GUARD_FILE);
        fsutil::write_atomic(&path, &self.encode())
            .with_context(|| format!("writing {}", path.display()))
    }

    /// The guard of the worktree with admin directory `admin`, if it has one.
    pub fn read(admin: &Path) -> Result<Option<Guard>> {
        let path = admin.join(GUARD_FILE);
        match fs::read(&path) {
            Ok(data) => Guard::decode(&data)
                .map(Some)
                .with_context(|| format!("reading {}", path.display())),
            Err(e) if fsutil::not_found(&e) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn remove(admin: &Path) -> Result<()> {
        let path = admin.join(GUARD_FILE);
        match fs::remove_file(&path) {
            Err(e) if !fsutil::not_found(&e) => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// The first of `commits` that brings a guarded line, with the path, the
    /// line and its layer.
    pub fn violation(
        &self,
        repo: &gix::Repository,
        commits: &[ObjectId],
    ) -> Result<Option<Violation>> {
        if self.is_empty() {
            return Ok(None);
        }
        for &id in commits {
            let commit = repo.find_commit(id)?;
            let tree = commit.tree_id()?.detach();
            let parents: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
            let parent_trees = (parents.iter())
                .map(|&p| super::layers::commit_tree(repo, p))
                .collect::<Result<Vec<_>>>()?;
            for (path, guarded) in &self.paths {
                let path_b = BStr::new(path);
                let Some(item) = lookup(repo, tree, path_b)? else {
                    continue;
                };
                let mut before = Vec::new();
                let mut same = false;
                for &pt in &parent_trees {
                    if let Some(p) = lookup(repo, pt, path_b)? {
                        same |= p.oid == item.oid;
                        before.push(repo.find_blob(p.oid)?.take_data());
                    }
                }
                if same {
                    continue;
                }
                let data = repo.find_blob(item.oid)?.take_data();
                let mut had: HashMap<&[u8], usize> = HashMap::new();
                for line in before.iter().flat_map(|b| lines(b)) {
                    *had.entry(key(line)).or_default() += 1;
                }
                for line in lines(&data).map(key) {
                    if had.get(line).is_some_and(|n| *n > 0) {
                        continue;
                    }
                    if let Some((layer, _)) = guarded.iter().find(|(_, l)| l == line) {
                        return Ok(Some(Violation {
                            commit: id,
                            path: path.clone(),
                            line: line.to_vec(),
                            layer: layer.clone(),
                        }));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// A commit that would bring a layer's uncommitted line.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub commit: ObjectId,
    pub path: Vec<u8>,
    pub line: Vec<u8>,
    pub layer: String,
}

impl Violation {
    fn describe(&self, refused: &str) -> String {
        format!(
            "gitfold: refusing {refused}: commit {} adds {}'s uncommitted line {:?} to {} \
             ({} isn't your work: it shows here as a layer, and git leaves its lines out of \
             what it commits). The commit was made without gitfold's filter (a libgit2 \
             tool, --no-filters, plumbing): make it again with git, or take the line out.",
            &self.commit.to_string()[..12],
            self.layer,
            String::from_utf8_lossy(&self.line),
            String::from_utf8_lossy(&self.path),
            self.layer,
        )
    }
}

/// What the guard checks, as git gives it to the hook on stdin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Hook {
    /// `reference-transaction prepared`: `<old> <new> <ref>` per line.
    RefUpdate,
    /// `pre-push`: `<local ref> <local oid> <remote ref> <remote oid>`.
    PrePush,
}

fn is_zero(hex: &str) -> bool {
    hex.bytes().all(|b| b == b'0')
}

/// The common directory of the worktree with admin directory `admin`.
fn common_dir(admin: &Path) -> Result<PathBuf> {
    let common = fs::read_to_string(admin.join("commondir"))
        .with_context(|| format!("reading {}/commondir", admin.display()))?;
    Ok(admin.join(common.trim()))
}

/// The commits `rev-list <args>` gives, in `common`.
fn rev_list(common: &Path, args: &[String]) -> Result<Vec<ObjectId>> {
    let mut all = vec!["rev-list".to_string(), format!("--max-count={MAX_COMMITS}")];
    all.extend(args.iter().cloned());
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    let out = super::cmd::output(common, &all)?;
    (out.split(|&b| b == b'\n'))
        .filter(|l| !l.is_empty())
        .map(|l| Ok(ObjectId::from_hex(l)?))
        .collect()
}

/// `gitfold guard <hook> <admin>`, run by the worktree's hook: checks the
/// updates on stdin; fails, saying why, if one would bring a guarded line.
pub fn run_hook(hook: Hook, admin: &Path, input: impl BufRead) -> Result<()> {
    let Some(guard) = Guard::read(admin)? else {
        return Ok(());
    };
    if guard.is_empty() {
        return Ok(());
    }
    let common = common_dir(admin)?;
    super::init(&common)?;
    let branch_ref = format!("refs/heads/{}", guard.branch);
    for line in input.lines() {
        let line = line?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (refused, new, hide): (String, &str, Vec<String>) = match (hook, &fields[..]) {
            (Hook::RefUpdate, [old, new, name]) if *name == branch_ref => {
                let hide = match is_zero(old) {
                    true => Vec::new(),
                    false => vec![format!("^{old}")],
                };
                (format!("to update {name}"), new, hide)
            }
            (Hook::PrePush, [_, local, remote_ref, remote]) => {
                let mut hide = vec!["--not".to_string(), "--remotes".to_string()];
                let known = !is_zero(remote)
                    && super::with_repo(|r| {
                        ObjectId::from_hex(remote.as_bytes()).is_ok_and(|o| r.has_object(o))
                    });
                if known {
                    hide.push((*remote).to_string());
                }
                (format!("to push {remote_ref}"), local, hide)
            }
            _ => continue,
        };
        if is_zero(new) {
            continue;
        }
        let mut args = vec![new.to_string()];
        args.extend(hide);
        let commits = rev_list(&common, &args)?;
        if let Some(v) = super::with_repo(|repo| guard.violation(repo, &commits))? {
            bail!("{}", v.describe(&refused));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_repo::Repo;

    #[test]
    fn file_round_trips() {
        let mut guard = Guard {
            branch: "me/x".into(),
            ..Default::default()
        };
        guard.paths.insert(
            b"reg.txt".to_vec(),
            vec![
                ("la".into(), b"fromA".to_vec()),
                ("lb".into(), b"x\ty".to_vec()),
            ],
        );
        guard.paths.insert(b"empty.txt".to_vec(), Vec::new());
        assert_eq!(Guard::decode(&guard.encode()).unwrap(), guard);
        assert!(Guard::decode(b"other").is_err());
    }

    /// A commit adding a guarded line is caught; one keeping it from a
    /// parent, or merging it from one, isn't.
    #[test]
    fn violations() {
        let r = Repo::new("guard");
        r.write("reg.txt", "one\n");
        r.commit("c0");
        r.git(&["checkout", "-q", "-b", "la"]);
        r.write("reg.txt", "one\nfromA\n");
        let leak = r.commit("leak");
        r.write("reg.txt", "one\nfromA\nmore\n");
        let after = r.commit("after");
        r.git(&["checkout", "-q", "main"]);
        r.write("reg.txt", "one\nmine\n");
        let mine = r.commit("mine");
        r.git(&["merge", "-q", "-s", "ours", "--no-edit", "la"]);
        r.write("reg.txt", "one\nmine\nfromA\n");
        r.git(&["commit", "-q", "-a", "--amend", "--no-edit"]);
        let merged = r.id("HEAD");
        let mut guard = Guard {
            branch: "main".into(),
            ..Default::default()
        };
        guard
            .paths
            .insert(b"reg.txt".to_vec(), vec![("la".into(), b"fromA".to_vec())]);
        let repo = r.open();
        assert!(guard.violation(&repo, &[mine]).unwrap().is_none());
        let v = guard.violation(&repo, &[mine, leak]).unwrap().unwrap();
        assert_eq!(
            (v.commit, v.layer.as_str(), &v.line[..]),
            (leak, "la", &b"fromA"[..])
        );
        assert!(guard.violation(&repo, &[after]).unwrap().is_none());
        assert!(guard.violation(&repo, &[merged]).unwrap().is_none());
    }
}
