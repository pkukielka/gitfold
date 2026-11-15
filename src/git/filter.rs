//! How git sees a mixed file: a clean/smudge filter that gitfold answers.
//!
//! A union file (`merge=union`) shows the workspace's own lines among other
//! layers' lines. git must see only the workspace's own, with HEAD on its
//! branch. So each such path gets the attribute `filter=gitfold-<id>`, in the
//! worktree's own attributes file, and the driver is defined only in the
//! worktree's own config, with `required = true` (`worktree_config::set_filter`):
//! - **clean** (git reads the file into an object: `add`, `status`, `diff`,
//!   `commit -a`, `stash`) strips the layers' lines;
//! - **smudge** (git writes an object out: `checkout`, `switch`, `reset`,
//!   `stash pop`, `merge`) puts them back.
//!
//! The same filter covers a file only editable layers changed (`Edited`):
//! there clean also strips the workspace's waiting edits (its changes to
//! the layers' uncommitted lines), and smudge puts them back.
//!
//! The index, commits and stashes hold only the workspace's lines; the file
//! shows everyone's. Both are a function of the file's two versions the view
//! has (`Versions`): the base's (below the layers) and the union (with the
//! layers' lines). Clean reverts the change from the base's version to the
//! union in the content it gets (a 3-way merge); where that conflicts (the
//! workspace's lines sit next to the layers'), the layers' added lines are
//! taken out, once each. Smudge applies the same change (a 3-way merge,
//! both sides kept where they touch, the layers' lines first, as a switch
//! places the workspace's own after them: git's `merge=union`).
//!
//! git talks to the driver with its long-running filter protocol
//! (`filter.<driver>.process`, gitprotocol-long-running-process(5)): one
//! process per git command, started only if a filtered path is converted.
//! That process (`gitfold filter-process`, `serve_git`) relays each request
//! to the daemon over `<common>/gitfold/.filter` (`Frame`), which answers it
//! from the workspace's view as it is now (`fs::workspace::layers`).

use std::io::{BufRead, Read, Write};

use anyhow::{Context, Result, bail};

use std::collections::{BTreeSet, HashSet};

use super::guard::{is_marker, norm, significant};
use super::layers::{
    added_lines, is_binary, keep_conflicts, key, lines, merge3, merge3_marked, without_lines,
};

/// The two versions of a union file a filter request is answered from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Versions {
    /// The base's version, below the layers.
    pub base: Vec<u8>,
    /// The union: the base's version with the layers' lines.
    pub union: Vec<u8>,
}

impl Versions {
    /// `work` (the file as it is) without the layers' lines.
    pub fn clean(&self, work: &[u8]) -> Vec<u8> {
        let Versions { base, union } = self;
        if work == union.as_slice() {
            return base.clone();
        }
        if base == union || [base, union].iter().any(|d| is_binary(d)) || is_binary(work) {
            return work.to_vec();
        }
        if let Some(clean) = merge3(union, work, base, keep_conflicts()) {
            return clean;
        }
        without_lines(work, &added_lines(base, union))
    }

    /// `own` (what git has) with the layers' lines.
    pub fn smudge(&self, own: &[u8]) -> Vec<u8> {
        let Versions { base, union } = self;
        if own == base.as_slice() {
            return union.clone();
        }
        if base == union || [base, union].iter().any(|d| is_binary(d)) || is_binary(own) {
            return own.to_vec();
        }
        if let Some(merged) = merge3(base, union, own, keep_conflicts()) {
            return merged;
        }
        // Where they touch: both sides kept, the layers' first (git's own
        // `merge=union`), if clean turns that back into `own` exactly (a
        // union of the two sides can bring back a line `own` removed); else
        // the other way round; else both sides kept all the same: never
        // `own` alone, which would lose the layers' lines from the file
        // (they would then look deleted by the workspace).
        let union_style = gix::merge::blob::builtin_driver::text::Conflict::ResolveWithUnion;
        let tries: [&dyn Fn() -> Option<Vec<u8>>; 4] = [
            &|| merge3(base, union, own, union_style),
            &|| place(base, union, own, true),
            &|| place(base, union, own, false),
            &|| merge3(base, own, union, union_style),
        ];
        if let Some(merged) = (tries.iter())
            .filter_map(|t| t())
            .find(|merged| self.clean(merged) == own)
        {
            return merged;
        }
        merge3(base, union, own, union_style)
            .unwrap_or_else(|| merge3_marked(base, union, own, "the layers' lines", GIT_LABEL).0)
    }
}

/// The two versions of a file editable layers changed (`state::layers`'s
/// `edited`) a filter request is answered from. Unlike a union file's
/// lines, the layers' lines here are the workspace's to change, but not
/// yet to commit (its branch doesn't have them): changing them is a
/// waiting edit, kept in the file and out of git until the layer commits
/// those lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edited {
    /// The base's version, below the layers.
    pub base: Vec<u8>,
    /// The version shown: the base's with the editable layers' lines.
    pub shown: Vec<u8>,
}

/// A file of `Edited` split into the workspace's own change and its
/// waiting edits (see `Edited::split`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Split {
    /// The base's version with the workspace's own changes: what git sees.
    pub own: Vec<u8>,
    /// The version shown with only the waiting edits: what stays in the file
    /// when git takes its own changes out (a checkout, a stash).
    pub kept: Vec<u8>,
    /// How many waiting edits (hunks) there are.
    pub waiting: usize,
    /// The lines of the version shown the waiting edits change or sit
    /// next to (indexes), or whose content they move: whose they are.
    pub touched: Vec<usize>,
}

/// One hunk of the change from the version shown to the file as it is.
struct Hunk {
    /// The lines of the version shown it replaces.
    shown: std::ops::Range<usize>,
    /// The file's lines it puts there.
    work: std::ops::Range<usize>,
    waiting: bool,
}

/// The label of the side of a conflict a smudge keeps in the file.
const KEPT_LABEL: &str = "the layers' work with your waiting edits";
/// The label of git's side.
const GIT_LABEL: &str = "the version git wrote";

