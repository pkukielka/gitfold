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
  syscalls) and after every Bash command runs
  `git status --porcelain --untracked-files=normal` itself with fsmonitor
  disabled (with its "bash edit diff" feature on, also an `--ignored=matching`
  variant). There is no API to hook into: the filesystem must be
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
   fs/workspaces/     fs/snapshots.rs     store.rs          fs/content.rs
   names: branches,   @<rev> registry     shared tree       per-inode
   namespaces            |                tables, file ids  read cache
      |                  |                   |                 |
   fs/workspace/         |                   |                 |
   lower + upper view    |                   |                 |
      |                  |                   |                 |
   git/worktree.rs  <----+           git/mod.rs (gitoxide object database)
                                  (lines are calls)

 on disk:  .git/worktrees/gitfold-<sha12>/   snapshot registrations
           .git/worktrees/gitfold-ws-<id>/   workspace registrations
           .git/gitfold/<id>/                workspace state: base, branch, markers, times, reads, lens, restack, upper/, tmp/
           .git/gitfold/.tasks/<id>/         task folders (kept after the workspace)
           .git/gitfold/.control             the daemon's control socket
           refs/gitfold/<id>                 keeps the workspace base from git gc
           refs/gitfold-seal/<id>/<n>        keeps a workspace's seals (the newest 16)
           refs/gitfold-lens/<id>            keeps a lensed workspace's view commit (and its layers commit)
           refs/gitfold-stack/<branch>       a stacked branch's parent base
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
| `src/fs/workspaces/mod.rs` | name resolution below the root; workspace and namespace registry; opening workspaces; the sync thread |
| `src/fs/workspaces/busy.rs` | busy marks: branches a request works on, not opened, removed or worked on meanwhile |
| `src/fs/workspaces/requests.rs` | the control requests served on workspaces: forks (with the request lock), applying prewarm and read-tracking settings, layer switches (telling the kernel what changed) |
| `src/fs/workspace/mod.rs` | one workspace: merged lower/upper view, lookups, cached listings, reads |
| `src/fs/workspace/write.rs` | copy-up and mutations |
| `src/fs/workspace/nodes.rs` | local id ↔ (parent, name) table, freed on kernel `forget` |
| `src/fs/workspace/rebase.rs` | moving the base to the branch tip invisibly; dropping upper files identical to the base |
| `src/fs/workspace/times.rs` | timestamps of files dropped from upper (committed, or restored unchanged); only the mount uses them |
| `src/fs/workspace/restack.rs` | a closed workspace's base moved visibly for a restack of its branch, its changed files stamped |
| `src/fs/workspace/reads.rs` | read tracking: recording the files read in a workspace, for `gitfold overlaps` |
| `src/fs/workspace/lens.rs` | layers in an open workspace: the view tree as its lower layer, switches and what the kernel must be told (and own lines in blended files carried over), git told (skip-worktree, excludes, the blends in the index, HEAD at the lens commit, the push guard) or surrendered; extracting commits made on the lens commit onto the branch (`sync_lens`) |
| `src/fs/names.rs` | telling the kernel when names in the root or a namespace changed |
| `src/fs/content.rs` | per-inode content cache and idle trimmer |
| `src/fs/stats.rs` | operation counters, logged at unmount |
| `src/fs/removal.rs` | removing a workspace or snapshot while mounted |
| `src/store.rs` | shared tree tables, file ids, directory layout, index walk; what is at a path of a tree (`Lower`, `entry_at`) |
| `src/state/mod.rs` | where a workspace's stored state is (`StateDir`: its id, state directory, registration and base pin; the files in it), ids, its base and branch files, paths below its root, whether an upper file matches a stored one |
| `src/state/markers.rs` | whiteouts and opaque markers, as an append-only log |
| `src/state/reads.rs` | the read log's format (`gitfold overlaps`) |
| `src/state/status.rs` | a workspace's uncommitted changes, ignored files, last change and line counts, read from its state directory (for `list`, `changes`, `overlaps` and `remove`) |
| `src/state/retire.rs` | the removal rules for a workspace or a snapshot's registration, shared by `remove` and the daemon |
| `src/state/fork.rs` | `branch --with-changes`: copying a workspace's state, registration and index into a new branch |
| `src/state/task.rs` | the task folder (the `.task` link): where it is stored, its `info/exclude` entry, copying it for forks |
| `src/state/restack.rs` | a restack under way in a workspace: the `restack` intent file (locked while it runs), and finishing or undoing one a crash interrupted |
| `src/state/seal.rs` | seals (snapshots, to users): a workspace's uncommitted work as a commit, kept by `refs/gitfold-seal/<id>/<n>`, numbered under a lock; the one its own `Stop` hook made last recorded in its state (`sealed`): whether an agent drives it |
| `src/state/lens.rs` | a workspace's layers: the `lens` file (each layer's seal, what it shows and how it updates, the layer a proposal stands in for; version 1 files read too), its lock (`lens.lock`), the view (view and hunk trees, mixed and blended paths, conflicts, notes) and which files are blended (`merge=union`) |
| `src/settings.rs` | every setting gitfold keeps in git config (per branch, per repository): typed reading and writing, copying for a fork |
| `src/fsutil.rs` | file helpers: atomic writes, times without following symlinks, copying a tree |
| `src/inode.rs` | inode encoding, path hash, constant timestamp |
| `src/refusal.rs` | why a command refused (typed, with its own exit code), carried over the control socket; every command's exit codes |
| `src/main.rs` | the commands (clap: their help; the arguments are a struct next to each command's code) and their dispatch, exit codes, `M_MMAP_THRESHOLD` |
| `src/inotify.rs` | an inotify watch on one directory (`watch`, `inbox --wait`) |
| `src/git/mod.rs` | thread-local gitoxide handles, tree, blob and ref access, merge bases, `merge-file`, `info/exclude` |
| `src/git/cmd.rs` | running git itself (`command`, `run`, `output`, `output_with_input`) |
| `src/git/attr.rs` | attributes as git reads them for a commit (`git check-attr --source`) |
| `src/git/index.rs` | streaming index v4 writer (with cache tree), stat info |
| `src/git/worktree.rs` | worktree registrations (snapshots, workspaces), base pin refs, git idle check |
| `src/git/index_edit.rs` | editing an index git wrote (v2, v3, v4): skip-worktree bits, stat data and ids of given paths, under git's `index.lock` protocol |
| `src/git/worktree_config.rs` | a linked worktree's own config: the excludes file for layers' new files (keeping the user's global ignore file); the push guard hooks, installed while HEAD is a lens commit |
| `src/git/hooks/` | the push guard's `pre-push` and the shell library the worktree's hooks share (embedded) |
| `src/git/stack.rs` | stacked branches' git side: the parent base (`refs/gitfold-stack/<branch>`, guessed by the fork point without it), restacking by plumbing, moving a branch and its stack ref in one transaction, whether a parent base landed on a target (merged, squashed, rebased) |
| `src/git/test_repo.rs` | unit tests' scratch repository |
| `src/git/lens.rs` | layers' git side: the view and hunk trees (blending; several layers' whole files merged), the layers (lens) and view commits, extracting commits made on the lens commit and moving the branch and HEAD in one transaction |
| `src/lock.rs` | the repository lock, held by a mounted daemon and by commands working on stored state unmounted |
| `src/git/promisor.rs` | partial clones: fetching what a tree load misses from the promisor remotes |
| `src/daemon/mod.rs` | mount point checks, unmount on SIGINT/SIGTERM/SIGHUP, fusermount |
| `src/procs.rs` | processes using a mount (from `/proc`) |
| `src/mount_table.rs` | the mount table: where a gitfold mount is (or a repository's), its source name (the repository) |
| `src/daemon/control.rs` | the daemon's control socket (`remove`, `branch --with-changes`, `prewarm`, `track-reads` and layer switches (`lens`) while mounted; stopping and resuming prewarm for `unmount`; its version) |
| `src/daemon/serve.rs` | the daemon in the foreground: mounting with FUSE, the control requests it answers, until unmounted |
| `src/prewarm.rs` | background walk of new snapshots and workspaces, in a walker process; the prewarm modes |
| `src/cli/mod.rs` | what commands share: the repository (`repo`), the workspace the current directory is in (`branch_or_current`; `workspace_at` from the mount, where `.git` turns git away), printing paths as git quotes them (`quote`, `write_path`), the repository lock or a daemon request (`local_or_daemon`) |
| `src/cli/branch.rs` | `branch` (and its forks), `prewarm`, `track-reads` |
| `src/cli/list.rs` | `list` (the stacks as a tree) and `changes` |
| `src/cli/remove.rs` | `remove` and `prune` |
| `src/cli/snapshot.rs` | `snapshot`: workspaces' work (seals, in the code) merged into one snapshot (and the `Stop` hook that takes one) |
| `src/cli/layer.rs` | `layer`: `add`, `remove`, the listing (the base line, the layers, their conflicts), `base` (handed to `stack`); the hooks that move live layers, moving a branch onto its base first, and deny edits of their files, offering a proposal |
| `src/cli/proposal.rs` | `propose`, `proposals`, `consent`, `accept` and `reject`: changes to a layer's files made in a fork of its workspace, checked against the workspaces that read them, written into the layer's workspace |
| `src/cli/stack/mod.rs` | `layer base` and `branch --base`: building a branch on another (its base), moving it onto its base's new commits or, its base taken away, onto the trunk, refusing to move pushed commits without `--yes`; the move the `UserPromptSubmit` hook does, re-parenting the children of a base that landed, `list`'s notes |
| `src/cli/stack/land.rs` | a stacked parent's target (its own parent, or the trunk), whether a parent base landed there (cached in `.git/gitfold/.stack-landings`) |
| `src/cli/hook.rs` | Claude Code hooks' input: the top-level strings of its JSON |
| `src/cli/claude/mod.rs` | Claude Code's hooks, which run in every repository: what they share |
| `src/cli/claude/worktree.rs` | its `WorktreeCreate`/`WorktreeRemove` hooks (`branch`/`remove --claude-hook`): its worktrees as workspaces of the repository's mount (mounted again at `gitfold.mountpoint` if needed); a git worktree where gitfold isn't used |
| `src/cli/claude/reads.rs` | its `PreToolUse` hook (`track-reads --claude-hook`): the files its Read tool and Bash commands read, sent to the mount's daemon |
| `src/shell.rs` | what a shell command reads: the files simple viewers and `<` name, from its syntax tree (`rable`) |
| `bin/gitfold`, `hooks/hooks.json`, `.claude-plugin/` | the Claude Code plugin (with `skills/`): its hooks, and the launcher they run, which uses a gitfold on `PATH` or downloads the release |
| `.github/workflows/release.yml` | on a version tag, the Linux release binaries (glibc 2.28) the launcher downloads |
| `src/cli/inbox/mod.rs` | `inbox` (and its Claude Code hook outputs) and `send` |
| `src/cli/inbox/message.rs` | the message format: listing, taking and parsing message files |
| `src/cli/inbox/watch.rs` | `watch`: waiting for a change in a task folder |
| `src/cli/overlaps.rs` | `gitfold overlaps`: what a workspace read or changed that changed elsewhere since |
| `src/cli/mount.rs` | `mount` (in the foreground the daemon, `daemon::serve`; else detached, with a log file), `unmount`, `version` |
| `src/bench/` | hidden `bench-read` and `bench-attrs` commands |
| `build.rs` | the build's version (crate version and a hash of its sources; the git commit for `--version`), for `gitfold version` |
| `bench/` | `bench.sh` (native vs mount), `probe.sh` (content reads), `writes.sh` (write workloads), `RESULTS.md` (measurement log) |
| `tests/workspace.sh` | end-to-end workspace test on a scratch clone |
| `tests/rebase.sh` | end-to-end test of rebases with git operations (conflicts, stash, amend, resets, bisect) |
| `tests/manage.sh` | end-to-end test of branch and its forks, list, changes, remove and prune |
| `tests/status.sh` | end-to-end test of what a workspace's status sees: deletions, half-way git operations, ignore rules, modes, nested repositories, the index's cache tree |
| `tests/mount.sh` | end-to-end test of mount and unmount, version, names that appear later, many packs, prewarm |
| `tests/agents.sh` | end-to-end test of overlaps, the task folder, inbox, send, watch, the Claude Code worktree hooks and the plugin's launcher |
| `tests/layers.sh` | end-to-end test of snapshots of work and layers in a workspace (refusals, git status and commits, surrender, remounts; live, frozen, commits-only and revision layers, re-adding, the listing; several layers on one file, merged or conflicting; blended files, the layers commit, extraction, the push guard; proposals, consent, accept and reject) |
| `tests/stacks.sh` | end-to-end test of bases (stacked branches): moves with no workspace, in an open workspace (hook and `layer base`) and an unmounted one, conflicts, uncommitted work in the way, pushed commits and `--yes`, `--frozen`, refusals that change nothing, the base taken away (also with `origin/main` as the trunk), `list`'s tree and notes; parents that landed (squash, rebase and merge commits, a tweaked squash, a stacked parent's parent, a merge of later parent commits, none when unmerged, an interrupted re-parent, a branch on the trunk's local branch) |
| `tests/fixture.sh` | the repository each isolated section of the four suites above starts from |
| `tests/run.sh` | runs suites at once (by default all but `workspace.sh`) and sums up; `SECTION` across them |
| `tests/promisor.sh` | end-to-end test of partial clones (fetches, stalls, backoff, remote choice) |
| `tests/race-modes.pl` | helper of `tests/workspace.sh`: lifted modes must stay invisible |
| `tests/cache-tree.pl` | helper of `tests/status.sh`: prints an index's cache tree, to compare with git's |
| `tests/lib.sh` | what the suites share: a directory per run, timeouts, sections, cleanup |

## Glossary

User-facing text uses three concepts only: **layer**, **base** and
**snapshot**. The code keeps some internal names (seal, lens, lensed,
restack, stack, surrender), marked *internal* below with the user-facing
term; they never appear in messages, help or the README.

- **Snapshot**: `@<rev>`, showing the tree of a commit, read-only.
- **Workspace**: `<branch>`, a writable view of a branch, made of a lower and an
  upper layer.
- **Base**: the commit whose tree is a workspace's lower layer: the branch tip,
  followed within a second of git moving the branch, once git is idle (see
  **rebase**, **idle**).
- **Rebase** (of a workspace, not `git rebase`): moving the base to the branch's
  new tip without changing what the workspace shows.
- **Stamp**: the mtime and ctime a lower file shows because a rebase or the
  unchanged-file cleanup dropped its upper copy, or a layer switch changed
  it (`times`).
- **Idle** (git in a workspace): HEAD on the branch, no operation stopped
  half-way (merge conflict, rebase, cherry-pick, revert, sequencer, bisect) in
  the worktree's admin directory, and no lock on the index, HEAD, the branch
  ref or `packed-refs` (`worktree::git_busy`). With files blended, HEAD is
  detached at the lens commit instead, and the rest is checked
  (`worktree::git_running`).
- **Lower**: the base tree, from the shared store (with layers shown, the
  view tree).
- **Upper**: every file that differs from the base, as real files in
  `.git/gitfold/<id>/upper`.
- **Seal** (*internal*; user-facing: a snapshot of a workspace's work): a
  commit of a workspace's uncommitted work: the tree it shows, ignored
  files left out, on its base, made with a fixed identity and date.
  `gitfold snapshot` and the `Stop` hook make one. Its refs outlive the
  workspace; the one the workspace's own `Stop` hook made last is recorded
  in its state (`sealed`), so a live layer never follows an earlier
  workspace's.
- **Layer**: another branch's work shown in a workspace, read-only: one of
  its workspace's seals, or a branch's or a revision's commit, as the
  changes since a pinned merge base. **Live** layers follow their branch
  (an agent's workspace: the seal its `Stop` hook made at the end of its
  last turn; a workspace without an agent: a seal taken at each of the
  reader's turn starts; or the tip), **frozen** ones stay as taken,
  revisions are **fixed**.
- **Lens** (*internal*; user-facing: the layers): the layers of a
  workspace, in the order added (also the name of the file storing them);
  **lensed** (*internal*; user-facing: "layers shown") means it has at
  least one.
- **View tree** (M): the hunk tree with the layers' other changes on top,
  whole files; where several layers change one, their 3-way merge against
  the base's version, conflict markers kept (the base's version if they
  can't merge at all); the lower layer while lensed.
- **Blended path** (hunk path): a file `merge=union` marks in the base's
  attributes, which the base has, a layer changed and none deletes: every
  layer's change merged into the base's version. Writable; upper holds it
  with the workspace's own lines on top.
- **Hunk tree** (H): the base's tree with only the blended paths blended.
- **Mixed path**: a path a layer changed (or deleted) that isn't blended:
  the layer's whole file (or the merge of several layers'), read-only,
  unless upper has it. It belongs to every layer whose change it shows.
- **Layers commit** (L; *internal*: the lens commit): HEAD, detached,
  while files are blended: tree H over the branch tip, parents the tip
  and the blended layers' seals, a `gitfold-lens` header. The **view commit** (V, tree M, first parent L)
  is what `refs/gitfold-lens/<id>` points at, keeping both.
- **Extraction**: copying the commits made on L onto the branch with only
  the workspace's changes (the line-set rule on blended paths), then
  moving HEAD to a new L with the last one's tree. A commit that changes a
  layer's lines **waits**.
- **Push guard**: the per-worktree `pre-push` hook that refuses to publish
  a layers commit while HEAD is one.
- **Proposal**: a fork of a layer's workspace (`<layer>-prop/<ws>`) where a
  workspace showing the layer edits the layer's files, shown in its place
  (the layer it stands in for kept with it, `instead` in the layers file,
  so the layer comes back as it was shown: live or frozen, all work or
  commits only); its changes are its seal against what it started from.
  **Consent**: the
  layer's work as now with those changes, checked in the view of each
  workspace showing the layer (merged with its own work, and tested).
- **Stacked branch** (*internal*: stack; user-facing: a branch with a
  base): a branch built on another, its **base** (`gitfold layer base`;
  the **parent** in the code, `gitfoldParent`); its **parent base** is the
  commit of the parent its own commits sit on (`refs/gitfold-stack/<branch>`).
- **Restack** (*internal*; user-facing: "moved onto its base",
  "updated"): copying a stacked branch's own commits onto its parent's
  tip, and moving the branch (and its workspace's files) there.
- **Switch**: changing what the layers show (a layer added, replaced,
  removed or moved to newer work), telling the kernel and git, and
  carrying the workspace's own lines in blended files over.
- **Surrender** (*internal*; user-facing: "git is off here"): turning git
  off in a workspace with layers whose index can't be
  edited, or whose HEAD left the lens commit's line: its `.git` names a
  nonexistent `/LAYERS SHOWN (...)` directory saying why.
- **Whiteout**: a recorded deleted lower path, so it stays hidden.
- **Opaque**: a directory recreated over a whiteout, or a lower directory
  copied into upper by a rename; its lower children stay hidden.
- **Copy-up**: copying a lower file to upper before its first modification.
- **Workspace id**: the branch name with `%`, `/` and a leading `.` escaped
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

- **Nothing incomplete enters the store.** A tree with anything missing below
  it (a partial clone) isn't stored, so a load after a fetch finds the gaps
  filled rather than a half-decoded tree. ([partial
  clones](#partial-clones-fetching-what-a-load-misses))
- **Index stat data equals `getattr`.** For every file the index records exactly
  the mtime, ctime, inode (low 32 bits), uid, gid, mode and size that `getattr`
  reports. Any change to attributes (timestamps, sizes after checkout filters,
  uid) must change `write_index` too, or `git status` reads every file. For a
  workspace this applies to the index gitfold writes when the registration is
  created (path-hash inodes, constant times); git maintains it afterwards, and
  rebases keep what git recorded true (stamps). Layers' paths are
  skip-worktree, which git doesn't compare; those a switch gives back carry
  the switch's stamp, which the index doesn't record yet, so git re-reads
  them once (a layer's changed files, not the tree). Blended paths' entries
  get the blend's id and size with zero times, so git reads each once
  rather than taking a size change as a change unread.
  ([rebases](#workspaces-follow-their-branch-invisible-rebases),
  [index](#a-synthesized-git-index-whose-stat-data-matches-the-filesystem))
- **Snapshot file attributes depend only on (blob, mode).** A snapshot file inode
  is shared by every snapshot that contains it, so nothing per snapshot or per
  path may show in its attributes: one timestamp, link count 1. Unchanged
  workspace files report the same attributes except the inode, the write bits
  (none on a layer's file), and their stamp once committed or switched.
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
  or adds files to or removes them from change; git ignores all three. The
  one visible change made outside a request, a layer switch, tells the kernel
  about every cached node, entry and listing it changes.
  ([rebases](#workspaces-follow-their-branch-invisible-rebases),
  [caching](#kernel-caching-does-the-heavy-lifting),
  [layers](#layers-other-workspaces-work-as-a-view-tree))
- **Names disappear only through `gitfold remove`**, so their entries can be
  cached long; otherwise listings only grow. A removal tells the kernel to drop
  the entry (`Notifier::inval_entry`) and its directory's listing. When a
  lookup adds a name to the root or a namespace directory, the daemon
  invalidates those directories' cached attributes and listings (`fs::names`);
  their mtime changes too, dropping a listing that was in flight. Their
  negative entries are cached only for dot names.
  ([caching](#kernel-caching-does-the-heavy-lifting))
- **Workspace local ids come from the path.** The index written with a new
  registration uses the same path hash (`inode::path_local`), so it matches
  `stat` after any remount; a rename or a hash collision only costs git a
  re-hash of that file.
  ([workspaces](#writable-workspaces-copy-on-write-inside-gitfold))
- **The workspace on-disk format is a compatibility contract**: the id escaping,
  the layout of `.git/gitfold/<id>/` (`base`, `branch`, `markers`, `times`,
  `reads`, `lens` and `lens-applied` record formats, `lens-git-off`,
  the `restack` intent, `restack-told`, `upper/`), task folders
  in `.git/gitfold/.tasks/<id>/` and the message format of their inboxes
  (README, "Messages"), the base-pinning ref `refs/gitfold/<id>`, the seal
  refs `refs/gitfold-seal/<id>/<n>`, the lens ref `refs/gitfold-lens/<id>`
  and the lens commit's `gitfold-lens 1` header (the push guard and a
  remount look for it), the stack refs `refs/gitfold-stack/<branch>` and
  the `branch.<b>.gitfoldParent`, `gitfoldRestack`,
  `gitfoldRestackBlocked`, `gitfoldLanding` and `gitfoldBaseOnTarget`
  settings, the
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
- *Workspace slots that stay the same across remounts:* would gain nothing for
  git (reported from a monorepo). Git compares only the inode's low 32 bits
  (with the default `core.checkStat`) and, unless built with `USE_STDEV`, not
  `st_dev`, so the index stat data already survives a remount, and the
  untracked cache too as long as the mount point stays the same.

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
data `getattr` reports, and with a cache tree. The index is version 4,
streamed.

**Why:** `git status` first `lstat`s every index entry and compares mtime,
ctime, inode, uid, gid and size with the index. When they match, the file is
clean without being read. With matching data, `git status` on spark reads 6
file contents out of 27.6k, and on Chromium it is as fast as native: 0.71 s vs
0.86 s on repeat runs, with the cache tree below (1.13 s before it).

Version 4 prefix-compresses paths: each entry stores how many bytes to drop from
the previous path plus the new suffix. Chromium's index is 38 MB instead of
70 MB. Tree tables keep tree order, and in that order a depth-first walk yields
paths already sorted as the index requires, so entries are written straight to
the output buffer: no collection, no sort, no per-entry allocation. Opening a
nearby Chromium snapshot went from 225 ms (version 2, collect and sort) to
122 ms, and to ~100 ms with the buffer preallocated from the previous index.

The cache tree (the `TREE` extension) records each directory's entry count and
tree id. `git status` compares the index with HEAD's tree, and skips every
directory whose recorded tree id matches HEAD's instead of comparing it entry
by entry. Git writes it on commit, not when it adds stat data, so without it a
fresh registration was compared entry by entry until the first commit. The same
walk writes it (a directory's node precedes its subdirectories'), from entry
counts kept per tree table since loading. Empty subtrees (git never creates
them) get nodes with 0 entries, as git's own `read-tree` writes them; on
Chromium all 46,362 nodes equal git's. A tree object not stored strictly in
git's order (out of order, or a name listed twice) gets an invalid node, which
git works out itself: the index entries written from it come sorted, so
they needn't make up that object (`read-tree` would still record
it; gitfold errs on the safe side). Warm `git status` of a Chromium snapshot:
947 ms without it, 722 ms with it (native 878 ms); the index grows 1.6 MiB
(4%), and opening takes no longer.

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

Git's untracked cache works on this: it trusts a directory's listing while its
mtime is unchanged and older than the index. A lower directory's names change
only by becoming an upper one (real times from then on); an invisible rebase
changes no names. Checked against uncached runs: files and directories added
and removed in lower and upper directories, `.gitignore` edits, commits,
`stash -u`, `clean`, a rebase. Lower directories, dated 2020-01-01, are never
too recent to trust ("racy"), unlike a fresh native checkout's in its first
second.

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
  Why that `stat` stays: the kernel invalidates a directory's attributes after
  every readdir (`fuse_invalidate_atime`, called from fs/fuse/readdir.c; it
  skips only read-only mounts, so `noatime` doesn't help). A cold directory
  therefore costs at least 2 READDIRPLUS requests (the second finds the end of
  the listing) and 1 GETATTR, which, dropped from the walker, only moves to the
  first `git status` (reported from a monorepo of over a million files). The kernel's faster
  paths are out of reach: FUSE passthrough needs `CAP_SYS_ADMIN` (`EPERM`
  otherwise) and FUSE-over-io_uring is off by default (`enable_uring=N`), so
  cutting the number of requests is the lever.

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
walker process (`gitfold prewarm-walk`) that walks it through the mount on a thread
pool. A separate process, because a daemon killed with SIGKILL while its own
threads wait on requests to its mount never finishes dying, and every access to
the mount hangs; a separate walker's requests just fail once the daemon is
gone. Walkers keep the mount busy, so the daemon stops its own before
unmounting on a signal, and `gitfold unmount` asks it to (over the control
socket), then stops any left below the mount point. At most `--prewarm-walks`
(2) walks run at once, the others queue: ten agents opening workspaces at once
started ten walks of half the CPUs each. Each walk uses 4 threads by default
(`--prewarm-threads`): its requests are served by the daemon, where more only
queue (reported: under 20 µs per request with up to 4 in flight, several times
that with 8-16). A walk already running or queued for the same path isn't added; a
removal drops its queued walk (and restarts it if refused); stopping for an
unmount stops every walk and starts none until the unmount, keeping them and
those of what opens meanwhile; if it fails, `gitfold unmount` lets them start
again. If `gitfold unmount` is interrupted in between, the daemon resumes by
itself once still mounted 60 s later (a lease, ended by a newer stop; the mount
point must still have the device it had, so neither a detached mount nor a
new one at the same path counts); a
walker killed from outside with SIGTERM (what `gitfold unmount` sends, for
daemons without the request) only drops the queue. A workspace's own mode, if
set (`branch.<name>.gitfoldPrewarm`, git config, so it stays with the branch),
overrides `--prewarm`, except `--prewarm none`, which turns prewarm off for the
whole mount. `gitfold prewarm <branch> <mode>` sets it and asks the daemon
(control socket) to apply it to the workspace if open: walk it now, or stop its
walk. `meta` (default) lists every directory and stats it afterwards
(see the read-write mount note under
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
- *Room for new packs.* gitoxide sizes the object database's pack index table
  once, at open (by default the packs on disk then, plus 10%, at least 32), and
  never grows it. On a mount that lived through days of fetches and repacks,
  lookups of objects in new packs then failed with "too few index slots"
  (reported from a monorepo, after its pack count had grown by half) until a remount. gitfold
  opens it with at least 16,384 slots (~110 bytes each, ~2 MB; slots of
  deleted packs are reused).
  Two open gitoxide issues in the same area, reported from the monorepo and not
  reproduced here:
  [#3179](https://github.com/GitoxideLabs/gitoxide/issues/3179) (a long-lived
  handle refreshing during a concurrent repack can panic or miss a pack) and
  [#2878](https://github.com/GitoxideLabs/gitoxide/issues/2878) (every lookup
  miss rescans the pack directory, even with `RefreshMode::Never`, which
  gitfold doesn't set anyway).
- *glibc.* After large frees, glibc raises its mmap threshold dynamically, after
  which large blob buffers come from heap arenas that are never returned to the
  OS (904 MB of heap after one pass over Chromium, vs 296-385 MB with a fixed
  threshold). gitfold pins
  `M_MMAP_THRESHOLD` to 128 KiB, calls `malloc_trim` after creating a snapshot,
  and an idle trimmer thread drops the content cache and trims once reads stop.

### Partial clones: fetching what a load misses

**What:** in a partial clone (`blob:none`, `tree:0`) gitoxide finds no lazy
fetch; a tree load (`Store::load_tree_local`) tries every child, collects what
is missing and fails with `git::Missing` listing all of it, without storing
anything incomplete. `Store::load_tree` goes through `promisor::load_tree`,
which fetches the list from the promisor remote and loads again, in rounds (a
fetched tree can reveal more). So every load fetches: snapshot and workspace
opens, a workspace's base following its branch, status for `remove` and `list`.
Opens load before taking their creation lock (a snapshot its commit tree, a
workspace its tip and stored base, after the cheap refusals), so a slow fetch
doesn't hold up other opens; a failed open leaves no state. Fetches take a lock
per root tree: opens of the same commit wait for one fetch, others fetch in
parallel.

**Why** (from a fork running it for weeks on a `blob:none` monorepo of over a
million files, reported): without it a workspace on a fetched branch failed to
open (`EIO`), as did `remove` of it; over a tenth of the head tree's blobs were
missing, and a plain
`git fetch` of a branch brought none of its changed files' blobs.

- **The fetch is git's own lazy fetch** (promisor-remote.c): `git -c
  fetch.negotiationAlgorithm=noop fetch <remote> --no-tags
  --no-write-fetch-head --recurse-submodules=no --filter=blob:none --stdin`,
  ids on stdin. `blob:none` whatever the clone's filter: a wanted tree comes
  with all its trees, a wanted blob despite the filter, so a `tree:0` clone
  opens in two rounds, not one per tree level. It updates no refs; each fetch
  writes its own pack.
- **Stopped on a stall, not on a total time:** a fork's first version capped a
  fetch at 300 s; a multi-GB download never finished, and restarted after every
  backoff. Now any stderr output (`--progress`) counts as progress; 120 s
  without it stops the fetch: SIGTERM to its process group (git removes its
  lock files), then SIGKILL, so ssh, index-pack or pack-objects don't linger
  holding the pipe. No prompts (`GIT_TERMINAL_PROMPT=0`), messages untranslated
  (`LC_ALL=C`, for the error's `fatal:` line), trace variables removed (a
  system-wide trace logged every fetch).
- **Backoff:** a tree whose fetch failed is refused for 30 s, checked before
  loading (the sync thread retries every second), lifted as soon as what it
  missed is there. The refusal is logged at debug level, the failure once. If
  every remote refused the objects (`not our ref`, or `does not allow request
  for unadvertised object`: the server doesn't serve objects by id, or they
  are gone), it is 10 min: retrying every 30 s only repeats it. A refused
  fetch's error names `uploadpack.allowAnySHA1InWant`.
- **The remotes** are git's: those with `promisor` (alone, or `= true`) or a
  `partialclonefilter`, in config file order (gitoxide's remote list is sorted
  by name, so the config sections are walked), the one `extensions.partialClone`
  names last (and only with `repositoryformatversion = 1`), as git moves it to
  the end. As in git, each is asked for what the ones before didn't bring
  (checked after one pack rescan, not one per object), so a wrong or
  unreachable first remote doesn't fail the open. Read once per mount.
- **Disk space:** no fetch starts with under 1 GiB free in the objects
  directory. A fetch is bounded by the commit's missing objects, but on a large
  repository that is tens of thousands; the fork's notes record a fetch without
  a want list pulling a 47 GB pack.
- **Finding what is missing doesn't rescan packs per miss.** gitoxide rescans
  the pack directory, under a store-wide lock, whenever an object isn't found,
  and a partial clone's walk misses once per absent blob: 60k missing blobs
  with 41 packs took 19.3 s to find. The walk uses handles with refresh off
  (`git::with_repo_no_refresh`), after one explicit refresh (`refresh_packs`,
  so a fetch's new pack is seen): 1.3 s. A missing object is one lookup
  (`try_find_header`, `try_find_object`). Every fetch adds a pack; the object
  database's slot table is sized for many ([memory hygiene](#memory-hygiene)).
- **The fetch's process group:** the daemon (no terminal) runs it in its own
  group, so a stall stops ssh and index-pack with it; from a terminal (`gitfold
  list`, `remove`) it stays in the foreground group, so ssh can ask for a
  passphrase; without one, nothing may ask either (`GIT_ASKPASS=true`,
  `SSH_ASKPASS_REQUIRE=never`: a desktop's ssh-askpass would block it until the
  stall limit). `--no-auto-maintenance`: a `gc --auto` repack would otherwise
  run inside the stall window, silently, and be killed. A fetch whose stderr a
  helper keeps open is still seen to end, and once git exits (normally or
  stopped), what it left in its group still holding stderr gets SIGTERM, so no
  reader thread is stranded (a helper that let go of stderr, like a credential
  cache daemon, is meant to outlive git). Git is reaped only after that (its exit is seen with `waitid`
  and `WNOWAIT`): until then its id, which names the group, can't be reused.
  At unmount bases move to their tip only if it is here: a hanging remote must
  not hold up the unmount (the next mount, or `remove`, catches up). An empty
  list is never fetched: `git fetch` would fall back to the remote's refspecs,
  everything (the fork's notes: 27M objects, 47 GB).
- **Signals reach what the daemon starts.** The daemon used to block
  SIGINT/SIGTERM/SIGHUP for a `sigwait` thread; the mask survives exec (Rust's
  `Command` doesn't reset it), so git and everything under it never received
  SIGTERM (blocked, pending) and only the SIGKILL 2 s later worked. A handler that writes to a pipe
  replaces it: handlers reset at exec.
**Rejected:** fetching per object as reads need it (one fetch per file; the
fork's notes: the store needs every blob's size anyway); a global fetch lock (a
slow fetch stalled every open, reads included); a total time limit (above).

### Forks: commands on a workspace's stored state

**What:** `gitfold branch --with-changes` (a fork) copies a workspace through
its state directory (`.git/gitfold/<id>/`: base, upper, markers, times) and its
registration's index, never through the mount, so it costs what the
workspace's changes weigh, not its tree. It follows a contract later commands
on the stored state can share:

- **Unmounted:** the command takes the repository lock (`lock`) and works
  on the state directory itself (`cli::local_or_daemon`).
- **Mounted:** it sends a control request; each feature has its own verb, so a
  daemon that doesn't know it refuses it ("older gitfold") instead of
  guessing. Requests that may take long (`Request::is_long`) are answered on
  threads of their own, so `stop-prewarm` never waits behind a copy.
- **In the daemon:** the branches are marked busy (`Workspaces::mark_busy`)
  while `creating` is held only to decide, so other workspaces open as usual.
  Readers (a fork's source) share a mark; removals, checks and a fork's target
  take it exclusively. Opening a branch waits for its readers (up to 30 s,
  never holding `creating`: a fork reading its source may need it) and is
  refused (`EBUSY`) while it is marked exclusively; readers arriving back to
  back can hold an open off until then. Unmount waits for the long requests in
  progress (`close_requests`, up to 60 s: one stuck in a fetch doesn't keep the
  daemon alive). An open workspace is copied under its `layers` write lock,
  which every change through the mount takes (shared or exclusive), so a copy
  never sees a write half-done; reads of that workspace wait too.
- **git's own writes** (the index, refs) don't go through the mount: a fork
  resolves the source's tip before and after its copy and refuses if it moved,
  and refuses a workspace with a git operation in progress, checked again
  under the lock (a `git stash` started meanwhile holds `index.lock` while it
  waits there). With `core.splitIndex`, the shared index files are linked too.
- **New state** is made in a dot directory (skipped by `list`) and renamed into
  place, and the fork's branch is created last (`update-ref` with an empty old
  value): nothing sees it half-made.

**Rejected:** a separate `fork` command (a fork is a branch created with
something more; one command keeps the naming and checks in one place); holding
`creating` across a whole fork (every other workspace's open waited for a copy
of gigabytes).

### Overlaps: reads from Claude Code's hook, writes from stored state

**What:** `gitfold overlaps` compares what a workspace touched with what a target
changed since the two diverged (`git diff <base>...<target>`, plus a target
workspace's uncommitted changes). Writes are the workspace's commits since it
diverged from the target and its stored uncommitted changes. Reads, with
tracking on, are what Claude Code's `PreToolUse` hook (`gitfold track-reads
--claude-hook`) reports: its Read tool's file, and the files simple viewers in
its Bash commands show (`shell::files_read`); the daemon appends each path to
`.git/gitfold/<id>/reads` (a `record-reads` control request; NUL-terminated,
once per path; a partial last path, from a crash or an append in progress, is
dropped). Before the tool runs: after it, the hook's `cwd` is the directory
the command left, and its own `cd` would count twice.

**Why:**

- **Off by default, per workspace:** `R` lines are for several agents on one
  repository; recording is cheap (an append per reported read, and the hook
  reports either way). The hook runs async before the tool: a read is
  recorded a few milliseconds after the call starts, so an `overlaps` at that
  very moment can miss it. The mount's `--track-reads` is the default; a branch's `gitfoldTrackReads`
  wins either way, like its prewarm mode, and `gitfold track-reads` applies
  it to an open workspace (a control request).
- **Not from the filesystem.** It sees a process and a file, not whether
  anyone looked at it. Tried and measured (2026-11-14):
  - All reads but searches': builds, test runs and `tar` read hundreds of
    files no one looked at (a `cargo build`: 59 in 0.01 s), and bulk readers
    are an open list (indexers, formatters, linters, `fd | xargs`).
  - Rates or counts per process: a slow build and a script reading 20 files
    at once defeat any threshold; `make` spreads a build over one `cc1` per
    source; process groups are shared by the harness, its searches and its
    commands.
  - Only listed readers (harnesses, viewers, editors): a listed program also
    reads in bulk in its own process (a harness indexing the tree, an IDE,
    `xargs wc -l`), and no rule about processes tells its deliberate reads
    from those.
  - The kernel's cache: the daemon sees only a file's first read, so a file
    a search read first is unseen when the agent reads it. Seeing every open
    (`--kernel-open`) costs a round trip each (~4.5 µs: seconds per
    whole-tree search on Chromium, [bench/RESULTS.md](bench/RESULTS.md),
    "opens"); evicting after a search costs the same on every later search;
    inotify `IN_ACCESS` doesn't say who read, fanotify tells only root.

  The harness alone knows what it showed the model. Other harnesses get `W`
  lines only, until they can report their reads.
- **Bash commands parsed, not only the Read tool:** Claude Code's hooks name
  no files a command read (only Read's `file_path`), so the hook parses the
  command, with `rable` (bash's grammar, validated against bash). Measured
  over the Claude Code transcripts of this project's development
  (`bench/transcript-reads.py`, 2026-11-14): of 4,182 deliberate reads,
  248 were Read tool calls and 3,934 files shown by viewers in Bash
  (`sed -n` most); the Grep tool ran 4 times against 3,769 Bash searches.
  The Read tool alone would miss 94% of them. Programs other than
  viewers reading repository files were a handful (`awk`, `diff`). Among
  commands with a viewer, the walk's constructs are common: loops 1,005,
  heredocs 866, `<` 826, `if` 570, `$(...)` 501, subshells and `<(...)` 190
  each. `pushd`/`popd`, `cd -P`/`-L` and `builtin cd` appeared in none
  (handled anyway, to the shell's rules).
- **One command for reads, writes and other workspaces:** a target that is a
  workspace brings its uncommitted changes, so comparing two workspaces is
  `--against <BRANCH>`, and every workspace `--against-workspaces`. A path
  matches through directories too (one workspace replaced `d` with a file,
  another changed `d/x`).
- **An `R` line is dropped where a `W` line names the same path and target**
  (unless `--reads`): a file the agent read and then changed is a `W`.
- **The default target is `origin/HEAD`** (else `main`), not the branch's
  upstream: after `git push -u` that is the branch's own remote branch, which
  its work is ahead of, so nothing would ever overlap. A target that is also a
  branch is diffed as `refs/heads/<name>` (git would prefer a tag of that
  name).

**Rejected:** separate commands for stale reads and for workspaces changing
the same paths (`stale`, `overlaps`), each a case of this one; reads from the
filesystem, in each of the forms above; parsing every Bash command for the
files it reads (pipelines, scripts and builds can't be told: simple viewers
only); a hand-written shell lexer for those (heredocs, `$((...))`, subshells
and loops each broke it; `rable` parses as bash does, for 3 crates); the Read
tool only (94% of reads missed, above); line-level conflicts in `overlaps`
(`git merge-tree` already says, for a merge someone actually asks about).

### The task folder: a virtual link to plain files

**What:** every workspace root shows a virtual symlink, `.task`
(`gitfold.taskDir`), to `.git/gitfold/.tasks/<id>/` (`gitfold.tasksDir/<id>/`),
served like the virtual `.git` file: a reserved local id (`TASKLINK_LOCAL`), no
node, not removable or renamable.

**Why:**

- **Outside the state directory,** so `remove` and branch deletion keep it:
  what a task produced besides code (specs, handoffs, notes) is worth more
  than its scratch files, and a branch of the same name finds it again.
- **Plain files, not part of the workspace view:** the view is cached by the
  kernel for a year (it changes only through the mount), so files written
  straight into the folder, by an orchestrator or in someone's notes, would
  stay unseen; behind a link they are ordinary files, seen at once, written at
  disk speed and watchable with inotify. Rebases, copy-up, whiteouts and the
  status walk never meet them.
- **Git doesn't follow symlinks:** `git clean -fdx` can't empty the folder
  (removing the link fails; it warns), and only the link needs an
  `info/exclude` entry. `git stash --all` fails on the link instead (a
  known limitation; `git stash -u` doesn't touch ignored files).
- **Turned off where the base tracks the name, or upper has it:** the link
  would hide those files; the name is configurable instead.

**Rejected:** a directory inside the workspace view, its paths mapped to the
folder (the first version): stale under the kernel's caching for files written
outside the mount, emptied by `git clean -fdx`, and entangled with rebases
and copy-up; a sandbox allowing writes only below the working directory was
its argument, but committing already needs writes to `.git`, where the folder
is by default. Keeping the folder in the state directory (lost with `remove`).

### Messages: files in the task folder, delivered by hooks

**What:** an inbox is a directory of the task folder; a message is one file
(headers, an empty line, a body), renamed into place by its sender and taken
by renaming it to a dot name, so each goes to one reader. `gitfold watch` is
the general primitive (inotify on a task folder's directory); `inbox` is the
convention built on it. Nothing goes through the daemon: it all works
unmounted, and anything can read or write the files.

**Why:**

- **Files, not a service:** orchestrators that already have mail (Gas Town's
  is in a SQL database) can ignore it, or write and read the same files
  without gitfold. The format is documented in the README as an interface.
- **Delivery by hooks, not polling:** an agent told to check its inbox spends
  tokens on every empty check. Claude Code hooks inject messages when a
  prompt is sent and after each tool call (as Gas Town does on
  `UserPromptSubmit`), and an `asyncRewake` hook on `Stop`, waiting on the
  inbox, wakes an idle session when one arrives; a lock (`.waiting`) keeps
  one such waiter per inbox, since every `Stop` starts one, and the waiter
  ends when its session (its parent) does. The `PostToolUse` hook ignores a
  subagent's tool calls: what it injects there wouldn't reach the session.
  Harnesses without hooks get `inbox --wait`, a blocking command.
- **Taken, then deleted once printed:** a reader that dies between leaves
  `.taken-<pid>-<name>`, which the next reader puts back; a watch whose
  directory is deleted or moved, or whose events overflowed, is made again
  and everything listed.
- **A generic watch:** directories other than the inbox (`reviews/`, a task
  folder where a handoff appears) serve uses we don't foresee; the inbox is a default with a
  name (`gitfold.inbox`), not a special case in the folder.

**Rejected:** typing messages into the agent's terminal (Gas Town's `gt
nudge` drives tmux): tied to tmux and to how each harness reads input;
`asyncRewake` does it inside the harness. Messages through the daemon's socket
(would need it mounted, and a protocol instead of files).

### Layers: other workspaces' work as a view tree

**What:** a workspace can show other branches' work, uncommitted
included, read-only, over its base and under its own changes. Each layer is
one **seal** of the other workspace (`state::seal`: a commit of the tree it
shows, ignored files left out, on its base, kept by
`refs/gitfold-seal/<id>/<n>`), or with `--commits` or without a workspace
the branch's tip, or a tag's or a commit's commit, with the merge base of
the two, pinned when the layer is added or moved. A **live** layer moves
at its agent's turn boundaries and when added again; a **frozen** one
(`--frozen`) and a revision (**fixed**) don't. The layers' changes
(`diff-tree <merge base> <seal>`, in the order added) applied to the
base's tree make the **view tree** (`state::lens::view`,
`git::lens::build_view`), loaded into the shared store and used as the
workspace's lower layer: whole files, a path several layers change merged
as `git merge-file` merges them against the base's version (conflict
markers kept, and listed), except files marked `merge=union`, which are
**blended** (every layer's change merged into the base's version, lines
both sides add kept both) and stay writable. While any file is blended,
HEAD is detached at a **layers commit** (the lens commit, in the code)
holding the blends, and commits made on it are **extracted** onto the
branch with only the workspace's changes. The layers are stored in `lens`
in the state directory; `gitfold layer add`/`remove` write it and ask the
daemon to switch (a `lens` control request). `gitfold snapshot` is the
read-only form: seals merged with `git merge-tree` into a commit opened as
a snapshot, with no change to any workspace.

**Why:**

- **What was applied, apart from what is stored:** `lens` is what the user
  (or a hook) asked for; `lens-applied` is what the daemon last applied: the
  layers file then, the paths git was told to skip, the blended paths, the
  lens commit. A workspace opens showing what was applied, so the view and
  the index, excludes and HEAD always describe the same layers, and the sync
  thread then switches to the stored layers if they differ (changed while
  the workspace was closed, or a switch that failed half-way). A switch the
  daemon refuses puts the stored layers back. Without the split, a layer
  added while unmounted showed at the next mount with git never told: `git
  commit -a` committed its files. Switches and the sync thread (extraction,
  rebases) take one lock per workspace, so they never interleave.
- **The view tree as the lower layer:** `Workspace::lower` already reads one
  stored tree, so lookups, listings, `getattr` and reads needed no new code
  (only `mixed` on lower files, for their write bits). Upper still wins over
  it, so the workspace's own files are never touched by a layer.
- **Seals, not live files:** a seal is immutable, so the other agent's
  writes change nothing a reader has cached, and need no invalidation. Made
  with a fixed identity and date, the same work gives the same commit, so
  sealing an unchanged workspace makes nothing new and moves no layer.
- **A pinned merge base:** a layer is what its workspace has that this one
  doesn't (its commits since they diverged, plus its uncommitted work). The
  merge base is pinned with the seal, so what a layer shows changes only at a
  switch, not when either branch moves.
- **Switches tell the kernel exactly what changed** (`Workspace::switch`):
  the old and new view trees are diffed; paths upper shadows are skipped; for
  each change, the highest path whose type changed (appeared, vanished,
  file ↔ directory) is an entry change in its directory (`inval_entry`, the
  directory's listing, its nodes orphaned), else the file's node, if the
  kernel holds it, loses its attributes and pages (`inval_inode`). The
  notifications go out after `layers` is released: `inval_entry` waits for
  lookups in the directory, which wait for `layers`. Changed files get the
  switch's time as their stamp, so incremental builds rebuild them, and
  everything else keeps its time. The cost is the layers' changes, not the
  tree. A switch is refused while git is busy in the workspace: it changes
  what the index describes. With a lens commit, it first extracts what was
  committed on it (`sync_lens`), and is refused while a commit waits.
- **Writes refused:** a mixed path's file has no write bits, and copy-up
  (writes, truncation, `chmod`), removal, renames from or onto it (or of a
  directory holding one) and creating a path a layer deleted are `EACCES`.
  Any of them would copy the layer's content into upper, where it becomes
  this workspace's own work, hidden from later switches and committed with
  its branch. `errno` carries no text, so Claude Code's edit tools are
  denied first by a `PreToolUse` hook giving the reason and the ways out
  (propose a change, see "Proposals"; ask the layer's agent with `gitfold
  send`; `gitfold layer remove` it), naming every layer whose change the
  file shows.
- **Several layers on one path merge; they don't stack.** Whoever added a
  layer last used to win a path, silently hiding the other's change. Now
  a path several layers change shows the fold of their entries, each
  merged against the base's entry as `git merge-file` does (an entry
  unchanged from the base, or the same on both sides, merges trivially).
  A text conflict keeps git's markers, labelled with the layers' names,
  so the reader sees both sides; anything that can't be merged (binary, a
  deletion against a change, a type change) shows the base's entry. Both
  are mixed paths (read-only, owned by every layer involved) and are
  reported: on the switch's reply, at the next turn, and in `gitfold
  layer`'s listing. The base's entry is the ancestor, not each layer's
  merge base: a layer older than the base then reads as reverting the
  base's newer change, as a single layer's whole file already does.
- **Git sees only the workspace's own work: skip-worktree and per-worktree
  excludes** (`tell_git`). The layers' paths the base tracks are marked
  skip-worktree in the worktree's own index, by an index editor
  (`git::index_edit`, a one-pass transcoder copying untouched entries' bytes,
  under git's `index.lock` protocol: ~50 ms for 5,000 paths in a 300k-entry
  v4 index; gix-index writes only v2/v3 and drops unknown extensions). Git
  then neither shows nor commits them and keeps the base's blob, through
  `status`, `diff`, `add -A`, `commit -a`, `stash`, `restore` and `reset`.
  Files layers add are excluded by a per-worktree `core.excludesFile`
  (`git::worktree_config`, `extensions.worktreeConfig` at format version 0):
  `info/exclude` is shared by every worktree, and the per-worktree setting
  replaces the user's global ignore file, so the generated file starts with
  its patterns. Removing the layers clears both.
- **Blended files, for files several agents append to** (a registry, a
  manifest): whole files would show either theirs or mine. A file is
  blended if `git check-attr --source=<base> merge` says `union`
  (the base's `.gitattributes`, or `info/attributes`), the base has it and
  no layer deletes it. Each layer's change is folded in as
  `merge3(base = merge base, ours = the blend so far, theirs = seal)`; two
  appends at one place conflict in every 3-way merge, so where both sides
  only add lines both are kept, ours first (what `union` does, but only there).
  Any other conflict leaves that layer's change out, as a note on the
  switch's reply, as does a whole-file layer version hiding a change the
  base made since the merge base. The base's tree with only the blends is
  the **hunk tree** H; the view tree M is H with the whole files on top.
  Blended files have no `mixed` entry, so they are writable: a copy-up
  takes the blend, and the workspace's own lines go on top.
- **HEAD detached at a layers commit L** (`tell_git`): the index alone can't
  split a file into theirs and mine; only a HEAD whose tree has their lines
  can. L has tree H (over the branch tip, if commits moved it past the
  base), parents the tip and the blended layers' seals, and a
  `gitfold-lens 1` header, so hooks and a remount recognise it without a
  list of ids; its subject says `commits go to <branch>; never push this
  commit`. The index has the blends' ids and sizes (zero times: git reads
  each file once), not skip-worktree, so `git diff` shows only the
  workspace's lines. Detached rather than symbolic to a per-worktree ref:
  measured on git 2.49, `gc` from another worktree pruned commits held only
  by other worktrees' `refs/worktree/*` (and a HEAD symbolic to one), but
  not one held by a detached HEAD. `refs/gitfold-lens/<id>` (common) pins
  the view commit V (tree M, parents L and every shown seal), keeping M, L
  and the seals. Turning the last blend off writes HEAD back as a symbolic
  ref to the branch and drops the ref and the guard; the files keep the
  workspace's lines, now changes against the branch's version.
- **Extraction on the sync thread** (`sync_lens`, `git::lens::extract`):
  when HEAD has moved from L by a first-parent chain of commits and git is
  otherwise idle (`git_running`), each commit C is copied onto the branch
  tip T as `merge(base = its parent P, ours = T, theirs = C)` with C's
  author, committer and message, except on blended paths, where P has the
  layers' lines and T doesn't: there the line-set rule decides. C may not
  remove or change a layer's line (one in P but not in T): it depends on
  that layer, and it and every later commit wait (logged), with nothing
  moved. Otherwise a 3-way merge of P, T and C, dropping the layers' lines;
  if that conflicts (a line appended after a layer's, its context), T's
  lines less those C removed, then C's new lines (a registry's lines are
  unique, their order doesn't matter). Then one `update-ref` transaction
  (`git::lens::advance`) moves the branch to the last copy and HEAD to a
  new L' with the agent's last commit's tree: what the index describes
  doesn't change, so git sees nothing happen. Either both move or, if git
  moved either meanwhile, neither, retried on the next pass. Merge commits
  made on L are flattened. A plain merge with `merge=union` would put the
  layers' lines into the commit, and one without it conflicts on every
  append.
- **The push guard** (`git::worktree_config::install_push_guard`): `git
  push` from a detached HEAD fails by itself, but `git push origin
  HEAD:main` would publish L, other agents' uncommitted work. While HEAD is
  a lens commit, the worktree's own `core.hooksPath` is
  `<admin>/gitfold-hooks`, whose `pre-push` refuses any pushed range (less
  what the remotes have) holding a commit with the header, then runs the
  user's `pre-push`; every other hook name is a shim running the user's (the
  path found from config without the worktree scope, at run time).
  `--no-verify` bypasses it.
- **Own lines carried over on a switch** (`carry_over`): when the blend
  under a file the workspace has written changes, upper becomes
  `merge-file(base = old blend, ours = upper, theirs = new blend)`, or, if
  that conflicts, the line-set rule (new blend less the lines upper
  removed, then upper's new lines). A conflicting file the rule can't take
  (not text) refuses the whole switch before anything changes (`your
  changes to it depend on lines the layers would no longer show`). The
  rewritten files are invalidated with the rest of the switch.
- **Surrender as the fallback:** where the index can't be edited (sparse
  checkout, whose re-application rewrites skip-worktree bits; a split or
  sparse index; an unknown required extension; the index locked), or HEAD
  left L's line while files are blended (`git switch`, `reset`, `rebase`,
  an amend of L itself: the index and files hold the layers' lines, which
  would be committed on the branch), the layers
  still show, but the virtual `.git` (now swappable) names a directory that
  doesn't exist: `gitdir: /LAYERS SHOWN (a+b): git is off here (<why>). Run
  gitfold layer remove --all to use git`. Every git command then fails
  with that text (exit 128), the registration and branch protection stay, and the `.git`
  file can't be written, so `git worktree repair` can't undo it. Why is kept
  in `lens-git-off`, so it survives remounts. gitfold's own commands find
  the workspace from the mount instead (`cli::workspace_at`).
- **The base isn't rebased while lensed** (`sync` waits): a rebase drops
  upper files identical to the new base and copies up files it changes, and
  with a view tree as lower either could turn a layer's content into this
  workspace's own. Commits still move the branch (with a lens commit, by
  extraction, L then built over the tip rather than the base); the base
  catches up once no layer shows.
- **Moves at turn boundaries, by hooks:** a workspace shown as a layer is
  sealed when its Claude Code session stops (`snapshot --claude-hook stop`), and
  a reader's live layers move to their latest work (below; for commits, the
  branch tips) when its next prompt is sent (`layer --claude-hook
  user-prompt-submit`), which tells the agent which changed, and records
  when each moved (the listing's `live, last HH:MM`). Neither agent sees
  the other's half-done turn, and nothing the reader read changes
  mid-turn. The agent's own `gitfold layer add` switches at once: a
  boundary it chose. Frozen layers and revisions never move: a reader
  that wants a fixed point says so, rather than turning a layer off.
- **A live layer follows an agent's turns, or a person's work as it is:**
  a live layer of an agent's workspace moves to its work as it was at the
  end of the agent's last turn; of a workspace without an agent, to its
  work as it is at the reader's turn start. Which one a workspace is, its
  own `Stop` hook tells: each run records itself in the workspace's state
  directory (`sealed`: `<seal> stop`, or `- stop` when no layer showed it,
  so no seal was made). A workspace with that record has an agent, and its
  layers follow only the seal recorded there (none: they stay as they
  are), never one taken half-way through its turn; `layer add` takes that
  one too, or one now if there is none (`--frozen`: one now). Any other
  workspace (no record, or one an earlier build wrote for any seal) has no
  turns: a seal is taken at each reader's turn start (and on `layer add`),
  so it follows every time, not once. A proposal accepted into an agent's
  workspace is recorded the same way (the owner's own boundary), so
  followers see it at their next turn. The record is in the workspace's
  state directory, not the highest-numbered ref: seal refs outlive the
  workspace (layers name them, and the id comes from the branch name), so
  after `gitfold remove` and a new workspace of that branch the newest ref
  is an earlier workspace's work. The state directory goes with the
  workspace, so its record can't be stale; the seal it names is kept from
  pruning. Rejected: following the newest seal whoever made it (a reader's
  turn start would then seal an agent's workspace half-way through its
  turn); deleting a workspace's seal refs on `remove` (refs that layers
  name must stay, so the newest could still be old); numbering
  incarnations in the ref names (a new ref layout, for the same result).
- **Seals of one workspace at once** (its `Stop` hook and readers' turn
  starts) are numbered, given their ref and recorded under an `flock` on
  `sealed.lock` in its state directory, so none takes another's number
  (`update-ref` creating the ref would fail). In a reader's turn start, a
  layer that can't move (its workspace's seal failing) is left as it is,
  and the others still move.
- **Read-modify-write of the layers under a lock:** `layer add`/`remove`,
  the hook moving live layers, `propose`, and `accept`/`reject` putting a
  layer back all read the layers file, change it and write it whole;
  without a lock two at once lose one's change. Each holds an exclusive
  `flock` on `lens.lock` in the workspace's state directory across the
  read, the change and the daemon's switch (the daemon only reads the
  file and takes its own per-workspace lock, so this can't deadlock). The
  repository lock (`lock.rs`) can't serve: a mounted daemon holds it for
  its life.
- **The layers file's format:** `version 2`, one line per layer: `layer
  <name> TAB <work|commits-only|commits> TAB <live|frozen|fixed> TAB <when
  it last moved, seconds since the epoch> TAB <commit> TAB <merge base>`,
  a proposal's line followed by `instead` and the same fields for the layer
  it stands in for. Version 1 files (`layer <name> TAB <on|off> TAB
  <commit> TAB <merge base>`, from builds before `--frozen`/`--commits`)
  are read, also inside `lens-applied`: `on` is live, all work, its time
  the file's; `off` (a hidden layer, which no longer exists) is dropped.
  The README names these "the layers files" and leaves the formats here.

**Rejected:**

- Other index tricks, tested on git 2.49 before building: the layers'
  versions staged in the index (`status` shows them as staged, and even a
  plain `git commit` commits them); `assume-unchanged` (a performance
  promise: `stash`, `restore` and `reset --hard` revert the layers' files,
  and `stash` lost an edit to a file both sides changed); an index per lens
  swapped on toggle (what it adds over skip-worktree is staged entries, which
  it loses or leaks into the other view).
- `git update-index --[no-]skip-worktree --stdin` for the toggles (~115 ms
  for 500k entries; kept as `set_skip_worktree_by_git`): the editor is
  faster, and can also set an entry's stat data and id.
- `info/exclude` for the layers' new files: shared by every worktree.
- A deliberately broken index, or no `.git`, as the surrender: git says
  "index file corrupt" and the usual repairs undo it; without `.git`, git
  finds the enclosing repository and succeeds.
- Layers following the other workspace live, every write at once: rewriting
  the reader's index while its git may run, and a view changing in the
  middle of its turn.
- Rebasing the base under the layers: see above; it waits instead.
- HEAD symbolic to a per-worktree `refs/worktree/lens`, or a
  `refs/worktree/*` ref keeping L: `gc` from another worktree prunes them.
- Extraction as a plain merge, with or without `merge=union`: it commits
  the layers' lines, or conflicts on every append.

### Proposals: a layer's files changed in a fork, with the consent of the affected

**What:** a workspace that needs a layer's file changed (a contract a root
agent wrote, shown read-only in each child) proposes the change with
`gitfold propose <path>`: the layer's workspace is forked (`branch
--with-changes`, through the daemon when mounted) into `<layer>-prop/<ws>`,
recorded in config (`gitfoldProposal`, `gitfoldProposedBy`) with what it
started from (its first seal) kept by `refs/gitfold-proposal/<id>`, and put
in the layer's place in the proposer's stack. The proposer edits the files
in the proposal's workspace, where they are its own. Its changes are its
seal against that start. `gitfold consent` builds what accepting gives (the
layer's work as now with those changes) and, for each workspace showing the
layer or a proposal to change it, checks that its own work merges with it
and runs a test in its view with it; `gitfold accept`, given a current
passed consent, writes the changed files into the layer's workspace, puts
the layer back in place of the proposal, tells the followers, and removes
the proposal (workspace, branch, start ref, seals no stack names).

**Why:**

- **Edits where the files are writable:** the alternative, routing writes
  to a layer's path into a fork from inside the reader's workspace, would
  need a second upper per workspace and a write path that changes meaning
  with the stack. A fork is a workspace like any other: editors, builds
  and git work there unchanged, and the deny hook only has to say where
  it is (it offers `gitfold propose <path>` first, and for a file the
  proposal already shows, its path in the proposal).
- **A copy of the layer's work, uncommitted included:** contracts are
  written in a workspace and rarely committed while children build on them;
  forking the branch tip would propose against a version nobody sees.
- **Its changes against its own start, not the layer as now:** the layer
  goes on changing; a diff against its current work would show the root's
  later edits as reverted by the proposal. `accept` refuses (exit 4) if the
  layer changed any proposed file since the start: writing the proposal's
  version would undo that silently. Then `reject`, and propose again.
- **Consent checks what accept writes:** the proposal forked the layer's
  work as it was; accept writes its changes onto the layer's work as it
  is. So the checked commit is the latter (the layer's seal now, its tree
  edited with the proposal's changes), and the record holds both the
  proposal's and the layer's seal: a later edit to either makes it stale,
  and accept refuses a stale or failed one (`--force` overrides). Accept
  also writes exactly the consented seal: a proposal edited since (or
  half-way through an edit) differs from it and is refused, and one
  edited while it is being accepted is kept with only those later edits.
- **Each follower's view, not its seal alone:** the test runs in a
  snapshot of what the workspace would show: its view tree rebuilt with
  the accepted commit in place of the layer (and of any proposal to change
  it), its other layers as they are, its own work on top (blended files
  then show its own version). `merges` is `git merge-tree` of its own work
  with the accepted commit: what committing both would meet. Snapshot
  registrations are removed after each test; the test gets a writable
  scratch directory (the snapshot is read-only), a timeout (its process
  group killed) and only the end of its output is read.
- **Every follower is checked, reads only annotate:** read records hold
  what an agent's tools opened, not what its build read (a header compiled
  against, a file added by the proposal), and a log can be stale, so `no`
  is weak evidence. They decide only `--only-readers` (skip definite
  non-readers) and who isn't told (told: everyone but definite
  non-readers). Nothing checked is `unchecked`, not a pass.
- **Who may accept:** the workspace the command runs in (from the current
  directory), the layer's; `-w` names one only from outside every
  workspace. Not a security boundary (any agent can `cd`), but an agent in
  its own workspace can't accept for another by mistake.
- **Retiring:** removal is forced (the proposal's changes are taken); a
  workspace still in use stays as a plain branch (no longer a proposal).
  Seals a pending switch still names stay until a later retire sweeps the
  refs of proposals that are gone.
- **Accepting is writing, not merging:** the layer's workspace gets the
  files as uncommitted changes through the mount (as its agent would write
  them, so the kernel and git are told as for any write), or offline into
  upper and the markers under the repository lock; the root commits as
  usual. Workspaces showing the layer move at their next turn by the
  existing hook (the layer is sealed at once, recorded as its agent's
  boundary if it has one); the proposer is switched back at once, its
  stack stored first, so a switch refused now happens when it can. The
  proposer's own proposal has no agent of its own (its edits are made
  from the proposer's session, whose `Stop` records only its own
  workspace), so the proposer's `UserPromptSubmit` hook seals it each turn.

**Rejected:**

- A write policy `propose` on a layer (design §1.3), redirecting writes
  in place: see above.
- Checking only the workspaces whose read records hold a changed file:
  those records miss what builds read, so a proposal could pass having
  checked nobody (kept as `--only-readers`).
- Mixing the follower's seal with the proposal's work: that tests the
  layer as it was when proposed, not what accept writes, and leaves out
  the follower's other layers.
- Merging the proposal into the layer's workspace on accept: the start
  makes a 3-way merge possible, but a merged contract nobody tested is
  what consent exists to avoid; the refusal sends it back instead.

### Stacked branches: restacks computed by plumbing, moved like a checkout

**What:** a branch b can be built on a parent branch a, its **base**
(`gitfold layer base a`, `gitfold branch b --base a`; "parent" in the
code, "move onto its base" in messages). The edge is a's name in
`branch.<b>.gitfoldParent` and b's **parent base** B, the commit of a its
own commits sit on, in `refs/gitfold-stack/<b>`; b's own commits are
`B..b`. When a's tip A moves past B, b is **restacked**
(`git::stack::restack`): each commit C of `B..b`, oldest first, becomes
`merge_trees(base = C^, ours = the copy so far, theirs = C)` with C's
author, committer and message (`git::lens::copy_commit`, shared with
extraction). A commit made empty is dropped; a merge in `B..b` is refused;
a conflict moves nothing and is recorded (`branch.<b>.gitfoldRestackBlocked`,
with A, so it is told once per parent tip). Then b moves by where it
lives (`cli::stack`):

1. **No workspace:** one `update-ref --stdin` transaction, b `C → C'` and
   the stack ref `B → A`, each compared-and-swapped. Refused if a git
   worktree has b checked out.
2. **A mounted workspace:** run by the CLI (`gitfold layer base a`, or the
   `UserPromptSubmit` hook for the session's workspace), not the daemon:
   write the intent (`state::restack`), `git read-tree -m -u C C'` in the
   workspace through the mount, the same transaction, delete the intent.
   Within a second the sync thread sees tip C' ≠ base C and does its
   ordinary invisible rebase, which finds the files git wrote in upper
   identical to C' and drops them (keeping their new times).
3. **An unmounted workspace without uncommitted changes**, under the
   repository lock: the intent, the index (`read-tree -m -i C C'`), the
   base moved to C' visibly with the changed files stamped now
   (`fs::workspace::restack_closed`), the transaction. With uncommitted
   changes it waits for the mount.

Restacks happen on demand or at the branch's agent's turn boundary,
never from the daemon's poll; `branch.<b>.gitfoldRestack=manual` leaves
them to `layer base --frozen`, run again. `layer base` run again restacks
now; `layer base <TRUNK>` takes b's base away, `B..b` replayed onto the
trunk's tip ("the trunk" is internal: users see "the main branch", named
as they know it, `main` for `origin/main`; `land::trunk_tip` takes the
newer of the local branch and the configured trunk, the configured one if
they diverged). A move onto a new base is made before the edge is
written (`rebase_onto`): a refusal (pushed commits, layers shown,
checked first) or a move that doesn't happen (a conflict, work in the way)
leaves the old edge, frozen setting and block record as they were. A stack is consistent bottom-up without a lock across
workspaces: if p2 was restacked and p3 hasn't been yet, p3's stored base is
p2's old tip, so its next restack replays exactly its own commits.

**Why:**

- **The parent base is stored, not derived.** With only `merge-base(a,
  b)`, a rewritten parent (amended, rebased, force-pushed) makes the next
  restack replay a's old commits, which conflicts on a's own rewritten
  lines (the design's experiments e4d and e4e). Graphite stores the same
  pair.
- **A ref, not config, for B:** the transaction moves b and B together,
  so a crash can't leave b restacked with B stale; config can't join a ref
  transaction. B is an ancestor of b, so the ref adds no `gc` risk. A
  missing or non-ancestor ref (`git branch -m` doesn't rename it) falls
  back to git's `--fork-point` rule, then the merge base.
- **Config for the rest:** git renames and deletes it with the branch, a
  fork copies it (`settings::copy_branch`: a fork of p2 is built on p1
  too), it works for branches without a workspace, and an orchestrator can
  set it without gitfold. The blocked record lives there too, so it works
  the same with or without a workspace.
- **Computed first, then moved:** the computation needs no worktree, index
  or lock and changes nothing, so a conflict leaves no half-done state in
  an agent's worktree (`git rebase` would leave `rebase-merge` there
  mid-task). The only refusal left when moving is uncommitted work in the
  way, which also changes nothing.
- **`read-tree -m -u`, then the refs**, for a mounted workspace: git's
  two-tree checkout keeps unstaged, staged and untracked work and refuses,
  changing nothing, when any of it is on a path the restack changes
  (experiment e8). Through the mount, its writes are ordinary writes: the
  kernel sees them, builds see new times, and the sync thread's existing
  rebase cleans up. No new filesystem code; the cost is copying up the
  restack's changed files once.
- **The intent file, locked while it runs:** a crash between the read-tree
  and the transaction would leave the index at C' and the branch at C.
  The intent (`<open|closed> C C' A <old stack ref>`) is written and
  `flock`ed under another name and hard-linked into place, so nobody sees
  it unlocked, and stays locked by the command doing it, which deletes it
  when done or when it stops before anything moved. Anyone else (the sync
  thread every pass, the next `layer base` or hook) recovers one nobody
  holds: tip at C' → set the stack ref if it isn't yet; tip at C and the
  index at C' on the changed paths → move the refs (closed: only if the
  base moved, else put the index back); index at C → forget it; the branch
  moved on and the index describes it → forget it; else leave it and
  report, and `gitfold layer base <TRUNK>` forgets it (the base taken
  away, nothing moved). While one is there, neither
  the base nor the layers move (layer switches are refused). A crash in the
  middle of the read-tree, which writes files before the index, leaves the
  files it wrote as uncommitted changes against C, as an interrupted `git
  checkout` does: nothing is lost, and the read-tree had refused to touch
  files with changes of their own.
- **With the intent held, the refs are read again:** a restack computed
  before another one of the same branch finished (the hook and `layer base`
  at once) would otherwise run its read-tree against the other's result,
  fail its compare-and-swap and put the files back under the new tip. The
  stack ref alone moves only with the branch verified where it was seen.
- **A closed workspace moves visibly**, unlike a rebase: there is no view
  to keep, and the restack's changes are meant to show. Only without
  uncommitted changes, so upper holds nothing but ignored files; one on a
  changed path (or a whiteout, or an opaque directory above one) waits.
- **Refused while layers are shown (v1):** a restack moves the base, which
  waits while lensed ("The base isn't rebased while lensed"). `layer base`
  refuses before anything changes, and every command gitfold advises for
  such a branch (`list`'s notes, the hook, `layer base`'s lines; `advice`)
  starts with `gitfold layer remove --all`, so following advice never
  meets this refusal.
- **Never pushes:** `layer base` prints `git push --force-with-lease=b:<tip>
  origin b:b` with the remote-tracking tip as the lease, and `list` says
  the same (`push with: ...`) while that tip isn't in the branch.
- **Pushed commits only with `--yes`:** `layer base` says what it moves
  first, and refuses (exit 4) if any of `B..b` is reachable from b's
  upstream (else `origin/<b>`): a rewrite of published history is asked
  for, not done in passing. The hook's moves, which follow what was
  already asked, aren't refused; they say the branch needs a force-push.

**Rejected:**

- `git rebase --update-refs` from the top of the stack: it skips branches
  checked out in other worktrees, tearing the stack (e2a); `git rebase` of a
  branch checked out elsewhere fails (e1).
- `update-ref` alone (or `git replay`): moves the branch, not its files; the
  sync thread would then keep the old view, the restack showing as a staged
  reverse change (e4).
- `git reset --keep C'` in the workspace: a staged new file became
  untracked (e7a).
- Moving the ref first and a "visible base move" in the daemon: new index
  work (adding and removing entries), and the unsafe order: ref moved,
  read-tree refused, the workspace left `MM` (e7c).
- The lens file for the edge: per workspace and only for open ones; an edge
  has no seal or view and must work for closed branches.
- Replaying `merge-base(a, b)..b` and dropping commits that become empty
  (jj-style): conflicts after a squash merge (e4d).

### A stacked branch's parent that landed: its base, found by content

**What:** at `layer base` and at the child's turn boundary, before a
restack, gitfold asks whether the child's parent base B landed on the
parent's **target**: the parent's own parent if it is stacked (stacked
pull requests merged into each other), else the **trunk**
(`gitfold.trunk`, else the branch `origin/HEAD` names, else `main` or
`master`). `git::stack::landing`, cheapest first:

1. B in the target's history, brought in by a merge (of B, or of commits
   the parent got later): the first commit of the target's first-parent
   line that has B, found by bisection (once one has it, every later one
   does); landed there unless that is B's own child on the line. On the
   line itself, B landed by a fast-forward if it wasn't in the target when
   recorded (see the guards below).
2. Else, if B has changes since it and the target diverged (MB): the
   first commit S of `rev-list --ancestry-path MB..target -- <paths B
   changed>` (the paths on its standard input) where `merge_trees(MB, S,
   B)` is S's tree, i.e. merging B there changes nothing: a squash, the
   last commit of a rebase merge. The newest 2,000 such commits at most,
   logged when that cuts the walk short.

The trunk includes the local branch tracking it (`main` for
`origin/main`): a branch stacked on it would otherwise be restacked on
every pull and its own commits, rewritten by `pull --rebase`, taken for
landed.

If so, the child is **re-parented** (`cli::stack::reparent`): `B..child`
replayed onto S like any restack (the same intent, read-tree and
transaction, its stack ref moved to S), then `gitfoldParent` set to the
target; or, the target being the trunk, its base forgotten altogether
(`built_on`: no parent, no stack ref), as a branch never stacked, so
`list` doesn't show it under the trunk and its listing says a plain
`Base: main`. The hook does this unasked; `layer base <target>` (the
target named as users know it, what the notes advise) or `layer base
<parent>` does the same on request, asking `--yes` for pushed commits. Config can't join the ref transaction, so the move is recorded
first in `branch.<b>.gitfoldLanding` (`<target> <S>`); the next sync
finishes it if the stack ref is at S (else forgets it, unless a
workspace's restack intent may still move the refs). Without it, a crash
between the two left the child stacked on its old parent, and the next
sync replayed it back onto the parent's pre-squash commits. Answers are
cached per (B, target) in `.git/gitfold/.stack-landings`: a landing while
the target still has it, or "not landed up to tip T", so after a fetch
only `T..` is walked. A landing detection missed is moved by hand:
`gitfold layer base <TRUNK>` replays the child's own commits onto the
trunk's tip and takes its base away, `layer base <BRANCH>` onto another
base (needed once the parent is deleted, since deleting a branch deletes
its config and the name of its own parent with it). A deleted parent's
children still have their stack refs saying where their own commits start.

**Why:**

- **The base, not the parent:** a fix the parent got after its squash is
  the parent's business; the child only needs what it sits on to have
  landed.
- **Content, not history, for squashes:** a squash or rebase merge makes
  new commits. "Merging B changes nothing" caught a squash, a rebase merge,
  a squash the maintainer tweaked elsewhere, and one after main changed
  context lines near the parent's (the design's e4a, e5a, e5b, e5d); the
  path-limited walk finds it after main edited the same lines again (e6),
  and yields S. Unmerged parents said no (e4b, unit tests).
- **Onto the landing commit S, not the target's tip** (the design's open
  question 2): it is the smallest rewrite, only the child's own commits,
  onto exactly what its parent became. Nothing else the trunk did since
  lands in an agent's workspace mid-task, and a conflict can only be with
  the parent's landed form. The pull request shows the same diff either
  way. Following the trunk's tip after that is an ordinary rebase.
- **Landed in the trunk, no base at all:** a parent of `main` would make
  the branch a stack of one, shown under main in `list`, with notes and a
  base line about following main, which gitfold doesn't do by itself. A
  branch whose `gitfoldParent` is the trunk anyway (set by an earlier
  gitfold, or by hand) isn't moved at turn boundaries, noted in `list` or
  updated by `--all`: teams differ on rebasing onto main automatically,
  and every fetch would rewrite every bottom branch.
- **Guards:** B with nothing of its own since the merge base is trivially
  a no-op merge, so it never counts; B on the target's first-parent line
  is also what a fresh parent (no commits yet) looks like, so a
  fast-forward is taken only if B wasn't in the target when gitfold set
  it: every `layer base`, `branch --base`, restack and re-parent records that
  (`branch.<b>.gitfoldBaseOnTarget = <B> <target> <yes|no>`, naming the
  base and target it is about, so a stale record says nothing). Without a
  record (branches stacked before it was kept), a fast-forward isn't
  taken (`layer base <TRUNK>` moves it by hand). A target moved back
  behind B finds
  nothing; one reset forward over B looks like a fast-forward, so it is
  treated as a guess like a squash. Main independently making
  the same change looks like a squash: the content is there, but the child
  would leave a parent that may still be going on. So a squash is said to
  "look" merged; `layer base` (asked for) follows it, and the turn-boundary
  hook follows it only once the parent's branch is gone (deleted here, or
  its upstream deleted on its remote, as a forge does when it merges), and
  otherwise tells the agent once.
- **The parent's own parent as target:** a stacked pull request is merged
  into its base branch; looking only at the trunk would miss it until the
  whole stack lands.

**Rejected:**

- Per-commit patch ids (`git cherry`, what `git rebase` skips by): they
  don't recognise a squash of several commits (e4a).
- The patch id of the whole range `diff MB..B` against each trunk commit:
  missed squashes where context lines changed (e5b, e5d).
- The forge's answer (`gh pr view`) as the source: gitfold shouldn't need a
  forge; content settles all but a squash rewritten at merge time, which
  `layer base <TRUNK>` covers.
- Re-parenting onto the trunk's tip: pulls unrelated changes into the
  child mid-task and widens what can conflict.

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
| Holding `creating` across a fork | every open waited for a copy | [forks](#forks-commands-on-a-workspaces-stored-state) |
| Read tracking on by default | `R` lines are for several agents on one repository | [overlaps](#overlaps-reads-from-claude-codes-hook-writes-from-stored-state) |
| Layers' versions staged, `assume-unchanged`, an index per lens | commit theirs, revert theirs, or lose staged entries; skip-worktree instead | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| `info/exclude` for layers' new files | shared by every worktree | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| Broken index or no `.git` as the surrender | repaired away; git finds the enclosing repository | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| Live layers | index rewritten under a running git; views change mid-turn | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| HEAD symbolic to `refs/worktree/lens` | `gc` from another worktree prunes L | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| Extraction as a plain merge (`merge=union` or not) | commits the layers' lines, or conflicts on every append | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| `git rebase --update-refs`, `update-ref` alone, `reset --keep` for restacks | skips other worktrees' branches; moves no files; loses staged state | [stacks](#stacked-branches-restacks-computed-by-plumbing-moved-like-a-checkout) |
| Patch ids (`git cherry`, a whole range's) for squash merges | miss squashes of several commits, or with context changed | [landings](#a-stacked-branchs-parent-that-landed-its-base-found-by-content) |
| The merge base instead of a stored parent base | a rewritten parent's old commits replayed | [stacks](#stacked-branches-restacks-computed-by-plumbing-moved-like-a-checkout) |

## Known limitations

User-visible effects are listed in README ("Workspaces", "Differences from a
checkout" and "Running it"); this is the why and what it would take.

- **Partial clones** fetch a commit's every missing blob at open, as a checkout
  would (sizes need the blobs): laziness ends at the commit. The remotes are
  read once per mount; the free-space floor is fixed, not an estimate of the
  fetch's size (the server doesn't say before sending). Fetch locks are per
  root tree: two commits opened at once download what they share twice.
  `@<sha>` of a commit the clone lacks isn't fetched (`ENOENT`). A `gitfold
  remove` while mounted fetches on the daemon's single control thread, so
  `version` or another `remove` waits meanwhile. If a branch moves between an
  open's preload and its creation lock, that open fetches under the lock. A
  daemon killed mid-fetch can leave git's `.keep` file pinning a pack.
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
- **While mounted, the daemon removes workspaces and snapshots** (`gitfold
  remove`): it holds their state in memory. The command asks it over a Unix
  socket at `.git/gitfold/.control` (`src/daemon/control.rs`), which the repository's
  permissions guard (an abstract socket name could be taken by any local user);
  it is reached as `/proc/self/fd/<dir>/.control`, since socket paths are
  limited to 107 bytes, and the daemon answers only its own user. A refusal's
  kind (not found, changes, in use: `remove`'s exit codes 3, 4, 5) travels on
  the answer's last line, where clients that don't know it just show it;
  `remove --check` is a verb of its own, which daemons that don't know it
  refuse. Without an answer (a daemon from before the control socket, one
  starting), `remove` asks for an unmount; a daemon being unmounted (detached
  while busy, or exiting) refuses, and exits only after a removal in progress.
  For an open workspace, the daemon first runs the checks while it is live (its
  own prewarm walkers don't count as users): whether a process of this user
  uses it (from `/proc`), then those of the offline `remove`; refusals almost
  always happen here, before anything else changes. One that isn't open is only
  checked once, while lookups of its name answer `EBUSY` (it can't be opened
  alongside). Then it takes the workspace out of its registry, so no request
  reaches it (waiting up to 5 s for requests in flight), runs all the checks
  again (what changed meanwhile counts), puts it back if any fails, else
  deletes its state and has the kernel drop the entry. While taken out,
  requests into it answer `EBUSY`, which the kernel doesn't cache (an `ENOENT`
  would leave negative entries for files that exist, if the removal is
  refused), and so do lookups of its name; other workspaces open as usual. A
  snapshot only leaves the root meanwhile (its inodes keep working), and holds
  off opening snapshots for the `/proc` scan and stopping its prewarm walker.
  Removed slots aren't reused: inodes the kernel still holds answer `ENOENT`,
  and a reopened branch gets new ones. Its prewarm walk is stopped only once it
  is taken out, and restarted if the removal is then refused. `remove --check`
  runs the same first checks; for a workspace that isn't open, lookups of its
  name answer `EBUSY` meanwhile.
  A daemon holds an exclusive `flock` on the git common
  directory itself (nothing is created for it) for its whole life, also while
  exiting after an unmount, and through bind mounts or other mount namespaces,
  so a repository has at most one daemon (two would share, and corrupt, the
  stored workspace state); `prune` and an offline `remove` take the same lock.
  Where directories can't be flocked (NFS without `local_lock=flock`), the
  lock file `.git/gitfold/.lock` is used instead (tests force it with
  `GITFOLD_TEST_LOCK_FILE`). Hosts sharing a repository over NFS with
  different `local_lock` options don't see each other's lock.
  `gitfold unmount` waits for this lock, also when the mount was busy and had
  to be detached (it lists the processes using it, from `/proc`, first),
  except when it or a process it runs under (its shell, `make`, ...) is
  inside the mount: it would wait for itself.
- **Layers combine whole files, except blended ones.** A file both
  workspaces changed shows this workspace's version only, unless marked
  `merge=union`; two layers' conflicting changes show conflict markers
  the reader can't resolve here (it can only remove one of them).
  Blending and extraction assume a blended file's lines are unique and
  their order doesn't matter where both sides append.
  A commit that changes a layer's lines waits until it is rewritten
  (`--amend`, if it is the last: a later commit can't undo it, and a
  rebase takes HEAD off L's line, turning git off); only the daemon log
  and the next switch (exit 4) say so, and nothing retries it against a
  newer seal of the layer. The push guard is a client hook: `--no-verify`
  skips it. A layer pins a seal that only the newest 16 seal refs keep: one
  not moved while its workspace sealed 16 times more can be pruned by `git
  gc`. Lost skip-worktree bits (`update-index --no-skip-worktree`, an index
  rebuilt by hand) aren't re-derived until the next switch, nor is the
  excludes file regenerated when the user's global ignore file changes
  (it copies that file). A fork doesn't copy the
  layers, but copies the index and upper as they are (skip-worktree bits,
  and in blended files the layers' lines, which become the fork's own).
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
  (`src/daemon/mod.rs`), but `kill -9` still leaves a stale mount for
  `fusermount3 -u`.

## Future work

Workspaces were built in phases: phase 1 is the copy-on-write core, phase 2
the [invisible rebases](#workspaces-follow-their-branch-invisible-rebases),
phase 3 the management commands (`branch`, `list`, `remove`, `prune`), a
compact node table and write measurements. Next:

- **Faster writes.** `FUSE_WRITEBACK_CACHE` (fewer requests per file, but the
  kernel then owns mtimes, which the index and rebases rely on), or passthrough
  on a second mount point for build output (passthrough needs
  `CAP_SYS_ADMIN`). Builds measured cost little (a
  full `cargo build` +17%); installs of many small files (`node_modules`) are
  the slow case, where opt-in symlinks to real directories for named ignored
  directories would be the cheap fix. See the write numbers in
  [Limitations](#known-limitations).
- **Parallel creates.** Creating an entry takes the workspace lock
  exclusively; per-directory locking would let parallel writers proceed.
- **Layers, next phases.** Telling the agent about a waiting commit (in the
  hook context, not only the daemon log) and retrying it when the layer
  moves. Proposals: a 3-way merge in `accept` instead of the refusal when
  the layer changed the files since (only safe once the proposal is
  rebased onto the layer's work); several proposals to one layer checked
  together (`consent` checks each alone); the `deps` log of reads of
  layers' files (design §4.3), so `consent` knows reads per seal rather
  than per path.
- **Stacks, next phases** (see
  [stacks](#stacked-branches-restacks-computed-by-plumbing-moved-like-a-checkout)).
  *Landings:* a forge's answer as a hint (`gh pr view`), `layer base
  --fetch`, and blocked restacks reaching the child's inbox, once per
  parent tip. *Layers and stacks together:* a child stacked on a
  parent and also showing the parent's uncommitted work as a layer is the
  natural pairing, but a restack moves the base, which waits while
  lensed, so v1 refuses it. Doing both at one boundary means switching the
  layer to the parent's newest seal with merge base A while the read-tree
  runs with the lens off for those paths, then rebuilding the view on C';
  and an extraction that "depends on" a layer whose lines the parent has
  since committed could offer `layer base` that parent instead. Uncommitted
  work that overlaps a restack could be carried over with a 3-way merge,
  as `carry_over` does for blends, rather than waiting. *Also:* `layer
  base --rebase` (a real `git rebase --onto A B` in the workspace for a
  blocked restack), merges in a stacked branch (flattened, as extraction
  does), re-signing copies when `commit.gpgSign` is set, an undo
  (the previous intent's C and B, if the branch is still at C'), and
  sharing `refs/gitfold-stack/*` across clones.
