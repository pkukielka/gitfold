# Benchmark results

Machine: 24 cores, 62 GB RAM, NVMe, Linux 6.12. Pack files and the native
checkout are in page cache in all runs. "cold" = fresh mount (kernel FUSE
caches empty), "warm" = repeated run on the same mount. `bench/bench.sh`.

Commands are what a coding agent runs: Claude Code's `grep` (embedded ugrep,
`-G --ignore-files --hidden -I`), `rg`, `find`, and the exact `git status`
invocation Claude Code issues itself (fsmonitor disabled).

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