impl Edited {
    /// The versions of a file shown as `shown` over the base's version
    /// `base`, when the branch's tip has `tip` (the base waits while layers
    /// show, so commits made meanwhile are the tip's only): git's own is the
    /// tip's, and the version shown is rebased onto it (the layers' change
    /// merged into `tip`), so what the workspace committed meanwhile is
    /// neither its change nor a waiting edit. Where the layers' change
    /// conflicts with those commits, the version shown as it is: the
    /// workspace's lines there, changing the layers', wait.
    pub fn on_tip(base: Vec<u8>, shown: Vec<u8>, tip: Vec<u8>) -> Edited {
        if tip == base {
            return Edited { base, shown };
        }
        let binary = [&base, &shown, &tip].iter().any(|d| is_binary(d));
        let shown = match binary {
            true => shown,
            false => merge3(&base, &shown, &tip, keep_conflicts()).unwrap_or(shown),
        };
        Edited { base: tip, shown }
    }

    /// `work` (the file as it is) split, hunk by hunk of its change from
    /// the version shown. A hunk is a waiting edit (it depends on the
    /// layers' lines) if it changes or deletes a layer's line, inserts lines
    /// with a layer's lines on both sides, spans lines of the base the
    /// layers deleted, puts back a layer's line (by content, wherever: a
    /// move, a reordering, a formatter's), touches a conflict (git's
    /// markers and what is between them), or moves lines another waiting
    /// edit takes away or puts in. Any other is the workspace's own, applied
    /// to the base's version where its lines are.
    ///
    /// Fail-safe: the split must give no layer's line or marker to git and
    /// lose nothing when git writes its version back (`rebuilds`). If it
    /// doesn't, the own hunks next to the layers' lines or to waiting ones
    /// wait too, and if that isn't enough, every change does: git then sees
    /// none of the file's changes rather than a wrong one. In a binary file
    /// every change waits.
    pub fn split(&self, work: &[u8]) -> Split {
        let Edited { base, shown } = self;
        if work == shown.as_slice() {
            return Split {
                own: base.clone(),
                kept: shown.clone(),
                ..Default::default()
            };
        }
        if [base, shown].iter().any(|d| is_binary(d)) || is_binary(work) {
            return self.all_waiting(work, 1);
        }
        let b: Vec<&[u8]> = lines(base).collect();
        let m: Vec<&[u8]> = lines(shown).collect();
        let w: Vec<&[u8]> = lines(work).collect();
        // Each shown line's base line (`None`: a layer's), and where the
        // layers deleted base lines (before shown line i: `gap[i]`).
        let mut from_base: Vec<Option<usize>> = vec![None; m.len()];
        let mut gap = vec![false; m.len() + 1];
        let (mut bi, mut mi) = (0usize, 0usize);
        for hunk in diff(base, shown) {
            while mi < hunk.after.start as usize {
                from_base[mi] = Some(bi);
                (mi, bi) = (mi + 1, bi + 1);
            }
            if hunk.after.is_empty() {
                gap[mi] = true;
            }
            (mi, bi) = (hunk.after.end as usize, hunk.before.end as usize);
        }
        while mi < m.len() {
            from_base[mi] = Some(bi);
            (mi, bi) = (mi + 1, bi + 1);
        }
        let layers_line = |i: usize| from_base.get(i).is_some_and(Option::is_none);
        // The layers' lines by content: shown lines that aren't the base's,
        // with words the base has nowhere.
        let base_lines: HashSet<Vec<u8>> = b.iter().map(|l| norm(key(l))).collect();
        let theirs: HashSet<Vec<u8>> = (0..m.len())
            .filter(|&i| layers_line(i))
            .map(|i| key(m[i]))
            .filter(|l| significant(l) && !is_marker(l))
            .map(norm)
            .filter(|n| !base_lines.contains(n))
            .collect();
        let is_theirs = |line: &[u8]| significant(key(line)) && theirs.contains(&norm(key(line)));
        let conflicted = conflict_lines(&w);
        let mut hunks: Vec<Hunk> = diff(shown, work)
            .map(|h| {
                let (s, e) = (h.before.start as usize, h.before.end as usize);
                let work = h.after.start as usize..h.after.end as usize;
                let waiting = match s == e {
                    true => s > 0 && layers_line(s - 1) && layers_line(s),
                    false => (s..e).any(layers_line) || (s + 1..e).any(|i| gap[i]),
                } || w[work.clone()].iter().any(|l| is_theirs(l))
                    || work.clone().any(|i| conflicted[i]);
                Hunk {
                    shown: s..e,
                    work,
                    waiting,
                }
            })
            .collect();
        // Lines moved from or to a waiting edit wait with it.
        loop {
            let moved: HashSet<Vec<u8>> = (hunks.iter().filter(|h| h.waiting))
                .flat_map(|h| m[h.shown.clone()].iter().chain(&w[h.work.clone()]))
                .map(|l| key(l))
                .filter(|l| significant(l))
                .map(norm)
                .collect();
            let mut more = false;
            for h in hunks.iter_mut().filter(|h| !h.waiting) {
                let mut lines = m[h.shown.clone()].iter().chain(&w[h.work.clone()]);
                if lines.any(|l| significant(key(l)) && moved.contains(&norm(key(l)))) {
                    h.waiting = true;
                    more = true;
                }
            }
            if !more {
                break;
            }
        }
        let assemble = |hunks: &[Hunk]| -> Split {
            // Own hunks as (base range, new lines); waiting ones as (shown
            // range, new lines).
            let mut own: Vec<(std::ops::Range<usize>, &[&[u8]])> = Vec::new();
            let mut waiting: Vec<(std::ops::Range<usize>, &[&[u8]])> = Vec::new();
            let mut touched = BTreeSet::new();
            for h in hunks {
                let (s, e) = (h.shown.start, h.shown.end);
                let new = &w[h.work.clone()];
                if h.waiting {
                    waiting.push((s..e, new));
                    touched.extend(s.saturating_sub(1)..(e + 1).min(m.len()));
                    let mine: HashSet<Vec<u8>> = new.iter().map(|l| norm(key(l))).collect();
                    touched.extend((0..m.len()).filter(|&i| {
                        layers_line(i) && significant(key(m[i])) && mine.contains(&norm(key(m[i])))
                    }));
                    continue;
                }
                // Where in the base: after the base line before it.
                let at = (0..s).rev().find_map(|i| from_base[i]).map_or(0, |i| i + 1);
                let range = match s == e {
                    true => at..at,
                    false => from_base[s].unwrap_or(at)..from_base[e - 1].map_or(at, |i| i + 1),
                };
                own.push((range, new));
            }
            Split {
                own: apply(&b, &own),
                kept: apply(&m, &waiting),
                waiting: waiting.len(),
                touched: touched.into_iter().collect(),
            }
        };
        let sound = |split: &Split| {
            let leaks = lines(&split.own).any(|l| {
                is_theirs(l) || (is_marker(key(l)) && !base_lines.contains(&norm(key(l))))
            });
            !leaks && self.rebuilds(&split.kept, &split.own, work)
        };
        let split = assemble(&hunks);
        if sound(&split) {
            return split;
        }
        // The own hunks next to the layers' lines or to waiting ones wait
        // too.
        let near: Vec<bool> = (0..hunks.len())
            .map(|i| {
                let h = &hunks[i];
                let (s, e) = (h.shown.start, h.shown.end);
                let next_to_waiting =
                    (i > 0 && hunks[i - 1].waiting && hunks[i - 1].shown.end == s)
                        || (i + 1 < hunks.len()
                            && hunks[i + 1].waiting
                            && hunks[i + 1].shown.start == e);
                (s > 0 && layers_line(s - 1)) || layers_line(e) || next_to_waiting
            })
            .collect();
        for (h, near) in hunks.iter_mut().zip(near) {
            h.waiting |= near;
        }
        let split = assemble(&hunks);
        if sound(&split) {
            return split;
        }
        self.all_waiting(work, hunks.len().max(1))
    }

