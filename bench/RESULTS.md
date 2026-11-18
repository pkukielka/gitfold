# Benchmark results

Machine: 24 cores, 62 GB RAM, NVMe, Linux 6.12. Pack files and the native
checkout are in page cache in all runs. "cold" = fresh mount (kernel FUSE
caches empty), "warm" = repeated run on the same mount. `bench/bench.sh`.

Commands are what a coding agent runs: Claude Code's `grep` (embedded ugrep,
`-G --ignore-files --hidden -I`), `rg`, `find`, and the `--ignored=matching`
`git status` Claude Code runs with its bash edit diff on (fsmonitor disabled).

## 2026-10-25, read-only prototype (no-open, readdirplus, prebuilt node table)

spark: 27.6k files, 210 MB of blobs. Snapshot creation 43 ms.

| cmd         | native warm | mount cold | mount warm |
|-------------|------------:|-----------:|-----------:|
| find        |       44 ms |     256 ms |      88 ms |
| claude-grep |      129 ms |     474 ms |     227 ms |
| rg          |       47 ms |     245 ms |      53 ms |
| git status  |       67 ms |     333 ms |     142 ms |

linux: 97k files, 1.67 GB of blobs. Snapshot creation 277 ms
(tree walk 21 ms, index 33 ms, node table 35 ms).

| cmd         | native warm | mount cold | mount warm |
|-------------|------------:|-----------:|-----------:|
| find        |       89 ms |     584 ms |     162 ms |
| claude-grep |      447 ms |    1841 ms |     703 ms |
| rg          |      104 ms |    1163 ms |     141 ms |
| git status  |      119 ms |     755 ms |     243 ms |

chromium: 512k files, 4.36 GB of blobs, 64 GB pack. Snapshot creation 555 ms
(tree walk 112 ms, index 162 ms, node table 226 ms).

| cmd         | native warm | mount cold | mount warm |
|-------------|------------:|-----------:|-----------:|
| find        |      471 ms |    2747 ms |     838 ms |
| claude-grep |     4159 ms |    8102 ms |    5263 ms |
| rg          |      366 ms |    3208 ms |     524 ms |
| git status  |      862 ms |    4084 ms |    1601 ms |

## What mattered

- `FUSE_NO_OPEN_SUPPORT` (reply ENOSYS to open): removes open/release round
  trips; warm rg went from 177 ms to 42 ms on spark (native 35 ms).
- Loading blob content lazily on first read, not on open: with KEEP_CACHE the
  kernel serves later opens from page cache, so eager loading decompressed
  every blob on every open.
- Long TTLs + FOPEN_KEEP_CACHE + FOPEN_CACHE_DIR: warm runs make almost no
  calls into the daemon (reads == number of files, once per mount).
- Synthesized index with matching stat data: `git status` is clean without
  reading any content (read=6 for 27k files).

## What did not matter (cold find on spark stays ~220-256 ms)

- Worker thread count (1..24), readdirplus vs plain readdir vs auto,
  FUSE_NO_OPENDIR_SUPPORT, prebuilding the node table instead of decoding
  trees on demand. Remaining cold cost looks like per-entry kernel work
  (inode/dentry instantiation) at ~8 us/entry.

## Open items at that point (resolved below)

- ~~Cold content reads bound by decompression~~: wrong, see findings below.
- Cold `git status` issues one lookup per index entry before any readdir, so
  readdirplus cannot pre-populate dentries for it. Solved by the prewarm.

## 2026-10-25, + prewarm + FUSE_NO_OPENDIR_SUPPORT (current defaults)

"cold" here = fresh mount after the default metadata prewarm finished
(`GF_WAIT_PREWARM=1 bench/bench.sh ...`). The prewarm is a parallel walk of
the new snapshot through the mount, started by the daemon right after the
snapshot is created (half the CPUs).

| repo     | snapshot | meta prewarm | content prewarm (opt-in) |
|----------|---------:|-------------:|-------------------------:|
| spark    |    32 ms |        36 ms |    244 ms (210 MB, 8 thr) |
| linux    |    92 ms |        67 ms |  1007 ms (1.67 GB, 8 thr) |
| chromium |   567 ms |       387 ms |  2630 ms (4.36 GB, 24 thr) |

chromium (512k files):

| cmd         | native warm | mount cold | mount warm |
|-------------|------------:|-----------:|-----------:|
| find        |      473 ms |     351 ms |     351 ms |
| claude-grep |     4231 ms |    5059 ms |    4609 ms |
| rg          |      371 ms |    2868 ms |     407 ms |
| git status  |      884 ms |    1156 ms |    1153 ms |

linux (97k files):

| cmd         | native warm | mount cold | mount warm |
|-------------|------------:|-----------:|-----------:|
| find        |       71 ms |      53 ms |      52 ms |
| claude-grep |      378 ms |     898 ms |     432 ms |
| rg          |       89 ms |     854 ms |      90 ms |
| git status  |       92 ms |     135 ms |     137 ms |

With `--prewarm content`, the first rg on chromium is 495 ms instead of 2868 ms.

### Findings

- Decompression is not the bottleneck: `gitfold bench-read` decompresses all
  of chromium's 4.36 GB in 760 ms on 24 threads (5.7 GB/s; 571 MB/s on one
  thread). The earlier "contention" reading was wrong: blob_ms is summed
  across threads, not wall time.
- Remaining cold content cost is per-file FUSE read round trips + page cache
  fill (~570k reads on chromium), not inflate.
- FUSE_NO_OPENDIR_SUPPORT removes an opendir/releasedir per directory per walk;
  listings stay cached. Warm find/git status went from ~2x native to <=1.3x.
  Side effect: the kernel caches the root listing too, so root mtime is bumped
  on snapshot creation and root attrs have zero TTL.
- Content prewarm costs page cache equal to the tree size; keep opt-in.

## 2026-10-25, shared store: memory with several snapshots

Model: every git tree decoded once into a 16 B/entry table shared by all
snapshots; one file id per distinct (blob, mode); file inode = file id, so the
kernel shares inodes and page cache across snapshots; per snapshot only one
8 B record per directory. Constant mtime (2020-01-01) for all entries.
Plus allocator fixes: fixed M_MMAP_THRESHOLD, malloc_trim after snapshot
creation and once reads go idle. `/tmp/gf-mem2.sh`-style run, chromium:

| step                         | daemon heap | kernel slab | page cache |
|------------------------------|------------:|------------:|-----------:|
| open @HEAD                   |       83 MB |     +561 MB |    +194 MB |
| open @HEAD~100               |       85 MB |     +713 MB |    +388 MB |
| open @HEAD~1000              |       90 MB |     +869 MB |    +582 MB |
| open @HEAD~5000              |      102 MB |    +1036 MB |    +775 MB |
| rg in @HEAD (2900 ms)        | 107 MB idle |    +1109 MB |   +5633 MB |
| rg in @HEAD~100 (400 ms)     | 108 MB idle |    +1109 MB |   +5677 MB |
| rg in @HEAD~1000 (530 ms)    | 107 MB idle |    +1112 MB |   +6078 MB |
| rg in @HEAD~5000 (660 ms)    | 107 MB idle |    +1119 MB |   +6838 MB |

Before (per snapshot, no sharing): ~390 MB heap (342 MB in the cleaner
measurement below) (1 GB after reads),
~600 MB slab, +4.2 GB page cache once grepped. For these 4 snapshots that
would be ~4 GB heap, ~2.4 GB slab, ~17 GB page cache.

Per additional snapshot now: heap +2..12 MB, slab ~+160 MB (dentries and
directory inodes stay per snapshot), page cache ~+190 MB on open (per-snapshot
directory listing pages + its git index file) plus only content not seen in
other snapshots when read. Snapshot creation for nearby commits ~230 ms
(new trees 2-40 ms; rest is index synthesis: sort, write, SHA-1 of ~60 MB).

Speed unchanged vs previous defaults (chromium warm: find 349 ms, rg 416 ms,
git status 1140-1160 ms; linux warm rg 92 ms).

Known gap: content is served in repository form. Checkout conversions from
.gitattributes (eol=crlf, LFS smudge, ident, filters) are not applied, so
e.g. spark's *.cmd files have LF instead of CRLF; git status is still clean.

## 2026-10-25, index v4 streaming + compact file-id table

- Tables keep git tree order (dirs compare as `name/`), so a depth-first walk
  emits index order: entries are streamed into an index v4 file (path prefix
  compression) with no per-entry allocation and no sort. Chromium index
  70 MB -> 38 MB; index phase ~160-230 ms -> 67-87 ms.
- File-id table: hashbrown HashTable<u32> hashed/compared through the file
  records instead of HashMap<(ObjectId, EntryKind), u32>: ~35 MB -> ~5 MB.

Chromium, 4 snapshots (HEAD, ~100, ~1000, ~5000):

| step            | heap before | heap now | snapshot creation now |
|-----------------|------------:|---------:|----------------------:|
| open @HEAD      |       83 MB |    59 MB |                556 ms |
| open @HEAD~100  |       85 MB |    61 MB |   122 ms (was 225 ms) |
| open @HEAD~1000 |       90 MB |    66 MB |   124 ms (was 242 ms) |
| open @HEAD~5000 |      102 MB |    77 MB |   171 ms (was 274 ms) |
| idle after rg   |      107 MB |    82 MB |                       |

git status unchanged with v4 (1140-1160 ms warm). Per-snapshot page cache on
open (~195 MB) is dominated by the kernel's cached directory listings (>= one
4 KB page per directory, 46k dirs), not the index.

## Checkout filters (eol / LFS): measured cost of attribute evaluation

