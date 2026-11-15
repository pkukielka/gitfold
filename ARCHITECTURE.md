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
           .git/worktrees/gitfold-ws-<id>/   workspace registrations (with union files: gitfold-attributes, gitfold-guard, gitfold-hooks/)
           .git/gitfold/<id>/                workspace state: base, branch, markers, times, reads, layers, layers.next, move, move-told, move-blocked, upper/, tmp/ (a branch without a workspace: its editable layers)
           .git/gitfold/.tasks/<id>/         task folders (kept after the workspace)
           .git/gitfold/.control             the daemon's control socket
           .git/gitfold/.filter              the socket git's filter (gitfold filter-process) asks
           refs/gitfold/<id>                 keeps the workspace base from git gc
           refs/gitfold-snapshot/<id>/<n>    keeps a workspace's snapshots (the newest 16)
           refs/gitfold-layers/<id>          keeps the view commit of a workspace showing layers
           refs/gitfold-own/<branch>         where a branch's own commits start (an editable layer's commit, or their merge)
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
| `src/fs/workspace/moves.rs` | the one path a workspace's branch is moved by (its own commits onto or off an editable layer), mounted (the daemon) or not, layers shown or not: a layer switch that also moves the base (`switch_with`), the view built over the new tip and the changed files stamped; what makes it wait (uncommitted or staged work on the paths it changes, git busy, another move), and the copies of committed files it drops |
| `src/fs/workspace/reads.rs` | read tracking: recording the files read in a workspace, for `gitfold overlaps` |
| `src/fs/workspace/layers.rs` | layers in an open workspace: the view tree as its lower layer, switches and what the kernel must be told (and own lines in union files carried over), git told (skip-worktree, excludes, the filter and the guard on union files, their stat data zeroed) or turned off; answering the filter (`filter`) |
| `src/fs/workspace/follow.rs` | live layers of a workspace without an agent, moved by its sync thread once it is idle: writes counted (`Activity`), the check every 2 s, the move through `state::follow` and `switch` |
| `src/fs/names.rs` | telling the kernel when names in the root or a namespace changed |
| `src/fs/content.rs` | per-inode content cache and idle trimmer |
| `src/fs/stats.rs` | operation counters, logged at unmount |
| `src/fs/removal.rs` | removing a workspace or snapshot while mounted |
| `src/store.rs` | shared tree tables, file ids, directory layout, index walk; what is at a path of a tree (`Lower`, `entry_at`) |
| `src/state/mod.rs` | where a workspace's stored state is (`StateDir`: its id, state directory, registration and base pin; the files in it), ids, its base and branch files, paths below its root, whether an upper file matches a stored one |
| `src/state/markers.rs` | whiteouts and opaque markers, as an append-only log |
| `src/state/reads.rs` | the read log's format (`gitfold overlaps`) |
| `src/state/status.rs` | a workspace's uncommitted changes, ignored files, last change and line counts, read from its state directory (for `list`, `changes`, `overlaps` and `remove`) |
| `src/state/retire.rs` | the removal rules for a workspace or a snapshot's registration, shared by `remove` and the daemon; a removed workspace's branch keeps its editable layers |
| `src/state/fork.rs` | `branch --with-changes`: copying a workspace's state, registration and index into a new branch (with its editable layers and own ref) |
| `src/state/task.rs` | the task folder (the `.task` link): where it is stored, its `info/exclude` entry, copying it for forks |
| `src/state/moves.rs` | a move under way in a workspace: the `move` intent file (locked while it runs), its steps (the index on the paths it changes, the base, the refs), and finishing or undoing one a crash interrupted |
| `src/state/snapshot.rs` | snapshots of work: a workspace's uncommitted work as a commit, kept by `refs/gitfold-snapshot/<id>/<n>`, numbered under a lock; the one its own `Stop` hook made last recorded in its state (`snapshotted`): whether an agent drives it; its `UserPromptSubmit` hook's run recorded too (`turns`): whether it has an agent of its own; a union file snapshotted with its own lines only, and what it saw and made there recorded as the snapshot's second parent |
| `src/state/follow.rs` | moving a workspace's live layers to their latest work (what each follows, a fresh snapshot of a person's workspace, storing and showing them), shared by the `UserPromptSubmit` hook and the daemon |
| `src/state/waiting.rs` | waiting edits: a workspace's changes to its editable layers' uncommitted lines, worked out from upper and the applied view, in words for the listing, the hook and the refusals |
| `src/state/guard_check.rs` | the safety net: a workspace's new commits checked with the guard's rule whenever its branch moves (`guard-checked`), what is found kept and said (`guard-alarms`) |
| `src/state/layers.rs` | a workspace's layers: the `layers` file (each layer's snapshot, what it shows and how it updates, see-only or editable, the layer a proposal stands in for; also a branch's editable layers without a workspace), `layers.next` (the layers a move that changes what the branch builds on will leave, settled once its own ref is there), which paths only editable layers changed (writable), its lock (`layers.lock`), the view (view and hunk trees, mixed and union paths, conflicts, notes, the listing's notes and warnings on union files) and which files are union-merged (`merge=union`) |
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
| `src/git/worktree_config.rs` | a linked worktree's own config: the excludes file for layers' new files (keeping the user's global ignore file); the filter on union files (driver and attributes file); the guard hooks |
| `src/git/hooks/` | the guard's `reference-transaction` and `pre-push`, and the shell library the worktree's hooks share (embedded) |
| `src/git/filter.rs` | the clean/smudge filter for union files (clean and smudge from the base's version and the union) and for files only editable layers changed (`Edited`: each hunk the workspace's own or a waiting edit), git's long-running filter protocol, the relay's frames to the daemon |
| `src/git/guard.rs` | the guard: the layers' uncommitted lines per filtered path (`gitfold-guard`), checking ref updates and pushes for commits that bring them (`gitfold guard`), and finding every such line in a range of commits (the safety net) |
| `src/daemon/filter.rs` | the filter socket (`.git/gitfold/.filter`) and `gitfold filter-process`, relaying git's requests to it |
| `src/git/stack.rs` | moves' git side (*internal* name): where a branch's own commits start (`refs/gitfold-own/<branch>`), the merge of several editable layers' tips (`merge_parts`, read back by `parts_of`), computing a move by plumbing, commits that change a layer's lines (`dependent`), moving a branch and its own ref in one transaction, whether an editable layer's commit landed on a target (merged, squashed, rebased, fast-forwarded) |
| `src/git/test_repo.rs` | unit tests' scratch repository |
| `src/git/layers.rs` | layers' git side: the view and hunk trees (union merging, from a snapshot's record of what its author saw where it has one; each layer's changes merged in per path; lines added among another branch's noted), the view commit, snapshots' records, copying commits and merging trees (for moves) |
| `src/lock.rs` | the repository lock, held by a mounted daemon and by commands working on stored state unmounted |
| `src/git/promisor.rs` | partial clones: fetching what a tree load misses from the promisor remotes |
| `src/daemon/mod.rs` | mount point checks, unmount on SIGINT/SIGTERM/SIGHUP, fusermount |
| `src/procs.rs` | processes using a mount (from `/proc`) |
| `src/mount_table.rs` | the mount table: where a gitfold mount is (or a repository's), its source name (the repository) |
| `src/daemon/control.rs` | the daemon's control socket (`remove`, `branch --with-changes`, `prewarm`, `track-reads`, layer switches (`layers`) and moves of a workspace's branch (`move`) while mounted; stopping and resuming prewarm for `unmount`; its version; the open workspaces, for `list`) |
| `src/daemon/serve.rs` | the daemon in the foreground: mounting with FUSE, the control requests it answers, until unmounted |
| `src/prewarm.rs` | background walk of new snapshots and workspaces, in a walker process; the prewarm modes |
| `src/cli/mod.rs` | what commands share: the repository (`repo`), the workspace the current directory is in (`branch_or_current`; `workspace_at` from the mount, where `.git` turns git away), printing paths as git quotes them (`quote`, `write_path`), the repository lock or a daemon request (`local_or_daemon`) |
| `src/cli/branch.rs` | `branch` (its forks, and `--edit`: a new branch built on another), `prewarm`, `track-reads` |
| `src/cli/list.rs` | `list` (who builds on whom, as a tree, branches without a workspace included) and `changes` |
| `src/cli/remove.rs` | `remove` and `prune` |
| `src/cli/snapshot.rs` | `snapshot`: workspaces' work merged into one snapshot (and the `Stop` hook that takes one) |
| `src/cli/layer/mod.rs` | `layer`: `add` and `remove` (of an editable layer, or `--edit`, handed to `moves`), the listing (the layers, what your commits sit on, notes, conflicts); the hooks that move live layers, moving the branch onto its editable layers' new commits first, and deny edits of a see-only layer's files, offering to build on it (`--edit`), a proposal, asking, removing the layer (and, for an edit that only adds lines, `merge=union`) |
| `src/cli/proposal.rs` | `propose`, `proposals`, `consent`, `accept` and `reject`: changes to a layer's files made in a fork of its workspace, checked against the workspaces that read them, written into the layer's workspace |
| `src/cli/layer/moves.rs` | moves of a branch's own commits (`move_onto`, one path whatever asked): onto an editable layer's commits (`layer add --edit`), off one (`layer add` without `--edit`, `layer remove`), at the `UserPromptSubmit` hook; saying what it moves, refusing pushed commits without `--yes` and commits that change a layer's lines when it comes off; who moves it (its ref alone, or its workspace through `fs::workspace::moves`); landings followed; the force-push printed; `list`'s and the listing's notes |
| `src/cli/layer/land.rs` | an editable layer's target (the one branch it builds on, or the trunk), whether the commit a branch sits on landed there (cached in `.git/gitfold/.landings`), whether the layer's branch is gone |
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
| `tests/layers.sh` | end-to-end test of snapshots of work and layers in a workspace (refusals, git status and commits, git off, remounts; live, frozen, commits-only and revision layers, re-adding, the listing; several layers on one file, merged or conflicting; union files, the filter and the guard; proposals, consent, accept and reject) |
| `tests/moves.sh` | end-to-end test of moves: a branch's own commits onto its editable layers' commits (`layer add --edit`, `branch --edit`) and off them (`layer add` without `--edit`, `layer remove`), with no workspace, an unmounted workspace and an open one (hook and `layer add`), conflicts, uncommitted work in the way, pushed commits and `--yes`, `--frozen`, commits that can't stand without a layer, several editable layers, landings (followed unasked only once the layer's branch is gone; a stale local `main`), moves with see-only layers and union files shown (also killed half-way), `list`'s tree and notes |
| `tests/fixture.sh` | the repository each isolated section of the four suites above starts from |
| `tests/run.sh` | runs suites at once (by default all but `workspace.sh`) and sums up; `SECTION` across them |
| `tests/promisor.sh` | end-to-end test of partial clones (fetches, stalls, backoff, remote choice) |
| `tests/race-modes.pl` | helper of `tests/workspace.sh`: lifted modes must stay invisible |
| `tests/cache-tree.pl` | helper of `tests/status.sh`: prints an index's cache tree, to compare with git's |
| `tests/lib.sh` | what the suites share: a directory per run, timeouts, sections, cleanup |

