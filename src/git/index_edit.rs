//! Editing an index git wrote, in place of git: skip-worktree set or cleared
//! on given paths, and their stat data and object id replaced, everything
//! else kept (the index version, the other entries' bytes, the extensions).
//!
//! gix-index can't do this: it writes only v2 and v3, so a v4 index would
//! nearly double in size, and it drops extensions it doesn't know. Hence a
//! small transcoder: one pass over the entries, copying the bytes of those
//! not edited. Toggling 5,000 paths in a 300k-entry v4 index (21 MB) takes
//! about 50 ms in a release build (`edit_300k_entries`): read, checksum
//! checked, rewritten, checksummed, synced and renamed.
//!
//! Having git do the toggling (`git update-index --stdin`) took ~115 ms for
//! 500k entries, measured.
//!
//! Indexes it refuses (`Refused`): split (`link`), sparse (`sdir`), or with
//! another extension git requires to read it. A worktree with
//! `core.sparseCheckout` (`sparse_checkout`) has its skip-worktree bits
//! rewritten by git, so callers check that too.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use sha1::{Digest, Sha1};

use super::cmd;
use super::index::{EXTENDED, NAME_MASK, SKIP_WORKTREE};

/// Bytes of an entry before its flags: ten 32-bit stat fields, the object id.
const STAT_LEN: usize = 40;
const OID_LEN: usize = 20;
const FLAGS_AT: usize = STAT_LEN + OID_LEN;
const HEADER_LEN: usize = 12;

/// Stat data of an index entry, as git compares it with `lstat` (each field
/// the low 32 bits).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    /// Seconds and nanoseconds.
    pub ctime: (u32, u32),
    pub mtime: (u32, u32),
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

/// What to change in a path's entry (stage 0); `None` keeps it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Edit {
    pub skip_worktree: Option<bool>,
    pub stat: Option<Stat>,
    pub oid: Option<ObjectId>,
}

/// What an edit did.
#[derive(Debug, Default)]
pub struct Edited {
    /// Entries whose bytes changed.
    pub changed: usize,
    /// Paths edited that have no stage-0 entry (absent, or unmerged).
    pub missing: Vec<Vec<u8>>,
}

/// An index this module won't edit, or can't now.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// `<index>.lock` exists: git (or another writer) is writing the index.
    Locked(PathBuf),
    /// A split index (`link`, `core.splitIndex`).
    SplitIndex,
    /// A sparse index (`sdir`): directories as entries.
    SparseIndex,
    /// Another extension git must understand to read the index.
    Extension(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Refused::Locked(lock) => write!(f, "git is busy: {} exists", lock.display()),
            Refused::SplitIndex => f.write_str("the index is split (core.splitIndex)"),
            Refused::SparseIndex => f.write_str("the index is sparse (index.sparse)"),
            Refused::Extension(sig) => write!(f, "the index has extension {sig:?}"),
        }
    }
}

impl std::error::Error for Refused {}

/// Applies `edits` (by path) to the index at `index`, under git's lock
/// protocol: `<index>.lock` created exclusively, written, synced, renamed over
/// the index. Fails with `Refused::Locked` if the lock exists, without waiting.
/// Nothing is written if nothing changes.
pub fn edit(index: &Path, edits: &HashMap<Vec<u8>, Edit>) -> Result<Edited> {
    let mut lock_name = index.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = PathBuf::from(lock_name);
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&lock) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Err(Refused::Locked(lock).into()),
        opened => opened.with_context(|| format!("creating {}", lock.display()))?,
    };
    let written = (|| {
        let data = fs::read(index).with_context(|| format!("reading {}", index.display()))?;
        let (out, edited) =
            transcode(&data, edits).with_context(|| format!("editing {}", index.display()))?;
        if out == data {
            return Ok((edited, false));
        }
        file.write_all(&out)?;
        file.sync_all()?;
        Ok((edited, true))
    })();
    match written {
        Ok((edited, true)) => {
            fs::rename(&lock, index)
                .inspect_err(|_| drop(fs::remove_file(&lock)))
                .with_context(|| format!("renaming {}", lock.display()))?;
            Ok(edited)
        }
        // Nothing renamed: the lock is still ours to remove (once renamed,
        // a lock at that path would be someone else's).
        result => {
            let _ = fs::remove_file(&lock);
            result.map(|(edited, _)| edited)
        }
    }
}