    /// Every change of `work` waiting: git sees the base's version.
    fn all_waiting(&self, work: &[u8], hunks: usize) -> Split {
        Split {
            own: self.base.clone(),
            kept: work.to_vec(),
            waiting: hunks,
            touched: (0..lines(&self.shown).count()).collect(),
        }
    }

    /// Whether git writing `own` back into the file with only the waiting
    /// edits, `kept` (as `smudge` merges them), gives `work` again, its
    /// lines at least, if not in the same order (an own line placed on the
    /// other side of a layer's).
    fn rebuilds(&self, kept: &[u8], own: &[u8], work: &[u8]) -> bool {
        if own == self.base.as_slice() {
            return kept == work;
        }
        self.merged(kept, own)
            .is_some_and(|merged| merged == work || counts(&merged) == counts(work))
    }

    /// `own` merged into `kept` (see `smudge`) with every line of both
    /// kept: each line either side added, none either side removed. Tried
    /// in turn: git's 3-way merge; each own hunk put where its base lines
    /// are in `kept`, its insertions after the lines `kept` has there, then
    /// before them; both sides kept where they touch, either first. `None`
    /// if none keeps every line.
    fn merged(&self, kept: &[u8], own: &[u8]) -> Option<Vec<u8>> {
        let expected = expected_counts(&self.base, kept, own)?;
        let union = gix::merge::blob::builtin_driver::text::Conflict::ResolveWithUnion;
        let tries: [&dyn Fn() -> Option<Vec<u8>>; 5] = [
            &|| merge3(&self.base, kept, own, keep_conflicts()),
            &|| place(&self.base, kept, own, true),
            &|| place(&self.base, kept, own, false),
            &|| merge3(&self.base, kept, own, union),
            &|| merge3(&self.base, own, kept, union),
        ];
        tries
            .iter()
            .filter_map(|t| t())
            .find(|merged| counts(merged) == expected)
    }

    /// `work` without the layers' lines and the waiting edits: what git sees.
    pub fn clean(&self, work: &[u8]) -> Vec<u8> {
        self.split(work).own
    }

    /// `own` (what git has) with the layers' lines and the waiting edits
    /// of `now`, the file as it is before git writes `own` out (`kept`:
    /// those waiting edits, if known already): git's own change merged into
    /// the file without its own changes, a 3-way merge from the base's
    /// version. Where they touch, both kept, in either order, if clean
    /// gives `own` back. Never at the cost of the layers' lines or the
    /// waiting edits: if nothing else keeps them, the merge with git's
    /// conflict markers (listed as a conflict, and never seen by git).
    pub fn smudge(&self, own: &[u8], now: &[u8], kept: Option<&[u8]>) -> Vec<u8> {
        let kept = match kept {
            Some(kept) => kept.to_vec(),
            None => self.split(now).kept,
        };
        if own == self.base.as_slice() {
            return kept;
        }
        if [&self.base, &kept].iter().any(|d| is_binary(d)) || is_binary(own) {
            // Every change of a binary file waits: git's version only where
            // nothing waits.
            return match kept == self.shown {
                true => own.to_vec(),
                false => kept,
            };
        }
        if let Some(merged) = self.merged(&kept, own) {
            return merged;
        }
        // Both changed the same lines: a 3-way merge that resolves it (both
        // sides made the same change), else git's markers.
        merge3(&self.base, &kept, own, keep_conflicts())
            .unwrap_or_else(|| merge3_marked(&self.base, &kept, own, KEPT_LABEL, GIT_LABEL).0)
    }

    /// Whether `work` shows conflict markers the version shown doesn't
    /// have: a conflict of the workspace's changes with the layers' (a
    /// switch carrying them, a smudge), not yet resolved.
    pub fn conflicted(&self, work: &[u8]) -> bool {
        has_markers(work) && !has_markers(&self.shown)
    }