## Glossary

User-facing text uses three concepts only: **layer** (see-only, or
editable with `--edit`), **base** (a workspace's) and **snapshot**. The
code keeps some internal names (stack, record, trunk), marked
*internal* below with the user-facing term; they never appear in
messages, help or the README.

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
  ref or `packed-refs` (`worktree::git_busy`).
- **Lower**: the base tree, from the shared store (with layers shown, the
  view tree).
- **Upper**: every file that differs from the base, as real files in
  `.git/gitfold/<id>/upper`.
- **Snapshot of work**: a commit of a workspace's uncommitted work: the
  tree it shows, ignored
  files left out, on its base (or its branch tip, when it committed and
  its base hasn't caught up, if it has the tip's changes), made with a
  fixed identity and date.
  `gitfold snapshot` and the `Stop` hook make one. Its refs outlive the
  workspace; the one the workspace's own `Stop` hook made last is recorded
  in its state (`snapshotted`), so a live layer never follows an earlier
  workspace's. A union file in it holds only the workspace's own lines;
  where they sit among other layers' lines, its **record** (*internal*:
  `git::layers::Record`, the snapshot's second parent) keeps what the workspace
  saw there and made of it.
- **Layer**: another branch's work shown in a workspace, read-only: one of
  its workspace's snapshots, or a branch's or a revision's commit, as the
  changes since it and the commit the view is built on diverged (their
  merge base, worked out per view). **Live** layers follow their branch
  (an agent's workspace: the snapshot its `Stop` hook made at the end of its
  last turn; a workspace without an agent: a snapshot taken whenever the layer
  moves; or the tip), at the reader's turn starts if it has an agent, else
  by the daemon once the reader is idle, **frozen** ones stay as taken,
  revisions are **fixed**. A layer is **see-only** (read-only), or
  **editable** (`--edit`, `Layer::edit`): the branch builds on it.
- **Layers**: the layers of a workspace, in the order added (also the
  name of the file storing them); **showing layers** means it has at least
  one.
- **View tree** (M): the hunk tree with the layers' other changes on top:
  at each path, each layer's change in turn as a 3-way merge (its merge
  base's version, the path so far starting from the base's, its own),
  conflict markers kept (the base's version where a change can't merge at
  all); the lower layer while showing layers.
- **Union path** (hunk path): a file `merge=union` marks in the base's
  attributes, which the base has, a layer changed and none deletes: every
  layer's change merged into the base's version. Writable; upper holds it
  with the workspace's own lines on top.
- **Hunk tree** (H): the base's tree with only the union paths union-merged.
- **Mixed path**: a path a layer changed (or deleted) that isn't union-merged:
  the base's version with the layers' changes merged in, read-only, unless
  upper has it or only editable layers changed it (**writable**). It
  belongs to every layer that changed it.
- **View commit** (V): tree M, parents the base and the shown layers'
  snapshots; what `refs/gitfold-layers/<id>` points at, keeping them.
- **Filter** (on union files): the clean/smudge driver `gitfold-<id>`, in
  the worktree's own config, that strips the layers' lines when git reads
  a union file and puts them back when git writes it (`git::filter`).
- **Guard**: the per-worktree `reference-transaction` and `pre-push` hooks
  that refuse a ref update or push bringing a layer's uncommitted lines of
  a union file onto the branch (`git::guard`).
- **Proposal**: a fork of a layer's workspace (`<layer>-prop/<ws>`) where a
  workspace showing the layer edits the layer's files, shown in its place
  (the layer it stands in for kept with it, `instead` in the layers file,
  so the layer comes back as it was shown: live or frozen, all work or
  commits only); its changes are its snapshot against what it started from.
  **Consent**: the
  layer's work as now with those changes, checked in the view of each
  workspace showing the layer (merged with its own work, and tested).
- **Built on** (*internal*: stack): a branch with editable layers. Its
  **own commits** are those after where they start, the **own ref**
  `refs/gitfold-own/<branch>`: one editable layer's commit, or with
  several, the **merge** of their tips gitfold makes (`merge_parts`, a
  fixed identity, rebuilt when any moves). The **parts** are the layers'
  names and the commits of theirs it sits on, read back from the merge's
  message. Every branch without editable layers sits on the **trunk**
  (*internal*; user-facing: the main branch, named as users know it).
- **Move** ("moves 3 commits of ui onto api"): copying a branch's own
  commits onto an editable layer's commit (or a new merge), or off one,
  and moving the branch, its own ref and its workspace's index, base and
  files there. An **intent** (`move` in the state directory) makes a
  crash recoverable.
- **Landing**: an editable layer's commit the branch sits on found in the
  layer's **target** (the one branch the layer builds on, else the trunk):
  merged, squashed, rebased or fast-forwarded there.
- **Switch**: changing what the layers show (a layer added, replaced,
  removed or moved to newer work), telling the kernel and git, and
  carrying the workspace's own lines in union files over.
