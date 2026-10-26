# gitfold

**Every branch and every commit of your git repository as a folder. No
checkout, no copying, ready in about a second.**

```
gitfold mount ~/src/monorepo ~/mnt/repo   # your existing clone, as folders
cd ~/mnt/repo/@v2.1.0                     # @ + commit, tag or branch: read-only
cd ~/mnt/repo/agent/task-17               # a branch: edit, commit, push
```

Built for coding agents and code review on big monorepos. Give every agent its
own branch and every reviewer any revision, side by side, without
`git worktree add` copying gigabytes for each one. Linux only (FUSE).

## Why gitfold

- **Opens in under a second.** A branch of Chromium (512k files, 4.4 GB) opens
  in 0.85 s; `git worktree add` takes over 17 s and 5.5 GiB of disk. Nothing is
  copied: unchanged files are read straight from git.
- **A real checkout to every tool.** Each folder is a linked git worktree, so
  `git status`, `commit`, `push`, editors, `rg` and agents work as usual, and
  reads run close to native speed once warm.
- **Costs only what you change.** A workspace stores just the files you edit,
  inside `.git/`, and drops them again once you commit. Changes survive
  unmounts and crashes.
- **Dozens side by side.** Open as many revisions and branches as you have
  agents: snapshots of nearby revisions share memory and page cache, and a
  Chromium workspace costs about 125 MB of daemon memory.
- **Your repository, as is.** Works on your existing clone: no new clone, no
  server, no root. Branches and commits are ordinary git.

## Quick start

Needs Linux with FUSE 3 (`fusermount3`, kernel 5.1+), git, and Rust 1.88+ to
build.

1. **Install:**

   ```
   git clone https://github.com/pkukielka/gitfold && cd gitfold
   cargo install --locked --path .        # or: nix develop -c cargo install ...
   ```

2. **Mount** your existing clone (it runs in the background until you unmount
   it):

   ```
   mkdir -p ~/mnt/repo
   gitfold mount ~/src/monorepo ~/mnt/repo
   ```

3. **Use** it. Each folder opens on first access, in about a second even on a
   large repository:

   ```
   cd ~/mnt/repo/@HEAD~5                       # any revision, read-only
   gitfold branch ~/src/monorepo my-task       # create a branch...
   cd ~/mnt/repo/my-task                       # ...and work in it
   echo fix >> README.md
   git commit -am "Fix" && git push -u origin HEAD
   ```

4. **Stop** the mount (from outside it), and remove the workspace when you no
   longer need it (its commits stay on the branch):

   ```
   cd ~
   gitfold unmount ~/mnt/repo
   gitfold remove ~/src/monorepo my-task
   ```