    /// `work` with each conflict (see `conflicted`) replaced by its first
    /// side, the workspace's.
    pub fn first_sides(work: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut side = 0;
        for line in lines(work) {
            let k = key(line);
            match side {
                0 if k.starts_with(b"<<<<<<<") && is_marker(k) => side = 1,
                1 if (k.starts_with(b"|||||||") && is_marker(k)) || k == b"=======" => side = 2,
                1 | 2 if k.starts_with(b">>>>>>>") && is_marker(k) => side = 0,
                2 => {}
                _ => out.extend_from_slice(line),
            }
        }
        out
    }
}

/// The hunks of a line diff from `from` to `to`.
fn diff<'a>(from: &'a [u8], to: &'a [u8]) -> impl Iterator<Item = imara_diff::Hunk> + 'a {
    use imara_diff::{Algorithm, Diff, InternedInput, sources::byte_lines};
    let input = InternedInput::new(byte_lines(from), byte_lines(to));
    let hunks: Vec<imara_diff::Hunk> = Diff::compute(Algorithm::Histogram, &input)
        .hunks()
        .collect();
    hunks.into_iter()
}

/// `own`'s change from `base` put into `kept` (another change of `base`)
/// where its lines are: each hunk's base lines must be in `kept` as they
/// are, side by side; an insertion goes after the lines `kept` inserted at
/// that spot (`after`) or before them. `None` if a hunk's lines aren't.
fn place(base: &[u8], kept: &[u8], own: &[u8], after: bool) -> Option<Vec<u8>> {
    let b_len = lines(base).count();
    let k: Vec<&[u8]> = lines(kept).collect();
    let o: Vec<&[u8]> = lines(own).collect();
    // Each base line's line in `kept`, if it kept it.
    let mut at: Vec<Option<usize>> = vec![None; b_len];
    let (mut bi, mut ki) = (0usize, 0usize);
    for h in diff(base, kept) {
        while bi < h.before.start as usize {
            at[bi] = Some(ki);
            (bi, ki) = (bi + 1, ki + 1);
        }
        (bi, ki) = (h.before.end as usize, h.after.end as usize);
    }
    while bi < b_len {
        at[bi] = Some(ki);
        (bi, ki) = (bi + 1, ki + 1);
    }
    let mut edits: Vec<(std::ops::Range<usize>, &[&[u8]])> = Vec::new();
    let mut end = 0;
    for h in diff(base, own) {
        let (s, e) = (h.before.start as usize, h.before.end as usize);
        let new = &o[h.after.start as usize..h.after.end as usize];
        let range = if s < e {
            let first = at[s]?;
            if (s..e).any(|i| at[i] != Some(first + i - s)) {
                return None;
            }
            first..first + e - s
        } else {
            let lo = match s {
                0 => 0,
                _ => at[s - 1]? + 1,
            };
            let hi = match s == b_len {
                true => k.len(),
                false => at[s]?,
            };
            let spot = if after { hi } else { lo };
            spot..spot
        };
        if range.start < end {
            return None;
        }
        end = range.end;
        edits.push((range, new));
    }
    Some(apply(&k, &edits))
}

/// How many times each line is in `data`.
fn counts(data: &[u8]) -> std::collections::HashMap<&[u8], usize> {
    let mut out = std::collections::HashMap::new();
    for line in lines(data) {
        *out.entry(key(line)).or_default() += 1;
    }
    out
}

/// The lines a merge of `ours` and `theirs` (both changes of `base`) has
/// when it keeps every line either added and none either removed: `None`
/// if they removed the same line (one of them changed what the other
/// removed).
fn expected_counts<'a>(
    base: &'a [u8],
    ours: &'a [u8],
    theirs: &'a [u8],
) -> Option<std::collections::HashMap<&'a [u8], usize>> {
    let (b, t) = (counts(base), counts(theirs));
    let mut out = counts(ours);
    let all: HashSet<&[u8]> = b.keys().chain(t.keys()).copied().collect();
    for line in all {
        let (had, n) = (
            b.get(line).copied().unwrap_or(0),
            t.get(line).copied().unwrap_or(0),
        );
        let entry = out.entry(line).or_default();
        // `theirs` added n - had of it, or removed had - n.
        match n >= had {
            true => *entry += n - had,
            false => *entry = entry.checked_sub(had - n)?,
        }
    }
    out.retain(|_, n| *n > 0);
    Some(out)
}

/// `lines` with `hunks` (a range of them replaced by new lines each, in
/// order) applied.
fn apply(lines: &[&[u8]], hunks: &[(std::ops::Range<usize>, &[&[u8]])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    for (range, new) in hunks {
        for line in &lines[at.min(range.start)..range.start] {
            out.extend_from_slice(line);
        }
        for line in new.iter() {
            out.extend_from_slice(line);
        }
        at = at.max(range.end);
    }
    for line in &lines[at.min(lines.len())..] {
        out.extend_from_slice(line);
    }
    out
}

/// Per line of `lines`, whether it is in a conflict: git's markers and
/// what is between them (an unterminated one runs to the end).
fn conflict_lines(lines: &[&[u8]]) -> Vec<bool> {
    let mut out = vec![false; lines.len()];
    let mut open = None;
    for (i, line) in lines.iter().enumerate() {
        let k = key(line);
        if open.is_none() && k.starts_with(b"<<<<<<<") && is_marker(k) {
            open = Some(i);
        }
        if let Some(start) = open
            && k.starts_with(b">>>>>>>")
            && is_marker(k)
        {
            out[start..=i].iter_mut().for_each(|c| *c = true);
            open = None;
        }
    }
    if let Some(start) = open {
        out[start..].iter_mut().for_each(|c| *c = true);
    }
    out
}

/// Whether `data` has a line opening git's conflict markers.
pub fn has_markers(data: &[u8]) -> bool {
    super::layers::lines(data).any(|l| l.starts_with(b"<<<<<<< "))
}

/// What git asks of the filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Clean,
    Smudge,
}