- **Git off** ("git is off here"): turning git off in a workspace with
  layers whose index can't be edited: its `.git` names a
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
  them once (a layer's changed files, not the tree). Union paths' entries
  get the union's id and size with zero times, so git reads each once
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
  `reads`, `layers` and `layers-shown` record formats, `layers-git-off`,
  `layers.next`, the `move` intent, `move-told`, `move-blocked`,
  `upper/`; for a branch without a workspace, `layers` alone), task folders
  in `.git/gitfold/.tasks/<id>/` and the message format of their inboxes
  (README, "Messages"), the base-pinning ref `refs/gitfold/<id>`, the snapshot
  refs `refs/gitfold-snapshot/<id>/<n>`, the layers ref `refs/gitfold-layers/<id>`
  and the files in a workspace's admin directory (`gitfold-attributes`,
  `gitfold-guard`, `gitfold-hooks/`), the own refs `refs/gitfold-own/<branch>`,
  the merge commit's message (`gitfold: built on ...` with `on <name>`
  lines, read back as its parts), the landing cache `.git/gitfold/.landings`, the
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
one **snapshot** of the other workspace (`state::snapshot`: a commit of the tree it
shows, ignored files left out, on its base, kept by
`refs/gitfold-snapshot/<id>/<n>`), or with `--commits` or without a workspace
the branch's tip, or a tag's or a commit's commit. A **live** layer moves
when added again, and at the reader's agent's turn boundaries, or in a
workspace without an agent, by the daemon once the workspace is idle; a
**frozen** one
(`--frozen`) and a revision (**fixed**) don't. The layers' changes
(`diff-tree <merge base> <snapshot>`, in the order added, the merge base of
the snapshot and the commit the view is built on, worked out for each view)
applied to the base's tree make the **view tree** (`state::layers::view`,
`git::layers::build_view`), loaded into the shared store and used as the
workspace's lower layer: at each path a layer changed, its change merged
into what is there (the base's version, or with the layers before it) as
`git merge-file` does, against its own merge base (conflict markers kept,
and listed), except files marked `merge=union`, which are
**union-merged** (every layer's change merged into the base's version, lines
both sides add kept both) and stay writable. git sees only the workspace's
own lines of a union file through a **clean/smudge filter** the daemon
answers (`git::filter`), HEAD on the branch, and a **guard** hook refuses
ref updates that would bring the layers' lines onto it (`git::guard`). The layers are stored in `layers`
in the state directory; `gitfold layer add`/`remove` write it and ask the
daemon to switch (a `layers` control request). `gitfold snapshot` is the
read-only form: snapshots merged with `git merge-tree` into a commit opened as
a snapshot, with no change to any workspace.

**Why:**

- **What was applied, apart from what is stored:** `layers` is what the user
  (or a hook) asked for; `layers-shown` is what the daemon last applied: the
  layers file then, the paths git was told to skip, the union paths. A
  workspace opens showing what was applied, so the view and the index,
  excludes and filter always describe the same layers, and the sync
  thread then switches to the stored layers if they differ (changed while
  the workspace was closed, or a switch that failed half-way). A switch the
  daemon refuses puts the stored layers back. Without the split, a layer
  added while unmounted showed at the next mount with git never told: `git
  commit -a` committed its files. Switches and the sync thread (rebases)
  take one lock per workspace, so they never interleave.
- **The view tree as the lower layer:** `Workspace::lower` already reads one
  stored tree, so lookups, listings, `getattr` and reads needed no new code
  (only `mixed` on lower files, for their write bits). Upper still wins over
  it, so the workspace's own files are never touched by a layer.
- **Snapshots, not live files:** a snapshot is immutable, so the other agent's
  writes change nothing a reader has cached, and need no invalidation. Made
  with a fixed identity and date, the same work gives the same commit, so
  snapshotting an unchanged workspace makes nothing new and moves no layer.
- **The merge base of the view's own base:** a layer is what its
  workspace has that this one doesn't (its commits since they diverged,
  plus its uncommitted work), so its changes are taken against
  `merge-base(B, snapshot)`, B the commit that very view is built on: the
  workspace's base for the view tree, the base for a proposal's check.
  It is worked out per view (remembered per process: commits don't
  change), never stored. Rejected: pinning it when the layer is added or
  moved. Against the base, it went stale when the base caught up; against
  the branch tip, it was ahead of the base the view was built on, which
  lags while layers show (commits made meanwhile).
  Either way the lines between the two commits counted as the layer's own
  changes: a layer of a branch moved onto this workspace's new commits
  then "changed" the lines those commits made and conflicted with the
  base falsely. Re-pinning at every catch-up was the other way out, but
  every place the base or tip moves would have to do it, under the
  layers' lock, and a crash in between leaves them disagreeing; computed
  per view, they can't. What a layer shows still changes only at a
  switch: views are built only then.
- **A snapshot's parent: what its tree is that commit plus the
  workspace's own work** (`snapshot::parent`). Its base, or, when the
  workspace committed and its base hasn't caught up (it waits while
  layers show), its branch tip, so a layer of it brings only what came
  after the commit. The tip only if the workspace has the tip's changes:
  at each path the base..tip diff touches, the snapshot's tree has the
  tip's entry, or the workspace's index has (committed through it, then
  edited further), or merging the tip's change into the tree's entry
  changes nothing (edited further on other lines). A tip moved without
  the files (`update-ref`, a push into the branch, `reset --soft` to a
  descendant) leaves it on the base: on the tip, the snapshot would undo
  the tip's changes. A file the tip added and the workspace edited
  further, unstaged, falls back to the base (wider, never wrong).
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
  what the index describes, and while HEAD is away from the branch.