/// The index `data` with `edits` applied (see `edit`). Its version is kept,
/// except that v2 and v3 are written as git does: v3 if an entry has extended
/// flags, else v2. `EOIE` and `IEOT` are dropped (offsets into the entries,
/// which may have moved; git writes them again), and `TREE` invalidated along
/// the path of each entry whose object id changed. If an entry changed,
/// `FSMN` is dropped too: it would vouch that the entry's file is unchanged
/// since the last fsmonitor query, so git wouldn't compare it (git then
/// starts over with a full scan).
pub fn transcode(data: &[u8], edits: &HashMap<Vec<u8>, Edit>) -> Result<(Vec<u8>, Edited)> {
    let mut r = Reader::new(data)?;
    let mut out = Vec::with_capacity(data.len() + 2 * edits.len());
    out.extend_from_slice(&data[..HEADER_LEN]);
    let mut extended = false;
    let mut edited = Edited::default();
    let mut found = HashSet::new();
    // Directories (`a/b`, the root as "") whose cache tree an edit invalidated.
    let mut stale_dirs = HashSet::new();
    while let Some(e) = r.next_entry()? {
        let edit = match (e.stage(), edits.get_key_value(r.path.as_slice())) {
            (0, Some((path, edit))) => {
                found.insert(path.as_slice());
                edit
            }
            _ => {
                extended |= e.ext != 0;
                out.extend_from_slice(&data[e.start..e.end]);
                continue;
            }
        };
        let at = out.len();
        out.extend_from_slice(&data[e.start..e.start + STAT_LEN]);
        if let Some(s) = edit.stat {
            // The stat fields, mode (index 6) skipped.
            let fields = [
                (0, s.ctime.0),
                (1, s.ctime.1),
                (2, s.mtime.0),
                (3, s.mtime.1),
                (4, s.dev),
                (5, s.ino),
                (7, s.uid),
                (8, s.gid),
                (9, s.size),
            ];
            for (i, v) in fields {
                out[at + 4 * i..at + 4 * i + 4].copy_from_slice(&v.to_be_bytes());
            }
        }
        let oid = &data[e.start + STAT_LEN..e.start + FLAGS_AT];
        match edit.oid {
            Some(new) if new.as_bytes() != oid => {
                out.extend_from_slice(new.as_bytes());
                let path = r.path.as_slice();
                stale_dirs.insert(Vec::new());
                for (i, _) in path.iter().enumerate().filter(|(_, b)| **b == b'/') {
                    stale_dirs.insert(path[..i].to_vec());
                }
            }
            _ => out.extend_from_slice(oid),
        }
        let mut ext = e.ext;
        match edit.skip_worktree {
            Some(true) => ext |= SKIP_WORKTREE,
            Some(false) => ext &= !SKIP_WORKTREE,
            None => {}
        }
        // Without extended flags left, the word goes (as git writes it).
        let flags = if ext != 0 {
            e.flags | EXTENDED
        } else {
            e.flags & !EXTENDED
        };
        out.extend_from_slice(&flags.to_be_bytes());
        if ext != 0 {
            out.extend_from_slice(&ext.to_be_bytes());
            extended = true;
        }
        if r.version == 4 {
            // The prefix-compressed path, unchanged.
            out.extend_from_slice(&data[e.name..e.end]);
        } else {
            // NUL-padded to a multiple of 8 bytes, at least one NUL.
            let name = &r.path;
            let len = (out.len() - at + name.len() + 8) & !7;
            out.extend_from_slice(name);
            out.resize(at + len, 0);
        }
        if out[at..] != data[e.start..e.end] {
            edited.changed += 1;
        }
    }
    edited.missing = edits
        .keys()
        .filter(|p| !found.contains(p.as_slice()))
        .cloned()
        .collect();

    while let Some((sig, body)) = r.next_extension()? {
        match &sig {
            b"link" => bail!(Refused::SplitIndex),
            b"sdir" => bail!(Refused::SparseIndex),
            b"EOIE" | b"IEOT" => continue,
            b"FSMN" if edited.changed > 0 => continue,
            // Optional extensions start with an uppercase letter.
            s if !s[0].is_ascii_uppercase() => {
                bail!(Refused::Extension(String::from_utf8_lossy(s).into_owned()))
            }
            b"TREE" if !stale_dirs.is_empty() => {
                let mut tree = Vec::with_capacity(body.len());
                invalidate_tree(body, &mut 0, &mut Vec::new(), &stale_dirs, &mut tree)
                    .context("reading the TREE extension")?;
                out.extend_from_slice(b"TREE");
                out.extend_from_slice(&(tree.len() as u32).to_be_bytes());
                out.extend_from_slice(&tree);
            }
            _ => {
                out.extend_from_slice(&sig);
                out.extend_from_slice(&(body.len() as u32).to_be_bytes());
                out.extend_from_slice(body);
            }
        }
    }
    let version: u32 = match r.version {
        4 => 4,
        _ if extended => 3,
        _ => 2,
    };
    out[4..8].copy_from_slice(&version.to_be_bytes());
    if r.hashed {
        let digest = Sha1::digest(&out);
        out.extend_from_slice(&digest);
    } else {
        // `index.skipHash`: a null trailer.
        out.extend_from_slice(&[0; OID_LEN]);
    }
    Ok((out, edited))
}