impl Op {
    fn byte(self) -> u8 {
        match self {
            Op::Clean => b'c',
            Op::Smudge => b's',
        }
    }

    fn of(byte: u8) -> Result<Op> {
        match byte {
            b'c' => Ok(Op::Clean),
            b's' => Ok(Op::Smudge),
            _ => bail!("not a filter operation: {byte}"),
        }
    }
}

// The relay's frames to the daemon, after a first line naming the workspace
// (`gitfold-filter 1\n<id>\n`): a request is the operation's byte, the
// path's length (4 bytes, big-endian), the path, the content's length (8
// bytes) and the content; an answer is 0 (the content follows) or 1 (an
// error message follows), the length (8 bytes) and the bytes.

/// The line that opens a relay's connection to the daemon.
pub const HELLO: &str = "gitfold-filter 1";

/// Writes a request for `path`'s `content`.
pub fn write_request(w: &mut impl Write, op: Op, path: &[u8], content: &[u8]) -> Result<()> {
    w.write_all(&[op.byte()])?;
    w.write_all(&u32::try_from(path.len())?.to_be_bytes())?;
    w.write_all(path)?;
    w.write_all(&(content.len() as u64).to_be_bytes())?;
    w.write_all(content)?;
    Ok(w.flush()?)
}

/// A request: what, for which path, of what content.
pub type Request = (Op, Vec<u8>, Vec<u8>);

/// Reads a request: `None` at the end of the stream.
pub fn read_request(r: &mut impl Read) -> Result<Option<Request>> {
    let mut op = [0u8];
    match r.read_exact(&mut op) {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        done => done?,
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut path = vec![0; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut path)?;
    let content = read_sized(r)?;
    Ok(Some((Op::of(op[0])?, path, content)))
}

/// Writes an answer: the content, or why there is none.
pub fn write_answer(w: &mut impl Write, answer: &Result<Vec<u8>>) -> Result<()> {
    let (tag, bytes) = match answer {
        Ok(content) => (0u8, content.clone()),
        Err(e) => (1u8, format!("{e:#}").into_bytes()),
    };
    w.write_all(&[tag])?;
    w.write_all(&(bytes.len() as u64).to_be_bytes())?;
    w.write_all(&bytes)?;
    Ok(w.flush()?)
}

/// Reads an answer.
pub fn read_answer(r: &mut impl Read) -> Result<Result<Vec<u8>>> {
    let mut tag = [0u8];
    r.read_exact(&mut tag)?;
    let bytes = read_sized(r)?;
    Ok(match tag[0] {
        0 => Ok(bytes),
        _ => Err(anyhow::anyhow!("{}", String::from_utf8_lossy(&bytes))),
    })
}

fn read_sized(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let len = u64::from_be_bytes(len);
    let mut out = Vec::with_capacity(len.min(1 << 24) as usize);
    r.take(len).read_to_end(&mut out)?;
    anyhow::ensure!(out.len() as u64 == len, "the stream ended early");
    Ok(out)
}

// git's pkt-line format (gitprotocol-common(5)): four hex digits giving the
// length, those included, then the data; `0000` is a flush.

/// The most data in one pkt-line.
const PKT_MAX: usize = 65516;

/// A pkt-line: its data, or `None` for a flush.
fn read_pkt(r: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = usize::from_str_radix(std::str::from_utf8(&len)?, 16).context("a pkt-line length")?;
    if len == 0 {
        return Ok(None);
    }
    anyhow::ensure!(len > 4, "a bad pkt-line length {len}");
    let mut data = vec![0; len - 4];
    r.read_exact(&mut data)?;
    Ok(Some(data))
}

/// A pkt-line of text: without its newline.
fn read_text(r: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    Ok(read_pkt(r)?.map(|mut line| {
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        line
    }))
}

/// The text pkt-lines up to a flush.
fn read_list(r: &mut impl BufRead) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    while let Some(line) = read_text(r)? {
        out.push(line);
    }
    Ok(out)
}

fn write_pkt(w: &mut impl Write, data: &[u8]) -> Result<()> {
    write!(w, "{:04x}", data.len() + 4)?;
    Ok(w.write_all(data)?)
}

fn write_text(w: &mut impl Write, line: &str) -> Result<()> {
    write_pkt(w, format!("{line}\n").as_bytes())
}

fn flush(w: &mut impl Write) -> Result<()> {
    w.write_all(b"0000")?;
    Ok(w.flush()?)
}

