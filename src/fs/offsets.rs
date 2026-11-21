//! Readdir offsets that stay valid while a directory changes.
//!
//! The kernel reads a directory in pages and resumes each page at the offset of
//! the last entry it received. An entry's position in a listing moves when
//! names before it are created or removed, so a position used as its offset
//! made a reader paging through a changing directory skip or repeat unchanged
//! entries (make, reading a directory lazily while parallel jobs created and
//! deleted files in it, missed a file). Instead an entry's offset is a hash of
//! its name and listings are served in offset order (as ext4 does): a reader
//! resuming at an offset sees every unchanged entry exactly once, whatever
//! changed meanwhile (except names sharing a hash, see `order`).
//!
//! Offsets: `.` is 1, `..` 2, a workspace root's `.git` 3 and task link 4, and
//! named entries from `FIRST`, all below 2^63 (the kernel's offset is signed).
//! Snapshot directories never change, so they keep positions as offsets.
//!
//! Offsets need to hold only while the mount is up (the kernel's cached
//! listings go with it), so the hash is seeded randomly per process: colliding
//! names can't be picked in advance. (The hash is fast, not cryptographic: with
//! many offsets observed, collisions could still be searched for, and a reader
//! may skip or repeat names next to them, see `order`.)

use std::hash::BuildHasher;
use std::sync::LazyLock;

use foldhash::fast::RandomState;

/// The offsets of the entries every listing of a directory has (see `FIRST`).
pub const DOT: u64 = 1;
pub const DOTDOT: u64 = 2;
/// The offsets of a workspace root's `.git` and task link.
pub const GITFILE: u64 = 3;
pub const TASKLINK: u64 = 4;
/// The first named entry's offset.
pub const FIRST: u64 = 5;

/// A listing entry: its offset, name and what it is.
pub type Entry<T> = (u64, Vec<u8>, T);

/// Names sharing a hash get the slots after its offset (see `order`).
const SLOTS: u64 = 16;

static SEED: LazyLock<RandomState> = LazyLock::new(RandomState::default);

/// The offset of entry `name`, the same in every listing while this process
/// runs: its hash, 56 bits of it, times `SLOTS`.
fn of(name: &[u8]) -> u64 {
    FIRST + (SEED.hash_one(name) >> 8) * SLOTS
}

/// `entries` with their offsets, in offset order. Names sharing a hash (a 1 in
/// 2^56 chance per pair) take the next free slots after its offset, in name
/// order: whether one of them is listed at the next slot depends on the
/// others, so a reader may skip or repeat one of them while another is created
/// or removed. Other names' offsets are never moved (unless more than `SLOTS`
/// share a hash).
pub fn order<T>(entries: Vec<(Vec<u8>, T)>) -> Vec<Entry<T>> {
    order_by(entries, of)
}

/// `order`, with `offset` giving each name's offset before collisions.
fn order_by<T>(entries: Vec<(Vec<u8>, T)>, offset: impl Fn(&[u8]) -> u64) -> Vec<Entry<T>> {
    let mut ordered: Vec<_> = (entries.into_iter())
        .map(|(name, t)| (offset(&name), name, t))
        .collect();
    ordered.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    for i in 1..ordered.len() {
        if ordered[i].0 <= ordered[i - 1].0 {
            ordered[i].0 = ordered[i - 1].0 + 1;
        }
    }
    ordered
}