/// Whether `core.sparseCheckout` is on for the git directory `git_dir` (a
/// worktree's admin directory reads its `config.worktree` too). Git then
/// re-applies the sparse patterns now and then, rewriting skip-worktree bits.
pub fn sparse_checkout(git_dir: &Path) -> Result<bool> {
    let out = cmd::command(git_dir)
        .args(["config", "--type=bool", "--get", "core.sparseCheckout"])
        .output()
        .context("running git config")?;
    match out.status.code() {
        Some(0) => Ok(out.stdout.trim_ascii() == b"true"),
        // Unset.
        Some(1) => Ok(false),
        _ => bail!(
            "git config core.sparseCheckout failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Copies the cache tree node at `pos` of `data` (a `TREE` extension) and its
/// subtrees to `out`, marking invalid those whose directory (`dir` and the
/// node's name) is in `stale`.
fn invalidate_tree(
    data: &[u8],
    pos: &mut usize,
    dir: &mut Vec<u8>,
    stale: &HashSet<Vec<u8>>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let start = *pos;
    let rest = &data[start..];
    let nul = rest.iter().position(|&b| b == 0).context("truncated")?;
    let lf = rest[nul..]
        .iter()
        .position(|&b| b == b'\n')
        .context("truncated")?
        + nul;
    let line = std::str::from_utf8(&rest[nul + 1..lf])?;
    let (count, subtrees) = line.split_once(' ').context("malformed")?;
    let count: i64 = count.parse()?;
    let subtrees: u32 = subtrees.parse()?;
    let mut end = start + lf + 1;
    if count >= 0 {
        end += OID_LEN;
        if end > data.len() {
            bail!("truncated");
        }
    }
    let len = dir.len();
    if start > 0 {
        // Below the root: `name` or `parent/name`.
        if !dir.is_empty() {
            dir.push(b'/');
        }
        dir.extend_from_slice(&rest[..nul]);
    }
    if count >= 0 && stale.contains(dir.as_slice()) {
        out.extend_from_slice(&rest[..=nul]);
        // Writing to a Vec can't fail.
        let _ = writeln!(out, "-1 {subtrees}");
    } else {
        out.extend_from_slice(&data[start..end]);
    }
    *pos = end;
    for _ in 0..subtrees {
        invalidate_tree(data, pos, dir, stale, out)?;
    }
    dir.truncate(len);
    Ok(())
}

/// The object ids of `paths`' stage-0 entries in the index `data` (those
/// it has).
pub fn oids_of(data: &[u8], paths: &HashSet<Vec<u8>>) -> Result<HashMap<Vec<u8>, ObjectId>> {
    let mut r = Reader::new(data)?;
    let mut out = HashMap::new();
    while let Some(e) = r.next_entry()? {
        if e.stage() == 0 && paths.contains(r.path.as_slice()) {
            let oid = &data[e.start + STAT_LEN..e.start + FLAGS_AT];
            out.insert(r.path.clone(), ObjectId::try_from(oid)?);
        }
    }
    Ok(out)
}

/// An entry's place in the index and its flags.
struct Entry {
    start: usize,
    /// Where its path starts (in v4, the prefix length to drop).
    name: usize,
    end: usize,
    flags: u16,
    ext: u16,
}

impl Entry {
    fn stage(&self) -> u16 {
        (self.flags >> 12) & 3
    }
}

/// Reads an index's entries, then its extensions, checking the checksum.
struct Reader<'a> {
    /// The index without its trailing checksum.
    data: &'a [u8],
    version: u32,
    left: u32,
    pos: usize,
    /// The path of the last entry read.
    path: Vec<u8>,
    /// Whether the trailer is a checksum (else null: `index.skipHash`).
    hashed: bool,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < HEADER_LEN + OID_LEN || &data[..4] != b"DIRC" {
            bail!("not a git index");
        }
        let version = be32(data, 4);
        if !(2..=4).contains(&version) {
            bail!("index version {version} is not supported");
        }
        let (body, trailer) = data.split_at(data.len() - OID_LEN);
        let hashed = trailer != [0; OID_LEN];
        if hashed && Sha1::digest(body).as_slice() != trailer {
            bail!("index checksum mismatch");
        }
        Ok(Reader {
            data: body,
            version,
            left: be32(data, 8),
            pos: HEADER_LEN,
            path: Vec::new(),
            hashed,
        })
    }

    fn next_entry(&mut self) -> Result<Option<Entry>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        let data = self.data;
        let start = self.pos;
        let truncated = || anyhow::anyhow!("index entry at byte {start} is truncated");
        if start + FLAGS_AT + 2 > data.len() {
            return Err(truncated());
        }
        let flags = be16(data, start + FLAGS_AT);
        let mut name = start + FLAGS_AT + 2;
        let mut ext = 0;
        if flags & EXTENDED != 0 {
            if self.version < 3 || name + 2 > data.len() {
                bail!("index entry at byte {start}: unexpected extended flags");
            }
            ext = be16(data, name);
            name += 2;
        }
        let mut path_at = name;
        if self.version == 4 {
            let (strip, len) = read_varint(&data[name..]).ok_or_else(truncated)?;
            let keep = self
                .path
                .len()
                .checked_sub(strip as usize)
                .ok_or_else(truncated)?;
            self.path.truncate(keep);
            path_at += len;
        } else {
            self.path.clear();
        }
        let nul = data[path_at..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(truncated)?;
        self.path.extend_from_slice(&data[path_at..path_at + nul]);
        let end = if self.version == 4 {
            path_at + nul + 1
        } else {
            start + ((name - start + nul + 8) & !7)
        };
        let len = flags & NAME_MASK;
        if end > data.len() || (len < NAME_MASK && usize::from(len) != self.path.len()) {
            return Err(truncated());
        }
        self.pos = end;
        Ok(Some(Entry {
            start,
            name,
            end,
            flags,
            ext,
        }))
    }

    /// The next extension's signature and body, once the entries are read.
    fn next_extension(&mut self) -> Result<Option<([u8; 4], &'a [u8])>> {
        let data = self.data;
        if self.pos == data.len() {
            return Ok(None);
        }
        let at = self.pos;
        if at + 8 > data.len() {
            bail!("index extension at byte {at} is truncated");
        }
        let sig: [u8; 4] = data[at..at + 4].try_into().expect("4 bytes");
        let end = at + 8 + be32(data, at + 4) as usize;
        if end > data.len() {
            bail!("index extension at byte {at} is truncated");
        }
        self.pos = end;
        Ok(Some((sig, &data[at + 8..end])))
    }
}

fn be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().expect("4 bytes"))
}