/// Serves git's long-running filter protocol on `input` and `output` (the
/// filter process's stdin and stdout) until git closes it, answering each
/// clean or smudge with `answer(op, path, content)`. An answer's error is
/// reported to git as `status=error` (git then fails the command, the
/// filter being required) and on stderr.
pub fn serve_git(
    input: &mut impl BufRead,
    output: &mut impl Write,
    mut answer: impl FnMut(Op, &[u8], Vec<u8>) -> Result<Vec<u8>>,
) -> Result<()> {
    let hello = read_list(input)?;
    anyhow::ensure!(
        hello.first().map(Vec::as_slice) == Some(b"git-filter-client")
            && hello.iter().any(|l| l == b"version=2"),
        "not git's filter protocol, version 2"
    );
    write_text(output, "git-filter-server")?;
    write_text(output, "version=2")?;
    flush(output)?;
    let offered = read_list(input)?;
    for cap in ["capability=clean", "capability=smudge"] {
        if offered.iter().any(|l| l == cap.as_bytes()) {
            write_text(output, cap)?;
        }
    }
    flush(output)?;
    loop {
        let keys = match read_pkt(input) {
            Ok(Some(first)) => {
                let mut keys = vec![first.strip_suffix(b"\n").unwrap_or(&first).to_vec()];
                keys.extend(read_list(input)?);
                keys
            }
            Ok(None) => continue,
            // git closed the pipe: done.
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let value = |key: &[u8]| {
            keys.iter()
                .find_map(|l| l.strip_prefix(key).map(<[u8]>::to_vec))
        };
        let op = match value(b"command=").as_deref() {
            Some(b"clean") => Op::Clean,
            Some(b"smudge") => Op::Smudge,
            other => bail!(
                "git asked for {:?}, not clean or smudge",
                other.map(String::from_utf8_lossy)
            ),
        };
        let path = value(b"pathname=").unwrap_or_default();
        let mut content = Vec::new();
        while let Some(pkt) = read_pkt(input)? {
            content.extend_from_slice(&pkt);
        }
        match answer(op, &path, content) {
            Ok(out) => {
                write_text(output, "status=success")?;
                flush(output)?;
                for chunk in out.chunks(PKT_MAX) {
                    write_pkt(output, chunk)?;
                }
                flush(output)?;
                // The status stays as it was.
                flush(output)?;
            }
            Err(e) => {
                eprintln!(
                    "gitfold: {}: {e:#}",
                    String::from_utf8_lossy(&path).trim_end()
                );
                write_text(output, "status=error")?;
                flush(output)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(base: &str, union: &str) -> Versions {
        Versions {
            base: base.into(),
            union: union.into(),
        }
    }

    /// Clean undoes smudge, and both give the view's versions exactly.
    #[test]
    fn round_trips() {
        let reg = v("one\ntwo\n", "one\ntwo\nfromA\nfromB\n");
        assert_eq!(reg.clean(b"one\ntwo\nfromA\nfromB\n"), b"one\ntwo\n");
        assert_eq!(reg.smudge(b"one\ntwo\n"), b"one\ntwo\nfromA\nfromB\n");
        for own in [
            "one\ntwo\nmine\n",
            "zero\none\ntwo\n",
            "one\nmid\ntwo\nmine\n",
            "one\n",
            "one\ntwo\nm1\nm2\nm3\n",
            "one\nTWO\nmine\n",
            "ONE\nTWO\n",
        ] {
            let smudged = reg.smudge(own.as_bytes());
            assert_eq!(
                String::from_utf8_lossy(&reg.clean(&smudged)),
                own,
                "smudged: {}",
                String::from_utf8_lossy(&smudged)
            );
        }
        // Own lines appended after the layers' (as a switch carries them).
        assert_eq!(
            reg.smudge(b"one\ntwo\nmine\n"),
            b"one\ntwo\nfromA\nfromB\nmine\n"
        );
        assert_eq!(
            reg.clean(b"one\ntwo\nfromA\nfromB\nmine\n"),
            b"one\ntwo\nmine\n"
        );
        // A line of mine among the layers'.
        assert_eq!(
            reg.clean(b"one\ntwo\nfromA\nmine\nfromB\n"),
            b"one\ntwo\nmine\n"
        );
        // Another file altogether (a bisect's): the layers' lines are kept
        // all the same, and clean gives it back.
        let other = b"other\ncontent\n";
        assert_eq!(text(&reg.smudge(other)), "other\ncontent\nfromA\nfromB\n");
        assert_eq!(reg.clean(&reg.smudge(other)), other);
        assert_eq!(reg.clean(other), other);
    }

    /// A layer's line in the middle of the base's lines.
    #[test]
    fn lines_inside() {
        // Both append at the end and git's side changed a line too (a merge
        // with a conflict resolved as merge=union): the layers' lines stay.
        let end = v("b1\nb2\nb3\n", "b1\nb2\nb3\nA\n");
        let own = b"b1\nX\nb3\nmine\n";
        assert_eq!(end.smudge(own), b"b1\nX\nb3\nA\nmine\n");
        assert_eq!(end.clean(&end.smudge(own)), own);
        let reg = v("a\nb\nc\nd\n", "a\nb\nfromA\nc\nd\n");
        assert_eq!(
            reg.smudge(b"a\nb\nc\nd\nmine\n"),
            b"a\nb\nfromA\nc\nd\nmine\n"
        );
        assert_eq!(
            reg.clean(b"a\nmine\nb\nfromA\nc\nd\n"),
            b"a\nmine\nb\nc\nd\n"
        );
        assert_eq!(
            reg.clean(b"a\nb\nfromA\nmine\nc\nd\n"),
            b"a\nb\nmine\nc\nd\n"
        );
    }

    fn edited(base: &str, shown: &str) -> Edited {
        Edited {
            base: base.into(),
            shown: shown.into(),
        }
    }

    fn text(data: &[u8]) -> &str {
        std::str::from_utf8(data).unwrap()
    }

    /// A method added inside a layer's uncommitted class waits; a change
    /// elsewhere is the workspace's own; both survive git writing the file.
    #[test]
    fn waiting_edits_split_off() {
        let api = edited("// api\n", "// api\nclass Api {\n    fn a() {}\n}\n");
        let mine = "// api\nclass Api {\n    fn a() {}\n    fn m() {}\n}\n";
        let split = api.split(mine.as_bytes());
        assert_eq!((text(&split.own), split.waiting), ("// api\n", 1));
        assert_eq!(text(&split.kept), mine);
        // The file as shown: nothing of its own, nothing waiting.
        let shown = api.split(api.shown.as_slice());
        assert_eq!((text(&shown.own), shown.waiting), ("// api\n", 0));
        // Own lines before the class, and after it: git sees them.
        let both = "// api\n// mine\nclass Api {\n    fn a() {}\n    fn m() {}\n}\nfn own() {}\n";
        let split = api.split(both.as_bytes());
        assert_eq!(text(&split.own), "// api\n// mine\nfn own() {}\n");
        assert_eq!(split.waiting, 1);
        assert_eq!(text(&split.kept), mine);
        // A changed line of the layer's waits.
        let changed = "// api\nclass Api {\n    fn a() { mine }\n}\n";
        let split = api.split(changed.as_bytes());
        assert_eq!((text(&split.own), split.waiting), ("// api\n", 1));
        // Smudge: git's version with the layer's lines and the waiting edit.
        assert_eq!(text(&api.smudge(b"// api\n", both.as_bytes(), None)), mine);
        let own = b"// api\n// mine\nfn own() {}\n";
        let smudged = api.smudge(own, both.as_bytes(), None);
        assert_eq!(text(&api.clean(&smudged)), text(own));
        assert!(text(&smudged).contains("fn m()"));
    }

    /// The base's lines a layer changed or deleted are the layer's.
    #[test]
    fn layers_changes_to_base_lines() {
        let f = edited("a\nb\nc\nd\n", "a\nB\nd\n");
        // Own change to a base line the layer left: own.
        let split = f.split(b"A\nB\nd\n");
        assert_eq!((text(&split.own), split.waiting), ("A\nb\nc\nd\n", 0));
        // A change to the layer's line: waits.
        let split = f.split(b"a\nBB\nd\n");
        assert_eq!((text(&split.own), split.waiting), ("a\nb\nc\nd\n", 1));
        // Appended at the end: own.
        let split = f.split(b"a\nB\nd\ne\n");
        assert_eq!((text(&split.own), split.waiting), ("a\nb\nc\nd\ne\n", 0));
        // Binary: every change waits.
        let bin = edited("x\0", "y\0");
        assert_eq!(bin.split(b"z\0").own, b"x\0");
    }

    /// Whatever the split, git never gets a layer's line or a marker, and
    /// git writing its version back loses nothing (`rebuilds`).
    fn safe(f: &Edited, work: &str) -> Split {
        let split = f.split(work.as_bytes());
        let theirs: Vec<&[u8]> = added_lines(&f.base, &f.shown)
            .into_iter()
            .map(key)
            .filter(|l| significant(l))
            .collect();
        for line in lines(&split.own).map(key) {
            let base_has = lines(&f.base).any(|b| key(b) == line);
            assert!(
                base_has || !theirs.contains(&line),
                "a layer's line {:?} in git's view: {}",
                text(line),
                text(&split.own)
            );
            assert!(
                !is_marker(line),
                "a marker in git's view: {}",
                text(&split.own)
            );
        }
        let back = f.smudge(&split.own, &split.kept, None);
        for line in lines(work.as_bytes()) {
            assert!(
                lines(&back).any(|l| l == line),
                "{:?} lost when git writes its version back: {}",
                text(line),
                text(&back)
            );
        }
        split
    }

    /// The review's cases (reviews/v2-round1, correctness 4): a layer's
    /// function moved above an own one, imports sorted, a line added inside
    /// a layer's function that looks like an own one's.
    #[test]
    fn moves_and_reorderings_wait() {
        // A layer's function moved above an own one: not a deletion of the
        // own function, not the layer's lines given to git.
        let f = edited(
            "fn a() {\n    x();\n}\n\nfn c() {\n}\n",
            "fn a() {\n    x();\n}\n\nfn b() {\n    let q = 1;\n}\n\nfn c() {\n}\n",
        );
        let work = "fn b() {\n    let q = 1;\n}\n\nfn a() {\n    x();\n}\n\nfn c() {\n}\n";
        let split = safe(&f, work);
        assert_eq!(text(&split.own), text(&f.base));
        assert!(split.waiting > 0);
        assert_eq!(text(&split.kept), work);
        // Imports sorted, the layer's among the base's.
        let f = edited(
            "use b;\nuse c;\n\nfn main() {}\n",
            "use b;\nuse c;\nuse a;\n\nfn main() {}\n",
        );
        let work = "use a;\nuse b;\nuse c;\n\nfn main() {}\n";
        let split = safe(&f, work);
        assert_eq!(text(&split.own), text(&f.base));
        assert_eq!(text(&split.kept), work);
        // A line added in the layer's function, the same as one of the
        // base's function: waits.
        let f = edited(
            "fn a() {\n    x();\n}\n",
            "fn a() {\n    x();\n}\n\nfn b() {\n    x();\n}\n",
        );
        let split = safe(
            &f,
            "fn a() {\n    x();\n}\n\nfn b() {\n    x();\n    y();\n}\n",
        );
        assert_eq!((text(&split.own), split.waiting), (text(&f.base), 1));
        // A formatter respacing every line: the layer's line waits, and
        // the base's with it, in the same hunk; git sees none of it.
        let f = edited("a = 1\n", "a = 1\nlayer_value = 2\n");
        let split = safe(&f, "a  =  1\nlayer_value  =  2\n");
        assert_eq!((text(&split.own), split.waiting), ("a = 1\n", 1));
        // Apart, the base's respaced line is the workspace's own.
        let f = edited("a = 1\nb\nc\n", "a = 1\nb\nc\nlayer_value = 2\n");
        let split = safe(&f, "a  =  1\nb\nc\nlayer_value  =  2\n");
        assert_eq!((text(&split.own), split.waiting), ("a  =  1\nb\nc\n", 1));
    }

    /// Conflict markers and what is between them are never the workspace's
    /// own (the review's case 7).
    #[test]
    fn conflicts_wait() {
        let f = edited(
            "fn a() {}\n\nfn z() {}\n",
            "fn a() {}\n\nfn b() {\n    v2();\n}\n\nfn z() {}\n",
        );
        let work = "fn a() {}\n\n<<<<<<< yours\nfn b() {\n    v1();\n    mine();\n}\n=======\nfn b() {\n    v2();\n}\n>>>>>>> api's newer work\n\nfn z() {}\n";
        let split = safe(&f, work);
        assert_eq!(text(&split.own), text(&f.base));
        assert_eq!(text(&split.kept), work);
        assert!(f.conflicted(work.as_bytes()));
        assert!(!f.conflicted(&f.shown));
        // An own change elsewhere still reaches git.
        let work = work.replace("fn z() {}", "fn z() { own }");
        let split = safe(&f, &work);
        assert_eq!(text(&split.own), "fn a() {}\n\nfn z() { own }\n");
        assert_eq!(
            text(&Edited::first_sides(work.as_bytes())),
            "fn a() {}\n\nfn b() {\n    v1();\n    mine();\n}\n\nfn z() { own }\n"
        );
    }

    /// An own line next to the layer's, either side, and an own deletion
    /// next to them: git sees them, and gets them back whole.
    #[test]
    fn own_changes_next_to_the_layers() {
        let f = edited("a\nb\n", "a\nb\nL1\nL2\n");
        for (work, own) in [
            ("a\nb\nL1\nL2\nmine\n", "a\nb\nmine\n"),
            ("a\nb\nmine\nL1\nL2\n", "a\nb\nmine\n"),
            ("a\nL1\nL2\n", "a\n"),
        ] {
            let split = safe(&f, work);
            assert_eq!((work, text(&split.own)), (work, own));
        }
    }

    /// Lines the workspace committed while layers showed (its base waits)
    /// are git's own, neither a change nor a waiting edit; where the
    /// layer's change conflicts with one, the workspace's line waits, and
    /// git never sees it reverted.
    #[test]
    fn on_the_branch_tip() {
        let base = "a\nX\nb\nc\n";
        let tip = "a\nX mine\nb\nc\n";
        let f = Edited::on_tip(base.into(), "a\nX\nb\nL\nc\n".into(), tip.into());
        let split = f.split(b"a\nX mine\nb\nL\nc\n");
        assert_eq!((text(&split.own), split.waiting), (tip, 0));
        let split = f.split(b"a\nX mine\nb\nL\nc\nown\n");
        assert_eq!(
            (text(&split.own), split.waiting),
            ("a\nX mine\nb\nc\nown\n", 0)
        );
        // The layer changes X too: its X'' shows, the workspace's X mine
        // waits, and git keeps its committed line.
        let f = Edited::on_tip(base.into(), "a\nX layer\nb\nc\n".into(), tip.into());
        let split = f.split(b"a\nX mine\nb\nc\n");
        assert_eq!((text(&split.own), split.waiting), (tip, 1));
    }

    /// Smudge never drops the layers' lines or the waiting edits: where git's
    /// version can't be merged in (correctness 1), they are kept with
    /// markers.
    #[test]
    fn smudge_keeps_the_layers_lines() {
        let f = edited("a\nb\nc\n", "a\nb\nL\nc\n");
        // git's version changes the lines around the layer's, as it can't
        // be placed.
        let now = b"a\nb\nL changed\nc\n";
        let out = f.smudge(b"a\nB\nC\n", now, None);
        assert!(text(&out).contains("L changed"), "{}", text(&out));
        let union = v("a\nb\nc\n", "a\nb\nL\nc\n");
        let out = union.smudge(b"a\nB\nC\n");
        assert!(text(&out).contains("L\n"), "{}", text(&out));
        // The waiting edits given (remembered from an earlier smudge) win
        // over the file's.
        let out = f.smudge(b"a\nb\nc\n", b"anything\n", Some(b"a\nb\nL kept\nc\n"));
        assert_eq!(text(&out), "a\nb\nL kept\nc\n");
    }

    fn pkt(data: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_pkt(&mut out, data.as_bytes()).unwrap();
        out
    }

    /// A session as git runs it: the handshake, a clean, a smudge, an error.
    #[test]
    fn git_protocol() {
        let mut input = Vec::new();
        for l in ["git-filter-client\n", "version=2\n"] {
            input.extend(pkt(l));
        }
        input.extend(b"0000");
        for l in [
            "capability=clean\n",
            "capability=smudge\n",
            "capability=delay\n",
        ] {
            input.extend(pkt(l));
        }
        input.extend(b"0000");
        for (cmd, path, content) in [
            ("clean", "reg.txt", "one\nA\n"),
            ("smudge", "reg.txt", "one\n"),
            ("clean", "bad.txt", "x\n"),
        ] {
            input.extend(pkt(&format!("command={cmd}\n")));
            input.extend(pkt(&format!("pathname={path}\n")));
            input.extend(b"0000");
            input.extend(pkt(content));
            input.extend(b"0000");
        }
        let mut output = Vec::new();
        let mut seen = Vec::new();
        serve_git(&mut input.as_slice(), &mut output, |op, path, content| {
            seen.push((op, path.to_vec()));
            if path == b"bad.txt" {
                bail!("no");
            }
            Ok(match op {
                Op::Clean => content.strip_suffix(b"A\n").unwrap().to_vec(),
                Op::Smudge => [content, b"A\n".to_vec()].concat(),
            })
        })
        .unwrap();
        let expected = [
            pkt("git-filter-server\n"),
            pkt("version=2\n"),
            b"0000".to_vec(),
            pkt("capability=clean\n"),
            pkt("capability=smudge\n"),
            b"0000".to_vec(),
            pkt("status=success\n"),
            b"0000".to_vec(),
            pkt("one\n"),
            b"00000000".to_vec(),
            pkt("status=success\n"),
            b"0000".to_vec(),
            pkt("one\nA\n"),
            b"00000000".to_vec(),
            pkt("status=error\n"),
            b"0000".to_vec(),
        ]
        .concat();
        assert_eq!(
            String::from_utf8_lossy(&output),
            String::from_utf8_lossy(&expected)
        );
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1], (Op::Smudge, b"reg.txt".to_vec()));
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_request(&mut buf, Op::Smudge, b"a/b.txt", b"content").unwrap();
        let (op, path, content) = read_request(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(
            (op, &path[..], &content[..]),
            (Op::Smudge, &b"a/b.txt"[..], &b"content"[..])
        );
        assert!(read_request(&mut &b""[..]).unwrap().is_none());
        let mut buf = Vec::new();
        write_answer(&mut buf, &Ok(b"x".to_vec())).unwrap();
        write_answer(&mut buf, &Err(anyhow::anyhow!("why"))).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_answer(&mut r).unwrap().unwrap(), b"x");
        assert_eq!(read_answer(&mut r).unwrap().unwrap_err().to_string(), "why");
    }
}
