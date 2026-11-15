//! The guard: no commit holding an editable or union layer's uncommitted
//! work reaches the workspace's branch, or a remote. It is the authority:
//! the filter (`git::filter`) keeps that work out of what git sees, but it
//! classifies by diff matching and can be wrong, and not everything runs it
//! (`git -c filter.<driver>.clean=cat`, `hash-object --no-filters`,
//! `update-index --cacheinfo`, libgit2-based tools, which ignore custom
//! filters even when required). So the worktree's hooks check the commits
//! themselves (`worktree_config::install_guard`):
//! - `reference-transaction` (state `prepared`, for every ref update git
//!   makes, plumbing included, and not skipped by `--no-verify`) refuses an
//!   update of the workspace's branch to a commit that would bring such work;
//! - `pre-push`, a second line, refuses pushing one;
//! - libgit2 runs no hooks: the daemon checks the commits once the branch
//!   moved (`state::guard_check`), and warns.
//!
//! What is guarded is recorded by the daemon at each switch, per path and
//! layer (`Guard`, in `<admin>/gitfold-guard`): the layer's version of the
//! file in its snapshot, the lines it adds that neither its commits nor the
//! branch have, and the branch's lines its uncommitted work removes. Each
//! check first drops what the layer has committed since (`Guard::current`),
//! so a frozen layer, or one that committed after the switch, isn't held
//! against its own commits, and leaves out the layers' own commits (a
//! fast-forward or a merge onto them). A commit then brings a guarded line
//! when its version of the path has it and none of its parents' versions
//! does, whatever the order of lines (contents are compared as sets), and
//! with lines compared without whitespace, long lines without punctuation
//! too, so a formatter's rewrite doesn't get one past. It also brings the
//! layer's uncommitted work when it removes a line the layer's work
//! removes, adds the layer's new file whole, deletes the file, or adds a
//! conflict marker there.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::{BStr, ByteSlice};
use gix::objs::tree::EntryKind;

use super::layers::{key, lines, lookup};
use crate::fsutil;

/// The guard's file in the worktree's admin directory.
pub const GUARD_FILE: &str = "gitfold-guard";
const HEADER: &str = "gitfold-guard 2\n";

/// The most commits one update or push is checked for.
const MAX_COMMITS: usize = 10_000;

/// How many words a line needs before punctuation stops counting in the
/// guard's comparison (`loose`): fewer, and lines that differ only in
/// punctuation are different code (`a(b)` and `a[b]`).
const LOOSE_WORDS: usize = 4;

/// What the guard keeps from the workspace's branch, per path: the layers
/// with uncommitted work there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Guard {
    pub branch: String,
    pub paths: BTreeMap<Vec<u8>, Vec<Entry>>,
}

/// One layer's uncommitted work at one path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub layer: String,
    /// The layer's version of the file in its snapshot (`None`: it deletes
    /// the file).
    pub blob: Option<ObjectId>,
    /// The lines it adds that neither its commits nor the branch have
    /// (without their newlines).
    pub added: Vec<Vec<u8>>,
    /// The branch's lines (its commits' too) its uncommitted work removes.
    pub removed: Vec<Vec<u8>>,
}

impl Guard {
    pub fn is_empty(&self) -> bool {
        self.paths.values().all(Vec::is_empty)
    }

    /// The layers it guards, each once.
    pub fn layers(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for entry in self.paths.values().flatten() {
            if !out.contains(&entry.layer.as_str()) {
                out.push(&entry.layer);
            }
        }
        out
    }