- **Writes refused:** a mixed path's file a see-only layer changed has no
  write bits, and copy-up (writes, truncation, `chmod`), removal, renames
  from or onto it (or of a directory holding one) and creating a path a
  layer deleted are `EACCES`. Any of them would copy the layer's content
  into upper, where it becomes this workspace's own work, hidden from
  later switches and committed with its branch. A path only editable
  layers changed (`View::writable`) is writable: the branch builds on
  them, so their work is its to change (see "Editable layers"). `errno`
  carries no text, so Claude Code's edit tools are denied first by a
  `PreToolUse` hook giving the reason and the ways out, naming every
  layer whose change the file shows. First, building on the see-only
  ones (`gitfold layer add A --edit`, every one needed when several
  changed the file, since one alone leaves the others' lines read-only:
  the rule is that a see-only layer's files stay read-only, and code that
  depends on another branch's code is built on that branch, as a stacked
  PR is), with what it means (`your commits will sit on A's (a pull
  request for your branch then includes A's commits until A lands)`) and,
  when A's change to the file isn't committed yet, that edits to A's
  lines wait until A commits them (see "Waiting edits"); not offered when
  one of them is a revision, a
  proposal, a branch that no longer exists or the main branch. Then a
  proposal (see "Proposals"), asking the layer's agent (`gitfold
  send`), removing the layer. An Edit or MultiEdit whose `new_string` is
  its `old_string` with whole lines inserted, or a Write whose content is
  the file's with lines inserted, adds the `merge=union` hint: a list
  file is better union-merged than owned. Rejected: in-place editing of a
  layer's file (the edit would be neither the layer's nor cleanly the
  workspace's: whose commit holds it, and how the next layer move merges
  it, have no good answer).
- **A layer brings only its own changes, merged in per path.** A layer
  used to apply whole files: one that changed line 50 showed its whole
  copy, line 10 too, stale if the base fixed it after the layer started,
  so the view silently undid the base's fix (a note said so). And whoever
  added a layer last used to win a path. Now each path a layer changed
  shows, for each layer in the order added, `merge(ancestor = the layer's
  merge base's entry, ours = the path so far, starting from the base's,
  theirs = the layer's)` as `git merge-file` does: what merging the
  layer's branch would give, one layer and several following one rule
  (the order doesn't matter for clean merges). Each layer is merged
  against its own merge base, so layers started from different older
  commits keep all their changes. Entries merge as in git: unchanged on
  one side or alike on both, trivially (a deletion the base didn't touch
  is a deletion; a change the base already has shows the base's, not
  owned); a mode change on one side wins. A text conflict keeps git's
  markers (`<<<<<<< main + A`, `>>>>>>> B`: the base named as users know
  it, main or the branch's editable layers), so the reader sees both
  sides; anything that can't be merged (binary, a deletion against a
  change, a type change, a new file under a file that stays) shows the
  base's entry. Both are mixed paths (read-only, owned by every layer that
  changed them) and are reported, naming who conflicts: the base, if the
  layer's change alone conflicts with it, else the layers before it whose
  changes alone conflict with it (all of them if none alone does); on the
  switch's reply, at the next turn, in the deny hook's reason and in
  `gitfold layer`'s listing. A path the base made a directory of stays
  the base's directory. A branch with no commit in common with the base
  has no merge base: `layer add` refuses it, and a view can't be built.
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
  its patterns. Removing the layers clears both. A file the base has that
  only editable layers changed isn't marked (`git_plan`): it is writable,
  and git sees it through the filter (below, "Waiting edits"). One only
  they have stays excluded, even with an upper copy: edits to it wait.
- **Union files, for files several agents append to** (a registry, a
  manifest): whole files would show either theirs or mine. A file is
  union-merged if `git check-attr --source=<base> merge` says `union`
  (the base's `.gitattributes`, or `info/attributes`), the base has it and
  no layer deletes it. Each layer's change is folded in as
  `merge3(base = merge base, ours = the union so far, theirs = snapshot)`; two
  appends at one place conflict in every 3-way merge, so where both sides
  only add lines both are kept, ours first (what `union` does, but only there).
  Any other conflict leaves that layer's change out, as a note on the
  switch's reply. Union merging and the other paths' merges are the same 3-way
  merge per layer; only union merging resolves append conflicts, and a path is
  one or the other, never both. The base's tree with only the unions is
  the **hunk tree** H; the view tree M is H with the whole files on top.
  Union files have no `mixed` entry, so they are writable: a copy-up
  takes the union, and the workspace's own lines go on top.
- **A snapshot records what its author saw in a union file**
  (`git::layers::Record`, written by `state::snapshot`). A snapshot holds only the
  workspace's own lines of a union file, against its base (`own_lines`),
  so a layer of it never brings another layer's lines through it. But
  that loses where those lines sat among the others': a line added inside
  another layer's block was then placed by the base's lines alone (after
  the block, by `union`'s ours-first rule). So where the workspace added
  lines among other layers' lines, the snapshot gets a second parent R: a
  commit whose tree has those files as the workspace left them (its
  upper copies), with a `gitfold-seen <base>` header naming the commit
  its view was built on, and whose only parent is a root commit S0 whose
  tree has the same files as it saw them (the hunk tree's unions). `git
  diff <snapshot>^2^ <snapshot>^2` is then the author's change in context. A view
  folds the layer's change to such a path in two steps: its commits up to
  the author's base (`merge3(MB[p], the union so far, base[p])`), then
  the author's change from what it saw (`merge3(S0[p], that, R[p])`, with
  `union`'s rule for appends at one place), so its lines land among the
  other layers' lines as the author put them. When that conflicts (what
  they sat among isn't there), it waits while other layers of the path
  fold in (one may bring those lines, whatever the order added), and
  then falls back to the snapshot's own lines against the merge base,
  with a note (`PlacedByBase`, listed as `Note:`). The
  merge base itself is still `merge-base(view base, snapshot)`, computed per
  view: R and S0 share no history with any branch, and S0's message
  names the branch, base and snapshot tree, so two snapshots never share it and
  it never becomes a merge base. Made with the fixed identity and date,
  an unchanged workspace makes the same R and the same snapshot (deduped);
  a snapshot with no lines among others' has no R, and one without R (or a
  branch's commit) is folded in as one step against the merge base.
  The record also shows lines added between two lines of another
  branch's, next to none of the base's (a method inside its class): a
  `Note::Among`, a `Warning:` in the listing (`state::layers::own_among`
  checks the workspace's own upper copies the same way), since such a
  file may not be a list. Rejected: L as the second parent (it has the
  view tree, but its parents are the other layers' snapshots, whose bases
  would then become merge bases of this snapshot, and a layer's change would
  be taken against another branch's commit); the record's trees in a
  commit header or note (objects reachable only from a header are pruned
  by `gc`, and notes need a ref of their own that no snapshot ref keeps);
  the author's whole file in the snapshot's tree (layers would bring the
  other layers' lines through it, even where those aren't shown).
  The branch's commits hold only the workspace's own lines, placed by
  the base's lines (the layers' lines aren't on the branch to place them
  by).
- **A clean/smudge filter for union files** (`tell_git`, `git::filter`):
  the index alone can't split a file into theirs and mine, but git's
  filter boundary can. Clean (git reading the file into an object) strips
  the layers' lines; smudge (git writing one out) puts them back. The
  index, commits and stashes hold only the workspace's lines, the file
  everyone's, and HEAD stays on the branch, so every git command is
  native: commit ids, `HEAD~1`, `--amend`, `reset`, `rebase -i`,
  `stash`, `merge`, `cherry-pick`, `revert`, `bisect` (E3,
  patches/06-e3-results in the design notes). Both are functions of the
  file's base version B and union H: clean is `merge(base = H, ours =
  file, theirs = B)`, else the file less H's added lines, once each;
  smudge is `merge(base = B, ours = H, theirs = content)`, else git's
  union resolution (either side first) or each own hunk put where its
  base lines are, if clean gives the content back exactly, else both sides
  kept, else git's markers: never the content alone, which would lose the
  layers' lines (they would look deleted). An exact round trip for content
  it smudged comes from a memo (by path and hash of the output → input,
  cleared at each switch), trusted only while the index has that input.
  - **Setup, per worktree:** `filter.gitfold-<id>.process` and `.required =
    true` in `config.worktree`, and `core.attributesFile` there pointing
    at `<admin>/gitfold-attributes` (the user's global attributes file,
    then `"/<path>" filter=gitfold-<id>` per union path, anchored, quoted,
    glob characters escaped). Other worktrees read neither, and an
    undefined driver is ignored (verified with git 2.49). Rejected:
    `$GIT_COMMON_DIR/info/attributes` (it has the highest precedence, but
    it is shared and the user's: every workspace would rewrite it, and
    gitfold reads `merge=union` from it). The per-worktree file has the
    lowest precedence, so an in-tree `.gitattributes` setting `filter` on
    the same path wins (LFS on a union file: not a case that occurs).
  - **The long-running protocol** (`filter.<driver>.process`): git starts
    `gitfold filter-process <common> <id>` once per command, and only if a
    filtered path is converted. The process relays each request to the
    daemon over `.git/gitfold/.filter` (`daemon::filter`, a thread per
    connection), where `Workspace::filter` answers from the view as it is
    now (the base and `hunk`, under `layers` read). With `required`, a
    daemon not answering fails the command, rather than having git see the
    layers' lines. The relay talks to the daemon rather than computing the
    answer itself so that it answers for the view the daemon shows,
    switches included.
  - **git's size rule:** a size other than the one in the index is taken
    as a change without reading (cleaning) the file. Each switch therefore
    zeroes the size and times of the union paths' entries, now or before
    (`index_to_plan`, with the index editor): git then reads them once and
    records real stat data. A file the workspace edits shows modified at
    once, as it should.
- **The guard** (`git::guard`, `worktree_config::install_guard`): what
  doesn't run the filter (`-c filter.<driver>.clean=cat`, `hash-object
  --no-filters`, `update-index --cacheinfo`, libgit2) can commit the
  layers' lines, and the filter's split can be wrong: the guard is the
  authority. While an editable or union layer shows uncommitted work
  (filtered files or not), the worktree's own `core.hooksPath` is
  `<admin>/gitfold-hooks`:
  - its `reference-transaction` (state `prepared`) runs `gitfold guard
    ref-update`, and `pre-push` runs `gitfold guard pre-push`; then each
    runs the user's hook of that name. Every other hook name is a shim
    running the user's (the path found from config without the worktree
    scope, at run time);
  - the guard reads `<admin>/gitfold-guard`, which each switch writes: per
    union path and path only editable layers changed (added, deleted),
    per layer with uncommitted work there: its version in its snapshot,
    the lines it adds that neither the commit the snapshot is on nor the
    base has, and the base's lines its work removes. Each check first
    drops what the layer's branch has committed since (frozen layers too);
  - it refuses an update of `refs/heads/<branch>` (or a push, of any ref)
    whose new commits (`new ^old`; `--not --remotes` for a push; never the
    layers' own branches' commits, so a fast-forward onto one passes) add
    such a line, one no parent has, remove such a removed line, add the
    layer's new file whole, delete the file, or add a conflict marker
    there. Lines are compared as sets, without whitespace, and long ones
    (four words or more) without punctuation too, so a formatter's
    rewrite or a move doesn't hide one. A merge of the layer's branch once
    it committed the lines passes.

  The hook sees every ref update, plumbing included (`commit-tree` and
  `update-ref`), and `--no-verify` doesn't skip it. libgit2 runs no hooks:
  its commits land on the branch, and a `git push` refuses them (E3). So
  the **safety net** (`state::guard_check`, the sync thread's
  `check_branch`, every second): whenever the branch tip differs from the
  one last checked (`guard-checked` in the state directory, recorded when
  the workspace opens, so commits made while it was closed are checked at
  the next mount), the new commits (`tip ^last`, without the layers'
  branches' own commits, onto which a move puts the branch) are checked
  with the guard's rule, every line found. Each commit and file found is
  kept in `guard-alarms` (`<commit> TAB <text>`) until the commit is no
  longer on the branch, said in the daemon's log, as `WARNING:` in the
  listing and in the turn hook's notes, naming the commit, the lines and
  `git reset <commit>^` (mixed: a soft reset's re-commit would be refused
  again). Rejected: rewriting such a commit (it is
  the user's tool's, maybe pushed; a warning keeps git's history theirs).
  The guard skips lines of punctuation alone (`}`): in code they are
  everyone's, and a commit adding its own `}` isn't a leak.
  `refs/gitfold-layers/<id>` (common) pins the view commit V (tree M,
  parents the base and every shown snapshot), keeping M and the
  snapshots. Turning the last union off removes the filter, the
  attributes file and the guard; the files keep the workspace's lines, now
  changes against the branch's version. Rejected after E3: the detached
  layers commit (worse on every axis but libgit2 commits; see the design
  notes, patches/05a, 05b, 06).
- **Own lines carried over on a switch** (`carry_over`): when the union
  under a file the workspace has written changes, upper becomes
  `merge-file(base = old union, ours = upper, theirs = new union)`, or, if
  that conflicts, the line-set rule (new union less the lines upper
  removed, then upper's new lines), or, if upper changed lines too, the
  filter's view of it: clean with the old union, smudge with the new, if
  clean gives the same lines back. Where none keeps the workspace's lines,
  the whole switch is refused before anything changes (`your changes to it
  depend on lines the layers would no longer show`). The rewritten files
  are invalidated with the rest of the switch.
- **Waiting edits: files only editable layers changed, through the same
  filter** (`git::filter::Edited`, `state::waiting`). Such a file (the
  view's `edited`: the base has it, the view shows it, only editable
  layers changed it) mixes the branch's lines with a layer's uncommitted
  ones, which the branch doesn't have yet and mustn't commit, so it gets
  the union files' driver attribute, guard lines and stat zeroing
  (`ShownLayers`' `B` records cover both). The difference is in what
  clean keeps. Each hunk of the diff from the file as shown (M) to the
  file as it is (W) is either the workspace's own, applied to the base's
  version after the base line before it, or a **waiting edit**: it changes
  or deletes a layer's line, inserts lines with a layer's lines on both
  sides (a method in the layer's new class), or spans base lines the
  layer deleted (the spec's dependency rule, by diff matching, no line
  ids). Clean gives the base's version with the own hunks; smudge merges
  git's version into the file without its own hunks (M with the waiting
  ones, from upper as it is: `merge(base = B, ours = M + waiting, theirs
  = git's)`, git's union resolution where they touch if clean gives git's
  version back), so `checkout`, `stash` and `reset --hard` keep the
  waiting edits. A binary file's every change waits.
  - **Stored where every edit is:** in upper, durably, and carried on each
    switch like a union file's own lines: `merge-file(old M, upper, new
    M)`, a conflict kept with git's markers (`yours` against `<layer>'s
    newer work`) rather than refusing the switch, and listed. A conflict
    is never the workspace's own: until its markers are edited out, clean
    gives the index's version (git sees no change it didn't make: no
    markers, no revert of committed lines). git's own version is the
    branch tip's, the view rebased onto it, so lines committed while the
    base waits are neither changes nor waiting edits. Content counts as
    well as place: a hunk putting back a layer's line anywhere (a move, a
    sort, a formatter) waits, and a split that would give git a layer's
    line or lose a line on git's way back waits whole. Turning the layer
    see-only strips upper to the own lines. Deleting the file is seen by
    git and refused by the guard; `git checkout -- <file>` brings it back,
    which is also how waiting edits are dropped.
    A crash after carrying but before the switch is recorded leaves the
    old view applied: the switch is made again, and carrying again changes
    nothing. When the layer commits the lines and the branch moves onto
    that commit, the file isn't the layer's any more (no longer
    filtered), and upper holds the branch's version plus the edits:
    ordinary changes. Its index entry, moved with the branch
    (`move_index`), has its size zeroed as a filtered path no longer
    filtered, so git reads it once. A copy no longer filtered that is what
    the view shows (a see-only layer's file now, say) is dropped, so it
    doesn't hide the layer's file and stay writable. A snapshot keeps
    only the workspace's own lines of the file in its tree, and the file as
    seen and as made in its record (`git::layers::Record`), so a layer of
    it never brings another's uncommitted lines or the waiting edits.
  - **A file only editable layers have** (`edited_new`, `ShownLayers`'
    `N` records): excluded, even with an upper copy; every change to it
    waits, and a snapshot leaves it out. A move doesn't count its upper
    copy as work in the way (`moves::own_paths`): once the layer commits
    it, the move brings it into the branch and the copy is an ordinary
    change.
  - **git's size rule, again:** a waiting edit changes the file's size,
    and git takes a size other than the one it recorded as a change
    without reading the file. So before a write burst to such a file the
    workspace zeroes its index entry's size (`unstat_edited`, the index
    editor, skipped while git holds the index: git is then writing the
    file itself), once until the index changes again.
  - **Listed and refused:** `state::waiting` works them out from upper and
    the applied view: per file, how many hunks wait, on whose lines, and
    whether they conflict (markers). The listing and the turn hook's notes
    say `N waiting edits in <file>: they change <A>'s uncommitted lines;
    they become yours to commit once <A> commits them`; `layer add A`
    (see-only), `layer remove A` and `remove --all` refuse (exit 4), listing
    them, while some change A's lines, as for commits that do.
  - **Rejected:** a separate store of waiting edits (a patch per file
    beside upper): upper is where editors write, so a second copy would
    have to be kept in step with every write, and a crash between them
    would lose one; recomputing them from upper and the view is cheap.
    Union files' lines changed by the workspace as waiting edits (the
    guard keeps those off the branch; a union file's lines among the
    others' are the workspace's own by design).
- **Git off as the fallback:** where the index can't be edited (sparse
  checkout, whose re-application rewrites skip-worktree bits; a split or
  sparse index; an unknown required extension; the index locked), the layers
  still show, but the virtual `.git` (now swappable) names a directory that
  doesn't exist: `gitdir: /LAYERS SHOWN (a+b): git is off here (<why>). To
  use git, remove the see-only layers: gitfold layer remove BRANCH` (not
  `--all`, which would also move the branch's commits off its editable
  layers). Every git command then fails
  with that text (exit 128), the registration and branch protection stay, and the `.git`
  file can't be written, so `git worktree repair` can't undo it. Why is kept
  in `layers-git-off`, so it survives remounts. gitfold's own commands find
  the workspace from the mount instead (`cli::workspace_at`). HEAD moves
  never turn git off: switches just wait until HEAD is back on the branch.
- **The base isn't rebased while showing layers** (`sync` waits): a rebase drops
  upper files identical to the new base and copies up files it changes, and
  with a view tree as lower either could turn a layer's content into this
  workspace's own. Commits still move the branch; the base
  catches up once no layer shows, and before layers go on in a workspace
  showing none (`catch_up_before_layers`: the switch rebases first, or is
  refused and retried). A move of the branch's own commits (below,
  "Editable layers") isn't a rebase: it moves the base visibly, as a
  switch, the view built over the new tip.
- **Moves at turn boundaries, by hooks:** a workspace shown as a layer is
  snapshotted when its Claude Code session stops (`snapshot --claude-hook stop`), and
  a reader's live layers move to their latest work (below; for commits, the
  branch tips) when its next prompt is sent (`layer --claude-hook
  user-prompt-submit`), which tells the agent which changed, and records
  when each moved (the listing's `live, last HH:MM`). Neither agent sees
  the other's half-done turn, and nothing the reader read changes
  mid-turn. The agent's own `gitfold layer add` switches at once: a
  boundary it chose. Frozen layers and revisions never move: a reader
  that wants a fixed point says so, rather than turning a layer off. The
  hook records that the reader has an agent (`turns` in its state
  directory, from its first prompt on; its `Stop` hook's `snapshotted` record
  counts too): the daemon then leaves its layers to its turns. Before its
  layers move, the hook moves the branch onto its editable layers' new
  commits (below); editable layers it couldn't move onto are held as they
  showed (their new commits would show as conflicts).
- **Without an agent, by the daemon, once idle:** a reader with neither
  record has no turns (a person works in it), so nothing would move its
  live layers. Its sync thread (`fs::workspace::follow`) checks every
  2 s (`CHECK_EVERY`) and moves them when (1) a layer has newer work (an
  agent's newer end-of-turn snapshot, a tip that moved, or a person's
  workspace written to since its last snapshot) and (2) nothing has written to
  the reader for `IDLE` (3 s), so files don't change under someone
  half-way through an edit, and git is idle in it as a switch wants it
  (HEAD on the branch). The check is cheap: a person's workspace is snapshotted only when
  its write count (counted by every write through the mount, `Activity`),
  its base or its branch tip (`git commit` writes nothing through the
  mount, and the base of one showing layers waits) changed since its
  layer last moved to a snapshot of it, and once it has been quiet for 1 s (a
  save isn't half-seen); a closed one, once per mount and then when its
  base or tip moves. A source counts as seen only once its layer moved;
  one whose layer couldn't (an error, only logged) is tried again after
  `BACK_OFF` (10 s). A move refused as a whole (the switch: git busy, own
  lines that depend on the layers) is tried again after `BACK_OFF`, or once the
  reader is written to: not snapshotted again, stored and restored every round.
  Before its layers move, its branch follows its editable layers' new
  commits, as the hook does (`cli::layer::follow_editable`, below,
  "Editable layers"), looked at only when one of their tips or its own
  moved since. The move is
  the hook's own (`state::follow::follow_live`), so the rules can't
  drift apart; it runs under the layers' `flock`, taken without waiting
  (`try_lock`: a command holding it goes first, the check retries next
  round), and shows the result through `Workspace::switch`, the path a
  `gitfold layer` request takes. Lock order is the commands': the
  `flock`, then the daemon's `layers_lock` inside `switch`; the sync thread
  holds neither while waiting, so nothing deadlocks. It is said in one
  daemon log line, and in the listing's `live, last HH:MM`. Rejected:
  a note in the workspace's inbox (agents read it; a person sees the files
  change, and notes would pile up unread); moving at once on every change
  (files changing mid-edit); snapshotting every shown workspace every round
  (a snapshot walks its changed files).
- **A live layer follows an agent's turns, or a person's work as it is:**
  a live layer of an agent's workspace moves to its work as it was at the
  end of the agent's last turn; of a workspace without an agent, to its
  work as it is when the layer moves. Which one a workspace is, its
  own `Stop` hook tells: each run records itself in the workspace's state
  directory (`snapshotted`: `<snapshot> stop`, or `- stop` when no layer showed it,
  so no snapshot was made). A workspace with that record has an agent, and its
  layers follow only the snapshot recorded there (none: they stay as they
  are), never one taken half-way through its turn; `layer add` takes that
  one too, or one now if there is none (`--frozen`: one now). Any other
  workspace (no record, or one in another form) has no
  turns: a snapshot is taken at each reader's turn start, when the daemon
  moves a reader's layers (above), and on `layer add`, so it follows
  every time, not once. A proposal accepted into an agent's
  workspace is recorded the same way (the owner's own boundary), so
  followers see it at their next turn. The record is in the workspace's
  state directory, not the highest-numbered ref: snapshot refs outlive the
  workspace (layers name them, and the id comes from the branch name), so
  after `gitfold remove` and a new workspace of that branch the newest ref
  is an earlier workspace's work. The state directory goes with the
  workspace, so its record can't be stale; the snapshot it names is kept from
  pruning. Rejected: following the newest snapshot whoever made it (a reader's
  turn start would then snapshot an agent's workspace half-way through its
  turn); deleting a workspace's snapshot refs on `remove` (refs that layers
  name must stay, so the newest could still be old); numbering
  incarnations in the ref names (a new ref layout, for the same result).
- **Snapshots of one workspace at once** (its `Stop` hook and readers' turn
  starts) are numbered, given their ref and recorded under an `flock` on
  `snapshot.lock` in its state directory, so none takes another's number
  (`update-ref` creating the ref would fail). In a reader's turn start, a
  layer that can't move (its workspace's snapshot failing) is left as it is,
  and the others still move.
- **Read-modify-write of the layers under a lock:** `layer add`/`remove`,
  the hook moving live layers, `propose`, and `accept`/`reject` putting a
  layer back all read the layers file, change it and write it whole;
  without a lock two at once lose one's change. Each holds an exclusive
  `flock` on `layers.lock` in the workspace's state directory across the
  read, the change and the daemon's switch (the daemon reads the file and
  takes its own per-workspace lock; when it moves a workspace's layers
  itself, it takes the `flock` without waiting, before that lock, so this
  can't deadlock). The
  repository lock (`lock.rs`) can't serve: a mounted daemon holds it for
  its life.
- **The layers file's format:** `version 1`, one line per layer: `layer
  <name> TAB <work|commits-only|commits> TAB <live|frozen|fixed> TAB <when
  it last moved, seconds since the epoch> TAB <commit> TAB <see|edit>`, a
  proposal's line followed by `instead` and the same fields for the layer
  it stands in for. No other version is read: files of the builds before
  editable layers aren't. `layers.next`, while a move that changes what
  the branch builds on is under way, is `onto <commit>` and then a layers
  file (below, "Editable layers").
  The README names these "the layers files" and leaves the formats here.

**Rejected:**

- Other index tricks, tested on git 2.49 before building: the layers'
  versions staged in the index (`status` shows them as staged, and even a
  plain `git commit` commits them); `assume-unchanged` (a performance
  promise: `stash`, `restore` and `reset --hard` revert the layers' files,
  and `stash` lost an edit to a file both sides changed); an index per layers
  swapped on toggle (what it adds over skip-worktree is staged entries, which
  it loses or leaks into the other view).
- `git update-index --[no-]skip-worktree --stdin` for the toggles (~115 ms
  for 500k entries; kept as `set_skip_worktree_by_git`): the editor is
  faster, and can also set an entry's stat data and id.
- `info/exclude` for the layers' new files: shared by every worktree.
- A deliberately broken index, or no `.git`, as git off: git says
  "index file corrupt" and the usual repairs undo it; without `.git`, git
  finds the enclosing repository and succeeds.
- Layers following the other workspace live, every write at once: rewriting
  the reader's index while its git may run, and a view changing in the
  middle of its turn.
- Rebasing the base under the layers: see above; it waits instead.
- A layers commit L (HEAD detached at a commit holding the unions, the
  commits made on it copied onto the branch): HEAD detached, `HEAD~1` and
  printed ids off, an amend window, git turned off when HEAD moved; it was
  safer only for libgit2 tools' commits (E3, patches/06 in the design
  notes). Replaced by the filter and the guard.
- Per-process file contents (git sees one file, the editor another):
  telling git apart is a guess, and a wrong guess commits the layers'
  lines silently.

### Proposals: a layer's files changed in a fork, with the consent of the affected

**What:** a workspace that needs a layer's file changed (a contract a root
agent wrote, shown read-only in each child) proposes the change with
`gitfold propose <path>`: the layer's workspace is forked (`branch
--with-changes`, through the daemon when mounted) into `<layer>-prop/<ws>`,
recorded in config (`gitfoldProposal`, `gitfoldProposedBy`) with what it
started from (its first snapshot) kept by `refs/gitfold-proposal/<id>`, and put
in the layer's place among the proposer's layers. The proposer edits the files
in the proposal's workspace, where they are its own. Its changes are its
snapshot against that start. `gitfold consent` builds what accepting gives (the
layer's work as now with those changes) and, for each workspace showing the
layer or a proposal to change it, checks that its own work merges with it
and runs a test in its view with it; `gitfold accept`, given a current
passed consent, writes the changed files into the layer's workspace, puts
the layer back in place of the proposal, tells the followers, and removes
the proposal (workspace, branch, start ref, snapshots no layers file names).

**Why:**

- **Edits where the files are writable:** the alternative, routing writes
  to a layer's path into a fork from inside the reader's workspace, would
  need a second upper per workspace and a write path that changes meaning
  with the layers. A fork is a workspace like any other: editors, builds
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
  is. So the checked commit is the latter (the layer's snapshot now, its tree
  edited with the proposal's changes), and the record holds both the
  proposal's and the layer's snapshot: a later edit to either makes it stale,
  and accept refuses a stale or failed one (`--force` overrides). Accept
  also writes exactly the consented snapshot: a proposal edited since (or
  half-way through an edit) differs from it and is refused, and one
  edited while it is being accepted is kept with only those later edits.
- **Each follower's view, not its snapshot alone:** the test runs in a
  snapshot of what the workspace would show: its view tree rebuilt with
  the accepted commit in place of the layer (and of any proposal to change
  it), its other layers as they are, its own work on top (union files
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
  Snapshots a pending switch still names stay until a later retire sweeps the
  refs of proposals that are gone.
- **Accepting is writing, not merging:** the layer's workspace gets the
  files as uncommitted changes through the mount (as its agent would write
  them, so the kernel and git are told as for any write), or offline into
  upper and the markers under the repository lock; the root commits as
  usual. Workspaces showing the layer move at their next turn by the
  existing hook (the layer is snapshotted at once, recorded as its agent's
  boundary if it has one); the proposer is switched back at once, its
  layers stored first, so a switch refused now happens when it can. The
  proposer's own proposal has no agent of its own (its edits are made
  from the proposer's session, whose `Stop` records only its own
  workspace), so the proposer's `UserPromptSubmit` hook snapshots it each turn.

**Rejected:**

- A write policy `propose` on a layer (design §1.3), redirecting writes
  in place: see above.
- Checking only the workspaces whose read records hold a changed file:
  those records miss what builds read, so a proposal could pass having
  checked nobody (kept as `--only-readers`).
- Mixing the follower's snapshot with the proposal's work: that tests the
  layer as it was when proposed, not what accept writes, and leaves out
  the follower's other layers.
- Merging the proposal into the layer's workspace on accept: the start
  makes a 3-way merge possible, but a merged contract nobody tested is
  what consent exists to avoid; the refusal sends it back instead.

### Editable layers: moves computed by plumbing, made by the workspace

**What:** a layer can be **editable** (`gitfold layer add a --edit`,
`gitfold branch b --edit a`): branch b builds on a. Its own commits sit
on a's commits, a's committed work is then b's own history (writable like
any file), and a's uncommitted work shows on top as any layer's does; a
path only editable layers changed is writable (`View::writable`). Every
branch without one sits on the trunk. b's own commits are `B..b`, B kept
by `refs/gitfold-own/<b>`: the commit of a that b sits on, or with
several editable layers, a merge of their tips that gitfold makes
(`git::stack::merge_parts`: identity `gitfold`, 2020-01-01, so the same
tips make the same commit; message `gitfold: built on a + c` and an `on
<name>` line per part, which `parts_of` reads back, so which commit of
each layer b sits on needs no record of its own), rebuilt when any of
them moves. When what b should sit on changes (a's tip moved, a layer
added, made see-only or removed, a landing followed), b is **moved**
(`cli::layer::moves::move_onto`, one path whatever asked): the target
commit is worked out first (the parts merged, or the trunk's tip with no
editable layer left; a conflict between the parts moves nothing and names
the paths), then the move (`git::stack::compute_move`): each commit C of
`B..b`, oldest first, becomes `merge_trees(base = C^, ours = the copy so
far, theirs = C)` with C's author, committer and message
(`git::layers::copy_commit`). A commit made empty is dropped; a merge in
`B..b` is refused; a conflict moves nothing and is recorded
(`move-blocked`: `<onto> <commit> <paths>`, so the hook says it once per
commit and target, and `list` notes it). Coming off a layer, the commits
that don't apply without it (`git::stack::dependent`: they change its
lines) are listed and nothing moves. Then b moves where it lives:

1. **No workspace:** one `update-ref --stdin` transaction, b `C → C'` and
   the own ref `B → A`, each compared-and-swapped. Refused if a git
   worktree has b checked out (naming the `git rebase --onto` to run
   there).
2. **A workspace,** mounted or not, layers shown or not
   (`fs::workspace::moves`): mounted, the daemon does it (a `move` control
   request with the intent); unmounted, the command, under the repository
   lock. Under the intent (`state::moves`): the index moved from C to C'
   on the paths the move changes, without the files (`update-index
   --index-info`; union files' entries left to the layers, which set
   them); the workspace's base moved to C', durably, by a layer switch
   that also moves the base (`Workspace::switch_with`), the view built
   over C' with the layers the move leaves, the files it changes stamped
   now; the refs in one transaction. Uncommitted work elsewhere stays as
   it is, mounted or not.

Asked moves (`layer add`/`remove`) say first what they move (`moves 3
commits of ui onto api`) and move on demand; the `UserPromptSubmit` hook
moves the session's workspace at its agent's turn start onto its live
editable layers' new commits; in a workspace without an agent, the
daemon's idle follow (`fs::workspace::follow`) does the same first, once
its editable layers' or its own tip moved since it last looked (the move
asked of the daemon itself, on a request thread of its own, while the sync
thread holds the layers' `flock`), so a person's waiting edits become
ordinary changes too once the layer commits. A move that changes what b builds on stores the
layers it will have in `layers.next` (`onto <A>`, then the layers) before
anything moves; they become b's layers once its own ref is at A
(`state::layers::settle`, by whoever reads them next), and are dropped
if it isn't. So a refusal or a move that doesn't happen leaves b built
on what it was. A chain is consistent bottom-up without a lock across
workspaces: if p2 was moved and p3 hasn't been yet, p3's own ref is p2's
old tip, so its next move replays exactly its own commits.

**Why:**

- **Where its own commits start is stored, not derived.** With only
  `merge-base(a, b)`, a rewritten a (amended, rebased, force-pushed) makes
  the next move replay a's old commits, which conflicts on a's own
  rewritten lines (the design's experiments e4d and e4e). Graphite stores
  the same pair. Without the ref (a branch not built on anything yet),
  its own commits start where it left the trunk, or where it and the
  target met, whichever is newer.
- **A ref for B:** the transaction moves b and B together, so a crash
  can't leave b moved with B stale. B is an ancestor of b, so the ref adds
  no `gc` risk.
- **The merge of several editable layers as an ordinary commit:** b's
  own commits then have one parent to sit on, `B..b` stays their range,
  and a pull request, `git log` and `git rebase` see plain history. The
  fixed identity makes it the same commit for the same tips, so rebuilding
  it when nothing moved changes nothing. A pull request for b shows every
  part's commits until they land: adding a second editable layer says so.
- **The layers file for what a branch builds on, in one place:** which
  layers are editable is a field of the layers file, also for a branch
  without a workspace (its state directory `.git/gitfold/<id>/` then holds
  little else; it isn't a workspace). One format, read and written under
  one lock (`layers.lock`), for the listing, the hook, `list`'s tree and
  the view; `gitfold remove` keeps a branch's editable layers, and a fork
  copies them with the own ref (`state::layers::copy_editable`). The
  trade-off: `git branch -m` renames neither (config was renamed with the
  branch): the layer is added again under the new name.
- **`layers.next`, settled by the own ref:** the layers file can't join
  the ref transaction. Written before and settled after, keyed by where
  the own ref must be, a crash at any point leaves b built on what its
  refs say: never on a layer it was moved off, nor on one it wasn't moved
  onto.
- **Computed first, then moved:** the computation needs no worktree, index
  or lock and changes nothing, so a conflict leaves no half-done state in
  an agent's worktree (`git rebase` would leave `rebase-merge` there
  mid-task). What is left to refuse when moving (work in the way, git busy)
  also changes nothing.
- **One move path, through the workspace:** a move changes the base, and
  the base can't move under a view tree by a rebase ("The base isn't
  rebased while showing layers"). So the move is a switch, which tells the
  kernel and git what changes, moving the base too: the view is built over
  C' with the layers as they will be, so the move's changes show (unlike a
  rebase, which keeps what the workspace shows), and an editable layer of
  a then shows only what a hasn't committed. The workspace's own
  uncommitted work stays in upper; one on a path the move changes is in
  its way and the move waits (`uncommitted changes to <paths> overlap the
  move; commit or stash them`, as `git read-tree -m` would refuse), as do
  staged changes there (`staged changes overlap the move; commit or
  unstage them`). Upper copies of committed files at those paths (left by
  commits made while layers showed: the base waited) aren't work and are
  dropped (`stale_copies`), as are files written back as they show, so
  the move's version shows. Done by the daemon while mounted (it holds the
  workspace's state in memory, as for any switch), by the command under
  the repository lock otherwise: the same code either way, so an
  unmounted workspace with uncommitted work moves too.
- **The intent file, locked while it runs:** a crash between the steps
  would leave the index, the base and the branch disagreeing. The intent
  (`<C> <C'> <A> <old own ref>`) is written and `flock`ed under another
  name and hard-linked into place, so nobody sees it unlocked, and stays
  locked by whoever does the move. Anyone else (the sync thread, the next
  hook, `layer add`) recovers one nobody holds: tip at C' → set the own
  ref if it isn't yet; tip at C and the index at C' on the changed paths
  → move the refs if the base moved, else put the index back; tip and
  index at C → forget it; the branch moved on and the index describes it
  → forget it; an intent it can't read, or anything else → leave it and
  report (`Recovery::Stuck`), and `gitfold layer remove --all` forgets it
  (every layer removed, nothing moved). A recovery that fails otherwise
  (a base it can't read, git failing) is an error: `remove --all` says it
  and forgets nothing. While one is there, the layers don't change (layer
  changes are refused: `<b> is being moved (or a move of it was
  interrupted ...); retry once it is done`).
- **With the intent held, the refs are read again:** a move computed
  before another one of the same branch finished (the hook and `layer
  add` at once) would otherwise move the files against the other's result
  (`moved_meanwhile`).
- **git's index lock from the index through the refs:** a `git commit`
  between the index step and the ref step would commit the moved index
  onto C, squashing a's new commits into one of b's. So the move creates
  `index.lock` itself (`state::moves::IndexLock`, git's own protocol:
  `git commit` then fails), writes the index through a copy renamed over
  it, checks the branch again (a commit may have ended since the move was
  planned), and removes the lock right after the ref transaction. The lock
  holds a marker line, so a crash's leftover is removed by the next
  recovery (it holds the intent: no move is running), and by the sync
  thread before it checks whether git is idle.
- **Never pushes:** a move prints `git push --force-with-lease=b:<tip>
  origin b:b` with the remote-tracking tip as the lease, and `list` says
  the same (`push with: ...`) while that tip isn't in the branch. The
  tip is b's branch of the same name on the remote b pushes to (its
  upstream's remote, else `origin`), never the upstream branch itself: a
  branch started from `origin/main` tracks main, and a force-push onto
  main is never what to run.
- **Pushed commits only with `--yes`:** an asked move says what it moves
  first (or, refused, what it would have moved, then why, with no
  `error:` line after), and refuses (exit 4) if any of `B..b` is
  reachable from that same remote-tracking branch: a rewrite of published
  history is asked for, not done in passing. The hook's moves, which
  follow what was already asked, aren't refused; they say the push to run
  when one is needed, and say nothing of a rewrite when b had no commits
  of its own (it only moved up with a).
- **Editable layers the hook couldn't move onto are held** as they showed
  (`follow_live`'s `hold`): their new commits, not under b's, would show
  as their changes, conflicting with the lines b's commits already have.

**Rejected:**

- `git rebase --update-refs` from the top of the chain: it skips branches
  checked out in other worktrees, tearing the chain (e2a); `git rebase` of
  a branch checked out elsewhere fails (e1).
- `update-ref` alone (or `git replay`): moves the branch, not its files;
  the sync thread would then keep the old view, the move showing as a
  staged reverse change (e4).
- `git reset --keep C'` in the workspace: a staged new file became
  untracked (e7a).
- `git read-tree -m -u` through the mount (an earlier design): the base
  couldn't move under shown layers, so they had to be taken off for the
  move and put back (a marker, two extra switches, and crash states of
  their own), the move's files were written into upper and dropped by the
  next rebase, and an unmounted workspace could move only without
  uncommitted changes.
- The edge in branch config (an earlier design): a second place for what
  a branch builds on, next to its layers, and records of its own for a
  landing in progress, since config can't join the ref transaction.
- Replaying `merge-base(a, b)..b` and dropping commits that become empty
  (jj-style): conflicts after a squash merge (e4d).
- Choosing what to build on from dependencies between changes: semantic
  dependencies (calling a's new function from b's own file) are invisible
  to them; building on is the user's choice.

### An editable layer that landed: found by content, per layer

**What:** before the hook moves a branch, and in `list`, the listing
and `layer remove`, gitfold asks of each editable layer a whether the
commit of it the branch sits on (its part, B) landed in a's **target**:
the one branch a builds on itself, if it has exactly one editable layer
(stacked pull requests merged into each other), else the **trunk**
(`gitfold.trunk`, else the branch `origin/HEAD` names, else `main` or
`master`). `git::stack::landing`, cheapest first:

1. B in the target's history, brought in by a merge (of B, or of commits
   a got later): the first commit of the target's first-parent line that
   has B, found by bisection (once one has it, every later one does);
   landed there unless that is B's own child on the line. On the line
   itself, B landed by a fast-forward, taken only once a's branch is gone.
2. Else, if B has changes since it and the target diverged (MB): the
   first commit S of `rev-list --ancestry-path MB..target -- <paths B
   changed>` (the paths on its standard input) where `merge_trees(MB, S,
   B)` is S's tree, i.e. merging B there changes nothing: a squash, the
   last commit of a rebase merge. The newest 2,000 such commits at most,
   logged when that cuts the walk short.

The trunk includes the local branch tracking it (`main` for
`origin/main`), and can't be an editable layer: every branch sits on it
already, and a branch built on it would be moved on every pull, its own
commits rewritten by `pull --rebase` taken for landed. Its tip, to move
onto, is the newer of the local branch and the configured trunk (the
configured one if they diverged).

If a landed, following it (`after_landing`) moves the branch's own
commits onto S like any move (the same intent and transaction, its own
ref moved to S), a gone from its layers; landed in another branch T,
T becomes an editable layer (if it isn't one already); with other
editable layers left, the commits sit on a merge of S and their tips
(the trunk kept as a part at S). Several landings followed at once keep
the newest commit of their target (one landing after another may be
processed out of order; two that diverged both stay, merged). The hook does it unasked only once a's
branch is gone (`land::gone`: deleted here, or its upstream deleted on
its remote and fetched with `--prune`), for every kind of landing: a
merge into main may be one of several while a goes on, and a squash is
found by content. Until then it is said (once, by the hook: `a looks
merged into main as <S>, but a is still there: gitfold layer remove a
moves b onto main with only its own commits`) and noted in `list` and
the listing, and `gitfold layer remove a` follows it, asking `--yes` for
pushed commits. Answers are cached per (B, target tip, whether a is gone)
in `.git/gitfold/.landings`: a landing while the target still has it, or
"not landed up to tip T", so after a fetch only `T..` is walked. A
landing detection missed is moved by hand: `gitfold layer remove a`
moves b's own commits off a, onto its other editable layers or the
trunk's tip.

**Why:**

- **The commit the branch sits on, not the layer's branch:** a fix a got
  after its squash is a's business; the branch only needs what it sits
  on to have landed.
- **Per editable layer:** with several, each lands on its own, in its
  own target; the merge the branch sits on is rebuilt from what is left.
- **Content, not history, for squashes:** a squash or rebase merge makes
  new commits. "Merging B changes nothing" caught a squash, a rebase
  merge, a squash the maintainer tweaked elsewhere, and one after main
  changed context lines near a's (the design's e4a, e5a, e5b, e5d); the
  path-limited walk finds it after main edited the same lines again (e6),
  and yields S. Unmerged layers said no (e4b, unit tests).
- **Onto the landing commit S, not the target's tip** (the design's open
  question 2): it is the smallest rewrite, only the branch's own commits,
  onto exactly what a became. Nothing else the trunk did since lands in
  an agent's workspace mid-task, and a conflict can only be with a's
  landed form. The pull request shows the same diff either way.
  Following the trunk's tip after that is an ordinary rebase.
- **Landed in the trunk, no editable layer left:** the branch sits on
  main, as a branch never built on anything, so `list` doesn't show it
  under main, and nothing follows main by itself: teams differ on
  rebasing onto main automatically, and every fetch would rewrite every
  bottom branch.
- **Guards:** B with nothing of its own since the merge base is
  trivially a no-op merge, so it never counts. B on the target's
  first-parent line is also what a fresh a (no commits yet) looks like,
  so a fast-forward is taken only once a's branch is gone. Main
  independently making the same change looks like a squash: the content
  is there, but the branch would leave a layer that may still be going
  on. So a squash is said to "look" merged, and every kind of landing is
  followed unasked only once a's branch is gone; `layer remove a`, asked
  for, follows it at once.
- **The layer's own layer as target:** a stacked pull request is merged
  into its base branch; looking only at the trunk would miss it until the
  whole chain lands.

**Rejected:**

- Per-commit patch ids (`git cherry`, what `git rebase` skips by): they
  don't recognise a squash of several commits (e4a).
- The patch id of the whole range `diff MB..B` against each trunk commit:
  missed squashes where context lines changed (e5b, e5d).
- The forge's answer (`gh pr view`) as the source: gitfold shouldn't need a
  forge; content settles all but a squash rewritten at merge time, which
  `layer remove` covers.
- Following onto the trunk's tip: pulls unrelated changes into the
  branch mid-task and widens what can conflict.
- A record of whether B was on the target when set (an earlier design),
  to take fast-forwards while a's branch exists: with landings followed
  unasked only once a is gone, the branch being gone is the guard.

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
| Layers' versions staged, `assume-unchanged`, an index per layers | commit theirs, revert theirs, or lose staged entries; skip-worktree instead | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| `info/exclude` for layers' new files | shared by every worktree | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| Broken index or no `.git` as git off | repaired away; git finds the enclosing repository | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| Live layers | index rewritten under a running git; views change mid-turn | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| A detached layers commit for union files | detached HEAD, ids and `HEAD~1` off, git off on HEAD moves | [layers](#layers-other-workspaces-work-as-a-view-tree) |
| `git rebase --update-refs`, `update-ref` alone, `reset --keep` for moves | skips other worktrees' branches; moves no files; loses staged state | [moves](#editable-layers-moves-computed-by-plumbing-made-by-the-workspace) |
| `read-tree -m -u` through the mount, layers taken off for a move | two extra switches and crash states of their own; unmounted only without uncommitted work | [moves](#editable-layers-moves-computed-by-plumbing-made-by-the-workspace) |
| The edge in branch config | a second place for what a branch builds on; records for landings in progress | [moves](#editable-layers-moves-computed-by-plumbing-made-by-the-workspace) |
| Patch ids (`git cherry`, a whole range's) for squash merges | miss squashes of several commits, or with context changed | [landings](#an-editable-layer-that-landed-found-by-content-per-layer) |
| The merge base instead of a stored own ref | a rewritten layer's old commits replayed | [moves](#editable-layers-moves-computed-by-plumbing-made-by-the-workspace) |

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
- **A workspace's own file wins whole, except union ones.** A file both
  workspaces changed shows this workspace's version only, unless marked
  `merge=union`; a layer's change conflicting with the base's or another
  layer's shows conflict markers the reader can't resolve here (it can
  only remove the layer, or ask its agent to update).
  Union merging and the filter assume a union file's lines are unique and
  their order doesn't matter where both sides append.
  A snapshot's record places its lines among the lines of the layers a view
  shows, not among the reader's own: in A's workspace, B's lines added
  inside A's block are placed by the base's lines (A's lines are in its
  upper copy, not in the hunk tree), noted, and A's copy gets them where
  the carry-over puts them. Doing better needs the carry-over to apply
  the record to the upper copy itself.
  git can't see a change to a layer's lines in a union file (a deletion,
  a formatter's rewrite): clean takes them out as the layer's whatever the
  workspace did to them, and one it no longer recognises shows as the
  workspace's own, which the guard then refuses to commit (in a union file;
  in a file only editable layers changed it is a waiting edit). libgit2-based
  tools ignore the filter and run no hooks: they see the layers' lines as
  changes, and a commit they make lands on the branch with them; the daemon
  warns once it sees the branch move (`state::guard_check`), and `git push`
  (pre-push) refuses it. A
  layer pins a snapshot that only the newest 16 snapshot refs keep: one
  not moved while its workspace snapshotted 16 times more can be pruned by `git
  gc`. Lost skip-worktree bits (`update-index --no-skip-worktree`, an index
  rebuilt by hand) aren't re-derived until the next switch, nor is the
  excludes file regenerated when the user's global ignore file changes
  (it copies that file). A fork copies only the editable layers (and the
  own ref), but the index and upper as they are (skip-worktree bits, and
  in union files the layers' lines, which become the fork's own).
- **Waiting edits, as diff matching sees them.** Which of the workspace's
  changes depend on an editable layer's uncommitted lines is decided per
  hunk of a line diff from the file as shown (`git::filter::Edited`):
  a hunk next to the layer's lines on one side only is the workspace's own,
  placed in the base's version after the base's line before it, which may
  read differently from where it sat (git's union resolution and the
  memo keep git's view and the file consistent). A union file's changes to
  the layers' lines aren't waiting edits (the guard refuses them). The
  index's size of a file only editable layers changed is zeroed before
  each write burst to it (`unstat_edited`), one index rewrite per burst: a
  git command between the view changing and git being told can still see
  a waiting edit as a change, once. In a snapshot's tree a workspace's
  waiting edits are left out (only its record has them), so a layer of it
  shows its own lines only; a file only editable layers have is left out
  whole. Moves carry uncommitted work over only in files mixed with the
  layers' lines: elsewhere, uncommitted work on a path a move changes makes
  it wait, and a conflict blocks it until rebased by hand. `git branch -m`
  doesn't rename a branch's editable layers or own ref (they are found by
  name).
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
- **Layers, next phases.** Waiting edits in union files (today the guard
  refuses a change to a layer's line there). Line ids, if diff matching
  turns out to misplace waiting edits (the design's Stage 2). Proposals: a 3-way merge in `accept` instead of the refusal when
  the layer changed the files since (only safe once the proposal is
  rebased onto the layer's work); several proposals to one layer checked
  together (`consent` checks each alone); the `deps` log of reads of
  layers' files (design §4.3), so `consent` knows reads per snapshot rather
  than per path.
- **Editable layers, next phases** (see
  [moves](#editable-layers-moves-computed-by-plumbing-made-by-the-workspace)).
  *Landings:* a forge's answer as a hint (`gh pr view`), fetching before
  checking, and blocked moves reaching the branch's inbox, once per
  layer tip. *Moves and layers together:* the hook switches twice at a
  boundary that moves both the commits and a live layer's work (once for
  the move, once following); one switch would do. Uncommitted work that
  overlaps a move outside the files mixed with the layers' lines could be
  carried over with a 3-way merge, as `carry_over` does for unions,
  rather than waiting. A note at commit time when a commit made while a
  see-only layer showed may need its code (`layer add --edit`). *Also:* a
  real `git rebase --onto A B` in the workspace for a blocked move,
  merges among a branch's own commits (flattened), re-signing copies when
  `commit.gpgSign` is set, an undo (the previous intent's C and B, if the
  branch is still at C'), pull requests built on several layers beyond
  the warning, and sharing `refs/gitfold-own/*` across clones.
