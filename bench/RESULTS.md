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

## What did not matter (cold find on spark stays ~220-250 ms)

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
| spark    |    32 ms |        36 ms |    244 ms (200 MB, 8 thr) |
| linux    |    92 ms |        67 ms |    1.0 s (1.6 GB, 8 thr)  |
| chromium |   567 ms |       387 ms |  2.6 s (4.2 GB, 24 thr)   |

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

With `--prewarm content`, the first rg on chromium is 495 ms instead of 2.9 s.

### Findings

- Decompression is not the bottleneck: `gitfold bench-read` decompresses all
  of chromium's 4.2 GB in 0.76 s on 24 threads (5.7 GB/s; 571 MB/s on one
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
| rg in @HEAD (2.9 s)          | 107 MB idle |    +1109 MB |   +5633 MB |
| rg in @HEAD~100 (0.40 s)     | 108 MB idle |    +1109 MB |   +5677 MB |
| rg in @HEAD~1000 (0.53 s)    | 107 MB idle |    +1112 MB |   +6078 MB |
| rg in @HEAD~5000 (0.66 s)    | 107 MB idle |    +1119 MB |   +6838 MB |

Before (per snapshot, no sharing): ~390 MB heap (1 GB after reads),
~600 MB slab, +4.2 GB page cache once grepped. For these 4 snapshots that
would be ~4 GB heap, ~2.4 GB slab, ~17 GB page cache.

Per additional snapshot now: heap +1..11 MB, slab ~+160 MB (dentries and
directory inodes stay per snapshot), page cache ~+190 MB on open (per-snapshot
directory listing pages + its git index file) plus only content not seen in
other snapshots when read. Snapshot creation for nearby commits ~0.23 s
(new trees 2-40 ms; rest is index synthesis: sort, write, SHA-1 of ~60 MB).

Speed unchanged vs previous defaults (chromium warm: find 349 ms, rg 416 ms,
git status 1.14 s; linux warm rg 92 ms).

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

git status unchanged with v4 (1.14-1.16 s warm). Per-snapshot page cache on
open (~195 MB) is dominated by the kernel's cached directory listings (>= one
4 KB page per directory, 46k dirs), not the index.

## Checkout filters (eol / LFS): measured cost of attribute evaluation

`gitfold bench-attrs` (chromium, 512k files): building a gix index from the
tree 190 ms; attribute lookups 580-630 ms single-threaded, 89-94 ms parallel.
Files that would need conversion with Linux defaults: chromium 1
(`text eol=crlf`), spark 19 (*.cmd/*.bat), linux 0 (no .gitattributes).