    /// The file's content: the header, `branch <name>`, then NUL-terminated
    /// records: `P<path>`, then per layer with work there `E<layer> TAB
    /// <blob or ->`, followed by its lines, `A<line>` added and `R<line>`
    /// removed.
    fn encode(&self) -> Vec<u8> {
        let mut out = format!("{HEADER}branch {}\n", self.branch).into_bytes();
        for (path, entries) in &self.paths {
            out.push(b'P');
            out.extend_from_slice(path);
            out.push(0);
            for e in entries {
                let blob = e.blob.map_or("-".to_string(), |b| b.to_string());
                out.extend_from_slice(format!("E{}\t{blob}", e.layer).as_bytes());
                out.push(0);
                for (tag, lines) in [(b'A', &e.added), (b'R', &e.removed)] {
                    for line in lines {
                        out.push(tag);
                        out.extend_from_slice(line);
                        out.push(0);
                    }
                }
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
            let entries = |path: &Option<Vec<u8>>, guard: &mut Guard| -> Result<usize> {
                let p = path.as_ref().ok_or_else(bad)?;
                Ok(guard.paths.get(p).ok_or_else(bad)?.len())
            };
            match record.split_first() {
                Some((b'P', p)) => {
                    guard.paths.entry(p.to_vec()).or_default();
                    path = Some(p.to_vec());
                }
                Some((b'E', e)) => {
                    entries(&path, &mut guard)?;
                    let tab = e.find_byte(b'\t').ok_or_else(bad)?;
                    let blob = match &e[tab + 1..] {
                        b"-" => None,
                        hex => Some(ObjectId::from_hex(hex)?),
                    };
                    let entry = Entry {
                        layer: String::from_utf8(e[..tab].to_vec())?,
                        blob,
                        ..Default::default()
                    };
                    guard
                        .paths
                        .get_mut(path.as_ref().ok_or_else(bad)?)
                        .ok_or_else(bad)?
                        .push(entry);
                }
                Some((tag @ (b'A' | b'R'), line)) => {
                    if entries(&path, &mut guard)? == 0 {
                        return Err(bad());
                    }
                    let list = guard
                        .paths
                        .get_mut(path.as_ref().ok_or_else(bad)?)
                        .ok_or_else(bad)?;
                    let entry = list.last_mut().ok_or_else(bad)?;
                    match tag {
                        b'A' => entry.added.push(line.to_vec()),
                        _ => entry.removed.push(line.to_vec()),
                    }
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

    /// The guard as of now: without what each layer has committed since it
    /// was recorded (its branch's tip has it), so a layer's commits, and
    /// those of a frozen layer, are never held against it.
    pub fn current(&self, repo: &gix::Repository) -> Result<Guard> {
        let mut tips: HashMap<&str, Option<ObjectId>> = HashMap::new();
        for layer in self.layers() {
            let tip = match repo.try_find_reference(format!("refs/heads/{layer}").as_str())? {
                Some(mut r) => Some(r.peel_to_commit()?.tree_id()?.detach()),
                None => None,
            };
            tips.insert(layer, tip);
        }
        let mut out = Guard {
            branch: self.branch.clone(),
            paths: BTreeMap::new(),
        };
        for (path, entries) in &self.paths {
            let mut kept = Vec::new();
            for e in entries {
                let Some(Some(tip)) = tips.get(e.layer.as_str()) else {
                    kept.push(e.clone());
                    continue;
                };
                let at_tip = lookup(repo, *tip, BStr::new(path))?;
                if at_tip.as_ref().map(|i| i.oid) == e.blob {
                    continue;
                }
                let data = match at_tip {
                    Some(item)
                        if matches!(item.kind, EntryKind::Blob | EntryKind::BlobExecutable) =>
                    {
                        repo.find_blob(item.oid)?.take_data()
                    }
                    _ => Vec::new(),
                };
                let has: HashSet<Vec<u8>> = lines(&data).map(|l| norm(key(l))).collect();
                let mut e = e.clone();
                e.added.retain(|l| !has.contains(&norm(l)));
                e.removed.retain(|l| has.contains(&norm(l)));
                kept.push(e);
            }
            if !kept.is_empty() {
                out.paths.insert(path.clone(), kept);
            }
        }
        Ok(out)
    }

    /// The first of `commits` that brings a layer's uncommitted work.
    pub fn violation(
        &self,
        repo: &gix::Repository,
        commits: &[ObjectId],
    ) -> Result<Option<Violation>> {
        Ok(self.scan(repo, commits, false)?.pop())
    }

    /// Everything of the layers' uncommitted work `commits` bring.
    pub fn violations(
        &self,
        repo: &gix::Repository,
        commits: &[ObjectId],
    ) -> Result<Vec<Violation>> {
        self.scan(repo, commits, true)
    }

    /// What of the layers' uncommitted work `commits` bring: `all`, or the
    /// first.
    fn scan(
        &self,
        repo: &gix::Repository,
        commits: &[ObjectId],
        all: bool,
    ) -> Result<Vec<Violation>> {
        let mut out = Vec::new();
        if self.is_empty() {
            return Ok(out);
        }
        let rules: Vec<(&Vec<u8>, Rules)> = (self.paths.iter())
            .filter(|(_, e)| !e.is_empty())
            .map(|(p, e)| (p, Rules::of(e)))
            .collect();
        for &id in commits {
            let commit = repo.find_commit(id)?;
            let tree = commit.tree_id()?.detach();
            let parents: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
            let parent_trees = (parents.iter())
                .map(|&p| super::layers::commit_tree(repo, p))
                .collect::<Result<Vec<_>>>()?;
            for (path, rules) in &rules {
                for (what, layer) in rules.brought(repo, path, tree, &parent_trees)? {
                    out.push(Violation {
                        commit: id,
                        path: path.to_vec(),
                        layer: layer.to_string(),
                        what,
                    });
                    if !all {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// A path's guard, ready to look lines up.
struct Rules<'a> {
    /// The first layer with work there, named when the whole file is.
    first: &'a str,
    /// The added lines, by `norm` and by `loose`.
    added: HashMap<Vec<u8>, &'a str>,
    added_loose: HashMap<Vec<u8>, &'a str>,
    removed: Vec<(&'a str, &'a [u8])>,
    /// The layers' new files, by content.
    new_file: HashMap<ObjectId, &'a str>,
}

impl<'a> Rules<'a> {
    fn of(entries: &'a [Entry]) -> Rules<'a> {
        let mut rules = Rules {
            first: &entries[0].layer,
            added: HashMap::new(),
            added_loose: HashMap::new(),
            removed: Vec::new(),
            new_file: HashMap::new(),
        };
        for e in entries {
            for line in &e.added {
                rules.added.entry(norm(line)).or_insert(&e.layer);
                if let Some(l) = loose(line) {
                    rules.added_loose.entry(l).or_insert(&e.layer);
                }
            }
            for line in &e.removed {
                rules.removed.push((&e.layer, line));
            }
            if let Some(blob) = e.blob {
                rules.new_file.entry(blob).or_insert(&e.layer);
            }
        }
        rules
    }

    /// What a commit (tree `tree`, its parents' trees `parents`) brings of
    /// the layers' uncommitted work at `path`, with whose it is.
    fn brought(
        &self,
        repo: &gix::Repository,
        path: &[u8],
        tree: ObjectId,
        parents: &[ObjectId],
    ) -> Result<Vec<(What, &'a str)>> {
        let path = BStr::new(path);
        let file = |tree: ObjectId| -> Result<Option<ObjectId>> {
            Ok(lookup(repo, tree, path)?
                .filter(|i| matches!(i.kind, EntryKind::Blob | EntryKind::BlobExecutable))
                .map(|i| i.oid))
        };
        let blob = |oid: ObjectId| -> Result<Vec<u8>> { Ok(repo.find_blob(oid)?.take_data()) };
        let before = parents
            .iter()
            .map(|&t| file(t))
            .collect::<Result<Vec<_>>>()?;
        let mut found = Vec::new();
        let oid = match file(tree)? {
            // Deleted: brought if every parent has it.
            None => {
                if !before.is_empty() && before.iter().all(Option::is_some) {
                    found.push((What::DeletesFile, self.first));
                }
                return Ok(found);
            }
            Some(oid) if before.contains(&Some(oid)) => return Ok(found),
            Some(oid) => oid,
        };
        if before.iter().all(Option::is_none)
            && let Some(&layer) = self.new_file.get(&oid)
        {
            found.push((What::AddsFile, layer));
            return Ok(found);
        }
        let data = blob(oid)?;
        let parent_sets = (before.iter().flatten())
            .map(|&p| Ok(Sets::of(&blob(p)?)))
            .collect::<Result<Vec<_>>>()?;
        let had = |line: &[u8]| parent_sets.iter().any(|s| s.has(line));
        for line in lines(&data).map(key) {
            if is_marker(line) {
                if !had(line) {
                    found.push((What::Marker(line.to_vec()), self.first));
                }
            } else if significant(line)
                && !had(line)
                && let Some(layer) = self.adds(line)
            {
                found.push((What::Adds(line.to_vec()), layer));
            }
        }
        if !self.removed.is_empty() && !parent_sets.is_empty() {
            let now = Sets::of(&data);
            for &(layer, line) in &self.removed {
                if parent_sets.iter().all(|s| s.has(line)) && !now.has(line) {
                    found.push((What::Removes(line.to_vec()), layer));
                }
            }
        }
        Ok(found)
    }

    /// The layer whose added line `line` is, if it is one.
    fn adds(&self, line: &[u8]) -> Option<&'a str> {
        (self.added.get(&norm(line)).copied())
            .or_else(|| loose(line).and_then(|l| self.added_loose.get(&l).copied()))
    }
}

/// The lines of a file as the guard compares them.
struct Sets {
    norm: HashSet<Vec<u8>>,
    loose: HashSet<Vec<u8>>,
}

impl Sets {
    fn of(data: &[u8]) -> Sets {
        let mut sets = Sets {
            norm: HashSet::new(),
            loose: HashSet::new(),
        };
        for line in lines(data).map(key) {
            sets.norm.insert(norm(line));
            if let Some(l) = loose(line) {
                sets.loose.insert(l);
            }
        }
        sets
    }

    fn has(&self, line: &[u8]) -> bool {
        self.norm.contains(&norm(line)) || loose(line).is_some_and(|l| self.loose.contains(&l))
    }
}

/// A line as the guard compares it: without whitespace, so a formatter's
/// respacing or rewrapping of a line doesn't get it past.
pub fn norm(line: &[u8]) -> Vec<u8> {
    line.iter()
        .filter(|b| !b.is_ascii_whitespace())
        .copied()
        .collect()
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// A long line (`LOOSE_WORDS` words or more) as the guard compares it too:
/// its words alone, so a formatter's quotes, commas and brackets don't get
/// it past.
pub fn loose(line: &[u8]) -> Option<Vec<u8>> {
    let words = line
        .split(|&b| !is_word(b))
        .filter(|w| !w.is_empty())
        .count();
    (words >= LOOSE_WORDS).then(|| line.iter().filter(|&&b| is_word(b)).copied().collect())
}

/// Whether a line says anything of whose it is: a line of punctuation
/// alone (`}`, `);`) or blank is everyone's.
pub fn significant(line: &[u8]) -> bool {
    line.iter().any(|&b| b.is_ascii_alphanumeric() || b >= 0x80)
}

/// Whether `line` is one of git's conflict markers (`<<<<<<< ours`,
/// `|||||||`, `=======`, `>>>>>>> theirs`).
pub fn is_marker(line: &[u8]) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line == b"=======" {
        return true;
    }
    [b'<', b'>', b'|'].iter().any(|&c| {
        line.len() >= 7
            && line[..7].iter().all(|&b| b == c)
            && line.get(7).is_none_or(|&b| b == b' ')
    })
}

/// What a commit brings of a layer's uncommitted work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// One of its uncommitted lines.
    Adds(Vec<u8>),
    /// The removal of a line its uncommitted work removes.
    Removes(Vec<u8>),
    /// Its new file, whole.
    AddsFile,
    /// The deletion of a file it has uncommitted work in.
    DeletesFile,
    /// A conflict marker, in a file it has uncommitted work in.
    Marker(Vec<u8>),
}

/// A commit that would bring a layer's uncommitted work.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub commit: ObjectId,
    pub path: Vec<u8>,
    pub layer: String,
    pub what: What,
}

impl Violation {
    /// `adds api's uncommitted line "x" to src/a.rs`, and so on.
    pub fn says(&self) -> String {
        let (layer, path) = (&self.layer, String::from_utf8_lossy(&self.path));
        let quoted = |l: &[u8]| format!("{:?}", String::from_utf8_lossy(l).trim());
        match &self.what {
            What::Adds(l) => format!("adds {layer}'s uncommitted line {} to {path}", quoted(l)),
            What::Removes(l) => format!(
                "removes {} from {path}, as {layer}'s uncommitted work does",
                quoted(l)
            ),
            What::AddsFile => format!("adds {path}, {layer}'s uncommitted file"),
            What::DeletesFile => format!("deletes {path}, which has {layer}'s uncommitted work"),
            What::Marker(l) => format!(
                "adds a conflict marker {} to {path} (a conflict with {layer}'s uncommitted work)",
                quoted(l)
            ),
        }
    }

    fn describe(&self, refused: &str) -> String {
        let layer = &self.layer;
        format!(
            "gitfold: refusing {refused}: commit {} {}. {layer} hasn't committed that, so your \
             branch can't have it yet (it shows here as a layer). gitfold's filter keeps it out \
             of what git sees, but this got past it: a tool that ignores the filter (a libgit2 \
             one, --no-filters, plumbing), or a change to {layer}'s lines gitfold took for \
             yours. Leave {layer}'s lines as {layer} has them in the commit (your changes to \
             them wait until {layer} commits them; `gitfold layer` lists them), then commit \
             again.",
            &self.commit.to_string()[..12],
            self.says(),
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
pub(crate) fn rev_list(common: &Path, args: &[String]) -> Result<Vec<ObjectId>> {
    let mut all = vec!["rev-list".to_string(), format!("--max-count={MAX_COMMITS}")];
    all.extend(args.iter().cloned());
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    let out = super::cmd::output(common, &all)?;
    (out.split(|&b| b == b'\n'))
        .filter(|l| !l.is_empty())
        .map(|l| Ok(ObjectId::from_hex(l)?))
        .collect()
}

/// The `rev-list` arguments leaving out the commits of `layers`' branches
/// (those that exist): a layer's own commits are its committed work.
pub(crate) fn hide_layers<'a>(layers: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    (layers.into_iter())
        .filter(|l| super::branch_exists(l))
        .map(|l| format!("^refs/heads/{l}"))
        .collect()
}

/// `gitfold guard <hook> <admin>`, run by the worktree's hook: checks the
/// updates on stdin; fails, saying why, if one would bring a layer's
/// uncommitted work.
pub fn run_hook(hook: Hook, admin: &Path, input: impl BufRead) -> Result<()> {
    let Some(guard) = Guard::read(admin)? else {
        return Ok(());
    };
    if guard.is_empty() {
        return Ok(());
    }
    let common = common_dir(admin)?;
    super::init(&common)?;
    let guard = super::with_repo(|repo| guard.current(repo))?;
    if guard.is_empty() {
        return Ok(());
    }
    let layers = hide_layers(guard.layers());
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
                hide.push("--not".to_string());
                (format!("to push {remote_ref}"), local, hide)
            }
            _ => continue,
        };
        if is_zero(new) {
            continue;
        }
        let mut args = vec![new.to_string()];
        args.extend(hide);
        args.extend(layers.iter().cloned());
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

    fn entry(layer: &str, blob: Option<ObjectId>, added: &[&str], removed: &[&str]) -> Entry {
        Entry {
            layer: layer.into(),
            blob,
            added: added.iter().map(|l| l.as_bytes().to_vec()).collect(),
            removed: removed.iter().map(|l| l.as_bytes().to_vec()).collect(),
        }
    }

    fn guard(paths: &[(&str, Vec<Entry>)]) -> Guard {
        Guard {
            branch: "main".into(),
            paths: (paths.iter())
                .map(|(p, e)| (p.as_bytes().to_vec(), e.clone()))
                .collect(),
        }
    }

    fn whats(found: &[Violation]) -> Vec<String> {
        found.iter().map(Violation::says).collect()
    }

    #[test]
    fn file_round_trips() {
        let blob = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let mut g = guard(&[
            (
                "reg.txt",
                vec![
                    entry("la", Some(blob), &["fromA", "x\ty"], &["gone"]),
                    entry("lb", None, &[], &[]),
                ],
            ),
            ("empty.txt", Vec::new()),
        ]);
        g.branch = "me/x".into();
        assert_eq!(Guard::decode(&g.encode()).unwrap(), g);
        assert_eq!(g.layers(), ["la", "lb"]);
        assert!(Guard::decode(b"other").is_err());
        assert!(Guard::decode(b"gitfold-guard 1\nbranch x\n").is_err());
    }

    #[test]
    fn normalised_lines() {
        assert_eq!(norm(b"  fn a( x ,y )  "), b"fna(x,y)");
        assert_eq!(loose(b"a(b)"), None);
        assert_eq!(
            loose(b"let total = compute('a', \"b\");").as_deref(),
            Some(&b"lettotalcomputeab"[..])
        );
        assert!(!significant(b"  });"));
        assert!(significant(b"x"));
        for marker in [
            &b"<<<<<<< yours"[..],
            b"=======",
            b">>>>>>> api",
            b"|||||||",
        ] {
            assert!(is_marker(marker), "{}", String::from_utf8_lossy(marker));
        }
        assert!(!is_marker(b"<<<<<<<< eight"));
        assert!(!is_marker(b"== x"));
    }

    /// A commit adding a guarded line is caught, however respaced; one
    /// keeping it from a parent, or merging it from one, isn't.
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
        r.write("reg.txt", "one\nmine\nfromA\n  fromA \n");
        let respaced = r.commit("respaced");
        let g = guard(&[("reg.txt", vec![entry("la", None, &["fromA"], &[])])]);
        let repo = r.open();
        assert!(g.violation(&repo, &[mine]).unwrap().is_none());
        let v = g.violation(&repo, &[mine, leak]).unwrap().unwrap();
        assert_eq!(
            (v.commit, v.layer.as_str(), &v.what),
            (leak, "la", &What::Adds(b"fromA".to_vec()))
        );
        assert!(g.violation(&repo, &[after]).unwrap().is_none());
        assert!(g.violation(&repo, &[merged]).unwrap().is_none());
        // A line the parent has (spacing aside) isn't brought by the commit;
        // a respaced copy of a guarded line is.
        assert!(g.violation(&repo, &[respaced]).unwrap().is_none());
        r.git(&["checkout", "-q", "--detach", "main~2"]);
        r.write("reg.txt", "one\n f r o m A  \n");
        let spaced = r.commit("spaced");
        let v = g.violation(&repo, &[spaced]).unwrap().unwrap();
        assert_eq!(v.what, What::Adds(b" f r o m A  ".to_vec()));
    }

    /// A formatter's rewrite (quotes, a trailing comma) of a long guarded
    /// line, or its move elsewhere, still brings it.
    #[test]
    fn rewritten_and_moved_lines() {
        let r = Repo::new("guard-format");
        r.write("api.rs", "use b;\nuse c;\n\nfn main() {}\n");
        let c0 = r.commit("c0");
        let g = guard(&[(
            "api.rs",
            vec![entry(
                "lead",
                None,
                &["use a;", "    let total = compute('a', 'b');"],
                &[],
            )],
        )]);
        r.write(
            "api.rs",
            "use a;\nuse b;\nuse c;\n\nfn main() {\n    let total = compute(\"a\", \"b\",);\n}\n",
        );
        let moved = r.commit("moved");
        let repo = r.open();
        let found = g.violations(&repo, &[moved]).unwrap();
        assert_eq!(
            whats(&found),
            [
                "adds lead's uncommitted line \"use a;\" to api.rs",
                "adds lead's uncommitted line \"let total = compute(\\\"a\\\", \\\"b\\\",);\" to \
                 api.rs"
            ]
        );
        // A short line differing in punctuation is another line.
        r.git(&["checkout", "-q", "--detach", &c0.to_string()]);
        r.write("api.rs", "use b;\nuse c;\n\nfn main() {}\nuse(a)\n");
        let other = r.commit("other");
        assert!(g.violations(&repo, &[other]).unwrap().is_empty());
    }

    /// The layer's new file, whole (even of punctuation alone); the deletion
    /// of a file it has uncommitted work in; a line its work removes; a
    /// conflict marker.
    #[test]
    fn files_deletions_and_markers() {
        let r = Repo::new("guard-files");
        r.write("api.rs", "// api\nfn keep() {}\nfn old() {}\n");
        r.write("x.rs", "x\n");
        r.commit("c0");
        r.write("new.rs", "}\n");
        let added = r.commit("added");
        let blob = r.id("HEAD:new.rs");
        let g = guard(&[
            ("new.rs", vec![entry("lead", Some(blob), &[], &[])]),
            ("api.rs", vec![entry("lead", None, &[], &["fn old() {}"])]),
            ("x.rs", vec![entry("lead", None, &[], &[])]),
        ]);
        let repo = r.open();
        assert_eq!(
            whats(&g.violations(&repo, &[added]).unwrap()),
            ["adds new.rs, lead's uncommitted file"]
        );
        r.git(&["rm", "-q", "x.rs"]);
        let deleted = r.commit("deleted");
        r.write("api.rs", "// api\nfn keep() {}\n");
        let removed = r.commit("removed");
        r.write(
            "api.rs",
            "// api\n<<<<<<< yours\nfn keep() {}\n=======\n>>>>>>> lead\n",
        );
        let marked = r.commit("marked");
        r.write("api.rs", "// api\nfn keep() { mine }\n");
        let own = r.commit("own");
        let repo = r.open();
        assert_eq!(
            whats(
                &g.violations(&repo, &[deleted, removed, marked, own])
                    .unwrap()
            ),
            [
                "deletes x.rs, which has lead's uncommitted work",
                "removes \"fn old() {}\" from api.rs, as lead's uncommitted work does",
                "adds a conflict marker \"<<<<<<< yours\" to api.rs (a conflict with lead's \
                 uncommitted work)",
                "adds a conflict marker \"=======\" to api.rs (a conflict with lead's uncommitted \
                 work)",
                "adds a conflict marker \">>>>>>> lead\" to api.rs (a conflict with lead's \
                 uncommitted work)",
            ]
        );
    }

    /// Every guarded line a commit brings is found (the safety net names
    /// them all); a line of punctuation alone is no one's.
    #[test]
    fn all_violations_but_punctuation() {
        let r = Repo::new("guard-all");
        r.write("api.rs", "// api\n");
        r.commit("c0");
        r.write("api.rs", "// api\nclass Api {\n    fn a() {}\n}\n");
        let leak = r.commit("leak");
        r.write(
            "api.rs",
            "// api\nclass Api {\n    fn a() {}\n}\nfn own() {\n}\n",
        );
        let own = r.commit("own");
        let lines = ["class Api {", "    fn a() {}", "}"];
        let g = guard(&[("api.rs", vec![entry("lead", None, &lines, &[])])]);
        let repo = r.open();
        let found = g.violations(&repo, &[leak]).unwrap();
        let found: Vec<&What> = found.iter().map(|v| &v.what).collect();
        assert_eq!(
            found,
            [
                &What::Adds(b"class Api {".to_vec()),
                &What::Adds(b"    fn a() {}".to_vec())
            ]
        );
        // A `}` of its own, added after the layer's: not the layer's.
        let g = guard(&[("api.rs", vec![entry("lead", None, &["}"], &[])])]);
        assert!(g.violations(&repo, &[own]).unwrap().is_empty());
    }

    /// What a layer has committed since the guard was written isn't held
    /// against it: its lines once its branch has them, the whole file once
    /// its branch has its version.
    #[test]
    fn current_drops_what_the_layer_committed() {
        let r = Repo::new("guard-current");
        r.write("api.rs", "// api\n");
        r.commit("c0");
        r.git(&["branch", "lead"]);
        let g = guard(&[(
            "api.rs",
            vec![entry(
                "lead",
                None,
                &["fn a() {}", "fn b() {}"],
                &["// api"],
            )],
        )]);
        assert_eq!(g.current(&r.open()).unwrap(), g);
        r.git(&["checkout", "-q", "lead"]);
        r.write("api.rs", "fn a() {}\n");
        r.commit("lead: a");
        let now = g.current(&r.open()).unwrap();
        assert_eq!(
            now.paths[&b"api.rs".to_vec()],
            [entry("lead", None, &["fn b() {}"], &[])]
        );
        // Its version, whole: nothing of it is guarded.
        r.write("api.rs", "fn a() {}\nfn b() {}\n");
        r.commit("lead: b");
        let blob = r.id("lead:api.rs");
        let g = guard(&[(
            "api.rs",
            vec![entry("lead", Some(blob), &["fn b() {}"], &[])],
        )]);
        assert!(g.current(&r.open()).unwrap().is_empty());
        // A layer with no branch: kept as it is.
        let g = guard(&[("api.rs", vec![entry("gone", None, &["fn c() {}"], &[])])]);
        assert_eq!(g.current(&r.open()).unwrap(), g);
    }
}