fn be16(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(data[at..at + 2].try_into().expect("2 bytes"))
}

/// Git's offset varint (`varint.c`): the value and its length in bytes.
fn read_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &b) in data.iter().enumerate().take(10) {
        if i > 0 {
            value = value.checked_add(1)?.checked_shl(7)?;
        }
        value |= u64::from(b & 127);
        if b & 128 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::index::tests::{age, git, git_in, repo, scratch, staged};
    use crate::git::index::{IndexWriter, StatInfo};
    use gix::objs::tree::EntryKind;
    use std::os::unix::fs::MetadataExt;
    use std::time::Instant;

    const FILES: [(&str, &str); 4] = [
        ("a.txt", "a\n"),
        ("d/b.txt", "b\n"),
        ("d/e/c.txt", "c\n"),
        ("z.txt", "z\n"),
    ];

    /// A repository in `dir` with `FILES` committed, an index of `version`
    /// with `TREE` and `UNTR` (the untracked cache, with `u.txt` untracked).
    /// Git writes v3 only if an entry has extended flags, so for v3 `z.txt`
    /// is skip-worktree.
    fn repo_at(dir: &Path, version: u32) {
        repo(dir, &FILES);
        git(dir, &["config", "core.untrackedCache", "true"]);
        fs::write(dir.join("u.txt"), "u\n").unwrap();
        age(&dir.join("u.txt"));
        git(
            dir,
            &["update-index", "--index-version", &version.to_string()],
        );
        if version == 3 {
            git(dir, &["update-index", "--skip-worktree", "z.txt"]);
        }
        git(dir, &["status", "--porcelain"]);
    }

    /// Where `a` and `b` first differ, for assertion messages.
    fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
        (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i))
    }

    /// An extension's signature and body.
    type Extension<'a> = ([u8; 4], &'a [u8]);

    /// The header and entries of index `data`, and its extensions.
    fn parts(data: &[u8]) -> (&[u8], Vec<Extension<'_>>) {
        let mut r = Reader::new(data).unwrap();
        while r.next_entry().unwrap().is_some() {}
        let entries = &data[..r.pos];
        let mut extensions = Vec::new();
        while let Some(ext) = r.next_extension().unwrap() {
            extensions.push(ext);
        }
        (entries, extensions)
    }

    fn signatures(data: &[u8]) -> Vec<String> {
        let (_, extensions) = parts(data);
        let sig = |s: &[u8]| String::from_utf8_lossy(s).into_owned();
        extensions.iter().map(|(s, _)| sig(s)).collect()
    }

    fn edits(paths: &[&str], edit: Edit) -> HashMap<Vec<u8>, Edit> {
        paths
            .iter()
            .map(|p| (p.as_bytes().to_vec(), edit))
            .collect()
    }

    fn skip(on: bool) -> Edit {
        Edit {
            skip_worktree: Some(on),
            ..Edit::default()
        }
    }

    fn refusal(e: anyhow::Error) -> Refused {
        e.downcast::<Refused>().unwrap()
    }

    /// Without edits, git's index of each version comes out byte for byte,
    /// a path longer than the flags' length field included.
    #[test]
    fn round_trip() {
        for version in 2..=4 {
            let dir = scratch(&format!("round-trip-{version}"));
            repo_at(&dir, version);
            let long = ["x"; 1500].join("/");
            let blob = staged(&dir, "a.txt").to_string();
            git(
                &dir,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{long}"),
                ],
            );
            let data = fs::read(dir.join(".git/index")).unwrap();
            assert_eq!(be32(&data, 4), version);
            assert_eq!(signatures(&data), ["TREE", "UNTR"]);
            let (out, edited) = transcode(&data, &HashMap::new()).unwrap();
            assert!(out == data, "v{version} changed");
            assert_eq!(edited.changed, 0);

            edit(&dir.join(".git/index"), &edits(&[&long], skip(true))).unwrap();
            assert!(git(&dir, &["ls-files", "-v"]).contains(&format!("S {long}\n")));
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Setting skip-worktree writes what `git update-index --skip-worktree`
    /// does (v2 becomes v3 as with git); git agrees on the bits and the
    /// status; clearing them gives back the original index.
    #[test]
    fn skip_worktree_as_git_does() {
        for version in 2..=4 {
            let dir = scratch(&format!("skip-{version}"));
            repo_at(&dir, version);
            let index = dir.join(".git/index");
            let original = fs::read(&index).unwrap();
            git(
                &dir,
                &["update-index", "--skip-worktree", "a.txt", "d/e/c.txt"],
            );
            let theirs = fs::read(&index).unwrap();
            fs::write(&index, &original).unwrap();

            let paths = ["a.txt", "d/e/c.txt", "nope"];
            let edited = edit(&index, &edits(&paths, skip(true))).unwrap();
            assert_eq!(edited.changed, 2);
            assert_eq!(edited.missing, [b"nope".to_vec()]);
            let ours = fs::read(&index).unwrap();
            // Git also invalidates the cache tree above the paths, needlessly.
            let (ours_entries, mut ours_ext) = parts(&ours);
            let (theirs_entries, mut theirs_ext) = parts(&theirs);
            let at = first_difference(ours_entries, theirs_entries);
            assert!(
                at.is_none(),
                "v{version}: differs from git's at byte {at:?}"
            );
            ours_ext.retain(|(sig, _)| sig != b"TREE");
            theirs_ext.retain(|(sig, _)| sig != b"TREE");
            assert_eq!(ours_ext, theirs_ext);
            assert_eq!(
                git(&dir, &["write-tree"]),
                git(&dir, &["rev-parse", "HEAD^{tree}"])
            );
            assert_eq!(be32(&ours, 4), version.max(3));
            let z = if version == 3 { 'S' } else { 'H' };
            assert_eq!(
                git(&dir, &["ls-files", "-v"]),
                format!("S a.txt\nH d/b.txt\nS d/e/c.txt\n{z} z.txt\n")
            );
            assert_eq!(git(&dir, &["status", "--porcelain"]), "?? u.txt\n");
            git(&dir, &["fsck", "--no-progress"]);

            // Git rewrote the index meanwhile (`status`); clear the bits of
            // the one written above.
            let (cleared, _) = transcode(&ours, &edits(&paths, skip(false))).unwrap();
            assert!(cleared == original, "v{version}: not back to the original");
            edit(&index, &edits(&paths, skip(false))).unwrap();
            assert_eq!(
                git(&dir, &["ls-files", "-v"]).matches("S ").count(),
                (version == 3) as usize
            );
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Replacing an entry's id and stat data stages that blob, clean against
    /// the file it was taken from, and invalidates the cache tree above it:
    /// `write-tree` gives what `git add` would.
    #[test]
    fn stat_and_id_replaced() {
        let dir = scratch("stat");
        repo_at(&dir, 4);
        let file = dir.join("d/e/c.txt");
        fs::write(&file, "new\n").unwrap();
        age(&file);
        let blob = git(&dir, &["hash-object", "-w", "d/e/c.txt"]);
        let m = fs::metadata(&file).unwrap();
        let edit = Edit {
            skip_worktree: None,
            stat: Some(Stat {
                ctime: (m.ctime() as u32, m.ctime_nsec() as u32),
                mtime: (m.mtime() as u32, m.mtime_nsec() as u32),
                dev: m.dev() as u32,
                ino: m.ino() as u32,
                uid: m.uid(),
                gid: m.gid(),
                size: m.size() as u32,
            }),
            oid: Some(ObjectId::from_hex(blob.trim().as_bytes()).unwrap()),
        };
        let index = dir.join(".git/index");
        edit_one(&index, "d/e/c.txt", edit);
        assert_eq!(signatures(&fs::read(&index).unwrap()), ["TREE", "UNTR"]);
        assert_eq!(
            git(&dir, &["status", "--porcelain"]),
            "M  d/e/c.txt\n?? u.txt\n"
        );
        let debug = git(&dir, &["ls-files", "--debug", "d/e/c.txt"]);
        assert!(
            debug.contains(&format!("ino: {}", m.ino() as u32)),
            "{debug}"
        );
        git(&dir, &["fsck", "--no-progress"]);
        let ours = git(&dir, &["write-tree"]);
        git(&dir, &["reset", "-q"]);
        git(&dir, &["add", "d/e/c.txt"]);
        assert_eq!(ours, git(&dir, &["write-tree"]));
        assert_ne!(ours, git(&dir, &["rev-parse", "HEAD^{tree}"]));
        fs::remove_dir_all(dir).unwrap();
    }

    fn edit_one(index: &Path, path: &str, e: Edit) {
        let edited = edit(index, &edits(&[path], e)).unwrap();
        assert_eq!(edited.changed, 1);
    }

    /// `EOIE` and `IEOT` (written by git with `index.threads`) are dropped,
    /// and git, threads on, still reads the index.
    #[test]
    fn offsets_dropped() {
        let dir = scratch("offsets");
        repo(&dir, &FILES);
        let blob = staged(&dir, "a.txt");
        let mut info = String::new();
        for i in 0..30_000 {
            info += &format!("100644 {blob} 0\tmany/{}/f{i}\n", i / 1000);
        }
        git_in(
            &dir,
            &["update-index", "--add", "--index-info"],
            info.as_bytes(),
        );
        let threads = ["-c", "index.threads=4"];
        let records = [
            "-c",
            "index.recordEndOfIndexEntries=true",
            "-c",
            "index.recordOffsetTable=true",
        ];
        let args = [
            &threads[..],
            &records[..],
            &["update-index", "--index-version", "4"],
        ];
        git(&dir, &args.concat());
        let index = dir.join(".git/index");
        let sigs = signatures(&fs::read(&index).unwrap());
        assert!(
            sigs.contains(&"EOIE".into()) && sigs.contains(&"IEOT".into()),
            "{sigs:?}"
        );

        edit_one(&index, "many/7/f7777", skip(true));
        let sigs = signatures(&fs::read(&index).unwrap());
        assert!(
            !sigs.contains(&"EOIE".into()) && !sigs.contains(&"IEOT".into()),
            "{sigs:?}"
        );
        let listed = git(&dir, &[&threads[..], &["ls-files", "-v"]].concat());
        assert_eq!(listed.lines().filter(|l| l.starts_with("S ")).count(), 1);
        assert!(listed.contains("S many/7/f7777\n"));
        git(&dir, &[&threads[..], &["status", "--porcelain"]].concat());
        fs::remove_dir_all(dir).unwrap();
    }

    /// Split and sparse indexes are refused, and so is a locked index, with
    /// the lock left alone.
    #[test]
    fn refusals() {
        let dir = scratch("refusals");
        repo(&dir, &[("d/a.txt", "a\n"), ("e/b.txt", "b\n")]);
        let index = dir.join(".git/index");
        let lock = dir.join(".git/index.lock");
        fs::write(&lock, "").unwrap();
        let e = edit(&index, &edits(&["d/a.txt"], skip(true))).unwrap_err();
        assert_eq!(refusal(e), Refused::Locked(lock.clone()));
        assert!(lock.exists());
        fs::remove_file(&lock).unwrap();

        let git_dir = dir.join(".git");
        assert!(!sparse_checkout(&git_dir).unwrap());
        git(
            &dir,
            &["sparse-checkout", "set", "--cone", "--sparse-index", "d"],
        );
        assert!(sparse_checkout(&git_dir).unwrap());
        let e = edit(&index, &edits(&["d/a.txt"], skip(true))).unwrap_err();
        assert_eq!(refusal(e), Refused::SparseIndex);

        git(&dir, &["sparse-checkout", "disable"]);
        git(&dir, &["update-index", "--split-index"]);
        let e = edit(&index, &edits(&["d/a.txt"], skip(true))).unwrap_err();
        assert_eq!(refusal(e), Refused::SplitIndex);
        assert!(!lock.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// The time to set skip-worktree on 5,000 paths of a 300k-entry v4 index
    /// (300 × 10 × 100 files), written and renamed under the lock. Run with
    /// `cargo test --release edit_300k -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn edit_300k_entries() {
        let dir = scratch("300k");
        let oid = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let mut w = IndexWriter::new(StatInfo { uid: 0, gid: 0 }, 0);
        let mut paths = Vec::new();
        for a in 0..300 {
            for b in 0..10 {
                for c in 0..100 {
                    paths.push(format!("dir{a:03}/sub{b}/file{c:02}.txt"));
                }
            }
        }
        for (i, p) in paths.iter().enumerate() {
            w.push(p.as_bytes(), EntryKind::Blob, &oid, 0, i as u32);
        }
        let index = dir.join("index");
        fs::write(&index, w.finish()).unwrap();
        let some: Vec<&str> = paths.iter().step_by(60).map(String::as_str).collect();
        assert_eq!(some.len(), 5000);
        for on in [true, false, true] {
            let t = Instant::now();
            let edited = edit(&index, &edits(&some, skip(on))).unwrap();
            let size = fs::metadata(&index).unwrap().len();
            println!(
                "{} entries, {size} bytes: {:?}",
                edited.changed,
                t.elapsed()
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
