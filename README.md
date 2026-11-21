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
`git worktree add` copying gigabytes for each one. Linux only (FUSE, no root).

## Why gitfold

- **Opens in under a second.** A branch of Chromium (512k files, 4.4 GB) opens
  in 0.87 s; `git worktree add` takes over 17 s and 5.5 GiB of disk. Nothing is
  copied: gitfold reads unchanged files straight from git.
- **A real checkout to every tool.** Each folder is a linked git worktree, so
  `git status`, `commit`, `push`, editors, `rg` and agents work as usual, and
  reads run close to native speed once warm.
- **Costs only what you change.** A workspace stores just the files you edit,
  as plain files under `.git/gitfold/`, keeps them across unmounts and gitfold
  crashes, and drops them once you commit.
- **Dozens side by side.** Snapshots of nearby revisions share memory and page
  cache. A Chromium workspace costs about 125 MB of daemon memory.
- **Your repository, as is.** Works on your existing clone: no new clone, no
  server. Branches and commits are ordinary git.
- **Agents building on each other's work.** A branch built on another takes
  its new commits by itself, and read-only views show a branch merged with
  everything built on it, uncommitted work included.

## Costs and limits

- **Writes go through FUSE.** Creating many small files is 4-6x slower than
  native; large writes, under 2x slower. A Linux kernel build is 2.5-4% slower
  than in a worktree, an incremental one 11-29% (mostly linking). See
  [Performance](docs/reference.md#performance).
- **First reads are cold.** The first `rg` over Chromium takes about 3 s,
  against 0.4 s natively, as gitfold decompresses files from git; later runs
  take 0.4 s.
- **Not quite a checkout.** gitfold shows Git LFS files as pointers, doesn't
  apply `.gitattributes` filters, can't initialise submodules, and gives
  unchanged files a fixed modification time. See [Differences from a
  checkout](docs/reference.md#differences-from-a-checkout).

## Quick start

Needs Linux with FUSE 3 (kernel 5.1+) and git; no root. For containers, other
libcs and NixOS, see [Requirements](#requirements). Use it by hand, or through
the Claude Code plugin, which gives each `claude -w` session its own workspace.
The [user guide](docs/guide.md) then walks through the situations where it
pays off.

### By hand

1. **Install**: download `gitfold-x86_64-linux` (or `-aarch64-`; glibc 2.28 or
   newer) from the [releases](https://github.com/pkukielka/gitfold/releases),
   check it against `SHA256SUMS`, and put it on `PATH` as `gitfold`. Or build
   it (Rust 1.93+):

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

3. **Use** it. Each folder opens on first access; `ls ~/mnt/repo` lists it
   only after that, so `cd` to it by name:

   ```
   cd ~/mnt/repo/@HEAD~5                       # any revision, read-only
   gitfold branch my-task --from main          # create a branch...
   cd ~/mnt/repo/my-task                       # ...and work in it
   echo fix >> README.md
   git commit -am "Fix" && git push -u origin HEAD
   ```

4. When done, **remove** the workspace (its commits stay on the branch; this
   works mounted or not) and **unmount**, both from outside the mount:

   ```
   cd ~/src/monorepo
   gitfold remove my-task
   gitfold unmount ~/mnt/repo
   ```

### With Claude Code

1. In Claude Code, **install the plugin**:

   ```
   /plugin marketplace add pkukielka/gitfold
   /plugin install gitfold@gitfold
   ```

   It downloads gitfold when first needed (see [Requirements](#requirements);
   on NixOS, [build it](#by-hand) and put it on `PATH`).

2. **Mount** your repository once: in a Claude Code session there, ask Claude
   to "mount this repository with gitfold" (at `~/mnt/<name>` unless you say
   where). It stays mounted after the session; when it isn't (after a reboot,
   say), the plugin mounts it again the next time you use it.

3. **Work**: `claude -w my-task` from the repository gets a gitfold workspace
   at `<mount point>/worktree-my-task`; so do subagents with
   `isolation: worktree`. When the session ends, Claude Code offers to remove
   the workspace; removing it discards its uncommitted changes, and the branch
   stays.

The skill's "Claude Code" section has the details
([skills/gitfold/SKILL.md](skills/gitfold/SKILL.md)).

## How it differs

- **`git worktree`** writes every file of every checkout; gitfold writes none
  and stores only what you change, at the cost of writes through FUSE.
- **Sparse checkout and partial clone** make a checkout smaller by leaving
  files out or fetching them later; gitfold shows the whole tree without
  writing it.
- **overlayfs** needs a checkout per base and root (or a user namespace), and
  its base can't move; gitfold's base is any git revision, and it moves with
  the branch.
- **EdenFS (Sapling) and VFS for Git** fetch files lazily from their own
  servers, outside plain git; gitfold works on an ordinary local clone.
- **jj workspaces** are full checkouts, and jj is a separate version control
  system built on git; gitfold stays plain git.
- **Stacked-branch tools** rebase a stack when you run them; gitfold moves
  branches onto their parents' new commits by itself, workspaces included.

Each is the better choice in some cases: small repositories, heavy builds, Git
LFS, macOS. See [gitfold and the alternatives](docs/comparison.md).

## How it works

gitfold is a FUSE filesystem with two kinds of directories:

- **Snapshots**, `<mount>/@<rev>`: the tree of any revision, read-only, read
  straight from the repository's object database.
- **Workspaces**, `<mount>/<branch>`: a branch you can edit. Unchanged files come
  from the object database; changed files are stored as real files under
  `.git/gitfold/`, so a workspace costs only its changes and survives unmounts.

Both are registered as linked git worktrees, so git commands inside them behave
as in a normal checkout.

The docs also use these terms:

- **Base**: the commit a workspace's unchanged files come from. It moves with
  the branch (commit, merge, rebase, reset) without changing what you see.
- **Parent, following**: the branch another is built on. A branch takes its
  parent's new commits by itself (it *follows* its parent) unless you pass
  `--no-follow` to `gitfold branch` when you create it, or later to
  `gitfold parent`.
- **Views**, `<mount>/@committed/<branch>` and `@live/<branch>`: read-only
  merges of a branch with every branch built on it (for the main branch, only
  those opened as workspaces and the branches they are built on); `@live`
  includes uncommitted work.
- **Task folder**, `<workspace>/.task` by default: a folder outside the branch
  for an agent's notes, status and inbox.

gitfold writes into the repository: worktree registrations under
`.git/worktrees/gitfold-*`, workspace changes under `.git/gitfold/`, refs
`refs/gitfold/*` that keep workspace bases from `git gc` (see [Cleaning
up](docs/reference.md#cleaning-up)), refs `refs/gitfold-base/*` where a
branch's own commits start (see [Branches built on
branches](docs/reference.md#branches-built-on-branches)), and settings in its
git config (`gitfold.*`, `branch.<name>.gitfold*`).

The [reference](docs/reference.md) covers every command in detail.

## Requirements

- Linux with FUSE 3 (`fusermount3` on `PATH`, access to `/dev/fuse`); no root
  needed. Tested on 6.12 and 6.18; kernel 5.1 or newer is required for the
  no-opendir mode gitfold depends on. Not macOS or Windows.
- Release binaries for x86_64 or aarch64 with glibc 2.28 or newer (tested on
  Ubuntu 24.04 and Rocky Linux 8); elsewhere (musl, NixOS), build from source.
- In a container, FUSE needs `--device /dev/fuse --cap-add SYS_ADMIN` (Docker;
  plus `--security-opt apparmor=unconfined` under AppArmor).
- git with linked worktree and index v4 support (any git from the last decade;
  tested with 2.49). Moving branches built on branches (`gitfold parent
  --update`, `--set`, following) and the views (`@committed`, `@live`) need
  git 2.40 or newer (before 2.43, a view's merges read `.gitattributes` from the
  checkout, not from the view); the views in a partial clone, 2.44.
- A full clone, or a partial clone (`git clone --filter=blob:none`, or
  `tree:0`; git 2.29 or newer) whose promisor remote gitfold can fetch from
  without a prompt (an ssh agent, a credential helper); see [Partial
  clones](docs/reference.md#partial-clones).
- To build: Rust 1.93 or newer (edition 2024; developed with 1.99), or Nix on
  x86_64-linux (`flake.nix` provides a dev shell with the toolchain). Without
  installing, the binary is `target/release/gitfold` after
  `cargo build --release`.

## Usage

```
# Mount a repository (starts in the background), stop it, show versions
gitfold mount [OPTIONS] <REPO> <MOUNTPOINT>
gitfold unmount <MOUNTPOINT>
gitfold version [<MOUNTPOINT>]

# Create a branch for a workspace (-w: with a workspace's changes;
# several --from: git merge of the others, run in its workspace)
gitfold branch <BRANCH> [-w] [--from <REV>]... [--prewarm <MODE>] [--track-reads on|off]

# Branches built on branches: create one on P; show or change what one is built on
gitfold branch <BRANCH> --parent <P> [--no-follow]
gitfold parent [<BRANCH>] [--set <P> | --none] [--follow | --no-follow] [--update [--yes]]

# See what changed: workspaces and the tree; a workspace's paths; who changes a path
gitfold list [--stat]
gitfold changes [<BRANCH>] [-z] [--stat]
gitfold changes -- <PATH>...

# What a workspace read or changed that changed elsewhere since
gitfold overlaps [<BRANCH>] [--against <REV>]... [--against-workspaces] [--reads|--writes] [-z]

# Messages: send to a task's inbox; take them; wait for a change in a task folder
gitfold send <BRANCH> [-s <SUBJECT>] [--from <NAME>] <TEXT>...
gitfold inbox [<BRANCH>] [--wait [--timeout <SECS>]] [--peek] [--json] [--claude-hook <EVENT>]
gitfold watch [<BRANCH>:]<DIR> [--timeout <SECS>] [--json]

# Housekeeping: delete a workspace (or a snapshot's), snapshot registrations;
# a workspace's prewarm mode; record a workspace's reads
gitfold remove <BRANCH|@REV> [--force] [--check] [--wait <SECS>]
gitfold prune
gitfold prewarm <BRANCH> <MODE>
gitfold track-reads <BRANCH> on|off

# Claude Code hooks (the plugin's; see the skill): worktrees, presence, turns, reads
gitfold branch --claude-hook worktree-create
gitfold remove --claude-hook worktree-remove
gitfold parent --claude-hook <EVENT>
gitfold track-reads --claude-hook pre-tool-use
```

The [reference](docs/reference.md#usage) explains how a command finds its
repository, what `mount` needs, the exit codes, `branch` with several
`--from`, the names below the mount point, and the mount's options and
prewarm settings. `gitfold <command> --help` lists any command's options.

## Development

Benchmark scripts are in `bench/`: `bench.sh` compares native and mounted runs
of the commands an agent issues, `probe.sh` isolates content reads, `writes.sh`
measures write workloads, `tree.sh` times branches built on branches and views,
and `kbuild.sh` times Linux kernel builds. `bench.sh` must run inside Claude
Code (it uses Claude Code's grep and needs `CLAUDE_CODE_EXECPATH`).

Tests: `cargo test` runs the unit tests (logic without a mount or git:
parsing, formats, settings, a few scratch files); the end-to-end suites drive
the release binary, a real mount and real git (build with `--release` first):

- `tests/manage.sh` (branch, list, changes, remove, prune), `tests/status.sh`
  (what a workspace's status sees: ignore rules, modes, half-way git
  operations, the index), `tests/mount.sh` (mount, unmount, version,
  prewarm), `tests/agents.sh` (overlaps, the task folder, messages),
  `tests/tree.sh` (branches built on branches: `parent`, following,
  landing), `tests/from.sh` (`branch` with several `--from`) and
  `tests/views.sh` (`@committed`, `@live`): one section at a time, each on a
  fresh repository. `SECTION=inbox tests/agents.sh` runs the sections whose
  name contains `inbox`, and
  `PARALLEL=1` runs them all at once (a few timing checks may then be flaky on
  a loaded machine). A section that ends early counts as a failure.
- `tests/run.sh [SUITE...]`: the suites at once, summed up (by default all
  but `workspace.sh`); `PARALLEL=1 tests/run.sh` takes about as long as the
  slowest, and `SECTION=inbox tests/run.sh` finds the section in any of them.
  Suites running side by side load the machine like `PARALLEL=1`: if a timing
  check fails, rerun that suite alone.
- `tests/rebase.sh`: git operations moving a workspace's branch.
- `tests/workspace.sh <repo>`: workspaces on a scratch clone of `<repo>`.
- `tests/promisor.sh`: partial clones.

Each run works in a directory of its own under `/tmp` (`gf-<suite>.XXXXXX`,
kept with `KEEP=1`), so runs can overlap; every gitfold command it runs gets
`GF_TIMEOUT` seconds (default 120), and stdin is `/dev/null`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for how it works and why, including the
code map.

## License

gitfold is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in gitfold by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
