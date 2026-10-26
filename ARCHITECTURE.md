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
- **Writable branches** (copy-on-write, diff persisted on disk) without giving up
  the read-only snapshots' speed.

Non-goals for now: macOS and Windows; applying checkout filters
(`.gitattributes` conversions, LFS).

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
      |                  |                   |                 |
   fs/workspaces.rs   fs/snapshots.rs     store.rs          fs/content.rs
   names: branches,   @<rev> registry     shared tree       per-inode
   namespaces            |                tables, file ids  read cache
      |                  |                   |                 |
   fs/workspace/         |                   |                 |
   lower + upper view    |                   |                 |
      |                  |                   |                 |
   worktree.rs  <--------+              git.rs (gitoxide object database)
                                  (lines are calls)

 on disk:  .git/worktrees/gitfold-<sha12>/   snapshot registrations
           .git/worktrees/gitfold-ws-<id>/   workspace registrations
           .git/gitfold/<id>/                workspace state: base, branch, markers, times, upper/, tmp/
           refs/gitfold/<id>                 keeps the workspace base from git gc
```

Names below the root are resolved by `Workspaces::resolve`, one component at a
time: `@<rev>` names are snapshots (or, if refs exist below them, namespace
directories such as `@origin/`), other names are branch workspaces (or branch
namespaces such as `feature/`).

A snapshot is created on the first lookup of its name (`Snapshots::resolve`):

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

A workspace is opened on the first lookup of an existing local branch
(`Workspaces::open_workspace`):

1. refuse (`EBUSY`) if another worktree has the branch checked out, or is
   rebasing or bisecting it;
2. resume its state from `.git/gitfold/<id>/`, or create it with the branch tip
   as base (`Workspace::open`; the base tree is loaded into the store), and
   point `refs/gitfold/<id>` at the base (`Workspace::pin_base`);
3. register a linked worktree on the branch, writing the index only if the
   registration is new (from the branch tip; git owns it afterwards);
4. add the name, start a prewarm walk and the workspace's sync thread, which
   moves the base to the branch tip whenever git moved it and is idle in the
   workspace (`Workspace::sync`, polling every second).

Then, per request (`fs/workspace/`):

- **read**: resolve the path in the merged view: the upper file if present, else
  the base tree's entry unless a whiteout or an opaque ancestor hides it;
- **write**: copy the file up (under `copy_lock`) on its first modification,
  then write the upper file;
- **delete**: record a whiteout if a lower entry would otherwise show through,
  then remove the upper file.

## Code map

| file | role |
|---|---|
| `src/fs/mod.rs` | FUSE adapter: translates requests and owns kernel-facing policy (TTLs, init flags, `ENOSYS` replies) |
| `src/fs/view.rs` | what an inode is: attributes, lookups, directory listings, content |
| `src/fs/snapshots.rs` | `@<rev>` registry: creating snapshots, listing them per namespace |
| `src/fs/workspaces.rs` | name resolution below the root; workspace and namespace registry; opening workspaces |
| `src/fs/workspace/mod.rs` | one workspace: merged lower/upper view, lookups, cached listings, reads |
| `src/fs/workspace/write.rs` | copy-up and mutations |
| `src/fs/workspace/nodes.rs` | local id ↔ (parent, name) table, freed on kernel `forget` |
| `src/fs/workspace/markers.rs` | whiteouts and opaque markers, as an append-only log |
| `src/fs/workspace/rebase.rs` | moving the base to the branch tip invisibly; dropping upper files identical to the base |
| `src/fs/workspace/status.rs` | a workspace's uncommitted changes and ignored files, read from its state directory (for `list` and `remove`) |
| `src/fs/workspace/times.rs` | timestamps of files dropped from upper (committed, or restored unchanged) |
| `src/fs/names.rs` | telling the kernel when names in the root or a namespace changed |
| `src/fs/content.rs` | per-inode content cache and idle trimmer |
| `src/fs/stats.rs` | operation counters, logged at unmount |
| `src/store.rs` | shared tree tables, file ids, directory layout, index walk |
| `src/inode.rs` | inode encoding, path hash, constant timestamp |
| `src/main.rs` | CLI (clap), mount options, `M_MMAP_THRESHOLD` |
| `src/worktree.rs` | worktree registrations (snapshots, workspaces), streaming index v4 writer, base pin refs, git idle check, daemon lock, workspace ids |
| `src/git.rs` | thread-local gitoxide handles, tree, blob and ref access |
| `src/prewarm.rs` | background walk of new snapshots and workspaces, in a walker process |
| `src/admin.rs` | `branch`, `list`, `remove` and `prune` commands |
| `src/mountpoint.rs` | mount point checks, unmount on SIGINT/SIGTERM/SIGHUP, the mount table |
| `src/daemon.rs` | `mount` in the background (detached, log file), `unmount` |
| `src/benchread.rs`, `src/benchattrs.rs` | hidden `bench-read` and `bench-attrs` commands |
| `bench/` | `bench.sh` (native vs mount), `probe.sh` (content reads), `writes.sh` (write workloads), `RESULTS.md` (measurement log) |
| `tests/workspace.sh` | end-to-end workspace test on a scratch clone |
| `tests/rebase.sh` | end-to-end test of rebases with git operations (conflicts, stash, amend, resets, bisect) |
| `tests/cli.sh` | end-to-end test of `branch`, `list`, `remove` and `prune` |
| `tests/race-modes.pl` | helper of `tests/workspace.sh`: lifted modes must stay invisible |
| `tests/lib.sh` | helpers shared by the tests |

## Glossary

- **Snapshot**: `@<rev>`, showing the tree of a commit, read-only.
- **Workspace**: `<branch>`, a writable view of a branch, made of a lower and an
  upper layer.
- **Base**: the commit whose tree is a workspace's lower layer: the branch tip,
  followed within a second of git moving the branch, once git is idle (see
  **rebase**, **idle**).
- **Rebase** (of a workspace, not `git rebase`): moving the base to the branch's
  new tip without changing what the workspace shows.
- **Stamp**: the mtime and ctime a lower file shows because a rebase or the
  unchanged-file cleanup dropped its upper copy (`times`).
- **Idle** (git in a workspace): HEAD on the branch, no operation stopped
  half-way (merge conflict, rebase, cherry-pick, revert, sequencer, bisect) in
  the worktree's admin directory, and no lock on the index, HEAD, the branch
  ref or `packed-refs` (`worktree::git_busy`).
- **Lower**: the base tree, from the shared store.
- **Upper**: every file that differs from the base, as real files in
  `.git/gitfold/<id>/upper`.
- **Whiteout**: a recorded deleted lower path, so it stays hidden.
- **Opaque**: a directory recreated over a whiteout, or a lower directory
  copied into upper by a rename; its lower children stay hidden.
- **Copy-up**: copying a lower file to upper before its first modification.
- **Workspace id**: the branch name with `%` and `/` escaped
  (`feature%2Flogin`); names `.git/worktrees/gitfold-ws-<id>` and
  `.git/gitfold/<id>`.
- **Namespace directory**: a directory for one component of longer names
  (`feature/` for branch `feature/x`, `@origin/` for rev `origin/main`).
- **Slot**: an index in the snapshot registry, or (separately) in the workspace
  registry. Every name gets its own slot, even when two names resolve to the
  same commit.
- **Local id**: the per-workspace part of a workspace inode. Derived from the
  path when the entry is first looked up, kept through renames, and freed when
  the kernel forgets the inode; looked up again, the same path gets the same id.
- **Tree table** (`TreeTable`): one decoded git tree, shared by all snapshots
  and workspaces that contain it; identified by a dense **tree id**.
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
- **Worktree registration**: the admin directory that makes a snapshot
  (`.git/worktrees/gitfold-<sha12>/`) or workspace
  (`.git/worktrees/gitfold-ws-<id>/`) a linked git worktree.
- **Dentry**: the kernel's cached name-to-inode link. A directory inode may have
  only one (one parent).
- **TTL**: how long the kernel may trust a cached entry or attribute without
  asking gitfold again.
- **readdirplus**: a FUSE readdir that also returns each entry's attributes, so
  listing a directory doesn't trigger a separate lookup per entry.
- **Page cache**: the kernel's cache of file contents, shared by everyone who
  reads the same inode.
- **Prewarm**: gitfold walking a new snapshot or workspace through its own
  mount to fill the kernel caches.

## Invariants

Break one of these and something fails, often silently. Most link to the
decision that creates them.

- **Index stat data equals `getattr`.** For every file the index records exactly
  the mtime, ctime, inode (low 32 bits), uid, gid, mode and size that `getattr`
  reports. Any change to attributes (timestamps, sizes after checkout filters,
  uid) must change `write_index` too, or `git status` reads every file. For a
  workspace this applies to the index gitfold writes when the registration is
  created (path-hash inodes, constant times); git maintains it afterwards, and
  rebases keep what git recorded true (stamps).
  ([rebases](#workspaces-follow-their-branch-invisible-rebases),
  [index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem))
- **Snapshot file attributes depend only on (blob, mode).** A snapshot file inode
  is shared by every snapshot that contains it, so nothing per snapshot or per
  path may show in its attributes: one timestamp, link count 1. Unchanged
  workspace files report the same attributes except the inode, the write bits,
  and their stamp once committed.
  ([inodes](#inode-numbers-computed-for-snapshots-path-derived-for-workspaces),
  [timestamps](#constant-timestamps))
- **Every directory inode has exactly one parent.** Directory inodes are per
  slot; never share a slot between two root names.
  ([inodes](#inode-numbers-computed-for-snapshots-path-derived-for-workspaces))
- **Tree tables are in tree order.** The streamed index relies on a depth-first
  walk producing index order, and `TreeTable::find` relies on it for binary
  search. Never re-sort or filter tables without checking both.
  ([store](#shared-content-addressed-store), [index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem))
- **Snapshots are immutable; workspaces change visibly only through the mount.** That is
  what makes the 1-year TTLs correct for both: the kernel sees every workspace
  change it caches. Nothing may modify `.git/gitfold/` behind the mount, and
  what gitfold itself changes outside a request (rebases, dropping unchanged
  upper files) must not change anything the kernel may have cached: content,
  size, mode, mtime, inode, the set of names. Only the ctime of a file copied up
  by a rebase, the atime of a dropped file, and the times (and, for a directory
  that becomes an upper one, the size) of directories a rebase creates in upper
  or adds files to or removes them from change; git ignores all three.
  ([rebases](#workspaces-follow-their-branch-invisible-rebases),
  [caching](#kernel-caching-does-the-heavy-lifting))
- **Names never disappear while mounted**, so their entries can be cached long;
  only listings grow. When a lookup adds a name to the root or a namespace
  directory, the daemon invalidates those directories' cached attributes and
  listings (`fs::names`); their mtime changes too, dropping a listing that was
  in flight. Their negative entries are cached only for dot names.
  ([caching](#kernel-caching-does-the-heavy-lifting))
- **Workspace local ids come from the path.** The index written with a new
  registration uses the same path hash (`inode::path_local`), so it matches
  `stat` after any remount; a rename or a hash collision only costs git a
  re-hash of that file.
  ([workspaces](#writable-workspaces-copy-on-write-inside-gitfold))
- **The workspace on-disk format is a compatibility contract**: the id escaping,
  the layout of `.git/gitfold/<id>/` (`base`, `branch`, `markers` and `times`
  record formats, `upper/`), the base-pinning ref `refs/gitfold/<id>`, the
  registration names `gitfold-<sha12>` and `gitfold-ws-<id>`, and the path
  hash. Changing any of them needs a migration, or every existing workspace's
  index stops matching (Chromium: 4.4 GB re-hashed). The daemon lock (an
  `flock` on the git common directory, or the `gitfold/.lock` file where that
  isn't supported) is a contract between builds too: changing it makes older
  and newer builds miss each other's daemons.
- **Workspace operations take `layers` before resolving a path**, and keep it
  while acting on the result: creating, removing and renaming exclusively;
  writes, attribute changes, fsync, reads, lookups and each readdirplus page
  shared (`Workspace::page`; concurrent copy-ups are serialized by
  `copy_lock`). Resolving first lets a concurrent
  rename redirect a write or read to another file, and a page served without the
  lock could hand the kernel a node for a just-deleted name, cached for a year.
  `layers` holds everything the view is computed from except upper (base,
  markers, stamps), so a rebase, which takes it exclusively, is atomic for
  every request.
- **Structural changes call `Workspace::changed` afterwards, still holding
  `layers`**: exclusively for create, remove and rename; shared for a copy-up
  turning a lower file into an upper one, where `page` reading the generation
  before building a listing makes one built concurrently count as stale.
  Invalidating first lets a concurrent readdir cache the old listing as
  current. Plain writes and attribute changes don't: listings
  carry no stat data for upper entries and stat them when serving a page.
  ([listing cache](#writable-workspaces-copy-on-write-inside-gitfold))
- **A workspace node is handed out together with the kernel reference it
  stands for** (`Nodes::claim`: lookup, create, mkdir, symlink, and each
  readdirplus entry, given back if it doesn't fit the reply), in one step under
  the node table's lock, because nodes are freed when the kernel's `forget`s bring
  the count to zero. Counting separately raced with `forget` and freed nodes the
  kernel still used (their inodes then answered `ENOENT`). Listings carry names,
  not nodes, so a listing never creates a node the kernel doesn't hold.
- **Copy-ups are serialized** (`copy_lock`): writers hold `layers` shared, and
  two of them copying up the same file could rename a fresh copy over the
  other's first write.
- **Durability and crash ordering:** a workspace write reaches the upper file
  before the reply, `fsync` is passed through, and markers are appended before
  the reply. Order matters where two things change: a whiteout is logged
  *before* the upper file is removed (the upper copy wins until then), copy-ups
  are written in `tmp/` (cleared on open) and renamed into place,
  copy-on-rename syncs its copies before marking the directory opaque and
  clears the whiteouts below only after the marker, `base` is
  written atomically last when a workspace is created, and a worktree
  registration's HEAD is written last (a directory without HEAD, or without an
  index, is rewritten). A rebase first prepares what is valid under both bases,
  then writes the stamps and the new base, then drops what the new base makes
  redundant (opaque markers first, synced: a file dropped below a marker still
  in place would vanish). Atomic writes (`worktree::write_atomic`: base, stamps,
  registrations, marker compaction) are synced before the rename. Marker
  appends and upper removals are not synced (a rebase syncs the copies,
  directories and markers of its step 1 before switching the base), so a power
  loss (unlike a daemon crash) can bring back a file deleted just before it, as on most file systems
  without an explicit sync.
- **`.git` in a workspace root is virtual** and cannot be created, replaced or
  renamed onto.
- **Encoding limits:** file ids fit in `u32` (the index stores the low 32 bits
  of the inode); dir index < `u32::MAX` (`u32::MAX` is the `.git` file);
  snapshot slots must stay below bit 62 (debug-asserted at < 2^29), workspace
  slots below bit 62 too (debug-asserted at < 2^18); workspace local ids use 44 bits, with
  0 for the directory itself and all ones reserved for `.git`.
- **There are no file handles.** `open` and `opendir` return `ENOSYS` (`open`
  replies with a dummy handle 0 under `--kernel-open`), so reads
  and writes are addressed by inode, and all state is keyed by inode.
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
about 0.66 s on 24 threads (6.6 GB/s, `gitfold bench-read`).

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
  hash-map slot and cached listing): ~610 bytes per entry, 342 MB per Chromium
  snapshot, nothing shared between snapshots. Four snapshots would need ~4 GB of
  heap, versus 77 MB now.
- *`HashMap<(ObjectId, EntryKind), u32>` for file ids:* a copy of every object id
  as key cost ~35 MB on Chromium; the id-only table costs ~5 MB.

### Inode numbers: computed for snapshots, path-derived for workspaces

**What:**

```
root:                    1
snapshot file:           1<<63 | file_id
snapshot directory:      (slot + 1) << 32 | dir_index
snapshot .git file:      (slot + 1) << 32 | u32::MAX
workspace or namespace:  1<<62 | ws_slot << 44 | 0        (the directory itself)
workspace entry:         1<<62 | ws_slot << 44 | local
workspace .git file:     1<<62 | ws_slot << 44 | (2^44 - 1)
```

Bit 63 marks snapshot files and bit 62 workspace-registry inodes; snapshot
directory inodes have neither. Workspace inodes need a small node table, covered
in [workspaces](#writable-workspaces-copy-on-write-inside-gitfold); everything
below is about snapshots.

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
- *Nothing to look up:* decoding is arithmetic, so snapshots have no inode
  table.

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
(1.13 s vs 0.88 s on repeat runs).

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
  would save the SHA-1 over the index file, but depends on how the reading git version
  treats a missing checksum.

### Constant timestamps

**What:** everything inside a snapshot reports mtime, ctime and atime
2020-01-01, and so do unchanged workspace entries. Only the mount root,
namespace directories (see [caching](#kernel-caching-does-the-heavy-lifting))
and changed (upper) workspace files and directories have real times, which
committed files keep (stamps; see
[rebases](#workspaces-follow-their-branch-invisible-rebases)).

**Why:** a file inode is shared by every snapshot containing that content, so it
can have only one timestamp, and the index must record the same value. Git and
agents don't rely on mtimes in a read-only tree. Workspaces use the same
constant for unchanged (lower) entries, so their index can be synthesized the
same way; changed (upper) files are real files and report their real times,
and keep them once dropped from upper.

### Names with slashes: one directory per component

**What:** branch `feature/x` is `<mount>/feature/x` and rev `origin/main` is
`<mount>/@origin/main`, through namespace directories (`feature/`, `@origin/`).
The kernel looks names up one component at a time, so `Workspaces::resolve` walks
them: for a non-`@` name, an existing branch is a workspace, a prefix of
existing branches is a namespace; for an `@` name, a prefix of existing refs
(below `refs/heads/`, `refs/remotes/`, `refs/tags/` or `refs/`) is a namespace,
anything else is resolved as a rev. Non-`@` names with a component starting
with `.` are rejected before any ref lookup (git forbids them in branch names,
and tools probe `.git` and the like constantly); `@` names go to rev
resolution, which logs a failure.

**Why:** real slashes look like the branch names people type. Git forbids
`feature` and `feature/x` coexisting as branches, so at most one prefix of a
path is a branch and the walk is unambiguous. Revs and branches are told apart by
the `@` (`@` names are revs, plain names are branches), so a branch named like a
tag or a SHA never clashes. The price: a branch whose name starts with `@` can't
be opened as a workspace.

**Rejected:**

- *Escaping `/` (e.g. `feature~x`):* unambiguous but not what anyone types.
- *Git's shorthand winning (`@origin` = `origin/HEAD`):* would make `origin/main`
  unreachable; the namespace wins, and `origin/HEAD` is `@origin/HEAD`.
- *Revs limited to one component* (the first version): `@origin/main` needed a
  workaround via the SHA.

### Writable workspaces: copy-on-write inside gitfold

**What:**

- *Layout:* a workspace is a linked worktree on its branch
  (`.git/worktrees/gitfold-ws-<id>`, HEAD `ref: refs/heads/<branch>`). Its state
  is `.git/gitfold/<id>/`: `base` (the commit of the lower layer, the branch's
  last tip), `branch` (the unescaped branch name, for `list`), `upper/` (every
  file that differs from base, as real files),
  `markers` (whiteouts and opaque markers, an append-only log), `times`
  (stamps) and `tmp/` (copy-ups in progress); `refs/gitfold/<id>` keeps
  the base commit from `git gc`, since nothing else references it once the
  branch moves on.
- *Reads* resolve a path in the merged view: upper if present, else the base
  tree's entry from the shared store unless a whiteout or opaque ancestor hides
  it. Directory listings merge both.
- *Writes:* a file is copied up on its first write (or created empty for a
  truncation to zero), under `copy_lock`, written in `tmp/` and renamed into
  place. Deleting a lower entry appends a whiteout; a directory recreated over
  one becomes opaque. Renaming a directory with lower content first copies it
  up and makes it opaque (copy-on-rename, see below); renaming onto an empty
  lower directory makes that one opaque.
- *Inodes* are `ws(slot, local)` with `local` = FNV-1a of the path at first
  lookup (probing on collision), kept through renames by a node table of
  (parent, name), counted per kernel reference and freed on `forget`.
- *Git:* the index is written when the registration is created, from the
  branch tip with path-hash inodes and constant times; afterwards git owns it.

Example: deleting `src/a.c` appends a whiteout for it; `mkdir src` after
`rm -r src` makes `src` opaque; the first write to `b.c` copies it to
`upper/b.c`.

**Why:**

- *Copy-on-write inside gitfold* rather than kernel overlayfs: overlayfs needs
  root or a user namespace (see above), and gitfold must control the lower layer
  (it moves with the branch; see
  [rebases](#workspaces-follow-their-branch-invisible-rebases)).
- *Upper as real files on the repository's filesystem:* simple, moved or
  deleted together with the repository, and inspectable with ordinary tools
  (read-only while mounted). Every write reaches the upper file before the
  reply and `fsync` is passed through, so a daemon crash loses nothing; a host
  crash can lose unsynced file data and the most recent deletions (see the
  durability invariant).
- *Markers as an append-only log:* the first version rewrote the whole whiteout
  set on every change, which made `rm -r` of a large lower directory quadratic.
  Appending (with compaction on open, and in process once the log is mostly
  history) took `rm -r` of spark's 7.5k-file directory to ~0.2 s.
- *Nodes freed on `forget`:* bounds the table to what the kernel holds, and lets
  a path recreated after deletion get its path-derived id back, which the git
  index expects.
- *Index from the branch tip:* it describes what HEAD points at. The base is
  normally the tip; when it lags (the branch moved while unmounted, or git isn't
  idle yet), the next rebase catches up and the index already matches.
- *Path-derived inodes:* a write must keep the inode the kernel already holds,
  so workspace files can't use the shared content-based file inodes. A per-mount
  counter would change on every remount and invalidate git's index (every file
  re-hashed: 4.4 GB on Chromium); a path hash is stable, so the index written at
  creation keeps matching.
- *Writes go through the daemon*, keeping `FUSE_NO_OPEN_SUPPORT` for the whole
  mount. FUSE passthrough would give changed files native speed, but it works
  only through `open`, and enabling opens for the mount made warm snapshot reads
  ~4x slower (spark `rg` 42 to 177 ms).
- *Refusing a branch checked out elsewhere*, as `git worktree add` does: two
  checkouts of one branch go stale as soon as one commits.
- *Implicit creation for existing branches only*: a lookup never creates a
  branch, so a typo can't. New branches come from `gitfold branch` or `git branch`.

**Rejected:**

- *Redirects for renamed lower directories* (as overlayfs' `redirect_dir`: a
  marker saying `new/` shows the base's `old/`): every lookup, listing, copy-up,
  rebase and status check would have to follow them, the rebase also moving
  them along with the base, and the marker format would change. Instead a
  renamed directory with lower content is copied into upper first
  (`Workspace::materialize`), up to 10,000 files or 100 MiB (0.23 s for 7,500
  files, 19 MB); above that, `EXDEV`, which `mv` handles by copying but
  `git mv` doesn't. The copies are ordinary upper files, so rebases drop them
  once the rename is committed.
- *Rewriting the marker sets on every change:* quadratic `rm -r` (see above).
- *Diff storage in `~/.local/share`:* orphaned diffs pile up when repositories
  move or are deleted.
- *Passthrough on a second mount point for workspaces:* native write speed, but
  two namespaces instead of one `<mount>/...`; revisit if writes through the
  daemon (e.g. build output) turn out too slow.
- *Rebuilding a directory listing per readdir page:* the kernel pages large
  directories in a few dozen entries; a spark workspace prewarm took 6 s
  (quadratic in a 7.5k-entry directory) versus 0.11-0.16 s with the listing
  built once, cached while paged, and lower entries taken from the tree table
  without a `stat`.

Measured on Chromium: a workspace opens in 0.85 s (registration plus index) and
prewarms in 1.34 s (vs 0.64 s for a snapshot: lookups go through the merged
view and allocate nodes); GNU `find` 0.34 s, warm `rg` 0.41 s and `git status`
1.19 s, all on par with a snapshot. Its node table costs ~67 MB of heap once
prewarmed (~120 bytes per entry), against ~2-11 MB per additional snapshot.

### Workspaces follow their branch: invisible rebases

**What:** each workspace has a thread that checks every second whether git is
idle in it and its branch tip differs from its base (`Workspace::sync`). If so,
it moves the base to the tip (`workspace/rebase.rs`) without changing anything
the workspace shows. From base B to tip N, only paths where B's and N's trees
differ are visited (tree ids are content-addressed, so equal subtrees are
skipped), in three steps that each leave a consistent view:

1. *Prepare*, valid under both bases: an untouched file N changes is copied up
   with the mtime it showed; a path absent from the view that N adds gets a
   whiteout; a lower directory N removes or replaces with a file becomes an
   upper directory (opaque over N's file), its files copied up the same way;
   in an opaque directory (one recreated over a deleted one, or renamed with
   lower content) where N has a
   directory, N's entries that upper doesn't have are whited out. These are
   synced before step 2.
2. *Switch*: record stamps for the upper files identical to N's (content and
   mode), persist them, then the new base.
3. *Clean up*, valid under N: drop those upper files (they now show N's file
   with their old mtime and ctime), the whiteouts of paths N doesn't have, and
   the opaque markers step 1 made unnecessary (so the files of a directory
   recreated by a code generator can be dropped once committed).

The same thread also drops upper files written since its last pass that are
identical to the base again (git rewrites unchanged files in `stash`,
`rebase --abort`, `checkout -- .`); it reads and hashes them under the shared
lock and drops those a `stat` under the exclusive lock shows unchanged since
(files changed in the last second wait for a later pass: timestamps move in
coarse ticks, so a write right after the hash could leave the `stat` as it
was).
Then it points `refs/gitfold/<id>` at the base, retrying on every pass until
that succeeds. The daemon also syncs every open workspace when it exits
(`Workspaces::sync_all`), and `gitfold remove` catches a workspace up offline if
its branch moved while unmounted, so its change count is measured against the
tip.

Example: `git commit -am fix` of 3 edited files; within a second the base is
the new commit, the 3 files leave `upper/`, and `stat` shows the same inode,
size, mode, mtime and ctime as before, which is what git just recorded in the
index, so `git status` stays clean by stat alone. After `git reset --soft
HEAD~1`, the files the undone commit changed are copied up with the mtime they
showed (their ctime is the copy's), and `git status` lists them as staged, as in a checkout.

**Why:**

- *Invisible, so timing doesn't matter:* `git commit && git status` (or a test
  run) can start before or after the rebase, or during it (it holds `layers`
  exclusively, so each request sees one side), and sees the same files either
  way. Stat data is preserved because git compares it with the index: a
  changed mtime would make git re-read the file, and the kernel caches
  attributes for a year, so nothing needs invalidating.
- *Only when idle:* during a conflicted merge, `rebase -i` or `bisect`, the
  branch tip is not a state to move to (HEAD is detached or the operation is
  half done); waiting costs nothing, since the view stays correct under the
  current base.
- *Polling:* about a dozen `stat`s and a ref read per workspace per second.
  Latency only decides how long committed files stay in `upper/`.
- *External moves are followed too* (`update-ref` and other
  plumbing; git refuses to move a branch checked out in a worktree): the view stays, and `git status` shows the difference to the new
  tip, as a checkout whose branch moved underneath it does.
- *Upper files are compared by content and mode*, not by git's index: the index
  only says what git last saw, and the comparison only reads files the commit
  changed (`rebase` measured 12 ms with the lock held for a 200-file commit in
  Spark, 62 ms for 2,000 files).

**Rejected:**

- *Git hooks* (`post-commit`, `post-rewrite`, `reference-transaction`): not run
  by plumbing such as `update-ref`, overridden by a user's `core.hooksPath`,
  and fired in the middle of multi-step operations, so the rebase would still
  need the idle check and invisibility; they would only cut the latency.
- *inotify on the admin directory:* the same, without the hook problems, but
  more moving parts than a one-second poll for no visible difference.
- *A visible rebase with kernel cache invalidation:* files' mtimes would return
  to the constant, so a command chained after a commit could see them change
  between two `stat`s, and git would re-read them.
- *An explicit `sync` command for external moves:* following them keeps one
  rule for every branch move, and the view never changes either way.

### Kernel caching does the heavy lifting

**What:** snapshot contents are immutable, and workspaces change only through
the mount or invisibly (rebases), so the kernel may cache everything as long as
it likes, and gitfold avoids every request it can:

- 1-year entry and attribute TTLs for everything, the root and namespace
  directories included (see below);
- negative entries: a lookup of a missing name answers inode 0 with a 1-year
  entry TTL in snapshots and workspaces, so the kernel remembers the absence
  (in the root and namespace directories only for dot names: other names
  can appear later);
- no open/release: `open` replies `ENOSYS` once and the kernel stops sending it
  (`FUSE_NO_OPEN_SUPPORT`);
- no opendir/releasedir (`FUSE_NO_OPENDIR_SUPPORT`); listings stay cached;
- readdirplus;
- `flush` and `access` reply `ENOSYS` so the kernel stops sending them; the
  mount uses `default_permissions`, so the kernel checks file modes itself, and
  by FUSE default only the user who mounted it can access it;
- content loaded lazily on the first read of a file.

**Why:**

- With opens, the kernel sent an open and a release per file even when the
  content was cached: warm `rg` on spark took 177 ms; without, 42 ms (native
  35 ms).
- Without opendir, warm Chromium `find` went from 0.84 s to 0.35 s (faster than
  native 0.47 s) and `git status` from 1.60 s to 1.15 s.
- **The root and namespace directories list names created on demand.** With
  `opendir` disabled the kernel caches every listing, theirs included. Earlier
  versions gave them short attribute TTLs (zero for the root, 1 s for
  namespaces) so a changed mtime would drop the cached listing. But with
  `default_permissions` the kernel re-checks permissions on every directory a
  path walks through, refreshing expired attributes: every absolute path
  through the mount cost a `getattr` of the root. Probing 3,558 Spark
  directories for `.ignore`, `.gitignore` and `.git` took 109 ms by absolute
  path vs 12 ms by relative path. Now they have 1-year TTLs too, and when a
  lookup adds a name the daemon tells the kernel to drop their attributes and
  listings (`Notifier::inval_inode`, before replying to that lookup;
  `fs::names`; their new mtime drops a listing that was in flight meanwhile):
  16 ms by absolute path (native 11 ms). A zero entry TTL for their names, as
  an even earlier version had, turned every path walk through `feature/` into a
  lookup (30,469 vs 52).
- **Negative entries:** without them, tools that probe every directory for
  `.gitignore`, `.ignore` or `.git` (`rg` with some versions and options) asked
  the daemon on every run: the probe above sent 35,568 lookups over three runs,
  with negative entries 14,234 (only the first run asks).
- **The mount is read-write** (for workspaces; snapshots report no write bits,
  so the kernel refuses writes there, and `EROFS` covers what it lets through).
  On a writable mount the kernel marks a directory's or file's atime stale
  after reading it, so the next `stat` asks the daemon, which a read-only mount
  skips. Chromium's first `find` after prewarm went from 351 to
  759 ms; prewarm now `stat`s what it read, moving that cost into the
  background (352 ms again). Warm runs are unaffected.

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

**What:** right after opening a snapshot or workspace, the daemon starts a
walker process (`gitfold prewarm`) that walks it through the mount on a
thread pool. A separate process, because a daemon killed with SIGKILL while
its own threads wait on requests to its mount never finishes dying, and every
access to the mount hangs; a separate walker's requests just fail once the
daemon is gone. Walkers keep the mount busy, so the daemon stops its own
before unmounting on a signal, and `gitfold unmount` stops those below the
mount point. `meta` (default) lists every directory
and stats it afterwards (see the read-write mount note under
[caching](#kernel-caching-does-the-heavy-lifting)); `content` also reads and
stats every file.

**Why:** listing every directory makes the kernel create every inode and
dentry. That matters most for `git status`, which `lstat`s every index entry
before it reads any directory, so readdirplus can't help it and each entry would
otherwise cost a separate lookup. The parallel walk takes 0.64 s for a Chromium
snapshot (0.39 s before the stat was added) and 1.34 s for a workspace; before
prewarm, a cold serial `find` on a fresh mount took 2.75 s. Content
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
  OS (904 MB of heap after one pass over Chromium, vs 296-385 MB with a fixed
  threshold). gitfold pins
  `M_MMAP_THRESHOLD` to 128 KiB, calls `malloc_trim` after creating a snapshot,
  and an idle trimmer thread drops the content cache and trims once reads stop.

## Rejected ideas (index)

| idea | why not | where |
|---|---|---|
| Kernel overlayfs (for snapshots or workspaces) | needs root or a user namespace | [FUSE](#fuse-reading-straight-from-the-object-database), [workspaces](#writable-workspaces-copy-on-write-inside-gitfold) |
| Escaped slashes in names, one-component revs | not what people type; `@origin/main` unreachable | [names](#names-with-slashes-one-directory-per-component) |
| Shared content inodes or per-mount counters for workspace files | writes must keep the inode; counters invalidate git's index on remount | [workspaces](#writable-workspaces-copy-on-write-inside-gitfold) |
| Opens + passthrough for writes | warm snapshot reads ~4x slower (or a second mount point) | [workspaces](#writable-workspaces-copy-on-write-inside-gitfold) |
| Redirects for renamed lower directories | every lookup, listing and rebase would follow them; copy-on-rename up to a limit instead | [workspaces](#writable-workspaces-copy-on-write-inside-gitfold) |
| Diff in `~/.local/share` | orphaned diffs | [workspaces](#writable-workspaces-copy-on-write-inside-gitfold) |
| Workspace listing rebuilt per readdir page | quadratic: 6 s vs 0.16 s prewarm | [workspaces](#writable-workspaces-copy-on-write-inside-gitfold) |
| Rewriting whiteout/opaque sets on every change | quadratic `rm -r`; append-only log instead | [workspaces](#writable-workspaces-copy-on-write-inside-gitfold) |
| Zero entry TTL for namespace names; zero or short attribute TTLs for the root and namespaces | a lookup per path walk; a getattr of the root per absolute path | [caching](#kernel-caching-does-the-heavy-lifting) |
| Go + go-git | slow object database on large repos | [FUSE](#fuse-reading-straight-from-the-object-database) |
| Node per entry, hash inodes | 610 B/entry, no sharing | [store](#shared-content-addressed-store) |
| `HashMap` keyed by object id for file ids | ~35 MB vs ~5 MB | [store](#shared-content-addressed-store) |
| Reusing a slot for two names | directory hardlink | [inodes](#inode-numbers-computed-for-snapshots-path-derived-for-workspaces) |
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

User-visible effects are listed in README ("Workspaces", "Differences from a
checkout" and "Running it"); this is the why and what it would take.

- **Checkout filters are not applied** (`eol=crlf`, LFS, `ident`,
  `working-tree-encoding`, custom filters). Investigated: gitoxide provides the
  whole pipeline; evaluating attributes for all of Chromium takes 94 ms in
  parallel (630 ms single-threaded) and can mostly be skipped with a pre-filter
  on the few patterns that enable a conversion; on Linux only 1 file in Chromium
  and 19 in Spark need converting. The work: compute converted sizes when a
  snapshot is created (they must match in `stat` and the index), give converted
  files their own file ids, and for LFS fetch missing objects on first read.
- **A rebase copies up every untouched file that changes**, under the exclusive
  lock: cheap after commits, but moving the branch far without touching the
  files (`reset --soft` to an old commit) blocks the workspace for a while
  (0.40 s to copy 2,857 files in Spark). Copying before taking the lock, then
  re-checking, would shorten the wait.
- **Upper files identical to the base are only noticed when written**: a file
  edited and then restored by hand is dropped, but one written just before an
  unmount or daemon restart (written paths are remembered in memory only), or
  among more than 100,000 written while git was busy or rebases kept failing,
  stays in upper until it
  is next written. Only disk use is affected.
- **Files in an opaque directory wait for the next rebase**: the cleanup of
  unchanged files doesn't look below one (a directory recreated over a deleted
  one, or one copied up by a rename); a rebase removes the marker where the new base has a directory, and
  over a file of the new base the directory stays opaque. Only disk use is
  affected.
- **Renaming a directory with more than 10,000 files or 100 MiB of lower
  content returns `EXDEV`** in workspaces (`git mv` of such a directory
  fails), as does one whose copy fails; smaller ones are copied into upper
  while the workspace's file operations wait. Hard links and special files
  are `EPERM`.
- **No file handles in workspaces either**: reads and writes go by path, so I/O
  on a file that was unlinked or renamed over while open fails. Writing (or
  reading) through a descriptor to a file without that permission (created
  with mode 0444, as `tar` and git do, or chmod'ed after opening) works: the
  daemon lifts the owner's bit for its own reopen and restores it, under a lock
  every `stat` of an upper file takes too, so nobody sees the lifted mode
  (`Workspace::open_upper`).
- **Workspace writes go through the daemon** (no passthrough): creating many
  small files is 4-6x slower than on a local disk (Spark's 27,599 files:
  3.00 s vs 0.53 s, the shape of a `node_modules` install), large writes under
  2x, and builds in between: a full `cargo build` of gitfold takes 15.99 s vs
  13.64 s (+17%), an incremental one 3.91 s vs 1.85 s. Each created file costs
  ~6 requests (lookup, create, write, tar's `utimes`, attribute reads after the
  write); the daemon's own work per request was already trimmed (no `mkdir` of
  every ancestor, no `stat` before each write). `FUSE_WRITEBACK_CACHE` would
  batch writes and save the attribute reads, at the cost of the kernel owning
  mtimes; passthrough on a second mount point would give native speed. Creates
  take the workspace lock exclusively, so parallel writers serialize there.
- **A workspace's node table costs ~120 bytes per entry the kernel has looked
  up** (~67 MB for a prewarmed Chromium workspace), against a few MB per
  additional snapshot. Most of it is hash table slack (power-of-two capacities)
  and the names.
- **Branch switching inside a workspace** works as in a worktree but leaves the
  directory named after the old branch, with its state under the old id, and
  the base stops following (syncing waits until HEAD is back on the original
  branch).
- **Workspaces are removed only while unmounted** (`gitfold remove`): the
  daemon holds their state in memory. Until then git treats the branch as
  checked out. A daemon holds an exclusive `flock` on the git common
  directory itself (nothing is created for it) for its whole life, also while
  exiting after an unmount, and through bind mounts or other mount namespaces,
  so a repository has at most one daemon (two would share, and corrupt, the
  stored workspace state); `remove` and `prune` take the same lock or refuse.
  Where directories can't be flocked (NFS without `local_lock=flock`), the
  lock file `.git/gitfold/.lock` is used instead, as earlier builds did, and
  an existing one is always locked too, so their daemons are still seen;
  daemons of builds before any lock are not.
  `gitfold unmount` waits for this lock, also when the mount was busy and had
  to be detached (it lists the processes using it, from `/proc`, first),
  except when it or a process it runs under (its shell, `make`, ...) is
  inside the mount: it would wait for itself.
- **`..` in directory listings reports the mount root's inode.** A listing
  doesn't know its parent's inode without storing a parent per directory. Path
  resolution uses the kernel's own parent links, so this only affects tools that
  read the `d_ino` of `..`.
- **Snapshots and workspaces are never unloaded** while mounted (kernel caches
  are reclaimable).
- **Each snapshot adds ~195 MB of page cache on open**, mostly the kernel's
  cached directory listings (at least one 4 KiB page per directory, 46k
  directories in Chromium).
- **Errors reach callers only as errno:** a rev that does not resolve is
  `ENOENT` (the daemon log says why), a missing branch `ENOENT` (not logged), a
  branch checked out elsewhere `EBUSY` (logged). Signals unmount cleanly
  (`mountpoint.rs`), but `kill -9` still leaves a stale mount for
  `fusermount3 -u`.

## Future work

Workspaces were built in phases: phase 1 is the copy-on-write core, phase 2
the [invisible rebases](#workspaces-follow-their-branch-invisible-rebases),
phase 3 the management commands (`branch`, `list`, `remove`, `prune`), a
compact node table and write measurements. Next:

- **Faster writes.** `FUSE_WRITEBACK_CACHE` (fewer requests per file, but the
  kernel then owns mtimes, which the index and rebases rely on), or passthrough
  on a second mount point for build output. Builds measured cost little (a
  full `cargo build` +17%); installs of many small files (`node_modules`) are
  the slow case, where opt-in symlinks to real directories for named ignored
  directories would be the cheap fix. See the write numbers in
  [Limitations](#known-limitations).
- **Parallel creates.** Creating an entry takes the workspace lock
  exclusively; per-directory locking would let parallel writers proceed.
