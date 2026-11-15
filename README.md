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

Needs Linux with FUSE 3 (`fusermount3`, kernel 5.1+) and git.

### With Claude Code

1. **Install the plugin**, in Claude Code:

   ```
   /plugin marketplace add pkukielka/gitfold
   /plugin install gitfold@gitfold
   ```

   It downloads gitfold when first needed (Linux x86_64 or aarch64, glibc
   2.28 or newer; on NixOS, [build it](#by-hand) and put it on `PATH`).

2. **Mount** your repository, once: in a Claude Code session in it, ask Claude
   to "mount this repository with gitfold" (at `~/mnt/<name>` unless you say
   where). It stays mounted after the session, and is mounted again by itself
   after a reboot.

3. **Work**: `claude -w my-task` from the repository (subagents with
   `isolation: worktree` too) gets a gitfold workspace, opened in about a
   second, at `<mount point>/worktree-my-task`. Removing it when the session
   ends discards its uncommitted changes; the branch stays.

The skill's "Claude Code" section has the details
([skills/gitfold/SKILL.md](skills/gitfold/SKILL.md)).

### By hand

1. **Install**: download `gitfold-x86_64-linux` (or `-aarch64-`) from the
   [releases](https://github.com/pkukielka/gitfold/releases) (glibc 2.28 or
   newer), check it against `SHA256SUMS`, and put it on `PATH` as `gitfold`;
   or build it (Rust 1.93+):

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
   gitfold branch my-task --from main          # create a branch...
   cd ~/mnt/repo/my-task                       # ...and work in it
   echo fix >> README.md
   git commit -am "Fix" && git push -u origin HEAD
   ```

4. **Clean up** the workspace when you no longer need it (its commits stay on
   the branch), mounted or not, and **stop** the mount when you are done with it
   (both from outside the mount):

   ```
   cd ~/src/monorepo
   gitfold remove my-task
   gitfold unmount ~/mnt/repo
   ```

The rest of this page is reference: [How it works](#how-it-works) ·
[Requirements](#requirements) · [Usage](#usage) · [Snapshots](#snapshots) ·
[Workspaces](#workspaces) · [Layers](#layers) · [Running it](#running-it) ·
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
[Cleaning up](#cleaning-up)), and `refs/gitfold-seal/*` and
`refs/gitfold-lens/*` that keep snapshots of work and layers commits (see
[Layers](#layers)).

## Requirements

- Linux with FUSE 3 (`fusermount3` on `PATH`, access to `/dev/fuse`); no root
  needed. Tested on 6.12; kernel 5.1 or newer is required for the no-opendir
  mode gitfold depends on.
- git with linked worktree and index v4 support (any git from the last decade;
  tested with 2.49).
- A full clone, or a partial clone (`git clone --filter=blob:none`, or
  `tree:0`; git 2.29 or newer) whose promisor remote gitfold can fetch from
  without a prompt (an ssh agent, a credential helper); see [Partial
  clones](#partial-clones).
- To build: Rust 1.93 or newer (edition 2024; developed with 1.99), or Nix on
  x86_64-linux (`flake.nix` provides a dev shell with the toolchain). Without
  installing, the binary is `target/release/gitfold` after
  `cargo build --release`.

## Usage

```
gitfold mount [OPTIONS] <REPO> <MOUNTPOINT>   # starts in the background
gitfold unmount <MOUNTPOINT>                  # stops it
gitfold version [<MOUNTPOINT>]                # this build, and its daemon's
gitfold branch <BRANCH> [-w] [--from <REV> | --base <BASE>] [--prewarm <MODE>] [--track-reads on|off]
                                              # create a branch for a workspace
                                              # (-w: with a workspace's changes;
                                              # --base: built on BASE)
gitfold list [--stat]                         # workspaces and their changes
gitfold changes [<BRANCH>] [-z] [--stat]      # a workspace's changed paths
gitfold send <BRANCH> [-s <SUBJECT>] [--from <NAME>] <TEXT>
                                              # a message to a task's inbox
gitfold inbox [<BRANCH>] [--wait [--timeout <SECS>]] [--peek] [--json] [--claude-hook <EVENT>]
                                              # take the messages in it
gitfold watch [<BRANCH>:]<DIR> [--timeout <SECS>] [--json]
                                              # wait for a change in a task folder
gitfold overlaps [<BRANCH>] [--against <REV>]... [--against-workspaces] [--reads|--writes] [-z]
                                              # what it read or changed that
                                              # changed elsewhere since
gitfold remove <BRANCH|@REV> [--force] [--check] [--wait <SECS>]
                                              # delete a workspace, or a snapshot's
gitfold prune                                 # delete snapshot registrations
gitfold prewarm <BRANCH> <MODE>               # a workspace's prewarm mode
gitfold track-reads <BRANCH> on|off           # record a workspace's reads
gitfold snapshot [<BRANCH|REV>...] [--commits] [--onto <REV>]
                                              # workspaces' work (merged), as a snapshot
gitfold layer [-w <BRANCH>]                   # a workspace's base and layers
gitfold layer add <BRANCH|REV> [--frozen] [--commits] [-w <BRANCH>]
                                              # another branch's work shown in it, read-only
gitfold layer remove <BRANCH>|--all [-w <BRANCH>]
gitfold propose <PATH>... [--layer <L>] [--from <DIR>] [-w <BRANCH>]
                                              # change a layer's files, in a fork of it
gitfold proposals [<L>]                       # the open proposals
gitfold consent <PROPOSAL> [--test <CMD>] [--timeout <SECS>] [--only-readers]
                                              # check it with every workspace showing the layer
gitfold accept <PROPOSAL> [--force] [-w <L>]  # take it into the layer's workspace
gitfold reject <PROPOSAL> [--reason <TEXT>] [-w <BRANCH>]
gitfold layer [-w <BRANCH>] base <BASE> [--frozen] [--yes]
                                              # build a branch on another's commits
                                              # (the trunk: off the stack)
gitfold layer base --all [--yes]              # every such branch, onto its base
gitfold branch --claude-hook worktree-create  # Claude Code's worktree hooks
gitfold remove --claude-hook worktree-remove  # (the plugin's; see the skill)
```

Commands other than `mount` work on the repository the current directory is
in, as git does (a workspace under the mount included). `-C <PATH>` runs any
command as if started in `<PATH>`, as `git -C` does. For `mount`, `<REPO>` is
the repository's top-level work tree or its git directory (not a
subdirectory). `<MOUNTPOINT>` must be an existing directory you own; use an
empty one, since its contents are hidden while mounted. A repository can be
mounted only once at a time (a second `gitfold mount` of it fails): two mounts
would share, and corrupt, the stored workspace changes. The mount is read-write
(for workspaces); snapshots are read-only (see [Snapshots](#snapshots)).

Exit codes, the same for every command: 0 success, 1 an error, 2 a usage
error, 3 not found (no such branch, workspace or snapshot, or not a valid
name), 4 uncommitted changes or a git operation in progress, 5 in use or busy.
Six commands add their own: `version <MOUNTPOINT>` (3 another build, 4 a
daemon too old to tell, 5 no answer), `overlaps` (6 something overlaps),
`snapshot` (4 conflicts), `consent` (4 a workspace fails, or none checked),
`layer base` (4 a branch not moved: commits already pushed, a conflict,
uncommitted work in the way, git busy, layers shown) and `inbox
--claude-hook rewake` (2, to wake the session).
`gitfold branch` starts the branch at HEAD unless `--from` (or `--base`) says
otherwise.

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
| `--prewarm none\|meta\|content` | `meta` | After opening a snapshot or workspace, walk it in the background to fill kernel caches. `meta` makes the first `find`/`git status` fast; `content` also reads every file, so the first grep is fast too, at the cost of page cache equal to the tree size. The directory is usable while this runs (chromium: 0.70 s for a snapshot, 1.16 s for a workspace). |
| `--prewarm-threads N` | 4 (or half the CPUs, if fewer) | Threads used by each prewarm walk. More only queue in the daemon. |
| `--prewarm-walks N` | 2 | Walks running at once; walks of what is opened meanwhile wait their turn. |
| `--track-reads` | off | Record the files Claude Code reads in workspaces, as its hook reports them, for `gitfold overlaps`. A workspace's own setting wins (`gitfold track-reads`). |
| `--threads N` | number of CPUs | FUSE worker threads. |

A workspace can have its own prewarm mode, kept across remounts in the branch's
git config: `gitfold branch <BRANCH> --prewarm none`, or `git config
branch.<BRANCH>.gitfoldPrewarm none` for an existing branch (read from the
repository's own config file, where `git config` writes it, when the workspace
opens; `--prewarm none` for the whole mount still wins). `gitfold prewarm
<BRANCH> none|meta|content` sets it and, while mounted, applies it at
once: an open workspace is walked now (a read-only session turning into a
coding one), or its walk stopped (`none`). `none` suits workspaces an agent
only reads a few files in: a walk of a tree of over a million files costs over
a million FUSE requests and more than a minute of the daemon's CPU.

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

**Snapshots of work, uncommitted included.** `gitfold snapshot <BRANCH>`
shows a workspace's work as it is now in a snapshot, and prints its path,
`<mount>/@<sha>` (default: the workspace you are in). The commit's tree is
what the workspace shows, ignored files left out (what `gitfold changes`
lists), its parent the workspace's base. A branch without a workspace, a
tag or a commit gives its commit; `--commits` takes only a branch's
commits even if it has a workspace. Several are merged, in order onto the
first (or onto `--onto <REV>`), as `git merge-tree` does, to check work
together:

```
cd $(gitfold snapshot main agent/api agent/client)   # build and test it there
```

Nothing in a workspace, its index or its branch changes. The commits are
made with a fixed identity and date (`gitfold`, 2020-01-01), so the same
work gives the same snapshot (`unchanged since its last snapshot`, on
stderr, for one workspace); a workspace's work is kept by a ref,
`refs/gitfold-seal/<id>/<n>`, numbered from 1, the newest 16 kept. It works
mounted or not: unmounted it prints the commit instead of the path (and
says so on stderr). It exits 4 on conflicts, listing the paths (`conflicts
merging <BRANCH>:`, then how to look at each alone), and 3 if a name is
neither a branch nor a revision (`no branch or revision <NAME>`). An
untracked nested repository is left out (logged).

### Workspaces

**Start.** Create a branch (gitfold only opens existing local branches) and
open it:

```
cd ~/src/monorepo
gitfold branch agent/task-17 --from origin/main
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

**Fork.** `gitfold branch -w <BRANCH> [--from <SOURCE>]` (`--with-changes`)
creates `<BRANCH>` at `<SOURCE>`'s tip with a copy of `<SOURCE>`'s uncommitted
changes and git index: the new workspace shows the same files and the same `git
status`, staged changes included, and from then on each changes on its own.
Without `--from`, the source is the workspace the command runs in. Only the
changes are copied (with reflinks on btrfs and XFS), so a fork costs what they
weigh, not the tree: an agent can try several approaches from the same
half-done state, or hand it to a reviewer. It works mounted or not; a source
never opened as a workspace gives just the new branch. A source with a git
operation in progress (a commit, a rebase) is refused, as is one whose branch
moves during the copy: retry once it is done.

**Overlaps.** `gitfold overlaps [<BRANCH>]` lists what a workspace touched that
changed elsewhere since: `W` for a path it changed (committed on its branch or
not) that the target changed too, `R` for a file it read that the target
changed, then the target and the path:

```
R	main	src/parser.rs
W	main	src/lexer.rs
W	agent/task-18	docs/api.md
```

The target is the remote's default branch (`origin/HEAD`, else `main`), or
each `--against <REV>` (not the branch's upstream: after `git push -u` that is
its own remote branch);
a target's changes are its commits since it diverged from the workspace's base.
A branch with a workspace adds that workspace's uncommitted changes, so
`--against <BRANCH>` gives the paths two workspaces both changed, and
`--against-workspaces` compares with every other workspace. A path counts if
the target changed it, a directory above it, or (a directory replaced by a
file) a path below it. It exits 6 if anything overlaps, 0 if not. `W` is about
paths, not lines: two agents editing different parts of one file usually merge
cleanly (`git merge-tree --write-tree` tells for sure).

`R` lines need read tracking, off by default: `gitfold mount --track-reads`
turns it on for the mount, `gitfold branch --track-reads on|off` or
`gitfold track-reads <BRANCH> on|off` for one workspace, whatever the mount
says (in git config, `branch.<BRANCH>.gitfoldTrackReads`; applied at once
while mounted). Reads are what Claude Code's hook reports (the plugin's
`gitfold track-reads --claude-hook pre-tool-use`): its Read tool's files, and
the files simple viewers in its Bash commands show (`cat`, `head`, `tail`,
`sed`, `less`, `nl` with file arguments), before the tool runs (so a
command's own `cd` counts once, and a call then denied is recorded too). The
filesystem records none (why: ARCHITECTURE, "Overlaps"). Other harnesses get
`W` lines only (`overlaps` notes when a tracked workspace has nothing
recorded). Without
tracking, `overlaps` shows `W` lines and says so; `--reads` then fails
(exit 3). A fork inherits what its source read, and its setting; turning
tracking off keeps what was recorded.

**The task folder.** Every workspace has a `.task` link to a directory for
what belongs to the task but not to its commits: a spec, drafts, notes, a
handoff, a PR description. The directory is `.git/gitfold/.tasks/<id>/`, plain
files outside the mount: kept after `gitfold remove` and after the branch is
deleted (a branch of the same name opened later finds it again), written at
disk speed, and seen at once whoever writes them, an orchestrator or you
included. Git doesn't follow the link, so it never sees or cleans what is in
it (`git clean -fdx` fails to delete the link, warns, and moves on); an entry
in the repository's `info/exclude`, which every worktree shares, keeps the
link out of `git status`. `gitfold branch -w` copies the folder with the
changes; `gitfold list` shows the first line of `.task/status` (what the agent
says it is doing), and writes in the folder count as activity. The link can't
be removed or renamed (`EPERM`). Two settings, in the repository's config:
`gitfold.taskDir` names the link (default `.task`; one name, for a project that
uses `.task`), and `gitfold.tasksDir` keeps the folders elsewhere, an absolute
path or `~/...` (say in your notes; then `<dir>/<id>/`). A workspace whose base
tracks a path of the link's name, or that has one among its changes, has no
task folder (the daemon logs why). Sandboxed agents need write access to the
folder's directory: by default inside `.git`, which committing needs anyway.

**Messages.** `gitfold send <BRANCH> <TEXT>` leaves a message in a task's
inbox, `inbox/` in its task folder (`gitfold.inbox` names it); the branch
needs no workspace yet. `gitfold inbox` takes the messages in the inbox of the
workspace you are in (or of `<BRANCH>`) and prints them: each is taken once,
however many read at a time. `--wait` waits for one if there are none (up to
`--timeout <SECS>`); `--peek` lists them without taking; `--json` for
programs. A fork (`branch -w`) leaves the inbox behind: its messages were for
the source's agent.
`--claude-hook` hands them to Claude Code's hooks: injected into the
conversation when a prompt is sent and after each tool call (not a
subagent's: its context isn't the session's), and waking an idle session when
one arrives (the skill has the configuration). `gitfold watch
[<BRANCH>:]<DIR>` is the general form: it waits until a file in a directory of
a task folder changes, prints which (`C` or `D` and the name, each name's
last change), and exits (`D .` if the directory itself goes); a supervisor
can wait on another task's folder (`agent/b:.`) for its handoff, an agent on
`reviews/` for work.

The format is plain files, for anything to read and write without gitfold:
one file per message in the inbox directory, named `<nanoseconds since the
epoch>-<sender>` (so names sort by arrival), holding header lines (`from:`,
`time:` in UTC, `subject:` if any), an empty line, and the body. A sender
writes a file whose name starts with a dot and renames it into place; a
reader takes a message by renaming it to `.taken-<pid>-<name>` before reading
it, so that it goes to one reader, and deletes it once printed (a message a
dead reader left is put back). Names starting with a dot are never messages.
A file without headers is all body; CRLF line ends are read as LF.
Watchers use inotify on the folder's directory, mounted or not.

**Finish.** Push or merge the branch from anywhere, then remove the workspace
with `gitfold remove <BRANCH>`, mounted or not (see
[Cleaning up](#cleaning-up) for when it refuses). `gitfold list` shows
how many files each workspace has changed and deleted, and how many ignored
files (build output) it holds, how long ago it last changed (`last`: a file
written, created or deleted, or a commit; a stalled agent stands out), with a
note if its branch was deleted or its base is behind the branch tip (mount once
to update it). A branch built on another (see
[Stacked branches](#stacked-branches)) is listed under its base, indented
with `└─` (with `-` for the counts if it has no workspace, as for its base),
the base in the `base` column, noting whether it is frozen, needs an update,
is blocked, or needs a force-push; `--stat` adds the lines added and removed
in total, reading every changed file. `gitfold changes
[<BRANCH>]` lists one workspace's uncommitted changes against its base,
untracked files included and whatever is staged, as `git add -A` and `git diff
--cached --name-status` would (`A`, `M`, `D` and the path, quoted as git quotes
it; `-z` for NUL-terminated, unquoted lines); run inside a workspace, it is
that workspace's. `--stat` adds the lines added and removed per path (`-` for
binary files), as `git diff --numstat` against the base would, and git's
`--shortstat` summary line. It reads the workspace's stored state, not every directory
of the tree as `git status` does, so a supervisor can poll many agents'
workspaces cheaply. Until it is removed,
git treats the branch as checked out, even while unmounted: you can't check it
out elsewhere or delete it.

**How it's stored.** The workspace id is the branch name with `%`, `/` and a
leading `.` escaped (`agent%2Ftask-17`). The worktree registration is
`.git/worktrees/gitfold-ws-<id>`; the changes are in `.git/gitfold/<id>/`:
`upper/` holds every file that differs from the workspace's base (the branch's
last tip, see above), as real files; `markers` records deleted files so they
stay deleted; `times` the timestamps of files dropped from upper (committed,
or restored unchanged); `reads` the files read, if tracked; `base` is that commit, kept from `git gc` by the ref
`refs/gitfold/<id>`; `branch` the branch name; `tmp/` copies in progress;
and the layers files, if it has layers (see [Layers](#layers)). Snapshots of its work are kept by refs
`refs/gitfold-seal/<id>/<n>`, a workspace's layers commit by
`refs/gitfold-lens/<id>`. Task folders are in `.git/gitfold/.tasks/<id>/` (see above), plain files you
can change any time. Don't modify the rest of `.git/gitfold/` while mounted.

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
- `git stash --all` (`-a`, which stashes ignored files too) fails in a
  workspace: it can't remove the task folder's link (`EPERM`) and leaves the
  working tree as it was, with a stash `git stash pop` refuses (`.task already
  exists`). Use `git stash -u`, which leaves ignored files alone; drop such a
  stash with `git stash drop`. `git clean -fdx` only warns about the link.
- After `git add` of files with `eol=crlf` attributes, git warns that line
  endings will be converted (see [Differences](#differences-from-a-checkout)).

### Layers

Agents working on parts of one change often need each other's work before
it is committed: an API one of them is writing, which another calls. Layers
show it, uncommitted included, without merging anything.

**Layers in a workspace.** `gitfold layer add <BRANCH>` shows another
branch's work in the workspace you are in (`-w <BRANCH>` names another
one), read-only, over its base and under its own changes:

```
cd ~/mnt/repo/agent/client
gitfold layer add agent/api             # agent/api's work, kept up to date
gitfold layer add agent/db --frozen     # agent/db's work as it is now, never updated
gitfold layer add agent/ui --commits    # only what agent/ui has committed, kept up to date
gitfold layer add v1.2                  # a tag or a commit: fixed by nature
gitfold layer remove agent/db           # stop showing it
gitfold layer remove --all              # stop showing every layer
gitfold layer                           # what's in my view:
Base: main

LAYER       SHOWS          UPDATES
agent/api   all work       live, last 14:07
agent/ui    commits only   live, last 13:50
v1.2        commits        fixed

Conflict: src/api.rs is changed by both agent/api and agent/ui
```

A layer of a branch with a workspace is that workspace's work, uncommitted
included (a snapshot of it, see above); of a branch without one, or with
`--commits`, the branch's commits; of a tag or a commit, that commit. It is taken as the
changes it makes since it and the workspace's base diverged; that merge base
is pinned with the layer, so it shows the same files whatever happens to
either branch until it moves. A live layer moves at the agent's turns (see
*Between turns* below) and whenever `layer add` names it again: a
workspace's to its newest snapshot (only one of its workspace as it is now,
never one left by an earlier workspace of that branch; if it has none yet,
one is taken then), a branch's to its tip. `--frozen` takes
it as it is now and leaves it there (`--frozen --commits`: the branch's
commits as they are now); a revision never moves. Adding a layer again
replaces it in place, with the flags given, and brings it up to date: the
same command refreshes a frozen layer and makes a live one live again.

A path a layer changed (or deleted) shows that layer's version; a file of
the workspace's own at that path wins over all of them and stays writable.
Where several layers change one file, it shows their 3-way merge against
the base's version (as `git merge-file`): merged if their changes don't
overlap, with git's conflict markers (`<<<<<<< agent/api`, `>>>>>>>
agent/ui`) if they do, and the base's version if they can't be merged at
all (a binary file, a deletion against a change, a type change). Either
conflict is listed (`Conflict: <path> is changed by both <A> and <B>`, the
first five, then `and N more`). Files marked for blending are the
exception (below): they show every layer's lines and the workspace's own.

The listing (`gitfold layer` alone) shows first what the branch is built
on (`Base: main`, or for a stacked branch its base, whether it follows it
and how many commits of its own sit on it: see [Stacked
branches](#stacked-branches)), then one line per layer, in the order
added: the name, what it shows (`all work`, `commits only`, or `commits`
for a branch without a workspace, a tag or a commit) and how it updates
(`live, last HH:MM`, when it last moved, the date too if not today;
`frozen at HH:MM`; `fixed`), then the conflicts; `No layers.` if there are
none. `add` and `remove` print a line saying what they did, then what the
daemon did: while mounted, it shows the change at once, printing notes
about the new view after its summary (`note: <path>: <A> and <B> change it
and conflict: it shows git's conflict markers`, `note: <path>: <layer>'s
change conflicts with what is below it, so it isn't blended in`, or
`<path>: <layer>'s version is older than your base's (it hides a change
made since); ask <layer> to update`); unmounted, it is stored and shown
when the workspace opens. A conflict names the layers whose changes
conflict, not every layer that changes the file. Exit codes: 3 no such
workspace, branch, revision or layer; 4 refused, nothing changed (the
layers shown and stored stay as they were; run it again once the cause is
gone): git busy in the workspace (a commit in progress, a stopped rebase),
a commit made on the layers commit that can't go onto the branch yet, the
workspace's own lines in a blended file that depend on lines that would go
(see below), or a branch built on a base (`layer add` on it: layers on a
branch with a base aren't supported yet; `gitfold snapshot <BASE>` shows
the base's work instead, or `gitfold layer base main` takes the branch off
its base first); 2 usage errors (a workspace as its own layer); 1 other
errors.

`layer add <BRANCH>` while your proposal to change BRANCH shows in its
place replaces the proposal (which stays, as a branch: `gitfold layer add
<PROPOSAL>` shows it again in the same place, `gitfold reject <PROPOSAL>`
drops it); `layer remove <BRANCH>` then removes the proposal's place.
Adding the branch the workspace's base came from (`layer add main`) shows
its new commits as they come; it says when there is nothing to show yet.

- **A layer's files are read-only.** They have no write bits; writing,
  truncating, removing, renaming them (or a directory holding one) and
  `chmod` fail with `EACCES`, as does creating a path a layer deleted. They
  are another agent's work in progress: a change made here would become this
  workspace's own file, hiding theirs and committed with this branch. To
  change one, propose a change (below), ask its agent (`gitfold send
  <BRANCH> <TEXT>`), or remove the layer (`gitfold layer remove <BRANCH>`)
  and edit your own version. A file several layers merge is all of theirs.
  In Claude Code, the plugin denies its edit tools such a file, saying this
  and naming every layer whose changes it shows.
- **Proposals: changing a layer's file (a contract).** A typical tree: a
  root agent writes the interfaces in a workspace (`contracts`), each child
  shows it as a layer and builds against it. When a child needs an
  interface changed:

  ```
  cd ~/mnt/repo/child-3
  gitfold propose api/billing.rs     # branch contracts-prop/child-3: a copy of
                                     # contracts' work, shown here in its place
  $EDITOR ~/mnt/repo/contracts-prop/child-3/api/billing.rs
  # the next turn shows the edits here (or now: gitfold layer add contracts-prop/child-3)
  # in contracts' workspace (the root):
  gitfold proposals                  # who proposed what (diff stat), consent so far
  gitfold consent contracts-prop/child-3 \
      --test 'CARGO_TARGET_DIR=$GITFOLD_SCRATCH/target cargo check'
  gitfold accept contracts-prop/child-3     # or: gitfold reject ... --reason <TEXT>
  ```

  `propose <PATH>... [--layer <L>] [--from <DIR>] [-w <WS>]` forks the
  layer's workspace (`branch --with-changes`, with its uncommitted work;
  for a layer without a workspace, its commits) into `<L>-prop/<WS>`,
  shown as all work, live (the layer defaults to the one whose file the first
  path shows, named with `--layer` where several layers merge it; paths
  are relative to the current directory in the
  workspace), copies `<DIR>/<PATH>` there with `--from`, and puts the
  proposal in the layer's place among the workspace's layers. There the files
  are yours: edit them in its workspace. Proposing again adds to the same
  proposal. It is recorded in config (`branch.<proposal>.gitfoldProposal`
  and `gitfoldProposedBy`), what it started from in
  `refs/gitfold-proposal/<id>`; its changes are its work (a snapshot) against
  that.

  `consent <PROPOSAL> [--test <CMD>] [--timeout <SECS>] [--only-readers]`
  checks what accepting it would give, the layer's work as now with the
  proposal's changes, with every workspace showing the layer (or a
  proposal to change it): whether the workspace's own work merges with it,
  and `CMD` (`sh -c`) run in the workspace's view with it in the layer's
  place (its other layers and its own work included), a read-only
  snapshot. `GITFOLD_WORKSPACE` names the workspace and `GITFOLD_SCRATCH`
  an empty writable directory, removed after (point build output there);
  the test is killed after `--timeout` seconds (default 600), and only the
  end of its output is kept. `--only-readers` skips the workspaces whose
  read records (`gitfold track-reads`) say they didn't read a changed file
  (by default they are checked too: a build reads files no agent opened).
  No workspace changes. It prints a table (workspace, read the files:
  `yes`/`no`/`unknown`, merges cleanly, test, then conflicts or the test's
  exit code and last line, and the snapshot), records the result with the
  proposal's and the layer's work checked, and exits 0 if all passed, 4 if
  any failed or none was checked. `--test` needs the repository mounted.

  `accept <PROPOSAL>`, in the layer's workspace, writes the files the
  proposal changed into it, as uncommitted work (through the mount, or into
  its stored state unmounted). Refused (exit 4) if the layer changed any of
  them since the proposal was made, or without a passed consent check of
  the proposal and the layer as they are now (`--force` accepts anyway).
  Workspaces showing the proposal show the layer again, at once, as they
  showed it before (live or frozen, all work or commits only: a frozen one
  is taken again, with the change); those showing the layer live get the
  change at their next turn (or now: `gitfold layer add <L>`), frozen ones
  when they add it again (`gitfold layer add <L> --frozen`), commits-only
  ones once the layer commits it. Each one but those whose read records
  say they didn't read the files gets a message saying which (`subject:
  contract changed`, the lines as proposed).
  The proposal's workspace and branch are removed; edits made to it while
  it was accepted stay in it as a proposal of their own, and a workspace in
  use stays as a plain branch to remove later. `reject <PROPOSAL> [--reason
  <TEXT>]`, in the layer's or the proposer's workspace, removes it untaken,
  shows the layer again as it showed before the proposal (a live one up to
  date, a frozen one as it was) and tells the proposer. Both act as the workspace
  you are in: `-w` names one only when run outside every workspace.
- **A switch** (any of the commands above, or the hook below) tells the
  kernel exactly what changed: the files whose content changed, the entries
  added, removed or changed in type, and their directories' listings.
  Changed files get the switch's time, so incremental builds rebuild them;
  everything else keeps its times.
- **Git sees only the workspace's own work.** While layers show, the
  layers' whole files git tracks are marked skip-worktree in the worktree's
  index (git neither shows nor commits them, and keeps the base's version), and
  the files they add are excluded in a per-worktree excludes file
  (`core.excludesFile` in the worktree's own config, with
  `extensions.worktreeConfig`; since it replaces your global ignore file
  there, it starts with that file's patterns). `git status`, `diff`, `add -A`
  and `commit -a` show and commit only the workspace's own changes, as do
  `gitfold changes`, `list` and `remove`. Removing the layers gives the paths
  back to git. If git refuses to touch a file you didn't change (a merge or
  rebase over a layer's path), run `gitfold layer remove --all` and retry.
- **Blended files: everyone's lines in one file.** A file several agents
  add to (a registry, a manifest, a list of modules) can be blended rather
  than shown whole: mark it `merge=union` in `.gitattributes`, e.g.
  `registry.txt merge=union` or `**/Cargo.toml merge=union`.
  The attributes are read from the workspace's base (which waits while
  layers show, so commit them first) and from `info/attributes` in the
  repository's git directory (applied at the next switch, such as `gitfold
  layer add`). A marked file the base has and no layer deletes shows the
  base's version with every layer's change merged in, lines both sides add
  kept both (git's own meaning of `merge=union`); a layer's change that conflicts with what is below it isn't
  blended in (a note says so). The file stays writable: the workspace's own
  lines go on top of the layers'.
- **The layers commit.** While any file is blended, HEAD is detached at a
  layers commit gitfold makes: the blends over the branch tip, its parents
  the tip and the blended layers' snapshot commits, a `gitfold-lens` header, and the
  subject `gitfold layers: agent/client + agent/api (commits go to
  agent/client; never push this commit)`. The index has the blends too, so
  `git status` and `git diff` show only the workspace's own lines in a
  blended file. `git status` says `HEAD detached at <id>`.
- **Commits made on the layers commit go onto the branch.** Commit normally:
  within about a second (once git is idle), gitfold copies each commit
  onto the branch with only the workspace's changes (same author, committer
  and message; the layers' lines left out), and moves HEAD to a new layers
  commit with the same tree, so nothing git shows changes. The commit's id
  on the branch differs from the one `git commit` printed; `git log
  <branch>` shows it. Once moved, `git commit --amend` would amend the layers
  commit, taking HEAD off its line (below). A commit that removes or
  changes a layer's line in a blended file depends on that layer's
  uncommitted work and can't go onto the branch alone: it waits on HEAD,
  above the layers commit, with every later commit (the daemon log says
  which paths), and nothing goes onto the branch meanwhile; layer switches
  exit 4 (`commit <id> can't go onto the branch alone (it changes lines of
  a layer's)`) until it is amended to leave the layer's lines as they are.
- **The push guard.** A layers commit holds other workspaces' uncommitted
  work, so the worktree gets its own `core.hooksPath` (per-worktree config)
  while one is HEAD, whose `pre-push` refuses a push that would publish a
  commit with a `gitfold-lens` header (`git push origin HEAD:main`), then
  runs your own `pre-push`; every other hook runs yours. `git push origin
  <branch>` pushes the copied commits and is fine. `--no-verify` skips the
  guard. Turning the last blend off puts HEAD back on the branch and
  removes the guard; uncommitted own lines stay, as changes to the branch's
  version.
- **Switches carry own lines over.** When a switch changes a blended file
  the workspace has written, its own lines are merged onto the new blend (a
  3-way merge; where that conflicts, the new blend's lines less those the
  workspace removed, then its own new lines). A conflicting file that isn't
  text refuses the switch (exit 4: `<path>: your changes to it depend on
  lines the layers would no longer show; commit them first, or keep the
  layers as they are`). A switch first moves the commits made on the layers
  commit to the branch, and is refused while one can't.
- **`LAYERS SHOWN`: where git can't be kept right, it is turned off.** If the
  index can't be edited (sparse checkout, a split or sparse index, the index
  locked meanwhile), or HEAD left the layers commit's line while files are
  blended (`git switch`, `checkout <branch>`, `reset --soft`, `rebase`: the
  index holds the layers' lines, which mustn't be committed on the branch),
  the layers still show, but the workspace's `.git` names a directory that
  doesn't exist, so every git command fails (exit 128) saying why:
  `fatal: not a git repository: /LAYERS SHOWN (agent/api): git is off
  here (sparse checkout is on). Run gitfold layer remove --all to use git`.
  It is kept across remounts, until `gitfold layer remove --all` (or a
  switch that can edit the index). gitfold commands still find the
  workspace there.
- **The base waits.** While a layer shows, the workspace's base doesn't
  follow its branch: commits work and move the branch as usual (with blended
  files, as copied from the layers commit), and the base catches up once no
  layer shows (moving it meanwhile could copy the layers' files into the
  workspace's own).
- **Between turns, in Claude Code.** The plugin's hooks snapshot a session's
  workspace when the session stops (`gitfold snapshot --claude-hook stop`,
  only if some workspace has it as a layer), and when a prompt is sent,
  move the session's workspace's live layers to their latest work (a
  workspace's newest snapshot, a branch's tip), telling the agent which layers changed and
  how many files, and any conflicts among them (`gitfold layer
  --claude-hook user-prompt-submit`). What a layer shows thus changes only
  between turns, never half-way through one, its agent's or the reader's.
  Outside such a session, `gitfold layer add <BRANCH>` brings a layer up
  to date. A third hook, `gitfold layer --claude-hook pre-tool-use` on Edit,
  Write, MultiEdit and NotebookEdit, denies writing a layer's file with the
  reason above.
- **Stored** in the workspace's state directory, in the layers files: the
  layers as added, and what the daemon last applied, so a workspace opens
  as git was told and then catches up with the stored layers (their
  formats are in [ARCHITECTURE.md](ARCHITECTURE.md)). Layers files of
  earlier builds are read: their shown layers become live, all work; their
  hidden ones are dropped. While files are blended, the worktree's HEAD is
  the layers commit (found again on a remount), and `refs/gitfold-lens/<id>`
  keeps it and the view from `git gc`. A fork (`branch -w`) starts without
  layers.
- **Limitations.** A file both the workspace and a layer changed shows the
  workspace's version, unless it is marked for blending. Blending assumes a blended
  file's lines are unique and their order doesn't matter where two sides
  append at the same place. `--no-verify` skips the push guard. A layer not
  updated while its workspace made 16 newer snapshots holds one no ref
  keeps any more, which `git gc` may prune: add it again (`gitfold layer add`).

### Stacked branches

Stacked pull requests, `main <- p1 <- p2`: p2 is built on p1's commits,
p1 is its **base**. When p1 gets review fixes (or is rebased), p2's own
commits have to move onto them. git can't do it where every branch is
checked out in a worktree (`git rebase --update-refs` leaves the other
worktrees' branches behind), and plain `update-ref` moves a branch without
its files. gitfold does both:

```
gitfold branch p2 --base p1        # p2 at p1's tip, built on it
gitfold layer base p1 -w p2        # or: an existing branch, its own commits moved onto p1
gitfold layer base p1              # again, in p2's workspace: onto p1's new commits now
gitfold layer base p1 --frozen     # built on p1 as it is, not following it by itself
gitfold layer base main            # the trunk: off the stack, its own commits onto main
gitfold layer base --all           # every branch built on another, bases first
gitfold list                       # p1
                                   # └─ p2   ... p1  (needs update: gitfold layer base p1)
```

- **What a move is.** p2's own commits are those after the commit of p1
  they sit on (kept by `refs/gitfold-stack/p2`; the base's name is
  `branch.p2.gitfoldParent` in git config). Each is copied onto p1's tip
  with the same author, committer and message, as `git rebase --onto p1
  <start> p2` would (signatures are dropped); one that becomes empty (p1
  made the same change) is dropped. Because where they start is stored, an
  amended or force-pushed p1 doesn't drag its old commits along. Merge
  commits in p2's own commits aren't moved (rebase by hand). A branch not
  built on another yet has as its own commits those since it and p1 met.
- **When.** `gitfold layer base p1` says what it moves (`moves 3 commits of
  p2 onto p1`), then moves it; run again, it moves p2 onto p1's new
  commits. Without it, at the start of p2's agent's turn: the
  `UserPromptSubmit` hook (`gitfold layer --claude-hook
  user-prompt-submit`, in the plugin) moves the session's workspace and
  tells the agent which files changed under it. Never in the middle of a
  turn. `--frozen` (`branch.p2.gitfoldRestack manual`) leaves it to `layer
  base p1 --frozen`, run again; `layer base p1` without it follows p1
  again.
- **Off the stack.** `gitfold layer base <TRUNK>` forgets p2's base and
  moves its own commits onto the trunk's tip; on a branch built on nothing
  it says so and does nothing.
- **Where p2 lives.** With no workspace, its ref moves (refused if a git
  worktree has it checked out). In a mounted workspace, its files move as in
  a checkout (`git read-tree -m -u`): unstaged, staged and untracked work
  is kept, and the new files get new times, so builds see them. In an
  unmounted one, only without uncommitted changes; otherwise it waits for
  the mount. A workspace showing layers isn't moved (`gitfold layer remove
  --all` first).
- **When it can't.** A commit that conflicts with p1's new ones leaves p2
  where it was: `layer base` says which commit and paths (exit 4), the hook
  tells the agent once, and `list` shows `blocked: conflicts on <paths>`
  until p2 is rebased by hand (`git rebase --onto p1 <start>` in its
  workspace; the next `layer base p1` records where its commits now start).
  Uncommitted work on a path the move changes waits (`waits: uncommitted
  changes to f.txt overlap the move`; the hook says it once while it stays
  so), retried at the next turn.
- **Pushing.** gitfold never pushes. `layer base` refuses (exit 4) to move
  commits already pushed (reachable from p2's upstream, or `origin/p2`)
  unless given `--yes`; the hook's moves aren't asked. After a move it
  prints the push to run for each branch whose remote-tracking branch it
  left behind, with a lease on that tip (`git push --force-with-lease=p2:<old>
  origin p2:p2`), and `list` shows `push with: git push --force-with-lease
  origin p2` until it is pushed.
- **Stored** in git: the base's name, whether it is frozen and a blocked
  move in the branch's config (renamed and deleted with it), where its own
  commits start in `refs/gitfold-stack/<branch>` (a fork keeps both; moved
  with the branch in one ref transaction; after `git branch -m` it is
  guessed from the base's reflog, as `git merge-base --fork-point` does),
  and, while a workspace is moved, a `restack` intent in its state
  directory, so a crash half-way is finished or undone (one gitfold can't
  tell apart is reported, and `gitfold layer base <TRUNK>` forgets it,
  moving nothing).
- **A base that landed.** When p1 is merged into main, p2 moves onto main
  with only its own commits, and is built on main from then on (`p2: p1
  looks squash- or rebase-merged into main as 9f8e7d; moved there with its
  own 2 commits and now built on main. Its pull request's base should now
  be main: push it, then gh pr edit p2 --base main`). What counts is
  whether the commit p2's own commits start from landed, so a fix p1 got
  after its merge doesn't stop it.
  - Found: a merge commit (also of commits p1 got later); a squash or
    rebase merge (merging the start into a main commit touching its files
    changes nothing), also one the maintainer tweaked, or with main
    editing the same lines later. A squash is found by content, so main
    making the same change on its own looks the same: it is said to "look"
    merged, `layer base p1` follows it, and the turn-boundary hook follows
    it only once p1's branch is gone (deleted here, or on its remote and
    fetched with `--prune`); until then it tells the agent once.
  - A fast-forward too: p2's start on main's first-parent line looks like
    a fresh p1 with no commits of its own, so whenever gitfold sets or
    moves it, it records whether it was on p1's target then
    (`branch.p2.gitfoldBaseOnTarget`). One that wasn't, and now is, came by
    a fast-forward; it "looks" merged and is followed as a squash is.
  - Not found, so not taken: a squash rewritten when merged: `gitfold
    layer base main -w p2` moves p2 onto main by hand (or `layer base
    <BRANCH>` onto what p1 was built on, once p1 is deleted: that deletes
    its own base's name too).
  - p2 lands on the commit that merged p1, not main's tip: only its own
    commits move, onto exactly what p1 became, and nothing else main did
    appears in its workspace mid-task; `layer base main` follows main's
    tip when wanted. A branch on the trunk (or on the local branch tracking
    it, `main` for `origin/main`) isn't moved at turn boundaries, nor said
    to need it: following main is on request.
  - The trunk is `gitfold.trunk` if set (`git config gitfold.trunk
    origin/main`), else the branch `origin/HEAD` names, else `main` or
    `master`; a base that is itself stacked lands on its own base instead
    (stacked pull requests merged into each other). `list` notes `p1
    landed in main` (or `looks landed`) `: needs update: gitfold layer base
    p1`. gitfold doesn't fetch: it sees what was fetched, and after a fetch
    it checks only the trunk's new commits.

### Partial clones

In a partial clone the object database lacks blobs (`blob:none`) or trees
(`tree:0`), which git fetches on demand. gitfold does the same: opening a
snapshot or workspace fetches what its commit misses from the promisor remote,
in one `git fetch` (the one git's own lazy fetch runs), before the directory
appears, so the first `ls` of a new snapshot can take as long as that download.
So do a workspace's base following a branch that moved, and the checks of
`gitfold remove` and `list`.

- The remote must let clients fetch objects by id, as git's own lazy fetch
  needs: hosting services do; a plain `git daemon` or ssh remote needs
  `uploadpack.allowAnySHA1InWant` (or `allowReachableSHA1InWant`) and
  `uploadpack.allowFilter`. Otherwise the fetch fails with `not our ref`.
- The whole commit is fetched, as a checkout would: gitfold needs every file's
  size to show it.
- The remotes are the ones git would use, in its order: those with
  `promisor = true` or a `partialclonefilter`, in config file order, the one
  `extensions.partialClone` names last. Each is asked for what the ones before
  it didn't bring. They are read once per mount: a remote added or flagged
  later is used after a remount (a changed URL at once).
- The daemon has no terminal to ask for credentials: use an ssh agent or a
  credential helper (`gitfold list` and `remove` run from a terminal let ssh
  ask). A fetch that fails (unreachable remote, refused) fails the open with
  `EIO` and is logged once; the same commit is refused for 30 s, unless its
  objects arrive meanwhile (`git fetch` by hand). A remote that refuses objects
  asked for by id (`not our ref`: the server needs
  `uploadpack.allowAnySHA1InWant`, or they are gone from it) is retried after
  10 min instead. A resumed workspace whose
  branch moved needs only its stored base: if the tip can't be fetched, it
  opens and follows the tip later.
- A fetch that prints nothing (no progress) for 120 s is stopped, with its ssh
  and index-pack; one making progress may take as long as it needs.
  `GITFOLD_FETCH_STALL_SECS` changes the limit.
- No fetch starts with less than 1 GiB free in `.git/objects`
  (`GITFOLD_FETCH_MIN_FREE_MB` changes it): a large commit can be tens of
  thousands of objects.

## Running it

- `gitfold mount` starts the daemon in the background, detached from the
  terminal (closing it doesn't stop the mount), and returns once the mount is
  up, or with the daemon's error. It logs to a file named after the mount point
  in `$XDG_STATE_HOME/gitfold/` (default `~/.local/state/gitfold/`), printing
  its path (`--log` to choose): one line per new snapshot commit, workspace,
  prewarm and workspace rebase, and operation counters on exit. `RUST_LOG=warn`
  for only problems. With `--foreground` it stays in the foreground and logs to
  stderr, for service managers.
- **Stop it with `gitfold unmount <MOUNTPOINT>`.** It waits until the daemon
  has exited, so `gitfold remove` or `prune` can follow right away. If the
  mount is busy (say a `git maintenance run --auto` a commit started), it is
  detached and `unmount` names the processes using it and waits for them to
  leave, however long that takes (an editor or another terminal inside it too).
  Only if `unmount` or a process it runs under (your shell) is itself inside
  the mount does it return at once: the daemon exits when you `cd` out. Before
  exiting, the daemon brings every open workspace's base up to its branch tip.
  Ctrl-C (in the foreground), SIGTERM, SIGHUP and `fusermount3 -u <MOUNTPOINT>`
  stop it as well; a second SIGINT or SIGTERM exits immediately.
- **After upgrading gitfold,** a mount keeps running the build it was started
  with. `gitfold version <MOUNTPOINT>` prints both builds and exits with 3 if
  they differ (unmount and mount again to upgrade), 4 if the daemon is too old
  to say, and 5 if it doesn't answer within 3 s (busy, say with a removal). A
  build is named by its version and a hash of its sources (`gitfold --version`
  adds the git commit, for people): the same sources give the same name however
  they were committed, so a commit that only changes docs asks for no remount.
- Only `kill -9` (or a crash) leaves the mount behind: accesses then fail with
  `Transport endpoint is not connected`, and `gitfold unmount <MOUNTPOINT>`
  cleans it up. `gitfold mount` detects such stale mounts, and mount points
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
  - On large trees, reclaim is what makes workspaces go cold. Reported from a
    monorepo of over a million files: each workspace held a couple of GB of
    kernel cache. Under ten concurrent full-tree `git status` runs, free memory
    (`MemFree`) fell below the kernel's low watermark while `MemAvailable` still
    showed tens of GB, and reclaim dropped millions of cached directory entries,
    which git then looked up again. That is the kernel's ordinary memory reclaim, not
    gitfold; size RAM for the workspaces in use at once.

### With coding agents

- [skills/gitfold/SKILL.md](skills/gitfold/SKILL.md) tells agents how to work
  in a mount: workspaces and snapshots, forking a workspace, layers, merge
  previews, cleaning up, and what differs from a checkout. It is a Claude Code skill:
  `ln -s "$PWD/skills/gitfold" ~/.claude/skills/gitfold` (or into a project's
  `.claude/skills/`). It is plain markdown, so other agents can be pointed at
  it from their instructions (`AGENTS.md`).
- The gitfold Claude Code plugin: see [Quick start](#quick-start) and the
  skill's "Claude Code" section.
- Workspaces an agent only reads a few files in are cheaper without the prewarm
  walk: give their branch prewarm mode `none` (see [Usage](#usage)).
- **Claude Code's git scans** (measured with Claude Code 2.1.322 and a git
  wrapper logging every call):
  - After every Bash command it runs `git --no-optional-locks status
    --porcelain --untracked-files=normal --ignore-submodules=dirty`, with
    fsmonitor and hooks off, killed after about 2 s
    ([anthropics/claude-code#97752](https://github.com/anthropics/claude-code/issues/97752)):
    about 9 per 8-command session, seconds each on a tree of over a million files. No
    Claude Code setting controls it (`CLAUDE_CODE_BASH_EDIT_DIFF=0`
    doesn't). It has no `--ignored`, so git's untracked cache can serve it:
    run `git config core.untrackedCache true` once in the repository (the
    config is shared by all its workspaces). The cache itself lives in each
    workspace's index, so run a plain `git status` once in each workspace to
    fill it (the scan, with `--no-optional-locks`, never writes it). On a
    spark workspace (27.6k files) it went from 41 ms to 19 ms, listing no
    directory. Its results matched uncached runs across additions and
    removals, `.gitignore` edits, commits, `stash -u`, `clean` and invisible
    rebases, and unchanged directories (dated 2020-01-01) are trusted at
    once, unlike those of a fresh checkout.
  - A `--ignored=matching` variant, which git doesn't serve from the
    untracked cache (each run lists the whole workspace), comes only from its
    "bash edit diff" feature (`CLAUDE_CODE_BASH_EDIT_DIFF`, or
    `bashEditDiffEnabled` in its settings), which a server-side flag can keep
    off by default (it was in these runs). `CLAUDE_CODE_BASH_EDIT_DIFF=0`
    keeps it off.
  - `CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING` changes none of these scans.
  - For read-only sessions, `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1` also cuts
    the git commands it runs.
- Orchestrators tearing down agent workspaces can use `gitfold remove`'s exit
  codes, `--check` and `--wait SECS` (see [Cleaning up](#cleaning-up)), and
  `gitfold version <MOUNTPOINT>` to find mounts still running an old build
  after an upgrade.

### Cleaning up

Worktree registrations are locked, so `git worktree prune` keeps them while
unmounted; later mounts reuse them.

- `gitfold prune` deletes the snapshot registrations; snapshots are
  registered again when opened. It refuses while the repository is mounted.
- `gitfold remove @<rev>` deletes one snapshot's registration
  (unmounted: of what `<rev>` names now). While mounted it also closes the
  snapshot (it leaves the mount root; opening it again registers it again), and
  keeps the registration while another open name (`@main` and `@<its hash>`)
  shows the same commit.
- `gitfold remove <BRANCH>` deletes a workspace: its registration, its
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
  last mounted, its base catches up first, as a mount would.
- `remove` works while the repository is mounted: the daemon does the removal,
  and the name leaves the mount at once (the branch, if it still exists, opens
  again as a fresh workspace). It refuses while a process of yours uses the
  workspace or snapshot (its working directory, an open or mapped file), naming
  it; processes in another container or sandbox (mount namespace) aren't seen,
  but the checks for uncommitted changes still apply.
- For scripts: `remove` exits with 3 if there is no such workspace or snapshot,
  4 if it has uncommitted or staged changes, a git operation stopped in it, or
  changes that can't be told (its base couldn't catch up), 5 if it is in use (a
  process, a git command updating a ref, an unmount in progress), and 1 on any
  other error. `--check` only checks: it exits as `remove` would (reporting
  uncommitted changes even while a process uses it) and removes nothing (it may
  bring the workspace's stored base up to its branch tip, as mounting would).
  `--wait SECS` retries while it is in use, for up to SECS seconds. While
  mounted, these need a daemon of this version (remount after upgrading).

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
| open | `git worktree add`: 17.30 / 17.22 s, 5.5 GiB | 0.86 s (0.10-0.12 s near an open one) | 0.87 s |
| prewarm (background) | | 0.70 s | 1.16 s |
| `find . -type f` (GNU find) | 0.47 / 0.47 s | 0.35 / 0.35 s | 0.35 / 0.35 s |
| `git status` | 0.87 / 0.86 s | 0.88 / 0.71 s | 0.94 / 0.72 s |
| `rg` | 0.37 / 0.37 s | 2.87 / 0.41 s | 3.27 / 0.40 s |
| daemon memory | | 63 MB | 124 MB |

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
measures write workloads. `bench.sh` must run inside Claude Code (it uses Claude Code's grep and needs
`CLAUDE_CODE_EXECPATH`).

Tests: `cargo test` runs the unit tests (logic without a mount or git:
parsing, formats, settings, a few scratch files); the end-to-end suites drive the release binary, a real
mount and real git (build with `--release` first):

- `tests/manage.sh` (branch, list, changes, remove, prune), `tests/status.sh`
  (what a workspace's status sees: ignore rules, modes, half-way git
  operations, the index), `tests/mount.sh` (mount, unmount, version,
  prewarm) and `tests/agents.sh` (overlaps, the task folder, messages): one
  section at a time, each on a fresh repository. `SECTION=inbox
  tests/agents.sh` runs the sections whose name contains `inbox`, and
  `PARALLEL=1` runs them all at once (a few timing checks may then be flaky on
  a loaded machine). A section that ends early counts as a failure.
- `tests/run.sh [SUITE...]`: the suites at once, summed up (by default all
  but `workspace.sh`); `PARALLEL=1 tests/run.sh` takes about as long as the
  slowest, and `SECTION=inbox tests/run.sh` finds the section in any of them.
  Suites running side by side load the machine like `PARALLEL=1`: if a timing
  check fails, rerun that suite alone.
- `tests/rebase.sh`: git operations moving a workspace's branch.
- `tests/layers.sh`: snapshots of workspaces' work, layers shown in a
  workspace (live, frozen, merged, blended: the layers commit) and
  proposals (sections as above).
- `tests/stacks.sh`: branches built on another (`layer base`) moved with
  no workspace, in an open workspace (by the hook and by `layer base`
  again) and in an unmounted one; conflicts, uncommitted work in the way,
  commits already pushed (`--yes`), `--frozen`, off the stack onto the
  trunk, `list`'s tree and notes; bases that landed (squash, rebase and
  merge commits, a tweaked squash, a stacked base's base, an unmerged base
  left alone).
- `tests/workspace.sh <repo>`: workspaces on a scratch clone of `<repo>`.
- `tests/promisor.sh`: partial clones.

Each run works in a directory of its own under `/tmp` (`gf-<suite>.XXXXXX`,
kept with `KEEP=1`), so runs can overlap; every gitfold command it runs gets
`GF_TIMEOUT` seconds (default 120), and stdin is `/dev/null`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for how it works and why, including the
code map.