/// The entries after `offset`.
pub fn after<T>(entries: &[Entry<T>], offset: u64) -> &[Entry<T>] {
    &entries[entries.partition_point(|e| e.0 <= offset)..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{DefaultHasher, Hasher};
    use std::time::Instant;

    fn names(n: usize) -> Vec<Vec<u8>> {
        let stems = ["rtc-pcf8563", "hid-hyperv", "amdgpu_dm", "fork", "README"];
        let exts = [".c", ".o", ".mod.c", ".mod.o", ".ko", ".cmd", ""];
        (0..n)
            .map(|i| {
                let (s, e) = (stems[i % stems.len()], exts[i / stems.len() % exts.len()]);
                format!("{s}-{i:06}{e}").into_bytes()
            })
            .collect()
    }

    /// The listing a reader resuming at each page's last offset gets, while
    /// `change` runs between pages.
    fn read_paged(
        mut names: Vec<Vec<u8>>,
        mut change: impl FnMut(usize, &mut Vec<Vec<u8>>),
    ) -> Vec<Vec<u8>> {
        let (mut seen, mut offset, mut page) = (Vec::new(), 0, 0);
        loop {
            let listing = order(names.iter().map(|n| (n.clone(), ())).collect());
            let got = after(&listing, offset);
            let got = &got[..got.len().min(25)];
            let Some(last) = got.last() else { return seen };
            seen.extend(got.iter().map(|e| e.1.clone()));
            offset = last.0;
            change(page, &mut names);
            page += 1;
        }
    }

    /// Every entry exactly once when entries already read are removed, and
    /// entries listed before the reader's position are created, between pages
    /// (with positions as offsets, each change shifted the rest).
    #[test]
    fn unchanged_entries_once() {
        let all = names(3000);
        let mut read = Vec::new();
        let seen = read_paged(all.clone(), |page, names| {
            if page == 3 {
                read = read_paged(names.clone(), |_, _| {})
                    .into_iter()
                    .take(100)
                    .collect();
                names.retain(|n| !read.contains(n));
            }
            if page == 5 {
                names.extend((0..100).map(|i| format!("new-{i}").into_bytes()));
            }
        });
        let mut counts = std::collections::HashMap::new();
        for s in &seen {
            *counts.entry(s).or_insert(0) += 1;
        }
        for n in &all {
            let count = counts.get(n).copied().unwrap_or(0);
            assert_eq!(
                count,
                1,
                "{} seen {count} times",
                String::from_utf8_lossy(n)
            );
        }
    }

    #[test]
    fn offsets_in_range_and_increasing() {
        let listing = order(names(10_000).into_iter().map(|n| (n, ())).collect());
        assert!(listing.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(listing[0].0 >= FIRST && listing.last().unwrap().0 < 1 << 63);
        assert!(
            names(1000)
                .iter()
                .all(|n| (of(n) - FIRST).is_multiple_of(SLOTS))
        );
    }

    /// `names` ordered with `offset`, as (offset, name).
    fn ordered_by(names: &[&str], offset: impl Fn(&[u8]) -> u64) -> Vec<(u64, String)> {
        let entries = names.iter().map(|n| (n.as_bytes().to_vec(), ())).collect();
        (order_by(entries, offset).into_iter())
            .map(|(off, name, ())| (off, String::from_utf8(name).unwrap()))
            .collect()
    }

    fn owned(want: &[(u64, &str)]) -> Vec<(u64, String)> {
        want.iter()
            .map(|&(off, name)| (off, name.to_string()))
            .collect()
    }

    /// Names sharing a hash take the next slots in name order; the next
    /// hash's names keep their offsets, with or without the collision.
    #[test]
    fn colliding_names_keep_to_their_slots() {
        // 1-letter names at FIRST, 2-letter ones a hash further.
        let offset = |name: &[u8]| FIRST + (name.len() as u64 - 1) * SLOTS;
        let both = ordered_by(&["b", "cc", "a", "dd"], offset);
        let want = [(5, "a"), (6, "b"), (21, "cc"), (22, "dd")];
        assert_eq!(both, owned(&want));
        let alone = ordered_by(&["b", "cc", "dd"], offset);
        assert_eq!(alone, owned(&[(5, "b"), (21, "cc"), (22, "dd")]));
    }

    /// More than `SLOTS` names sharing a hash spill into the next hash's
    /// slots, still each listed once at increasing offsets.
    #[test]
    fn colliding_names_overflowing_their_slots() {
        let many: Vec<String> = (0..SLOTS + 2).map(|i| format!("n{i:02}")).collect();
        let mut names: Vec<&str> = many.iter().map(String::as_str).collect();
        names.push("x");
        let got = ordered_by(&names, |name| match name {
            b"x" => FIRST + SLOTS,
            _ => FIRST,
        });
        let offsets: Vec<u64> = got.iter().map(|g| g.0).collect();
        assert_eq!(offsets, (FIRST..FIRST + SLOTS + 3).collect::<Vec<_>>());
    }

    fn fnv1a(name: &[u8]) -> u64 {
        name.iter().fold(0xcbf29ce484222325, |h, &b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        })
    }

    fn sip(name: &[u8]) -> u64 {
        let mut h = DefaultHasher::new();
        h.write(name);
        h.finish()
    }

    fn time(runs: u32, mut f: impl FnMut() -> usize) -> f64 {
        let mut sink = 0;
        let start = Instant::now();
        for _ in 0..runs {
            sink += f();
        }
        std::hint::black_box(sink);
        start.elapsed().as_secs_f64() * 1e6 / runs as f64
    }

    /// The listing code before and after: building a listing (sort by name vs
    /// hash and sort by offset) and serving it in pages of 25 (skip to an index
    /// vs binary search). `cargo test --release offsets::tests::bench --
    /// --ignored --nocapture`
    #[test]
    #[ignore]
    #[allow(clippy::print_stdout)]
    fn bench() {
        for n in [100, 1_000, 7_500, 100_000] {
            let input: Vec<(Vec<u8>, u32)> = names(n).into_iter().zip(0..).collect();
            let runs = (2_000_000 / n as u32).max(20);
            let hash = |h: fn(&[u8]) -> u64| {
                time(runs, || {
                    input.iter().map(|(name, _)| h(name) as usize & 1).sum()
                })
            };
            let (fold, fnv, sipped) = (hash(of), hash(fnv1a), hash(sip));
            let old_build = time(runs, || {
                let mut e = input.clone();
                e.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                e.len()
            });
            let new_build = time(runs, || order(input.clone()).len());
            let mut sorted = input.clone();
            sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let ordered = order(input.clone());
            let old_serve = time(runs, || {
                (0..n)
                    .step_by(25)
                    .map(|first| {
                        sorted
                            .iter()
                            .enumerate()
                            .skip(first)
                            .take(25)
                            .map(|(i, e)| i + e.1 as usize)
                            .sum::<usize>()
                    })
                    .sum()
            });
            let new_serve = time(runs, || {
                (0..n)
                    .step_by(25)
                    .map(|first| {
                        let offset = if first == 0 { 0 } else { ordered[first - 1].0 };
                        (after(&ordered, offset).iter().take(25))
                            .map(|e| e.0 as usize + e.2 as usize)
                            .sum::<usize>()
                    })
                    .sum()
            });
            println!(
                "{n:>7} entries: hash all names: foldhash {fold:8.1} µs, FNV-1a {fnv:8.1} µs, SipHash {sipped:8.1} µs | \
                 build: old {old_build:8.1} µs, new {new_build:8.1} µs | serve all pages: old {old_serve:7.1} µs, new {new_serve:7.1} µs"
            );
        }
    }
}
