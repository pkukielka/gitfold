# gitfold

Any git revision as a directory, without a checkout.

```
$ mkdir -p ~/mnt/repo
$ gitfold mount ~/src/monorepo ~/mnt/repo > /tmp/gitfold.log 2>&1 &
$ cd ~/mnt/repo/@HEAD~3                # or @HEAD, @<sha>, @v1.2.0 ...
$ git status                           # works: it's a real (linked) git worktree
$ rg TODO                              # works, at close to native speed once warm
```

gitfold is a FUSE filesystem. Each `<mount>/@<rev>` directory shows the tree of
that revision, read straight from the repository's object database, and is
registered as a linked git worktree, so git commands inside it behave as in a
normal checkout.

It is built for coding agents and code review on large repositories, where
`git worktree add` of a 20-40 GB monorepo is slow and needs a full copy on disk,
and where an agent needs many revisions side by side. On Chromium (512k files)
the first revision opens in 0.6-0.8 s and each nearby one in 0.1-0.2 s, and
revisions share almost everything: each additional one costs a few MB of daemon
memory plus reclaimable kernel cache (see [Running it](#running-it)).

**Scope today:** snapshots are read-only. gitfold suits agents and reviewers
that read, search and run git queries; agents that edit files need writable
branch workspaces, which are planned ([ARCHITECTURE.md](ARCHITECTURE.md#future-work)).
Although the mounted files are read-only, gitfold does write into the
repository: one worktree registration per snapshotted commit under
`.git/worktrees/gitfold-*` (see [Cleaning up](#cleaning-up)).

## Requirements

- Linux with FUSE 3 (`fusermount3` on `PATH`); no root needed. Tested on 6.12;
  kernel 5.1 or newer is required for the no-opendir mode gitfold depends on.
- git with linked worktree and index v4 support (any git from the last decade;
  tested with 2.49).
- A recent stable Rust (edition 2024; developed with 1.99), or Nix:
  `flake.nix` provides a dev shell with the toolchain.

## Build

```
nix develop -c cargo build --release     # or, with Rust installed: cargo build --release
cargo install --path .                   # optional: puts `gitfold` on PATH
```

The binary is `target/release/gitfold`.

## Usage

```
gitfold mount [OPTIONS] <REPO> <MOUNTPOINT>
```

`<REPO>` is the repository's work tree (or any path git accepts as the
repository). `<MOUNTPOINT>` must be an existing, empty directory you own.

Inside the mount point:

- `@<rev>` is a snapshot. `<rev>` is anything that resolves to a commit using
  git's revision syntax (parsed by gitoxide), as long as it is a single path
  component: `@HEAD`, `@HEAD~5`, `@HEAD^`, `@<sha>`, `@<short-sha>`, `@v1.2.0`.
  Revs containing `/` (e.g. `origin/main`) cannot be named; use the commit
  instead: `cd ~/mnt/repo/@$(git -C ~/src/monorepo rev-parse origin/main)`.
- A rev that doesn't resolve looks like a missing directory (`No such file or
  directory`); the daemon logs why (`cannot resolve @<rev>: ...`).
- A snapshot is created on first access and pinned for the life of the mount:
  `@HEAD` keeps showing the commit HEAD pointed to when it was first opened. To
  see a newer commit, open it by SHA (as above) or remount.
- `ls <MOUNTPOINT>` lists only the snapshots opened so far; it starts empty, so
  revisions can't be browsed or tab-completed.
- Everything is read-only: writes fail with `EROFS`.

Options:

| option | default | |
|---|---|---|
| `--prewarm none\|meta\|content` | `meta` | After creating a snapshot, walk it in the background to fill kernel caches. `meta` makes the first `find`/`git status` fast; `content` also reads every file, so the first grep is fast too, at the cost of page cache equal to the tree size. The snapshot is usable while this runs (chromium: 0.4 s for `meta`). |
| `--prewarm-threads N` | half the CPUs | Threads used by the prewarm walk. |
| `--threads N` | number of CPUs | FUSE worker threads. |
| `--kernel-open` | off | Debug/benchmark only: let the kernel send open/release for every file. Slower. |

### Recommended agent workflow

Resolve the revision to a SHA yourself and give each task its own snapshot:

```
sha=$(git -C ~/src/monorepo rev-parse origin/main)
cd ~/mnt/repo/@$sha
```

If agents grep a fresh snapshot immediately, consider `--prewarm content`: on
chromium it reads everything in the background in 2.6-3.8 s (24 vs 8 threads),
after which the first `rg` takes 0.5 s instead of 2.9 s.

### Git inside a snapshot

Each snapshotted commit is registered as a linked worktree under
`<repo>/.git/worktrees/gitfold-<sha12>` (the first 12 hex digits of the commit),
with a detached HEAD; the snapshot's `.git` file points there. `git status`,
`log`, `diff`, `show`, `blame` etc. work as in a checkout.

Names that resolve to the same commit (`@HEAD` and `@<its sha>`) share one
registration; `git worktree list` shows it once, under the first name opened.
Git works in all of them.

Commands that write files (`checkout`, `reset --hard`, `stash`, merges) fail with
`EROFS`. Commands that only change `.git` (e.g. `commit`, `reset --soft`)
succeed and move that worktree's HEAD away from the snapshot's files, after which
`git status` shows differences. Don't use them in a snapshot.

## Running it

- `gitfold mount` runs in the foreground until the file system is unmounted.
  Run it in the background (or under a service manager) and redirect its output;
  it logs one line per snapshot and prewarm, and fuser warnings for
  unimplemented operations such as extended attributes. Set `RUST_LOG=warn` for
  less output.
- **Stop it with Ctrl-C, `kill` (SIGTERM) or `fusermount3 -u <MOUNTPOINT>`.**
  On a signal the daemon unmounts itself, logs operation counters and exits. If
  the mount is busy (a shell's working directory is inside it), it is detached
  at once and the daemon exits when the last process leaves; signal again to
  exit immediately.
- Only `kill -9` (or a crash) leaves the mount behind: accesses then fail with
  `Transport endpoint is not connected`, and `fusermount3 -u <MOUNTPOINT>`
  cleans it up. `gitfold mount` detects such stale mounts, and mount points
  that are missing, not directories or already mounted, and says so.
- **Cost of a long-running mount:** snapshots stay loaded until unmount. Each
  additional chromium snapshot costs 1-11 MB of daemon heap, ~160 MB of kernel
  inode/dentry cache and ~190 MB of page cache when opened (mostly cached
  directory listings), plus page cache for file contents not already cached by
  another snapshot. Kernel caches are reclaimable; if evicted, the next access
  is just slower.

### Cleaning up

Worktree registrations are locked, so `git worktree prune` keeps them while
unmounted; later mounts reuse them. To remove all of them (works after
unmounting):

```
git -C <REPO> worktree list --porcelain \
  | awk '/^worktree /{p=$2} /^locked/{print p}' | grep '/@' \
  | while read -r p; do git -C <REPO> worktree unlock "$p"; done
git -C <REPO> worktree prune
```

Or delete `$(git -C <REPO> rev-parse --git-common-dir)/worktrees/gitfold-*`.

## Differences from a checkout

- **File contents are served as stored in git.** Checkout conversions from
  `.gitattributes` are not applied: `eol=crlf`, Git LFS smudging, `ident`,
  `working-tree-encoding` and custom filters. With git's defaults on Linux
  (`core.autocrlf` unset) only files with such explicit attributes are affected:
  19 in Apache Spark, 1 in Chromium. Git LFS files show their pointer files.
  `git status` is unaffected, since its index is built from the same unconverted
  data.
- **Timestamps:** everything inside a snapshot has mtime/ctime 2020-01-01; see
  [ARCHITECTURE.md](ARCHITECTURE.md#constant-timestamps).
- **Shared inodes:** files with identical content and mode share an inode,
  within a snapshot and across snapshots. They report link count 1, so `cp -a`,
  `tar`, `du` and `rsync -H` don't treat them as hardlinks; tools that compare
  device and inode (`find -samefile`) see them as the same file.

## Performance

Chromium (512k files, 4.4 GB at HEAD, 64 GB pack), 24 cores, 62 GB RAM, pack
and native checkout in page cache, gitfold after the default metadata prewarm.
"git status" is the exact invocation Claude Code issues itself
(`--porcelain=v2 --untracked-files=normal --ignored=matching`, fsmonitor off).
Details and more repositories: [bench/RESULTS.md](bench/RESULTS.md).

| | native checkout | gitfold |
|---|---:|---:|
| open a revision | `git worktree add` (not measured) | 0.56-0.81 s first, 0.10-0.17 s nearby |
| `find . -type f` | 473 ms | 351 ms |
| `git status` | 884 ms | 1.15 s |
| `rg`, first run in a fresh snapshot | 371 ms | 2.9 s (0.5 s with `--prewarm content`) |
| `rg`, repeat | 371 ms | 0.41 s |
| `rg` in a 2nd snapshot ~1 day older than one already grepped | | 0.40 s |
| daemon heap, 4 snapshots open (HEAD, ~1 day, ~2 days, ~1 week older) | | 77-82 MB |

The slow first `rg` is the cost of decompressing and caching every file it
reads; later reads come from the kernel page cache, which snapshots share for
identical files.

Benchmark scripts are in `bench/`: `bench.sh` compares native and mounted runs of
the commands an agent issues, `probe.sh` isolates content reads.

See [ARCHITECTURE.md](ARCHITECTURE.md) for how it works and why, including the
code map.