The rest of this page is reference: [How it works](#how-it-works) ·
[Requirements](#requirements) · [Usage](#usage) · [Snapshots](#snapshots) ·
[Workspaces](#workspaces) · [Running it](#running-it) ·
[Cleaning up](#cleaning-up) ·
[Differences from a checkout](#differences-from-a-checkout) ·
[Performance](#performance) · [Development](#development)

## How it works

gitfold is a FUSE filesystem with two kinds of directories:

- **Snapshots**, `<mount>/@<rev>`: the tree of any revision, read-only, read
  straight from the repository's object database.
- **Workspaces**, `<mount>/<branch>`: a branch you can edit. Unchanged files come
  from the object database; changed files are stored as real files under
  `.git/gitfold/`, so a workspace costs only its changes and survives unmounts.

Both are registered as linked git worktrees, so git commands inside them behave
as in a normal checkout.

gitfold writes into the repository: worktree registrations under
`.git/worktrees/gitfold-*`, workspace changes under `.git/gitfold/`, and refs
`refs/gitfold/*` that keep workspace bases from `git gc` (see
[Cleaning up](#cleaning-up)).

## Requirements

- Linux with FUSE 3 (`fusermount3` on `PATH`, access to `/dev/fuse`); no root
  needed. Tested on 6.12; kernel 5.1 or newer is required for the no-opendir
  mode gitfold depends on.
- git with linked worktree and index v4 support (any git from the last decade;
  tested with 2.49).
- To build: Rust 1.88 or newer (edition 2024; developed with 1.99), or Nix on
  x86_64-linux (`flake.nix` provides a dev shell with the toolchain). Without
  installing, the binary is `target/release/gitfold` after
  `cargo build --release`.

## Usage

```
gitfold mount [OPTIONS] <REPO> <MOUNTPOINT>   # starts in the background
gitfold unmount <MOUNTPOINT>                  # stops it
gitfold branch <REPO> <NAME> [--from <REV>]   # create a branch for a workspace
gitfold list <REPO>                           # workspaces and their changes
gitfold remove <REPO> <BRANCH> [--force]      # delete a workspace (unmounted)
gitfold prune <REPO>                          # delete snapshot registrations
```

`<REPO>` is the repository's top-level work tree or its git directory (not a
subdirectory). `<MOUNTPOINT>` must be an existing directory you own; use an
empty one, since its contents are hidden while mounted. A repository can be
mounted only once at a time (a second `gitfold mount` of it fails): two mounts
would share, and corrupt, the stored workspace changes. The mount is read-write
(for workspaces); snapshots are read-only (see [Snapshots](#snapshots)).
`gitfold branch` starts the branch at HEAD unless `--from` says otherwise.

Names below the mount point:

- `@<rev>` is a snapshot. `<rev>` is anything that resolves to a commit using
  git's revision syntax (parsed by gitoxide): `@HEAD`, `@HEAD~5`, `@<sha>`,
  `@<short-sha>`, `@v1.2.0`. Ref names containing `/` work as typed
  (`@origin/main`, `@origin/main~3`), since each `/` is a directory level;
  other syntax with `/` (`@HEAD^{/fix}`, `@:/msg`) can't be used, so resolve it
  to a SHA first. `@origin` is then a directory, so git's `origin` shorthand is
  `@origin/HEAD`.
- `<branch>` is a workspace on an existing local branch: `feature/login` is
  `<mount>/feature/login`, through a `feature/` directory. Git doesn't allow
  `feature` and `feature/login` to coexist, so this is never ambiguous. Branches
  whose name starts with `@` can't be opened (`@` means a snapshot).
- Snapshots and workspaces are opened on first access (a workspace resumes its
  saved changes). `ls <MOUNTPOINT>` lists only what has been opened so far; it
  starts empty, so names can't be browsed or tab-completed.
- A name that doesn't resolve looks like a missing directory (`No such file or
  directory`); for `@<rev>` the daemon logs why (`cannot resolve @<rev>: ...`),
  for branch names it doesn't.
- Snapshots are pinned for the life of the mount: `@HEAD` keeps showing the
  commit HEAD pointed to when it was first opened. To see a newer commit, open
  it by SHA or remount.

Options:

| option | default | |
|---|---|---|
| `--prewarm none\|meta\|content` | `meta` | After opening a snapshot or workspace, walk it in the background to fill kernel caches. `meta` makes the first `find`/`git status` fast; `content` also reads every file, so the first grep is fast too, at the cost of page cache equal to the tree size. The directory is usable while this runs (chromium: 0.64 s for a snapshot, 1.37 s for a workspace). |
| `--prewarm-threads N` | half the CPUs | Threads used by the prewarm walk. |
| `--threads N` | number of CPUs | FUSE worker threads. |
| `--kernel-open` | off | Debug/benchmark only: let the kernel send open/release for every file. Slower. |

### Snapshots

Read-only: files and directories have no write bits, so writes fail with
`EACCES`; `chmod` and setting times (`touch -d`, `utimensat`) fail with
`EROFS`. Each snapshotted commit is registered as a linked worktree under
`<repo>/.git/worktrees/gitfold-<sha12>` (the first 12 hex digits of the commit)
with a detached HEAD, so `git status`, `log`, `diff`, `show`, `blame` etc. work.
Names that resolve to the same commit (`@HEAD` and `@<its sha>`) share one
registration; `git worktree list` shows it once, under the first name opened.

Git commands that write files (`checkout`, `reset --hard`, `stash`, merges) fail.
Commands that only change `.git` (e.g. `commit`, `reset --soft`)
succeed and move that worktree's HEAD away from the snapshot's files, after which
`git status` shows differences. Don't use them in a snapshot; use a workspace.

Recommended for agents: resolve the revision to a SHA yourself and give each task
its own snapshot (`cd <mount>/@$(git rev-parse origin/main)`). If agents grep a
fresh snapshot immediately, consider `--prewarm content`: on chromium it reads
everything in the background in 2.6-3.8 s (24 vs 8 threads), after which the
first `rg` takes about 0.5 s instead of 2.9 s.

### Workspaces

**Start.** Create a branch (gitfold only opens existing local branches) and
open it:

```
gitfold branch ~/src/monorepo agent/task-17 --from origin/main
cd ~/mnt/repo/agent/task-17
```

Starting from a remote branch makes it the upstream, so push with
`git push -u origin HEAD`. Workspaces are opened by name, not created with
`mkdir` (the mount root is read-only); create the branch first.

`gitfold branch` runs `git branch`, refusing names that start with `@` (they
would open as snapshots) and names whose earlier workspace is still stored (it
would be resumed; `gitfold remove` it first). Creating the branch with git
directly also works, without these checks.

Like `git worktree add`, a branch that is checked out elsewhere (for example in
your main checkout), or being rebased or bisected there, is refused with
`EBUSY` (`Device or resource busy`); the daemon logs where it is checked out.
Create another branch from it instead.

`@agent/task-17` is a separate, read-only snapshot of the branch tip when it was
opened; it doesn't see the workspace's uncommitted changes, and both can be open
at once. Recommended for agents: one branch and workspace per task.

**Work.** Edit, create, delete and rename files normally. The workspace is a
linked worktree whose HEAD is the branch, so `git add`, `commit`, `diff`,
`log` etc. work and commits move the branch in your repository. Changes are
written to `.git/gitfold/` as they happen (and `fsync` is passed through), so
unmounting, remounting or a daemon crash keeps them. Don't switch branches
inside a workspace (`git switch`, `git checkout <branch>`): it works as in any
worktree, but the directory keeps the old branch's name and its stored changes.

**Commits, merges, rebases** work as usual, also from outside the workspace
(`reset`, `commit --amend`, ...). Within about a second, once git is idle (no
conflicts, `rebase -i` or `bisect` pending), gitfold moves the workspace's base
to the new branch tip. No file changes content, mode or mtime in the process,
so commands chained after a commit (`git commit && git status`) see the same
files either way, and committed files, as well as files git rewrote unchanged
(`stash`, `rebase --abort`), stop taking space in `upper/`.

**Finish.** Push or merge the branch from anywhere, unmount, then remove the
workspace with `gitfold remove <REPO> <BRANCH>` (see
[Cleaning up](#cleaning-up) for when it refuses). `gitfold list <REPO>` shows
how many files each workspace has changed and deleted, and how many ignored
files (build output) it holds, with a note if its branch was deleted or its
base is behind the branch tip (mount once to update it). Until it is removed,
git treats the branch as checked out, even while unmounted: you can't check it
out elsewhere or delete it.

**How it's stored.** The workspace id is the branch name with `%` and `/`
escaped (`agent%2Ftask-17`). The worktree registration is
`.git/worktrees/gitfold-ws-<id>`; the changes are in `.git/gitfold/<id>/`:
`upper/` holds every file that differs from the workspace's base (the branch's
last tip, see above), as real files; `markers` records deleted files so they
stay deleted; `times` the timestamps of files dropped from upper (committed,
or restored unchanged); `base` is that commit, kept from `git gc` by the ref
`refs/gitfold/<id>`; `branch` the branch name; `tmp/` copies in progress.
Don't modify `.git/gitfold/` while mounted.

**Limitations.**

- Moving the branch far without changing the files (`reset --soft` to an old
  commit) copies every file that differs into `upper/`, during which the
  workspace's file operations wait: 0.40 s to copy 2,857 files in Apache Spark.
- Renaming a directory from the base commit copies its files into `upper/`
  first, while the workspace's file operations wait (0.23 s for a 7,500-file,
  19 MB directory in Apache Spark). Above 10,000 files or 100 MiB, or if the
  copy fails, it returns `EXDEV` instead, as overlayfs does: `mv` then copies
  and deletes, but `git mv` of such a directory fails. Renaming files, and
  directories you created, works directly.
- Hard links and special files (`mknod`) are not supported (`EPERM`); symlinks
  are.
- Reads and writes are addressed by path, so reading or writing a file that was
  deleted or replaced (renamed over) while open fails.
- Writes go through the gitfold daemon. Creating many small files is several
  times slower than on a local disk: untarring Apache Spark's 27,599 files
  takes 3.00 s instead of 0.53 s, which is what a `node_modules` install looks
  like. Builds cost much less: a full `cargo build` of gitfold takes 15.99 s
  instead of 13.64 s, an incremental one 3.91 s instead of 1.85 s (see
  [Performance](#performance)).
- After `git add` of files with `eol=crlf` attributes, git warns that line
  endings will be converted (see [Differences](#differences-from-a-checkout)).

## Running it

- `gitfold mount` starts the daemon in the background, detached from the
  terminal (closing it doesn't stop the mount), and returns once the mount is
  up, or with the daemon's error. It logs to a file named after the mount
  point in `/gitfold/` (default `~/.local/state/gitfold/`),
  printing its path (`--log` to choose): one line per new snapshot commit,
  workspace, prewarm and workspace rebase, and operation counters on exit. `RUST_LOG=warn` for only problems. With `--foreground` it
  stays in the foreground and logs to stderr, for service managers.
- **Stop it with `gitfold unmount <MOUNTPOINT>`.** If the mount is busy (a
  shell's working directory is inside it), it is detached at once and the
  daemon exits when the last process leaves, so `cd` out before `gitfold
  remove` or `prune`; otherwise `unmount` waits until the daemon has exited,
  and they can follow right away. Before exiting, the daemon brings every open
  workspace's base up to its branch tip. Ctrl-C (in the foreground), SIGTERM,
  SIGHUP and `fusermount3 -u <MOUNTPOINT>` stop it as well; a second SIGINT or
  SIGTERM exits immediately.
- Only `kill -9` (or a crash) leaves the mount behind: accesses then fail with
  `Transport endpoint is not connected`, and `gitfold unmount <MOUNTPOINT>`
  cleans it up. If accesses hang instead (killed while a prewarm walk was
  running), `echo 1 > /sys/fs/fuse/connections/<N>/abort` releases them, with
  `<N>` the minor device number from `grep gitfold /proc/self/mountinfo` (the
  third field is `0:<N>`); no root needed. `gitfold mount` detects such stale mounts, and mount points
  that are missing, not directories or already mounted, and says so.
- **Cost of a long-running mount:** snapshots and workspaces stay loaded until
  unmount.
  - Each additional chromium snapshot costs 2-11 MB of daemon heap, ~160 MB of
    kernel inode/dentry cache and ~195 MB of page cache when opened (mostly
    cached directory listings), plus page cache for file contents not already
    cached by another snapshot.
  - A chromium workspace costs ~125 MB of daemon memory once prewarmed (~67 MB
    of it the node table: one ~120-byte entry per path the kernel has looked
    up; ~58 MB in total with `--prewarm none`, measured before the node table
    was compacted). Its unchanged files don't share page cache with snapshots
    or other workspaces.
  - Kernel caches are reclaimable; if evicted, the next access is just slower.

### Cleaning up

Worktree registrations are locked, so `git worktree prune` keeps them while
unmounted; later mounts reuse them. With the repository unmounted (both commands
refuse while any of its workspaces or snapshots is mounted):

- `gitfold prune <REPO>` deletes the snapshot registrations; snapshots are
  registered again when opened.
- `gitfold remove <REPO> <BRANCH>` deletes a workspace: its registration, its
  stored changes and its base ref. Its commits stay on the branch. It refuses if
  the workspace has uncommitted or staged changes (it lists them), or a git
  operation is in progress or stopped half-way in it (a merge with conflicts,
  `rebase -i`, a detached HEAD or another branch checked out), unless
  `--force`, which discards them. "Uncommitted" is roughly what `git status`
  would show: files that differ from the workspace's base (changed, new or
  deleted; of a mode, only the executable bit counts). Untracked files git
  ignores (build output, by `.gitignore`, `.git/info/exclude` or
  `core.excludesFile`, by default `~/.config/git/ignore`) don't count: they are
  deleted with the workspace, as `git worktree remove` does, and `remove` says
  how many (without `--force`). An untracked nested repository counts as one
  change unless ignored. If the branch moved since the workspace was
  last mounted, it refuses until a mount lets the base catch up (or
  `--force`).

The same by hand (bash), for example to remove everything at once: unmount,
then use the repository's absolute git directory (a relative one would make
`rm` act on the current directory):

```
common=$(git -C <REPO> rev-parse --path-format=absolute --git-common-dir)
```

Snapshot registrations only (keeps workspaces):

```
rm -rf "$common"/worktrees/gitfold-[0-9a-f]*
```

A workspace: its registration and its changes, together (deleting only one leaves
a workspace gitfold can't resume correctly). This discards uncommitted changes;
commits are on the branch and stay there.

```
id=agent%2Ftask-17
rm -rf "$common/worktrees/gitfold-ws-$id" "$common/gitfold/$id"
git -C <REPO> update-ref -d "refs/gitfold/$id"
```

All workspaces:

```
rm -rf "$common"/worktrees/gitfold-ws-* "$common"/gitfold
git -C <REPO> for-each-ref --format='%(refname)' refs/gitfold \
  | xargs -r -n1 git -C <REPO> update-ref -d
```

## Differences from a checkout

- **File contents are served as stored in git.** Checkout conversions from
  `.gitattributes` are not applied: `eol=crlf`, Git LFS smudging, `ident`,
  `working-tree-encoding` and custom filters. With git's defaults on Linux
  (`core.autocrlf` unset) only files with such explicit attributes are affected:
  19 in Apache Spark, 1 in Chromium. Git LFS files show their pointer files.
  `git status` is unaffected, since its index is built from the same unconverted
  data; `git add` of such files in a workspace prints git's line-ending warning.
- **Timestamps:** unchanged files have mtime/ctime 2020-01-01; files changed in
  a workspace have their real times, and keep them once committed. See
  [ARCHITECTURE.md](ARCHITECTURE.md#constant-timestamps).
- **Inodes:** in snapshots, files with identical content and mode share an inode,
  within a snapshot and across snapshots. They report link count 1, so `cp -a`,
  `tar`, `du` and `rsync -H` don't treat them as hardlinks; tools that compare
  device and inode (`find -samefile`) see them as the same file. In workspaces,
  inodes are per path.
- **Submodules** appear as empty directories, as in a checkout without
  `git submodule update`; they can't be initialized inside the mount.

## Performance

Chromium (512k files, 4.4 GB at HEAD, 64 GB pack), 24 cores, 62 GB RAM, pack
and native checkout in page cache, gitfold after the default metadata prewarm,
a fresh workspace; first run / repeat. "git status" is the invocation Claude
Code issues itself. Details, methodology and more repositories:
[bench/RESULTS.md](bench/RESULTS.md).

| | native checkout | snapshot | workspace |
|---|---:|---:|---:|
| open | `git worktree add`: 17.30 / 17.22 s, 5.5 GiB | 0.84 s (0.10-0.12 s near an open one) | 0.85 s |
| prewarm (background) | | 0.64 s | 1.37 s |
| `find . -type f` (GNU find) | 0.47 / 0.47 s | 0.36 / 0.35 s | 0.35 / 0.35 s |
| `git status` | 0.86 / 0.88 s | 1.15 / 1.13 s | 1.20 / 1.17 s |
| `rg` | 0.37 / 0.37 s | 2.98 / 0.40 s | 3.36 / 0.41 s |
| daemon memory | | 59 MB (4 snapshots: 77 MB, 82 MB after grepping) | 125 MB |

The slow first `rg` is the cost of decompressing and caching every file it
reads; later reads come from the kernel page cache, which snapshots share for
identical files (a second snapshot ~1 day older than one already grepped:
0.40 s for its first `rg`).

Writes, on Apache Spark (`bench/writes.sh`: a workspace vs a plain directory on
the same ext4 disk, or for the appends the scratch clone's checkout; first /
repeat):

| workload | native | workspace |
|---|---:|---:|
| untar the checkout (27,599 files, 227 MiB) | 0.53 / 0.53 s | 3.00 / 2.80 s |
| `rm -r` of it | 0.33 / 0.33 s | 1.23 / 1.23 s |
| write a 1 GiB file | 0.40 / 0.39 s | 0.77 / 0.71 s |
| `cp -r sql/` (21,094 files) | 0.48 / 0.46 s | 1.81 / 1.83 s |
| append to 1,000 tracked files | 0.03 / 0.03 s | 0.11 / 0.04 s |

Every file created costs about six requests to the daemon (lookup, create,
write, tar's `utimes`, and the attribute reads that follow a write), so file
creation is 4-6x slower than native; large writes are under 2x. The first
appends to tracked files copy them into `upper/`.

Builds write fewer, larger files, so they suffer much less. `cargo build`
(debug, offline) of gitfold itself, a workspace vs a native clone; `target/`
is 715 MB in 1,677 files:

| build | native | workspace |
|---|---:|---:|
| full | 13.64 s | 15.99 s |
| incremental (one source file touched) | 1.85 s | 3.91 s |
| no-op | 0.17 s | 0.36 s |

## Development

Benchmark scripts are in `bench/`: `bench.sh` compares native and mounted runs of
the commands an agent issues, `probe.sh` isolates content reads, `writes.sh`
measures write workloads. Tests: `tests/workspace.sh <repo>` (workspaces, on a
scratch clone), `tests/rebase.sh` (git operations moving the branch) and
`tests/cli.sh` (`branch`, `list`, `remove`, `prune`). Build with `--release`
first; the tests use scratch directories under `/tmp` (kept with `KEEP=1`);
`bench.sh` must run inside Claude Code (it uses Claude Code's grep and needs
`CLAUDE_CODE_EXECPATH`).

See [ARCHITECTURE.md](ARCHITECTURE.md) for how it works and why, including the
code map.
