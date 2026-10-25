# Architecture

How gitfold works and why it works that way. Most decisions were made by
measuring; all numbers come from [bench/RESULTS.md](bench/RESULTS.md) (24 cores,
62 GB RAM, Linux 6.12) on three repositories: Apache Spark (27.6k files),
the Linux kernel (97k files) and Chromium (512k files, 4.4 GB at HEAD, 64 GB
pack). User-facing behaviour is described in [README.md](README.md).

Contents: [Goals](#goals-and-non-goals) · [Overview](#overview) ·
[Code map](#code-map) · [Glossary](#glossary) · [Invariants](#invariants) ·
[Decisions](#decisions) · [Rejected ideas](#rejected-ideas-index) ·
[Limitations](#known-limitations) · [Future work](#future-work)

## Goals and non-goals

Goals:

- **Many revisions of a huge repository, side by side.** The motivating case is
  coding agents and code review on 20-40 GB monorepos, where `git worktree add`
  is too slow and too big, and a hand-managed pool of worktrees is too rigid.
- **Look like a normal checkout to an agent.** Agents run `git status`,
  `git diff`, `find`, `grep`/`rg` and read files. Claude Code, for example, runs
  all searches through Bash (its `grep` is an embedded ugrep using ordinary
  syscalls) and periodically runs
  `git status --porcelain=v2 --untracked-files=normal --ignored=matching` itself
  with fsmonitor disabled. There is no API to hook into: the filesystem must be
  fast at plain `stat`, `readdir` and `read`.
- **Any Linux distribution, no root.**
- **Writable branches later** (copy-on-write, diff persisted on disk). The
  read-only design must not block that.

Non-goals for now: macOS and Windows; applying checkout filters
(`.gitattributes` conversions, LFS); writes.

## Overview

```
 agent: find / rg / git ...
          |  syscalls
   Linux VFS + FUSE  ---- dentry, inode and page caches (do most of the work once warm)
          |  /dev/fuse
   fuser worker threads
          |
   fs/mod.rs (FUSE adapter)
          |
   fs/view.rs (what an inode is)
      |                 |                     |
   fs/snapshots.rs    store.rs             fs/content.rs
   @<rev> registry    shared tree tables   per-inode read cache
      |               and file ids            |
   worktree.rs           \                    |
   (.git/worktrees/,      `---- git.rs (gitoxide object database) ----'
    index v4)
                                  (lines are calls)
```

A snapshot is created on the first lookup of `@<rev>` in the mount root
(`Snapshots::resolve`):

1. resolve the rev to a commit (`git::resolve`);
2. decode every git tree not seen before into the shared store
   (`Store::load_tree`, parallel over subtrees);
3. lay out the snapshot's directories (`Store::dir_table`: one 8-byte `DirRec`
   per directory, holding its tree id and the index of its first subdirectory);
4. stream a git index (`Store::write_index` into `IndexWriter`) and register a
   linked worktree (`worktree::register`);
5. add the name to the root and start a background prewarm walk
   (`prewarm::spawn`).

Nothing per file is stored for a snapshot. An inode number says where its data
lives; for example inode `(3 + 1) << 32 | 17` is directory 17 of slot 3: entry
17 of that snapshot's `DirRec` array gives a tree id, which names a shared
`TreeTable`.

## Code map

| file | role |
|---|---|
| `src/fs/mod.rs` | FUSE adapter: translates requests and owns kernel-facing policy (TTLs, init flags, `ENOSYS` replies) |
| `src/fs/view.rs` | what an inode is: attributes, lookups, directory listings, content |
| `src/fs/snapshots.rs` | `@<rev>` registry: resolving names, creating snapshots, root listing |
| `src/fs/content.rs` | per-inode content cache and idle trimmer |
| `src/fs/stats.rs` | operation counters, logged at unmount |
| `src/store.rs` | shared tree tables, file ids, directory layout, index walk |
| `src/inode.rs` | inode encoding, constant timestamp |
| `src/worktree.rs` | worktree registration, streaming index v4 writer |
| `src/git.rs` | thread-local gitoxide handles, tree and blob access |
| `src/prewarm.rs` | background walk of new snapshots |
| `src/mountpoint.rs` | mount point checks, unmount on SIGINT/SIGTERM |
| `src/benchread.rs`, `src/benchattrs.rs` | hidden `bench-read` and `bench-attrs` commands |
| `bench/` | `bench.sh` (native vs mount), `probe.sh` (content reads), `RESULTS.md` (measurement log) |

## Glossary

- **Snapshot**: one root entry `@<rev>`, showing the tree of a commit.
- **Slot**: a snapshot's index in the registry. Every root name gets its own
  slot, even when two names resolve to the same commit.
- **Tree table** (`TreeTable`): one decoded git tree, shared by all snapshots
  that contain it; identified by a dense **tree id**.
- **File id**: a dense `u32` per distinct (blob, mode). Mode is part of the
  identity because the executable bit and symlink-ness change a file's
  attributes.
- **`DirRec`**: a snapshot's record for one directory (tree id + first
  subdirectory index). A snapshot owns one array of them; a directory's position
  in it is its **dir index**.
- **Tree order**: git's order for tree entries: names compare bytewise, with a
  directory name followed by an implicit `/`. Example: `a.txt`, `a/b`, `a0`
  (because `.` 0x2E < `/` 0x2F < `0` 0x30). It is also the git index's path
  order.
- **Worktree registration**: the admin directory
  `<repo>/.git/worktrees/gitfold-<sha12>/` that makes a snapshot a linked git
  worktree.
- **Dentry**: the kernel's cached name-to-inode link. A directory inode may have
  only one (one parent).
- **TTL**: how long the kernel may trust a cached entry or attribute without
  asking gitfold again.
- **readdirplus**: a FUSE readdir that also returns each entry's attributes, so
  listing a directory doesn't trigger a separate lookup per entry.
- **Page cache**: the kernel's cache of file contents, shared by everyone who
  reads the same inode.
- **Prewarm**: gitfold walking a new snapshot through its own mount to fill the
  kernel caches.

## Invariants

Break one of these and something fails, often silently. Each links to the
decision that creates it.

- **Index stat data equals `getattr`.** For every file the index records exactly
  the mtime, ctime, inode (low 32 bits), uid, gid, mode and size that `getattr`
  reports. Any change to attributes (timestamps, sizes after checkout filters,
  uid) must change `write_index` too, or `git status` reads every file.
  ([index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem))
- **File attributes depend only on (blob, mode).** A file inode is shared by
  every snapshot that contains it, so nothing per snapshot or per path may show
  in its attributes: one timestamp, link count 1.
  ([inodes](#inode-numbers-are-computed-not-stored),
  [timestamps](#constant-timestamps))
- **Every directory inode has exactly one parent.** Directory inodes are per
  slot; never share a slot between two root names.
  ([inodes](#inode-numbers-are-computed-not-stored))
- **Tree tables are in tree order.** The streamed index relies on a depth-first
  walk producing index order, and `TreeTable::find` relies on it for binary
  search. Never re-sort or filter tables without checking both.
  ([store](#shared-content-addressed-store), [index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem))
- **Everything below the root is immutable.** That is what makes the 1-year TTLs
  correct. Anything mutable (the root today, writable workspaces later) needs a
  zero TTL or explicit invalidation. ([caching](#kernel-caching-does-the-heavy-lifting))
- **The root's mtime changes whenever a snapshot is added.** It is the only
  thing that invalidates the kernel's cached root listing.
- **Encoding limits:** file ids fit in `u32` (the index stores the low 32 bits
  of the inode); dir index < `u32::MAX` (`u32::MAX` is the `.git` file);
  slot + 1 < 2^31 (bit 63 marks files).
- **There are no file handles.** `open` and `opendir` return `ENOSYS`, so all
  state is keyed by inode.
- **Never hold the content-cache lock across decompression**; it serializes
  every read.

## Decisions

Each decision: what we do, why (with the measurement), and what was rejected.

### FUSE, reading straight from the object database

**What:** a FUSE filesystem that reads trees and blobs from the repository's
object database through gitoxide, in-process, with one thread-local repository
handle per thread.

**Why:** checkouts are what we are avoiding. Reading objects in-process is cheap:
spawning `git cat-file` per file costs ~2.4 ms of process startup, while one
long-lived reader fetched 200 random spark files in 37 ms. Decompression is not
a bottleneck either: gitoxide inflates all 4.4 GB of Chromium's files at HEAD in
0.76 s on 24 threads (`gitfold bench-read`).

**Rejected:**

- *Kernel overlayfs over some lower layer:* needs `CAP_SYS_ADMIN`; unprivileged
  users can only mount it inside a user namespace, where the mount is invisible
  to processes outside it, and several distributions restrict unprivileged user
  namespaces. FUSE mounts work for any user through the setuid `fusermount3`.
- *Go (go-fuse + go-git):* go-fuse is the more mature FUSE library, but go-git
  is slow and memory-hungry on large repositories, and libgit2 bindings for Go
  are unmaintained. On monorepos the object database is the hot path, so that
  decided it for Rust + gitoxide.

### Shared, content-addressed store

**What:** `store.rs` decodes each git tree once into a `TreeTable` (16-byte
entries plus one name buffer, in tree order), keyed by tree id, and gives each
distinct (blob, mode) a file id. File ids live in a hashbrown `HashTable<u32>`
that stores only the ids; hashing and equality look up the (object id, kind)
record by id, and the first 8 bytes of the (random) object id serve as the hash.

**Why:** snapshots of nearby commits share almost everything. Chromium commits
~6.5 days apart share 81% of trees and 96% of files; ~22 hours apart, over 99%.
A new snapshot only decodes trees it hasn't seen (2-40 ms on Chromium), so
opening it is dominated by writing its index. Daemon heap for Chromium: 59 MB
with one snapshot, 77 MB with four (HEAD, ~1 day, ~2 days and ~1 week older),
82 MB after reads with the idle trimmer.

**Rejected:**

- *A node per entry* (separately allocated, holding its full path, child list,
  hash-map slot and cached listing): ~610 bytes per entry, 390 MB per Chromium
  snapshot, nothing shared between snapshots. Four snapshots would need ~4 GB of
  heap, versus 77 MB now.
- *`HashMap<(ObjectId, EntryKind), u32>` for file ids:* a copy of every object id
  as key cost ~35 MB on Chromium; the id-only table costs ~5 MB.

### Inode numbers are computed, not stored

**What:**

```
file:       1<<63 | file_id
directory:  (slot + 1) << 32 | dir_index
.git file:  (slot + 1) << 32 | u32::MAX
root:       1
```

**Why:**

- *Files are identified by content*, so the same file has the same inode in
  every snapshot. The kernel then keeps one inode and one page-cache copy for
  all snapshots containing it, which a pool of real worktrees cannot do.
  Grepping a Chromium snapshot ~1 day older than one already grepped added
  44 MB of page cache instead of 4.4 GB.
- *Directories are per slot*, because the kernel allows a directory inode only
  one dentry (parent). Two root names resolving to the same commit (`@HEAD` and
  `@<its sha>`) share their `DirRec` array and worktree registration, but get
  separate slots, so separate directory inodes.
- *Nothing to look up:* decoding is arithmetic, so there is no inode table.

**Rejected:**

- *`hash(commit, path)` inodes with a node table:* the per-node design above.
- *Reusing a slot for a second name of the same commit:* creates a directory
  hardlink (two dentries for one directory inode). An earlier version did this;
  the code review caught it.

### Snapshots are real linked worktrees

**What:** each snapshotted commit gets
`<repo>/.git/worktrees/gitfold-<sha12>/` with `HEAD` (detached at the commit),
`commondir`, `gitdir`, `locked` and `index`; the snapshot's virtual `.git` file
points there.

**Why:** every git command works unmodified, including tools that use libgit2
or gitoxide. For writable branches it also brings git's own protection for
checked-out branches for free: git refuses `branch -f`, `branch -D`, `checkout`,
`fetch` into and `rebase` of a branch checked out in another worktree (plumbing
such as `update-ref` bypasses it, which a real worktree tolerates the same way).
`locked` keeps `git worktree prune` from deleting registrations while
unmounted.

**Rejected:**

- *Symlinking `.git` to the repository's `.git`:* the snapshot would share HEAD
  and the index with the main checkout, so `git status` would compare the wrong
  commit and the wrong index.
- *A `git` wrapper on `PATH` translating commands:* fragile, and it wouldn't
  cover tools that link git libraries.

### A synthesized git index whose stat data matches the filesystem

**What:** gitfold writes each snapshot's index itself, with exactly the stat
data `getattr` reports. The index is version 4, streamed.

**Why:** `git status` first `lstat`s every index entry and compares mtime,
ctime, inode, uid, gid and size with the index. When they match, the file is
clean without being read. With matching data, `git status` on spark reads 6
file contents out of 27.6k, and on Chromium it is about 30% slower than native
(1.15 s vs 0.88 s).

Version 4 prefix-compresses paths: each entry stores how many bytes to drop from
the previous path plus the new suffix. Chromium's index is 38 MB instead of
70 MB. Tree tables keep tree order, and in that order a depth-first walk yields
paths already sorted as the index requires, so entries are written straight to
the output buffer: no collection, no sort, no per-entry allocation. Opening a
nearby Chromium snapshot went from 225 ms (version 2, collect and sort) to
122 ms, and to ~100 ms with the buffer preallocated from the previous index.

**Rejected:**

- *Index version 2 with collect-and-sort:* twice the size, twice the time.
- *Letting git build the index on first `git status`:* git would re-hash every
  file, reading all content (4.4 GB on Chromium).
- *Skipping the index checksum (`index.skipHash`):* considered, not tried. It
  would save ~15 ms per snapshot, but depends on how the reading git version
  treats a missing checksum.

### Constant timestamps

**What:** everything inside a snapshot reports mtime, ctime and atime
2020-01-01. Only the mount root has a live mtime (see
[caching](#kernel-caching-does-the-heavy-lifting)).

**Why:** a file inode is shared by every snapshot containing that content, so it
can have only one timestamp, and the index must record the same value. Git and
agents don't rely on mtimes in a read-only tree; in writable workspaces,
modified files will be real files with real timestamps.

### Kernel caching does the heavy lifting

**What:** snapshot contents are immutable, so the kernel may cache everything as
long as it likes, and gitfold avoids every request it can:

- 1-year entry and attribute TTLs for everything below the root;
- no open/release: `open` replies `ENOSYS` once and the kernel stops sending it
  (`FUSE_NO_OPEN_SUPPORT`);
- no opendir/releasedir (`FUSE_NO_OPENDIR_SUPPORT`); listings stay cached;
- readdirplus;
- `flush` and `access` reply `ENOSYS` so the kernel stops sending them; after
  that `access(2)` always succeeds. Nothing needs protecting: the mount is
  read-only and, by FUSE default, only accessible to the user who mounted it;
- content loaded lazily on the first read of a file.

**Why:**

- With opens, the kernel sent an open and a release per file even when the
  content was cached: warm `rg` on spark took 177 ms; without, 42 ms (native
  35 ms).
- Without opendir, warm Chromium `find` went from 838 ms to 351 ms (faster than
  native 473 ms) and `git status` from 1.60 s to 1.15 s.
- **The root is the exception:** it lists snapshots created on demand. Whether
  the kernel caches a directory's listing is decided by flags returned from
  `opendir`; with `opendir` disabled it caches every listing, the root's
  included. So the root's attributes have zero TTL (re-fetched on every `stat`
  and `opendir`), and its mtime changes whenever a snapshot is added; the kernel
  drops a cached listing when it sees a new mtime.

**Rejected:**

- *Loading content in `open()`:* decompressed the blob again on every open even
  though the kernel already had it cached.
- *A content map per open handle:* slower, and a mutex held across decompression
  serialized all reads.

What did **not** matter for cold walks (fresh mount, empty kernel caches; spark
`find` stayed at 220-256 ms): worker thread count (1 to 24), readdirplus vs
plain readdir vs auto, no-opendir, and building the node table up front instead
of decoding trees on demand. The remaining cold cost is per-entry kernel work
(inode and dentry creation, ~8 µs per entry), which led to prewarming.

### Prewarm instead of tricks

**What:** right after creating a snapshot, the daemon walks it through its own
mount on a thread pool. `meta` (default) lists every directory; `content` also
reads every file.

**Why:** listing every directory makes the kernel create every inode and
dentry. That matters most for `git status`, which `lstat`s every index entry
before it reads any directory, so readdirplus can't help it and each entry would
otherwise cost a separate lookup. The parallel walk takes 387 ms for Chromium;
before prewarm, a cold serial `find` on a fresh mount took 2.75 s. Content
prewarm takes 2.6-3.8 s for Chromium (24 vs 8 threads), after which the first
`rg` takes 0.5 s instead of 2.9 s. It costs page cache equal to the tree size,
so it is opt-in.

**Rejected:** *answering searches with `git grep`.* The idea was to detect a
bulk reader (FUSE requests carry the caller's pid, so `/proc/<pid>/cmdline`
gives the grep invocation), run `git grep` underneath, and only expose matching
files to that process. The filesystem can only shape what grep sees, not what it
prints: hiding non-matching files silently changes the output of `grep -c`
(which prints `file:0`), `-v`, `-L`, `find` predicates and pipelines, and grep
and `git grep` differ in regex dialect. Wrong search results are the worst
failure mode for an agent. It also wouldn't be faster: `git grep` on spark took
250 ms, versus 104 ms for Claude Code's grep on a native checkout. Prewarming
gets the speed with no correctness risk.

### Memory hygiene

**What and why:**

- *Content cache, 64 MB, clear-all eviction.* Reads arrive per inode, in chunks;
  the cache only has to span the chunks of files being read now, because the
  page cache holds content afterwards. An LRU would add bookkeeping for no
  measured benefit.
- *No gitoxide caches.* gitoxide's delta-base cache (96 MB per thread by default,
  24 FUSE threads) plus a 32 MB per-thread object cache that an earlier gitfold
  version enabled cost ~1.9 GB of heap on Chromium without making reads faster:
  trees are decoded once and content is cached by the kernel. With both off,
  raw decompression even got faster (5.5 to 6.6 GB/s). gitfold opens the
  repository with `core.deltaBaseCacheLimit=0` and sets the object cache to 0
  (which is gitoxide's default anyway).
- *glibc.* After large frees, glibc raises its mmap threshold dynamically, after
  which large blob buffers come from heap arenas that are never returned to the
  OS (~600 MB retained after one pass over Chromium). gitfold pins
  `M_MMAP_THRESHOLD` to 128 KiB, calls `malloc_trim` after creating a snapshot,
  and an idle trimmer thread drops the content cache and trims once reads stop.

## Rejected ideas (index)

| idea | why not | where |
|---|---|---|
| Kernel overlayfs | needs root or a user namespace | [FUSE](#fuse-reading-straight-from-the-object-database) |
| Go + go-git | slow object database on large repos | [FUSE](#fuse-reading-straight-from-the-object-database) |
| Node per entry, hash inodes | 610 B/entry, no sharing | [store](#shared-content-addressed-store) |
| `HashMap` keyed by object id for file ids | ~35 MB vs ~5 MB | [store](#shared-content-addressed-store) |
| Reusing a slot for two names | directory hardlink | [inodes](#inode-numbers-are-computed-not-stored) |
| Symlinked `.git` | shares HEAD and index with the main checkout | [worktrees](#snapshots-are-real-linked-worktrees) |
| `git` wrapper on `PATH` | fragile, misses git libraries | [worktrees](#snapshots-are-real-linked-worktrees) |
| Index v2 + sort | twice the size and time | [index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem) |
| Letting git build the index | re-hashes all content | [index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem) |
| `index.skipHash` | not tried; reader compatibility | [index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem) |
| Content load in `open()`, per-handle content map | re-decompression; serialized reads | [caching](#kernel-caching-does-the-heavy-lifting) |
| More threads, readdirplus modes | no effect on cold walks | [caching](#kernel-caching-does-the-heavy-lifting) |
| Content prewarm by default | page cache = tree size | [prewarm](#prewarm-instead-of-tricks) |
| Answering searches with `git grep` | silently wrong results, not faster | [prewarm](#prewarm-instead-of-tricks) |
| gitoxide caches | ~1.9 GB, no speedup | [memory](#memory-hygiene) |
| LRU content cache | no measured benefit | [memory](#memory-hygiene) |

## Known limitations

User-visible effects are listed in README ("Differences from a checkout" and
"Running it"); this is the why and what it would take.

- **Checkout filters are not applied** (`eol=crlf`, LFS, `ident`,
  `working-tree-encoding`, custom filters). Investigated: gitoxide provides the
  whole pipeline; evaluating attributes for all of Chromium takes 89-94 ms in
  parallel (630 ms single-threaded) and can mostly be skipped with a pre-filter
  on the few patterns that enable a conversion; on Linux only 1 file in Chromium
  and 19 in Spark need converting. The work: compute converted sizes when a
  snapshot is created (they must match in `stat` and the index), give converted
  files their own file ids, and for LFS fetch missing objects on first read.
- **Revs containing `/` can't be named** (`@origin/main`): the root sees one
  path component at a time.
- **`..` in directory listings reports the mount root's inode.** A listing
  doesn't know its parent's inode without storing a parent per directory. Path
  resolution uses the kernel's own parent links, so this only affects tools that
  read the `d_ino` of `..`.
- **Snapshots are never unloaded** while mounted (1-11 MB of heap each on
  Chromium; kernel caches are reclaimable).
- **Each snapshot adds ~190 MB of page cache on open**, mostly the kernel's
  cached directory listings (at least one 4 KB page per directory, 46k
  directories in Chromium).
- **Errors reach callers only as errno:** a rev that does not resolve is
  `ENOENT` (the daemon log says why). Signals unmount cleanly (`mountpoint.rs`),
  but `kill -9` still leaves a stale mount for `fusermount3 -u`.

## Future work

**Writable branch workspaces.** They will appear as `<mount>/<branch>/...`.
Branch names may contain `/`; that is unambiguous because git forbids `feature`
and `feature/x` from coexisting, so at most one prefix of a path is a branch.
Read-only snapshots stay under `@`, so branch names never collide with tags or
SHAs. The plan:

- copy-on-write implemented inside gitfold (not kernel overlayfs, for the
  rootless reason above). Modified files are real files in a per-workspace
  directory on disk, which survives unmount; FUSE passthrough (kernel 6.9+) can
  give them native read and write speed;
- a workspace's base is the commit it has checked out, as in a real worktree.
  When HEAD moves inside the workspace (commit, checkout, reset), diff entries
  identical to the new base are dropped and untouched files that differ between
  the old and new base are materialized, so visible content never changes
  underneath an agent. Committed changes leave the diff by themselves;
- following a branch that moved elsewhere becomes an explicit `sync`, never
  automatic;
- each workspace is a linked worktree on its branch, so git's checked-out-branch
  protection applies.

Code-wise, workspaces sit next to `fs/snapshots.rs`, and the adapter dispatches
on inode kind. The current encoding leaves room: slots use bits 32-62 and only
need far fewer than 2^31 values, so a range of high slot numbers (or a second
flag bit) can mark workspace inodes. Workspace files can't use the shared
content-based file inodes once modified, and their attributes are mutable, so
they need short TTLs or explicit invalidation (see [Invariants](#invariants)).