`gitfold bench-attrs` (chromium, 512k files): building a gix index from the
tree 190 ms; attribute lookups 580-630 ms single-threaded, 89-94 ms parallel.
Files that would need conversion with Linux defaults: chromium 1
(`text eol=crlf`), spark 19 (*.cmd/*.bat), linux 0 (no .gitattributes).

## Additional measurements (logged retroactively, 2026-10-25)

Taken during development on the same machine; referenced by README.md and
ARCHITECTURE.md. Sizes are decimal (MB = 10^6 bytes) unless marked MiB.

Repository sizes: spark 27,599 files, 210 MB of blobs at HEAD; Linux kernel
97,203 files, 1.67 GB; chromium 511,958 files, 4.36 GB (4,158 MiB), 64 GB pack.

Object database access (spark, warm): `git cat-file -p` per file, 200 random
files: 472 ms (~2.4 ms per process); one `git cat-file --batch` for the same
200 files: 37 ms; `git grep -c SparkContext HEAD`: 250 ms; Claude Code's
grep (embedded ugrep) on the native checkout: 102-104 ms; rg: 30-35 ms.

open vs no-open (spark, `bench/probe.sh`, directories warm, content cold/warm):
with kernel opens rg 276 ms cold / 177 ms warm; with FUSE_NO_OPEN_SUPPORT
186 ms / 42 ms; native warm 35 ms. This run is separate from the bench.sh
table above (native rg 47 ms there).

Snapshot overlap, chromium (by object id, from `git ls-tree -r -t`):

| vs HEAD   | committed before HEAD | trees shared | files shared | entries in unshared trees |
|-----------|----------------------:|-------------:|-------------:|--------------------------:|
| HEAD~100  |              ~22 h    |        99.4% |       99.94% |                      2.3% |
| HEAD~1000 |              ~2.3 d   |        95.3% |        99.2% |                     16.2% |
| HEAD~5000 |              ~6.5 d   |        81.2% |        96.1% |                     37.0% |

Old per-node design: 342 MB heap after creating one chromium snapshot of 558k
entries, ~610 bytes per entry.

Daemon heap after one full content read of chromium (rg over everything),
old per-node design, by setting:

| setting                                              | heap    |
|------------------------------------------------------|--------:|
| defaults then (32 MB/thread object cache set by gitfold, 96 MB/thread delta-base cache, 512 MB blob cache) | 4451 MB |
| delta-base cache 0                                   | 3117 MB |
| object cache 0                                       | 3335 MB |
| object cache 0 + delta-base cache 0                  | 2509 MB |
| ... + blob cache 64 MB                               | 1015 MB |
| MALLOC_ARENA_MAX=2 alone                             | 4073 MB |

Speed with object and delta-base caches off: unchanged (cold rg 2600 ms vs 2900 ms,
warm unchanged); bench-read 24 threads 6.6 GB/s vs 5.5 GB/s.

glibc retention, shared store, 4 chromium snapshots: heap 904 MB after the
first full rg; with a fixed M_MMAP_THRESHOLD 296-385 MB; with the idle trimmer
107 MB once reads stop.

Prewarm content, chromium: 3800 ms with 8 threads, 2600 ms with 24.

After index buffer preallocation (latest build): nearby chromium snapshots
97 ms (HEAD~100) and 116 ms (HEAD~1000); first snapshot 556-814 ms across runs
(tree decoding varies run to run).

bench-attrs, chromium: attribute lookups 630 ms single-threaded and 94 ms
parallel in the corrected measurement.

Not measured: time of `git worktree add` on a 20-40 GB monorepo.

## 2026-10-25, writable workspaces (phase 1) and read-write mount

Spark (scratch clone; workspace on a branch at HEAD vs snapshot @HEAD, same
mount, after prewarm; first / second run):

| cmd        | snapshot      | workspace     |
|------------|---------------|---------------|
| find       | 12 / 12 ms    | 11 / 11 ms    |
| rg         | 185 / 61 ms   | 196 / 45 ms   |
| git status | 118 / 71 ms   | 131 / 75 ms   |

Fixes found while measuring:

- Workspace readdir rebuilt the merged listing for every page the kernel asked
  for (quadratic in a 7.5k-entry directory) and stat-ed every entry: prewarm of
  a spark workspace 6000 ms. Listing built once per directory and cached while
  paged, lower entries taken from the tree table without a stat: 111-162 ms.
- Root and namespace directories (`feature/` for `feature/x`) had a zero TTL for
  both entry and attributes, so every path walk through `feature` was a lookup:
  3 rg runs in `feature/x` made 30,469 lookups. Entries now get a long TTL
  (`entry_with_ttls`): 52 lookups. Namespace attributes get a 1 s TTL instead
  of 0, because `default_permissions` re-checks them on every path walk:
  getattr 64,426 -> 34,009 (snapshot: 33,981).
- The mount is now read-write (workspaces), and on a writable mount the kernel
  marks atime stale after each readdir/read, so the next stat goes to the daemon.
  Chromium first find after prewarm went 351 -> 759 ms, first git status
  1160 -> 1290 ms; prewarm now stats each directory (and file, for content
  prewarm) after reading it: back to 352 ms and 1177 ms. `default_permissions`
  itself made no difference (63 ms vs 63 ms spark cold find without the fix).

Chromium snapshot after these changes: find 352 / 351 ms, rg 2970 / 433 ms,
git status 1177 / 1167 ms, Claude's grep 5443 / 4646 ms (native 471, 370, 908,
4243 ms).

Chromium workspace on a new branch at HEAD: open 845 ms (worktree registration
+ index), prewarm 1434 ms; find (bfs) 90 / 86 ms for all 511,663 regular files; git
status (Claude's invocation) 1227 / 1226 ms; rg 3413 / 416 ms; an edit shows
as ` M README.md`.

`tests/workspace.sh ~/work/spark`: 39 checks (edits, truncation, chmod,
symlinks, utimes, deletions, opaque recreated directories, renames incl. EXDEV
fallback, git add/commit, persistence across remount, namespaces, slashed rev
snapshots, refusing a branch checked out elsewhere, read-only snapshots/root),
all passing.

## 2026-10-26, workspace review fixes and re-measurement

Fixes from code review (all covered by `tests/workspace.sh`, now 53 checks):
listing invalidation after (not before) each mutation, lock before path
resolution, setattr without following symlinks (utimensat AT_SYMLINK_NOFOLLOW),
atomic rmdir emptiness check, copy-ups via a private tmp/ directory, upper
directories created 0755 regardless of umask, fsync passed through, node table
freed on kernel forget (re-created files get their path-derived inode back),
bare repositories' HEAD not treated as a checkout, workspace index written from
the branch tip when a registration is (re)created.

Markers as an append-only log (compacted on open and in process): `rm -r` of
spark's 7.5k-file `sql/hive/src/test/resources/golden` in a workspace takes
199-226 ms; the log stays bounded (~130 kB right after, 72 bytes after the next
open). The previous whole-file rewrite per change was quadratic.

Chromium, one session, current build (first / repeat; native column from the
same session; `find` is GNU find, run by bench.sh):

| cmd        | native        | snapshot      | workspace (fresh) |
|------------|---------------|---------------|-------------------|
| open       |               | 839 ms        | 847 ms            |
| prewarm    |               | 641 ms        | 1366 ms           |
| find       | 474 / 473 ms  | 355 / 353 ms  | 353 / 350 ms      |
| git status | 864 / 881 ms  | 1145 / 1131 ms| 1200 / 1171 ms    |
| rg         | 367 / 366 ms  | 2975 / 403 ms | 3357 / 410 ms     |
| Claude grep| 4226 / 4256 ms| 5287 / 4599 ms|                   |

Snapshot prewarm 641 ms vs 387 ms before the read-write mount (it now stats each
directory it read). The earlier "workspace find 90 ms" was measured with
Claude Code's `find` shell function (bfs), not GNU find, and is not comparable.

Daemon heap with one chromium workspace: 177 MB after prewarm, 58 MB with
`--prewarm none` (node table ~120 MB, ~215 B per looked-up entry). One snapshot
alone: 59 MB.

## 2026-10-26, second review round

Fixed (and covered by `tests/workspace.sh`, 57 checks): nodes are claimed
together with the kernel reference in one locked step (a separate count raced
with `forget` and could free a node the kernel still used); listings no longer
create nodes (readdirplus claims per sent entry and gives back entries that
don't fit); freed directory nodes drop their child maps; `tmp/` is cleared on
open and a failed copy-up removes its temp file (leftovers made later copy-ups
fail with EEXIST); whiteouts are logged before the upper file is removed, and
before a rename (crash ordering); `base` is written atomically, worktree
registrations write HEAD last and are rewritten if incomplete; copy-up, create
and mkdir set exact modes regardless of the daemon's umask; reads, readlink,
getattr and lookup resolve and read under the workspace lock; a cached listing
is dropped only when a request starts past its end (it was rebuilt by the
kernel's closing request); markers are ordered sets, so "markers below a
directory" is a range query instead of a full scan.

No speed or memory change: spark warm find 29 ms, rg 45 ms, git status 70 ms;
chromium workspace after prewarm 176 MB heap, GNU find 350 / 352 ms, git status
1190 / 1187 ms, prewarm 1344 ms.

## 2026-10-26, third review round

Fixed (and covered by `tests/workspace.sh`, now 61 checks): readdirplus pages
are served with the workspace lock held (`Workspace::page`), so a racing `rm`
can't make a page hand the kernel a node for the deleted name (stress test:
300 create/delete cycles while `ls -l` loops, no phantom entries); the
workspace base is pinned by `refs/gitfold/<id>` so `git gc` can't prune it
after amend/rebase/reset; listings no longer cache stat data for upper
entries, so writes and attribute changes don't invalidate cached listings
(only structural changes and copy-ups do); plain-readdir inode numbers probe
like real allocation; `mknod`/`link` outside workspaces answer EROFS;
removing the virtual `.git` answers EPERM; atomic writes are synced before
the rename. Spark speed unchanged (warm find 30 ms, rg 45 ms, git status 74 ms).

## 2026-10-26, fourth review round

No critical or major findings. Minor fixes (tests/workspace.sh now 62 checks): snapshot files and
directories report no write bits (`test -w` used to say yes on a mount that is
no longer `ro`); listings are invalidated as soon as a copy-up or creation
happens, even if the rest of the operation then fails.

## 2026-10-26, phase 2: invisible rebases

Workspaces now move their base to the branch tip (1 s poll, only while git is
idle). Spark workspace, times from the daemon log (whole rebase / with the
workspace lock held):

| change                                   | copied up | dropped | total     | locked    |
|------------------------------------------|----------:|--------:|----------:|----------:|
| commit of 200 edited files               |         0 |     200 |  14.21 ms |  11.58 ms |
| commit of 2,000 edited files             |         0 |   2,000 |  80.08 ms |  61.90 ms |
| `reset --soft HEAD~303` (2,858 changed)  |     2,857 |       0 | 408.17 ms | 404.34 ms |
| `reset --soft` back to the tip           |         0 |   2,857 | 109.18 ms | 109.18 ms |

Of the 2,858 paths the reset changed, 2,857 were files the view showed (copied
up) and 1 a file only the older commit had (whited out).

The first version synced the whole file system (`syncfs`) on every rebase: 549 ms
for the 200-file commit. It now syncs only what the rebase created (files,
directories, the marker log): `fsync` per file up to 64 of them, one `syncfs`
above that (with an fsync per file, the 2,857-copy rebase took 2,260 ms instead
of 404 ms). `git status` after each commit and after the reset back to the tip:
0 lines, 73 ms (clean by stat alone: dropped files keep their mtime and ctime);
after `reset --soft HEAD~303` it lists the undone commits' files as staged.
`tests/rebase.sh`: 101 checks (merges and rebases with conflicts, `--abort`,
stash, amend, resets, recreated directories, file/directory type changes, a
rebase retried after failed copies, a stale opaque marker, bisect, an external
`update-ref`, 20 chained `commit && status && cat` runs racing the rebases),
all passing.

## 2026-10-26, phase 3: compact node table, writes

Node table: one hash table of 32-byte records keyed by local id, names in one
shared buffer (compacted when mostly garbage), and a (parent, name) index of
local ids only, replacing a map of boxed nodes plus a per-directory map of
boxed names. Chromium workspace, daemon anonymous memory after prewarm
(558,320 entries), two runs each:

| build                    | anonymous memory (MB) | node table (MB, minus 58 MB baseline) |
|--------------------------|----------------------:|--------------------------------------:|
| before                   | 182.2 / 181.8         | ~124 (~222 B/entry)                   |
| names inline, flat index | 148.9 / 148.5         | ~91 (~163 B/entry)                    |
| final                    | 125.0 / 125.1         | ~67 (~120 B/entry)                    |

56% of Chromium's names are longer than 23 bytes, so inline names alone saved
little. Speed unchanged (before / after, ms, two runs): GNU find 345 347 /
346 344, git status 1203 1172 / 1202 1188, rg 4132 1413 / 4040 1412 (this `rg` is `rg -q` of a never-matching pattern
on the mount path, not `bench.sh`'s invocation, hence the slower repeat).

Writes (`bench/writes.sh ~/work/spark`, ms, two runs each; native is a plain
directory on the same ext4 disk):

| workload                            | native  | workspace (before) | workspace |
|-------------------------------------|--------:|-------------------:|----------:|
| untar the checkout (27,599 files, 227 MiB) | 525 530 | 3448 3354          | 2997 2801 |
| rm -r of it                         | 326 329 | 1301 1218          | 1234 1231 |
| write a 1 GiB file                  | 397 391 | 766 722            | 772 711   |
| cp -r sql/ (21,094 files)           | 479 462 | 2240 2171          | 1810 1832 |
| append to 1000 files                | 31 30   | 111 44             | 114 43    |

"Before": creating a file `mkdir`ed every ancestor directory in upper (each
failing with EEXIST) and stat-ed the new path three times; a write stat-ed the
file before opening it. Now a create is a stat of the parent, open, fchmod and
fstat, and a write opens first (`O_NOFOLLOW`), copying up only if the upper file is missing (ENOENT/ENOTDIR). In a
separate run with counters (untar of `sql/` and `cp -r sql/`: 46,113 created
files and directories) the daemon counted per created entry 1.0 lookups, 1
create, 1.2 writes, 0.5 setattrs (tar's utimes) and 2.4 getattrs (the kernel
re-reads attributes after a write), ~6 requests; during that untar (2,513 ms)
it was busy 1,780 ms, mostly in system calls. Writes are already up to 1 MB per request (the kernel's
`max_pages` limit).

## 2026-10-26, copy-on-rename

Renaming a workspace directory with lower content used to return EXDEV (`git mv`
of a directory from the base commit failed). It is now copied into upper first,
up to 10,000 files or 100 MiB. Spark, `rename(2)` of `sql/hive/src/test/resources/golden`
(7,500 files, 19 MB) in a fresh workspace, `--prewarm none`: 234 ms; `git
status --porcelain` right after: 663 ms (7,507 lines: the old files deleted,
the new directory untracked; nothing staged). `sql/` (21,094 files)
still returns EXDEV.

## 2026-10-26, builds in a workspace

`cargo build --offline` (debug) of gitfold itself: a workspace vs a native clone
on the same ext4 disk, dependencies in the shared cargo cache. The workspace's
`target/` (715 MB, 1,677 files; 1,938 counted as ignored by `gitfold list`)
lives in upper.

| build                                  | native   | workspace |
|----------------------------------------|---------:|----------:|
| full                                   | 13642 ms |  15990 ms |
| incremental (one source file touched)  |  1851 ms |   3905 ms |
| no-op                                  |   165 ms |    357 ms |
| `git status --porcelain` after         |     4 ms |      4 ms |

Large build outputs cost little (full build +17%); the incremental rebuild is
2.1x, mostly relinking the debug binary through the daemon. Many small files
are the expensive case: untarring Spark's 27,599 files takes 2997 ms vs 525 ms
(see "phase 3"), which is what a `node_modules` install looks like (`npm` was
not available to measure directly).

## 2026-10-26, `git worktree add` on Chromium

The comparison the README leads with, on the Chromium bench clone (pack in page
cache): `git worktree add --detach <dir> HEAD` took 17299 ms, and 17218 ms on a
repeat after removing it; the worktree uses 5,634 MiB of disk. A gitfold
workspace of the same commit opens in 845 ms and copies nothing.

## 2026-10-26, negative entries and long TTLs for the root

Reported from a large corporate repository: warm `rg` ~10x slower than native,
most of its `stat` calls failing (per-directory probes of `.gitignore`,
`.ignore`, `.git`). Lookups of missing names answered ENOENT, which the kernel
doesn't cache, and the root's zero attribute TTL made every absolute path
through the mount refresh the root's attributes (`default_permissions`
re-checks every directory on the way).

Spark snapshot, one process stat-ing `.ignore`, `.gitignore` and `.git` in each
of its 3,558 directories by absolute path, or by relative path from the
snapshot root, three runs per fresh mount (`--prewarm none`):

| build           | absolute paths (ms) | relative paths (ms) | lookups (3 runs) |
|-----------------|--------------------:|--------------------:|-----------------:|
| before          | 222 191 187         | 145 111 112         | 35,568           |
| after           | 155 16 17           | 150 13 12           | 14,234           |
| native checkout | 12 11 12            | 9 9 9               |                  |

After, only the first run asks the daemon (3,557 directories plus 10,674
missing names). An intermediate build with negative entries but the old root
TTL took 109 ms by absolute path vs 12 ms by relative path: one `getattr` of
the root per absolute path.

The root and namespace directories now get 1-year TTLs like everything else;
the daemon invalidates their attributes and listings (fuser's notifier) when a
lookup adds a name. Negative entries are cached forever in snapshots and
workspaces, and for dot names in the root and namespaces too. `rg --files` of
the snapshot (ripgrep 15.2, default options) was already near native (24-27 ms
vs 20-26 ms) both before and after: this ripgrep doesn't probe every directory.

## 2026-10-26, cache tree in the synthesized index

Reported from a large corporate repository: git's untracked cache (`UNTR`,
which git adds to an existing index itself) was there but the cache tree
(`TREE`) wasn't, and adding it halved `git status` there. Git writes the
cache tree on commit (and `read-tree`), not when `status` refreshes the index,
so a registration's index went without it until the first commit.

Chromium bench clone, snapshot of HEAD on a fresh registration, after
prewarm, `git status --porcelain`, four runs per round:

| build           | runs, round 1 / round 2 (ms)          |
|-----------------|---------------------------------------|
| without `TREE`  | 1305 946 946 949 / 1128 949 954 939   |
| with `TREE`     | 907 728 718 722 / 902 718 730 725     |
| native checkout | 887 882 866 (three runs)              |

The index grows from 39,918 KiB to 41,535 KiB (46,362 cache tree nodes, all
equal to those `git read-tree HEAD` writes); opening the snapshot took the same
time (867 / 876 ms without, 860 / 840 ms with).

## 2026-11-12, prewarm threads per walk: 4 instead of half the CPUs

Reported from a monorepo of over a million files: a request cost under 20 µs
with up to 4 in flight and several times that with 8-16, so two walks of half the CPUs each (the old
default) kept the daemon saturated. The default is now 4 threads per walk.

Chromium bench clone, 24 CPUs, otherwise idle, metadata walk right after
opening (one run each, ms):

| walk      | 12 threads (old default) | 4 threads |
|-----------|-------------------------:|----------:|
| snapshot  | 481 479                  | 699 713   |
| workspace | 1343                     | 1098      |

Alone on an idle machine a snapshot walk takes longer with 4 threads; a
workspace walk, whose requests cost more in the daemon, already got faster.
With several walks (or agents) at once, fewer threads per walk leave the
daemon room.

## 2026-11-12, re-measurement of the README table

After the cache tree, the prewarm queue (4 threads per walk) and the other
changes since the last full run. Chromium bench clone (511,958 files), 24
CPUs, `GF_WAIT_PREWARM=1 bench/bench.sh ~/work/bench/chromium
PermissionRequestManager`, and the same with `GF_TARGET=<branch>` for a fresh
workspace (a branch at HEAD; `bench.sh` gained `GF_TARGET` and prints the
daemon's anonymous memory). ms, first run / repeat on a fresh mount after
prewarm; native from the same runs:

| cmd         | native          | snapshot     | workspace    |
|-------------|-----------------|--------------|--------------|
| open        |                 | 856          | 868          |
| prewarm     |                 | 698          | 1159         |
| find        | 467 / 478       | 349 / 352    | 351 / 351    |
| Claude grep | 5236 / 4223     | 5950 / 4188  | 5695 / 4119  |
| rg          | 376 / 365       | 2871 / 405   | 3266 / 395   |
| git status  | 865 / 861       | 884 / 706    | 942 / 721    |

Native from the workspace run: find 475 / 466, Claude grep 4172 / 4227, rg
360 / 362, git status 869 / 849. Daemon anonymous memory after the warm runs:
snapshot 63 MB after find, 64 MB after git status, 73 MB after Claude grep,
216 MB right after rg (the content cache, trimmed once reads stop);
workspace 124, 123, 134 and 298 MB.

`git status` (Claude Code's bash-edit-diff invocation, `--ignored=matching`, so without
git's untracked cache) is now as fast as native on the first run and faster
on the repeat: 1145 / 1131 ms for a snapshot and 1200 / 1171 ms for a
workspace before the cache tree.

## 2026-11-12, partial clones: finding missing objects

A partial clone's tree load tries every child to list everything missing at
once. gitoxide rescans the pack directory (under a store-wide lock) on every
lookup that misses, so with handles left at the default refresh mode each
absent blob cost a rescan. 60,000 files, `blob:none` clone, unreachable
remote, time until the open fails (the walk, then a fast-failing fetch):

| packs | refresh on every miss | refresh once, then none |
|------:|----------------------:|------------------------:|
| 1     | 2,250 ms (review)     | 1,288 ms                |
| 41    | 19,342 / 19,204 ms    | 1,284 / 1,268 ms        |

Every fetch adds a pack, so the old cost grew with use.

## 2026-11-12, Claude Code's per-command scan with git's untracked cache

After every Bash command Claude Code runs `git --no-optional-locks status
--porcelain --untracked-files=normal --ignore-submodules=dirty` (fsmonitor
and hooks off); it has no `--ignored`, so the untracked cache can serve it.
On a spark workspace (27.6k files, 3,558 directories), warm, 10 runs each:

| core.untrackedCache                   | per scan | directories listed (`opendir`) |
|---------------------------------------|---------:|-------------------------------:|
| false                                 |    41 ms |                          3,558 |
| true (filled by a plain `git status`) |    19 ms |                              0 |

The scan never writes the cache (`--no-optional-locks`). With
`--ignored=matching` git bypasses the cache and lists every directory.
Results matched uncached runs after additions and removals in lower and
upper directories, `.gitignore` edits, commits, `stash -u`, `clean` and an
invisible rebase.

## 2026-11-14, opens: what seeing every read would cost

`bench/opens.sh`: a fresh workspace (prewarm none) per repo, mode and command;
cold = first run, warm = median of three; opens per run from the daemon's
counters. "open" is `--kernel-open` (every `open()` a round trip, contents
still cached: `FOPEN_KEEP_CACHE`), "no-open" the default.

| repo (files)    | cmd         | no-open cold | open cold | no-open warm | open warm | opens/run |
|-----------------|-------------|-------------:|----------:|-------------:|----------:|----------:|
| spark (28k)     | rg          |       201 ms |    278 ms |        37 ms |    146 ms |    26,892 |
| spark           | claude-grep |       419 ms |    457 ms |        97 ms |    151 ms |    27,605 |
| spark           | tar         |      1585 ms |   1834 ms |       311 ms |    545 ms |    24,106 |
| linux (97k)     | rg          |       906 ms |   1062 ms |        84 ms |    510 ms |    97,508 |
| linux           | claude-grep |      1351 ms |   1487 ms |       357 ms |    689 ms |    97,506 |
| linux           | tar         |      9753 ms |   6820 ms |       966 ms |   1684 ms |    97,055 |
| chromium (512k) | rg          |      3757 ms |   5063 ms |       400 ms |   2680 ms |   509,973 |
| chromium        | claude-grep |      9037 ms |   9866 ms |      4168 ms |   4326 ms |   511,600 |
| chromium        | tar         |     29409 ms |  34593 ms |      4827 ms |   9743 ms |   510,674 |

`find` and Claude Code's `git status` open no files (git: 6-224 opens) and are
unchanged. An open costs about 4.5 µs; it is seconds per search only on large
trees, but agents search often. So read tracking doesn't use opens
(ARCHITECTURE, "Overlaps"). `claude-grep` hides most of it (ugrep is slow on its own); Claude
Code's Grep tool runs `rg`.

## 2026-11-17, re-measurement after the tree of branches

After branches built on branches, following (a daemon thread walking the tree every second), views
(`@committed`, `@live`, a request gate, new names under `@`) and the follow-ups. Same machine and
command as 2026-11-12: `GF_WAIT_PREWARM=1 bench/bench.sh ~/work/bench/chromium
PermissionRequestManager`, and `GF_TARGET=<branch>` for a fresh workspace. ms, first run / repeat
on a fresh mount after prewarm (2026-11-12 in brackets):

| cmd         | native      | snapshot                    | workspace                   |
|-------------|-------------|-----------------------------|-----------------------------|
| open        |             | 882 (856)                   | 906 (868)                   |
| prewarm     |             | 703 (698)                   | 1153 (1159)                 |
| find        | 474 / 474   | 350 / 348 (349 / 352)       | 351 / 353 (351 / 351)       |
| Claude grep | 5214 / 4228 | 7142 / 4187 (5950 / 4188)   | 5685 / 4167 (5695 / 4119)   |
| rg          | 375 / 362   | 2624 / 407 (2871 / 405)     | 2960 / 412 (3266 / 395)     |
| git status  | 855 / 855   | 881 / 697 (884 / 706)       | 944 / 714 (942 / 721)       |

No regression: everything is within run-to-run noise. The one outlier, the snapshot's first Claude
grep (7142 ms), is a first cold read of 4 GB of blobs; the workspace's first run, on the same data,
matches 2026-11-12 (5685 vs 5695), as do all repeats. Daemon anonymous memory after the warm runs:
snapshot 63, 77, 256 and 64 MB (2026-11-12: 63, 73, 216, 64; the rg figure is the transient
content cache), workspace 124, 135, 292 and 124 MB (124, 134, 298, 123).

`git status` in the workspace now prints one line natively absent: `!! .task`, the task folder's
symlink, listed only because Claude Code's invocation asks for ignored files (`--ignored=matching`).
It came with task folders (2026-11-13), not with this work.

Idle cost of the new daemon threads: a mounted Chromium clone with a workspace open, nothing
changing, used 2 clock ticks (20 ms) of CPU in 60 s.

## 2026-11-17, the tree of branches and views

`bench/tree.sh ~/work/bench/chromium PermissionRequestManager`, same machine as above, release
build. It mounts the clone (default flags) and builds a family at HEAD: `n` with one commit, and
`a`, `b`, `c` built on it (`--parent n`), each three commits on files of its own, `a` and `b` both
also appending to `AUTHORS` (one conflict, listed in `.gitfold-conflicts`). Commits are made
without a checkout (a temporary index, `commit-tree`). `a`, `b` and `c` have workspaces, each with
one uncommitted edit. Other families for the move and landing rows, below; everything is deleted
at the end. "reads" cats 1,023 files (every 500th of `git ls-files`, 8 MB). ms; cold = fresh
mount, warm = repeat; two full runs, which agreed within a few %, the second shown.

Bulk reads. first: the first access on a fresh mount (`@HEAD`: `stat`, then its metadata
prewarm is waited for, as bench.sh does; a view: `cat .gitfold-conflicts`, i.e. made and merged);
complete: until `@live` shows all three members' uncommitted edits (polled). Views have no
prewarm.

| cmd         | `@HEAD` (prewarmed) | `@committed/n`  | `@live/n`                     |
|-------------|---------------------|-----------------|-------------------------------|
| first       | 863-895             | 771-803         | 5766-5795, complete 15.7-16.0 s |
| find        | 350 / 350           | 2904 / 720      | 2755 / 764                    |
| Claude grep | 5193 / 4144         | 8718 / 4655     | 8811 / 5320                   |
| rg          | 2598 / 406          | 3687 / 428      | 3586 / 1402                   |
| reads       | 56 / 26             | 100 / 31        | 86 / 28                       |
| daemon RSS  | 85 MB               | 86 MB           | 260 MB                        |

(`@live` cold/warm are measured once the view is complete.)

Views, refresh and rest:

| what | first | repeat |
|---|---:|---:|
| `@live`: an edit saved in `a`'s workspace, until seen in the view (3 rounds) | 4042 | 4043, 3924 |
| `@committed`: `git commit` in `c`'s workspace, from its end until seen | 596 | 755 |
| resting view (`GITFOLD_VIEW_REST_SECS=5`, rested 10 s), nothing changed: `@committed` | 3 | 3 |
| same, `@live` | 189 | 3 |
| `@live`, an edit saved in `a` while it rested, the first access (sees it) | 1081 | 988 |
| `@committed`, a commit on `b` while it rested, the first access (sees it) | 17 | 17 |

Branch operations (total wall time of the command):

| what | first | repeat |
|---|---:|---:|
| `gitfold branch t --from a --from c` (no conflict; opens `t`, git merges there) | 3816 | 3748 |
| `gitfold branch t -w --from a --from c` (each with an uncommitted edit, both carried) | 7978 | 7184 |
| native: `git switch -c t a && git merge c` in a full worktree | 2063 | 1607 |
| native: `git merge-tree --write-tree a c` (in memory) | 3 | 3 |
| `gitfold parent u1 --update`, u1 without a workspace (2 commits onto a new parent commit) | 112 | 107 |
| native: `git replay --onto` of the same (in memory, no refs) | 79 | 77 |
| `gitfold parent u2 --update`, u2's workspace open, clean | 1946 | 1944 |
| same, with an uncommitted edit (carried) | 4961 | 4838 |
| native: `git rebase` in a full worktree, clean | 1969 | 1936 |
| native: `git rebase --autostash` there, with an uncommitted edit | 5257 | 5116 |
| `gitfold parent c7` (c7 on p7 on g7), p7 not landed | 72 | 73 |
| same after p7 is squash-merged into g7 and g7 moved on (note computed) | 56 | 57 |
| `gitfold changes -- AUTHORS`, 11 branches, 11 workspaces | 9059 | 9120 |
| `gitfold changes -- docs` | 9149 | 9122 |

The landing rows are the same with the recorded check dropped (`git config --unset
branch.c7.gitfoldLanding`): 72 and 57 ms. Landing is checked against the parent's parent (`g7`)
because the script doesn't touch main; the not-landed row checks `origin/main` and `main` too.

Notes:

- **`gitfold changes -- PATH` takes 9 s, and that is git, not gitfold:** 8.27 s of it is `git
  for-each-ref --no-merged=origin/main refs/heads` (GIT_TRACE2_PERF), which, for a branch that is
  not merged, walks all of `origin/main`'s 1.85M commits: this clone has no commit-graph. In a
  `git clone --shared` of it with `git commit-graph write --reachable` (15 s, once), the same
  query takes 2 ms. The rest, 11 workspaces' changes and 11 `git diff`s, is ~0.8 s. Clones that
  have had `git gc`/`git maintenance` have a commit-graph; a fresh `git clone` doesn't.
- **`@live`'s first access is slow on a fresh mount: 5.8 s, and the view is complete only after
  ~16 s.** Opening `@live/n` resumes the three workspaces and makes their live trees one after
  another (the log shows the workspaces resumed 5 s apart), each a `git add -A` on a copy of the
  index through a workspace with nothing cached and no prewarm, like a cold `git status` without
  prewarm (4.1 s on Chromium, 2026-10-25). The first access returns at the 5 s wake limit with
  the members made so far. Once made, an edit shows in ~4 s (the 2 s quiet period, then ~2 s:
  the member's `git add -A` with warm caches, the merge, up to 50 ms of polling), and a view that
  rested catches up in ~1 s at its first access.
- **Views are read like a snapshot without prewarm** (2026-10-25's prototype numbers: find
  2.7 s, Claude grep 8.1 s, rg 3.2 s cold): first reads pay for metadata the prewarm would have
  loaded. Warm, `@committed` matches `@HEAD` except find (720 vs 350 ms). Warm `@live` rg is
  1.4 s vs 0.4 s, likely because its entries live only 1 s in the kernel (`LIVE_TTL`), so a warm
  run asks the daemon again (not verified). Its daemon held 260 MB, with the three workspaces the view opened.
- `@committed` itself is cheap: made and merged (three members, one conflict) in under 0.8 s,
  less than opening a snapshot; a commit shows in it in 0.6-0.75 s; a resting one wakes in 3 ms,
  17 ms when a commit came meanwhile.
- Moves cost what git's rebase costs: in a workspace as natively in a full checkout (1.9 vs
  1.9-2.0 s clean; 4.8-5.0 vs 5.1-5.3 s with `--autostash`, which on Chromium is ~3 s by itself);
  without a workspace 0.11 s, near `git replay`'s 0.08 s.
- A trial merge is ~2x native (3.8 vs 1.6-2.1 s): opening the new workspace (~1 s), then git's
  merge in it, through the mount, while the new workspace's metadata prewarm (2.9 s) runs. `-w`
  doubles it again (7.2-8.0 s), copying each source's uncommitted state; one earlier run's first
  `-w` took 12.2 s, where the log shows the two sources being opened in this mount first
  (resumed 4 s apart). Not broken down further.
- The landing check is cheap (56-73 ms), recorded or not.

## 2026-11-17, views performance and commit-graph

Same machine, clone and command as the section above (`bench/tree.sh ~/work/bench/chromium
PermissionRequestManager`), before (main at a0b1f64, re-run today) and after four changes:

- `@live` makes its members' live trees side by side (at most 4 at once) instead of one after
  another;
- views are walked by the metadata prewarm when they open, as snapshots are (cold runs now start
  once it is done, as for `@HEAD`; the new column "prewarmed" is from the first access);
- `@live`'s entries and attributes are cached long, as `@committed`'s (they were cached 1 s);
- the mount daemon keeps the commit-graph current, at the lowest priority: git's own `git
  maintenance run --task=commit-graph --auto`, which writes it with `git commit-graph write
  --reachable --split` once 100 commits are missing. (Measured with an earlier form that ran
  that write itself when a branch tip was missing; the write is the same.) **The bench clone has
  had a commit-graph since this run** (written by gitfold at a mount, 15.4 s, 125 MB, 2.18M
  commits): later baselines of commit walks on it are with one.

The views rows were measured without a commit-graph first (its upkeep turned off), then all
again with it: they agreed within a few %, the second shown. ms, first / repeat:

| | before | after |
|---|---:|---:|
| `@committed/n` first access (made, merged) | 785-795 | 776-790 |
| `@committed/n` prewarm done (from the first access) | none | 1856-1908 |
| `@committed/n` find, cold / warm | 2876 / 748 | 352 / 356 |
| `@committed/n` Claude grep | 9039 / 4618 | 5483 / 4141 |
| `@committed/n` rg | 3682 / 432 | 2866 / 353 |
| `@committed/n` reads | 94 / 29 | 60 / 30 |
| `@live/n` first access | 5779-5802 | 5777-5793 |
| `@live/n` complete (all three edits shown) | 16058-16571 | 6198-6287 |
| `@live/n` prewarm done | none | 7275-7331 |
| `@live/n` find | 2820 / 766 | 353 / 361 |
| `@live/n` Claude grep | 8856 / 5304 | 5495 / 4149 |
| `@live/n` rg | 3583 / 1410 | 2869 / 359 |
| `@live/n` reads | 91 / 28 | 56 / 29 |
| daemon RSS, `@committed` / `@live` runs | 85 / 262 MB | 146 / 320 MB |
| `@live` resting, nothing changed, first access | 347, 4 | 3, 3 |
| `@live` resting, an edit meanwhile, first access | 1066, 972 | 1072, 993 |
| `gitfold changes -- AUTHORS` | 9043 / 9038 | 829 / 819 |
| `gitfold changes -- docs` | 9047 / 9056 | 836 / 809 |

`@HEAD`, refresh, the other rest rows, trial merges, moves and the landing check are unchanged
(within noise): `@HEAD` find 349-353, rg 2606-2638 / 402-427; an edit shows in `@live` in
3.99-4.05 s, a commit in `@committed` in 0.6-0.7 s; `branch t --from a --from c` 3.8 s;
`parent u2 --update` 1.9 s clean, 4.8-4.9 s dirty; landing 56-77 ms. The commit-graph doesn't
speed those: they are dominated by worktree work, not commit walks.

Notes:

- **The 1 s TTL was the cause of slow warm `@live` reads, verified:** a dedicated mount (`n` and
  one child with a workspace, `@live/n` complete, then rg once cold and 1 or 3 times warm, op
  counts from the daemon at unmount, the difference per warm run): before, each warm rg cost
  278k lookups (and 1.4 s, 0.8-0.9 s when run back to back within the second); after, 0 lookups,
  as `@committed` (0 before and after), and 0.35 s. A resting view still wakes on the next path
  from above it (the kernel forgets its root's entry when it rests, as for `@committed`) and on
  any listing in it.
- **`@live` complete: 16 s to 6.2 s**, the three live trees made at once (each `git add -A`
  through a workspace with nothing cached takes ~5 s alone, a little more side by side). The
  first access still returns at the 5 s wake limit (5.8 s, the tips' merge included): the
  slowest live tree takes longer than that.
- **Cold reads in views now match `@HEAD`'s** (find 0.35 s vs 2.8-2.9 s): the walk loads the
  metadata, 1.9 s after `@committed`'s first access, 7.3 s after `@live`'s (measured with its
  walk waiting for the live trees; it now starts once they are in). The cost: the daemon holds
  the walked nodes, ~60 MB more for a Chromium-sized view. A view is walked once, when it opens;
  not when it wakes, and not for what a refresh changed (the kernel was told to forget those
  paths; reading them loads them again).
- **`changes -- PATH`: 9.0 s to 0.83 s.** The commit-graph write took 15.4 s at mount (nice 19,
  idle I/O class), in the background; an incremental one (`--split`, a branch moved) takes
  0.14 s. Requests never wait for it.

## 2026-11-18, conflict notices

`bench/notices.sh ~/work/bench/chromium /tmp/gf-v041/target/release/gitfold` (the second argument
a v0.4.1 build, for the idle row), same machine as above, release build of feat/conflict-notices
with one debug line added (`conflict notices: <branch> at <tip>: checked in <time>`, the
notices thread's time per step). It mounts the clone and makes, at HEAD: `p` with children `a`
and `b`; `x` and `s1`..`s6` without a parent (siblings under the root), `s1` with children `s1a`,
`s1b`: 12 workspaces (13 open, with the clone's own `bench-ws`), each with one commit and one
uncommitted edit on Markdown files of its own. Commits are made without a checkout (a temporary
index, `commit-tree`, `update-ref`), timed from the ref update; a notice is timed until its file
is in the inbox folder (polled every 10 ms). Each commit is on a new file (a conflict told once
isn't told again). Cold = the first commit on a fresh mount, warm = the median of the next 4; two
full runs, which agreed within a few %, the second shown. ms:

| what | cold | warm |
|---|---:|---:|
| 1. `a` commits a line on a file `b` changed uncommitted: until in `b`'s inbox | 2310 | 1932 |
| same, `a`'s inbox (the committer) | 2317 | 1938 |
| same, `p`'s inbox (where they land) | 2323 | 1944 |
| the check (notices thread) | 1580 | 986 |
| 2. `a` commits a file no one else changed: until checked (no messages) | | 946 |
| the check | | 13 |
| 3. `x` (no parent) commits, 12 workspaces related, no overlap: until checked | | 961 |
| the check | | 33 |
| same, `s3` edits the file uncommitted: until in `s3`'s inbox | | 1962 |
| the check | | 999 |

4. Idle, nothing changing, 12 workspaces open: the daemon's CPU (user + system, its git children
included) in 60 s: this build 190-220 ms (four measurements over two runs), v0.4.1 170-190 ms
(two), at 10 ms tick resolution.

Notes:

- **A notice arrives ~2 s after the commit; the check is a live tree.** The sync thread polls the
  tip every second (up to 1 s before the step is seen: the 930-960 ms of the no-overlap rows,
  check included), then the check: ~1 s warm, 1.6 s for the first one on a fresh mount, nearly
  all of it the other workspace's live tree (`git add -A` through the mount, as `@live` makes
  it). The three messages are written within 15 ms of each other.
- **What no one meets costs nothing:** 13 ms with 2 related workspaces, 33 ms with 12 (the
  stored-changes prefilter, a merge base and one `git diff` of the committed paths per
  workspace, serially: ~2 ms each). No live tree is made, no merge run. The serial `git diff` per
  candidate isn't worth parallelising at this size.
- **The commit path costs nothing while idle:** within tick noise of v0.4.1 (a difference of at
  most ~30 ms of CPU a minute with 12 workspaces).
- **Case 3 stands for a commit on the main branch too.** It commits on `x`, a branch without a
  parent, because the bench clone's checkout is on main and isn't moved; a main branch commit
  is checked against the same set (main's view, the branches without a parent and what is
  built on them), so it costs the same.
