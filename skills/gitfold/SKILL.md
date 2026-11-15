---
name: gitfold
description: How to work in a gitfold mount (a repository served as a FUSE filesystem, with read-only @<rev> snapshots and writable per-branch workspaces), and how to run several agents on it. Use when the working directory is under a gitfold mount, when the user mentions gitfold, or when setting up workspaces for parallel agents.
---

# Working with gitfold

gitfold serves a git repository as a filesystem. Below the mount point
(say `~/mnt/repo`):

- `@<rev>/` is a **snapshot**: the files of any commit, read-only, opened in
  about a second however large the repository (`@HEAD~5`, `@v1.2`,
  `@origin/main`, `@<sha>`).
- `<branch>/` is a **workspace**: a writable worktree of a local branch, also
  opened in about a second. Only what you change is stored.

Both are real git worktrees: `git status`, `log`, `diff`, `blame`, `commit`
work as usual. Find out where you are with `git rev-parse --show-toplevel`
and `gitfold list`. gitfold commands other than `mount` work on the
repository the current directory is in: the original clone, or any workspace
or snapshot (not the mount root itself), like git; `-C <PATH>` as with git.

## In a workspace

- **Work normally:** edit, build, test, `git add`, `git commit`, `git push -u
  origin HEAD`. Changes are saved as they happen and survive remounts.
- **Stay on your branch.** Don't `git switch` or `git checkout <branch>` in a
  workspace: the directory keeps its name and stored changes. For another
  branch, use its own workspace.
- **Read other revisions without checking them out:** `cd <mount>/@<rev>` or
  `cat <mount>/@origin/main/path/to/file`, instead of `git stash`,
  `git checkout <rev>` or `git worktree add`. Prefer `@<sha>`
  (`@$(git rev-parse origin/main)`): a name like `@main` stays at the commit
  it showed when first opened.
- **Your changes, cheaply:** `gitfold changes` prints `A`/`M`/`D` per path
  (like `git diff --name-status`, untracked files included) from stored state,
  without the full-tree scan `git status` does. Prefer it on large trees.
  `gitfold changes --stat` adds lines added and removed, like
  `git diff --numstat`.
- **Commits from elsewhere are fine:** if the branch moves (a commit, rebase or
  reset done in another worktree), the workspace follows within about a second
  without its files changing.
- **What differs from a checkout:**
  - No checkout filters: Git LFS files are pointer files, `eol=crlf` files keep
    LF.
  - Submodules are empty directories and can't be initialized.
  - No hard links (`EPERM`).
  - Renaming a committed directory of more than 10,000 files or 100 MiB fails
    with `EXDEV`. `mv` then copies, but `git mv` fails: move the files instead.
  - Creating many small files is several times slower than on a local disk.
    For dependency installs (`node_modules`) and build caches, an output
    directory outside the mount is faster (e.g. `CARGO_TARGET_DIR`).
  - Unchanged files are dated 2020-01-01.

## In a snapshot

Read-only: writes fail with `EACCES` or `EROFS`, as do `checkout`, `reset
--hard` and `stash`. To build or test a snapshot, write outputs outside it.
Don't commit in one: commits move its HEAD away from its files. To change
something, create a branch and work in its workspace.

## The task folder

Every workspace has a `.task` folder (a link to plain files outside the
workspace) that git doesn't see, can't clean, and that outlives the workspace
and its branch. Keep there what belongs to the task but not to its commits:

- `spec.md`: what to build (an orchestrator can write it before you start);
- `status`: one line on what you are doing now; `gitfold list` shows it;
- `notes/`, drafts, design sketches, findings worth keeping;
- `handoff.md`: what the next agent needs to know;
- `pr.md`: the PR description, for `gh pr create --body-file .task/pr.md`.

A fork (`gitfold branch -w`) copies it. Use `git stash -u` rather than
`git stash --all`, which fails on the folder's link. Its name can differ per repository
(`git config gitfold.taskDir`); check with `ls -a` at the workspace root.

## Messages between agents

```
gitfold send agent/task-18 -s "lexer API" "I renamed Lexer::next to Lexer::advance"
gitfold inbox                  # take (and print) the messages sent to you
gitfold inbox --wait --timeout 600   # none yet? wait up to 10 minutes for one
gitfold inbox --peek           # list them without taking
gitfold watch agent/b:.        # wait until something in agent/b's task folder changes
```

Each message is taken once. Messages are files in `.task/inbox/`, so anything
can send one: an orchestrator, a script, a person (`gitfold send <BRANCH>
<TEXT>` from the repository, as `user`).

**Delivery without polling, in Claude Code:** the gitfold plugin's hooks
deliver them (see [Claude Code](#claude-code) below). Without the plugin, add
to `~/.claude/settings.json` (the commands do nothing outside a gitfold
workspace):

```json
{
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command",
      "command": "gitfold inbox --claude-hook user-prompt-submit" }] }],
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command",
      "command": "gitfold inbox --claude-hook post-tool-use" }] }],
    "Stop": [{ "hooks": [{ "type": "command",
      "command": "gitfold inbox --claude-hook rewake",
      "async": true, "asyncRewake": true, "timeout": 86400 }] }]
  }
}
```

Messages then arrive in the conversation by themselves: with your next
prompt, after the agent's next tool call, or, when the session is idle, by
waking it (the `Stop` hook waits for a message and wakes the session when one
comes; one waiter per inbox). Other harnesses: run `gitfold inbox --wait`
between tasks, or as a background command that ends when a message arrives.

## Getting gitfold, and mounting a repository

With the gitfold Claude Code plugin, `gitfold` in your Bash is the plugin's
launcher: the first run downloads the release binary (a few MB, checked
against the release's checksums), and later runs use it. **Don't build
gitfold from source, and don't install Rust or cargo for it.** If `gitfold`
isn't found (no plugin), tell the user how to get it: the plugin (below), or
a release binary from https://github.com/pkukielka/gitfold/releases. Build
it only if they ask you to. If it says no release binary runs on this system
(not Linux, or NixOS without `nix-ld`), tell the user; don't work around it.

To mount a repository (the user asks you to, or a worktree couldn't be made
because the repository was never mounted):

```
mkdir -p ~/mnt/<name>
gitfold mount <the repository's top-level directory> ~/mnt/<name>
```

Use the mount point the user names, else `~/mnt/<repository name>`; it must
be an empty directory of theirs. The mount runs in the background and
outlives the session; with the plugin, it is mounted there again by itself
after a reboot. Tell the user where it is, and that `gitfold unmount <mount
point>` stops it.

## Claude Code

The gitfold plugin bundles this skill and the hooks:

```
/plugin marketplace add pkukielka/gitfold
/plugin install gitfold@gitfold
```

It runs the `gitfold` on `PATH`, or else the release matching the plugin
(Linux x86_64 and aarch64, glibc 2.28 or newer), downloaded to
`~/.local/share/gitfold/bin` (see above). With it:

- **Claude Code's worktrees are workspaces** (`claude -w <name>`,
  `EnterWorktree`, subagents with `isolation: worktree`): branch
  `worktree-<name>`, at `<mountpoint>/worktree-<name>`, from the session's
  HEAD, opened in about a second. Removing one (`ExitWorktree`, or choosing
  removal when the session ends) discards its uncommitted changes, as Claude
  Code does; the branch stays. Subagents' worktrees are kept when they finish:
  `gitfold remove` them.
- **The mount comes back by itself.** A repository used with gitfold but not
  mounted (after a reboot, say) is mounted again where `gitfold mount` last
  mounted it (`gitfold.mountpoint` in its git config), with default options;
  if none is recorded, creating a worktree fails, saying so: mount it.
- **Other repositories are left alone:** a git worktree in
  `.claude/worktrees/<name>`, as without the plugin.
- **Where gitfold can't run** (not Linux, or no release binary that runs, as
  on NixOS without `nix-ld`: build gitfold and put it on `PATH`), the same,
  except that in a repository used with gitfold creating a worktree fails,
  saying why.
- **Inbox messages arrive in the session** (see [Messages](#messages-between-agents)).
- **Layers move between turns** (see [Layers](#layers)), and so do your
  commits onto the editable layers you build on (see [Editable
  layers](#editable-layers-building-on-another-branch)).

The hooks are `gitfold branch --claude-hook worktree-create`, `gitfold remove
--claude-hook worktree-remove`, the `inbox --claude-hook` ones above, and for
layers `gitfold snapshot --claude-hook stop` (`Stop`), `gitfold layer
--claude-hook user-prompt-submit` (`UserPromptSubmit`) and `gitfold layer
--claude-hook pre-tool-use` (`PreToolUse` on `Edit|Write|MultiEdit|NotebookEdit`). A
plugin's `WorktreeCreate` hook doesn't run for `claude --worktree --tmux`; for
that, put the two worktree hooks in `settings.json` instead.

## Starting work

```
gitfold branch agent/task-17 --from origin/main   # in the repository or a workspace
cd ~/mnt/repo/agent/task-17
```

Workspaces are opened by name, not created with `mkdir`: the mount root is
read-only. A branch checked out elsewhere (say the main checkout) is refused
with `EBUSY`; branch from it instead.

**To try something without losing the current state,** fork the workspace:

```
gitfold branch -w agent/task-17-b              # from inside agent/task-17
gitfold branch -w agent/task-17-b --from agent/task-17   # from anywhere
```

The new branch starts at the same commit with a copy of the uncommitted
changes, staged ones included. Only the changes are copied, so this is cheap
even on a huge tree. Each workspace then changes on its own.

## Running many agents

- **One branch and workspace per task.** Workspaces cost what their changes
  weigh, so dozens are fine. An agent that only reads needs only a snapshot:
  `cd <mount>/@<sha>`.
- **Agents that only read a few files:** `gitfold branch <BRANCH> --prewarm
  none` skips the background walk that lists the whole tree when a workspace
  opens.
- **Watching them:** `gitfold list` shows each workspace's changed, deleted and
  ignored file counts, and how long ago it last changed (a file or a commit):
  an agent idle for long stands out. `--stat` adds line totals.
  `gitfold changes <BRANCH>` shows one's changed paths. All read stored
  state, so polling many workspaces is cheap.
- **Is the work stale, or colliding?** `gitfold overlaps` (inside a workspace, or
  `gitfold overlaps <BRANCH>`) lists paths the workspace changed (`W`) or read (`R`) that its
  remote's default branch (`origin/HEAD`, else `main`) changed since they
  diverged. `--against <REV>` picks
  another target. `--against <BRANCH>` of another workspace includes its
  uncommitted changes, and `--against-workspaces` checks every other one: two
  agents editing the same files. It exits 6 if anything overlaps, 0 if not,
  so it's a cheap check before pushing or a poll for a supervisor. A `W` path
  often merges cleanly; the merge preview below tells for sure.
- **Recording reads:** `R` lines need read tracking, off by default. Turn it
  on per workspace with `gitfold branch <BRANCH> --track-reads on` (or
  `gitfold track-reads <BRANCH> on` for an existing one), or for the whole
  mount with `gitfold mount --track-reads`. Turn it on before the agent
  starts: files read before aren't recorded. Reads are what Claude Code's
  hook (the plugin's) reports: the Read tool's files, and the files `cat`,
  `head`, `tail`, `sed` ... show in Bash; with other agents, `overlaps` has
  `W` lines only.
- **Merge previews:** to see what several workspaces look like merged,
  uncommitted work included, before merging anything:

  ```
  cd $(gitfold snapshot main agent/task-17 agent/task-18)   # build and test there
  ```

  It merges each workspace's work (its uncommitted changes included), or a
  branch's or revision's commit, in order onto the first (or `--onto
  <REV>`), and prints the merge as a snapshot path. `--commits` takes only
  the branches' commits. No workspace, index or branch changes. It exits 4
  on conflicts, listing the paths. The same work gives the same snapshot.
  With one branch, `gitfold snapshot <BRANCH>` shows that workspace's work
  alone, as `<mount>/@<sha>` (unmounted, it prints the commit).

  Committed work only, without gitfold: make the merge commit with git and
  open it as a snapshot. No working tree, index or ref is touched (git 2.38
  or later). This is a bash function: it runs in a subshell, so the identity
  doesn't leak into your shell.

  ```
  merge_preview() (
    export GIT_AUTHOR_NAME=preview GIT_AUTHOR_EMAIL=preview@localhost \
      GIT_AUTHOR_DATE=2020-01-01T00:00:00Z GIT_COMMITTER_NAME=preview \
      GIT_COMMITTER_EMAIL=preview@localhost GIT_COMMITTER_DATE=2020-01-01T00:00:00Z
    c=$(git rev-parse "$1") || return
    shift
    for b in "$@"; do
      t=$(git merge-tree --write-tree "$c" "$b") || { echo "conflicts merging $b" >&2; return 1; }
      c=$(git commit-tree "$t" -p "$c" -p "$b" -m preview)
    done
    echo "@$c"
  )
  cd ~/mnt/repo/$(merge_preview main agent/task-17 agent/task-18)
  ```

  The fixed identity and date make the same commits give the same `@<sha>`,
  so repeated previews reuse one snapshot. `git merge-tree --write-tree
  --name-only <A> <B>` lists conflicting paths. Nothing references the
  preview commit: git prunes it after its snapshot is removed
  (`gitfold remove @<sha>`).
- **Cleaning up:** `gitfold remove <BRANCH>` deletes a workspace (its commits
  stay on the branch). It refuses if the workspace has uncommitted changes
  (exit 4), is in use by a process (exit 5) or doesn't exist (exit 3).
  `--check` only reports, `--wait SECS` retries while it is in use, and
  `--force` discards changes. Ignored files (build output) go with it.
  `gitfold remove @<rev>` and `gitfold prune` drop snapshot registrations.
  Never `rm -rf` a workspace or snapshot through the mount.
- **`git status` on a large tree:** run `git config core.untrackedCache true`
  once in the repository, then a plain `git status` once in each workspace.
  Later scans, including the ones coding tools run after every command, skip
  unchanged directories.

## Layers

A **layer** is another branch's work shown in your workspace, read-only:
its changed files over your branch's commits, under your own changes. For a branch
with a workspace that is its work as it is, uncommitted included. Several
agents building parts of one change can then build and test against each
other's work as it is, without merging or committing anything. A
see-only layer (the default) never changes your commits: they hold only
your own changes. An editable one (`--edit`) is a branch you build on:
see [Editable layers](#editable-layers-building-on-another-branch).

```
gitfold layer                          # your layers, what your commits sit on, conflicts
gitfold layer add agent/api            # agent/api's work, kept up to date
gitfold layer add agent/api --frozen   # its work as it is right now, never updated
gitfold layer add agent/api --commits  # only what it has committed, kept up to date
gitfold layer add v1.2                 # a tag or a commit: fixed
gitfold layer remove agent/api         # stop showing it
gitfold layer remove --all             # stop showing every layer
```

- **Adding a layer again replaces it** with the flags you give, and brings
  it up to date (a frozen one too). A branch is never shown twice (nor
  with your proposal to change it: adding one replaces the other).
- **The listing** (`gitfold layer`) shows one line per layer: what it
  shows (`all work`; `commits only` with --commits; `commits (no
  workspace)` for a branch without one; `commits` for a tag or a commit),
  how it updates (`live, last 14:07`, with `; updates at your next turn`
  when its source has newer work; `frozen at 14:02`; `fixed`) and, for an
  editable one, `built on` (`built on (with db): your 3 commits sit on
  them` with several); `Your commits sit on main.` if none is editable;
  then `Note: ...` lines (a landing, a blocked move, an update needed, the
  push to run), `Conflict: <path> ...` lines (below), or `No layers.`
- **Not a layer:** the main branch (your branch has its commits already),
  and on a branch without a workspace, a see-only layer (it shows in a
  workspace; `--edit` works there).
- **A see-only layer's files are read-only here.** They are another
  agent's work in progress; a change made here would become your own copy
  of the file, hiding theirs from you and landing in your commits. Writing
  one fails with `EACCES` (Claude Code's edit tools are denied, saying
  whose it is). Ways out, in this order:
  - **build on its branch** if your code depends on its code (as a stacked
    pull request would): `gitfold layer add <BRANCH> --edit` puts your
    commits on its commits, and its files become yours to edit (see
    [Editable layers](#editable-layers-building-on-another-branch)). A
    pull request for your branch then includes its commits until it lands.
    For a file several see-only layers changed, the denial names every
    `--edit` needed;
  - **propose the change**: `gitfold propose <file>` (see "Contracts"
    below): you edit a copy of the layer's workspace, shown here in its
    place, and its agent accepts it;
  - **ask its agent** to change it: `gitfold send <BRANCH> -s "<file>" "<what
    and why>"`; it shows here once they have made it (next turn);
  - or **`gitfold layer remove <BRANCH>`**, then edit your own version (a
    file of yours at a layer's path always wins over the layer, and stays
    yours).

  If you only wanted to add lines to a list (a changelog, a registry, a
  module list), the denial says so: if the order of its lines doesn't
  matter, it can be marked `merge=union` (below), and then every workspace
  adds its lines to it.
- **Whole files:** a file both you and a layer changed shows your version,
  unless it is union-merged (below).
- **A layer brings only its own changes.** A file it changed shows your
  branch's version with the layer's changes merged in (as merging its
  branch would), so a fix your branch got after the layer started stays,
  and a layer of a branch built on your commits brings only what it did
  after them. Several layers changing one file are merged in one after
  another. If a layer changed the same lines as your branch or another
  layer, the file shows git's conflict markers (`<<<<<<< main + <A>` ...
  `>>>>>>> <B>`, named after what your branch builds on: main, or your
  editable layers), still read-only, and `gitfold layer` lists it
  (`Conflict: <path>: <A>'s change conflicts with main's`, or `... is
  changed by both <A> and <B>`); a file that can't be merged at all
  (binary, deleted on one side and changed on the other) shows your
  branch's version, also listed. Such a file belongs to every layer that
  changed it: ask them (`gitfold send`) to update or agree, or `gitfold
  layer remove` one of them.
- **A branch with no history in common with yours** can't be a layer
  (`layer add` refuses, exit 4).
- **`git status` shows only your work.** The see-only layers' files are
  hidden from git (skip-worktree, and excluded if new): `status`, `diff`,
  `add -A`, `commit -a` see and commit only your changes, as do `gitfold
  changes` and `list`. Commit and push normally (with union files, see
  below). If git refuses to touch a see-only layer's file (a merge or
  rebase over it), remove that layer (`gitfold layer remove <A>`) and
  retry; `--all` would also take your commits off your editable layers.
- **Union files: a file several agents add to.** For a registry, a
  manifest or a module list that you and a layer both append to, mark it in
  your branch's attributes so it shows everyone's lines instead of one
  whole file: add `<path> merge=union` (e.g. `registry.txt merge=union`) to
  `.gitattributes` and commit it before adding the layers, or,
  uncommitted, to `$(git rev-parse --git-common-dir)/info/attributes`,
  then add the layers again (`gitfold layer add <BRANCH>`). Only files your
  branch has are union-merged. A union file shows your branch's version
  with every layer's lines and stays writable: add your lines anywhere
  (lines both sides add at one place are all kept). Your lines keep their
  place among the layers' lines in the workspaces that show your work
  along with those layers (your snapshot records what you saw); where
  those layers aren't shown, your lines are placed by the lines that
  branch has. A `note:` after `gitfold layer add` says if a layer's change
  to it couldn't be union-merged in, or was placed by your branch's lines.
- **`Warning: ... lines in <file> sit among <A>'s: this file may not be a
  list`** (in `gitfold layer`, and after a switch) means lines were added
  between two of another branch's lines, inside its block (a method in its
  class). Union merging is for lists; code that goes inside another branch's
  code belongs on that branch: build on it (`gitfold layer add <A>
  --edit`), or ask its agent.
- **With union files, git shows and commits only your lines.** HEAD stays
  on your branch: a filter gitfold answers strips the layers' lines when git
  reads the file and puts them back when git writes it, so `git status`,
  `git diff`, `add -p`, `commit -a`, `--amend`, `stash`, `reset`, `rebase`,
  `switch` and `push` work as usual and hold only your lines. Commit and
  push normally.
- **`gitfold: refusing to update refs/heads/<you>: commit <id> adds <A>'s
  uncommitted line ...`** means a commit would have put another agent's
  unfinished lines on your branch: one made without the filter
  (`--no-filters`, plumbing, a libgit2 tool), or after a formatter changed
  their lines (then `git diff` shows their lines as yours). Nothing moved.
  Restore their lines as the file showed them (`git checkout -- <file>`
  and redo your own change, or undo the formatter's change to their lines),
  then commit again. `--no-verify` doesn't skip this check; `git push` checks
  the same.
- **Don't change other agents' lines in a union file** (reorder, reformat or
  delete them): git doesn't see such changes as yours to commit. To change
  their lines, ask their agent with `gitfold send`.
- **`fatal: not a git repository: /LAYERS SHOWN (<layers>): git is off here
  (<why>)`** means gitfold couldn't hide the layers from git here (sparse
  checkout, a split or sparse index), so it turned git off rather than let
  you commit others' work. The files still show. To use git again, remove
  the see-only layers (`gitfold layer remove <A>`; `gitfold layer` lists
  them). Don't reach for `--all`: it also takes your commits off your
  editable layers. `gitfold` commands keep working meanwhile.
- **Layers move between turns, not during them.** With the gitfold plugin,
  your live layers move when your next prompt starts, and you are told
  which layers changed (files you read from them may have changed: read
  them again). A live layer of an agent's workspace moves to its work as it
  was at the end of the agent's last turn; of a workspace without an agent
  (one a person edits), to its work as it is at your turn's start. A
  branch without a workspace, or `--commits`, moves to the branch's tip.
  Frozen layers and revisions never move. `gitfold layer add <BRANCH>`
  brings a layer up to date at once.
- **Without an agent, layers move by themselves.** In a workspace where no
  Claude Code hook has run (a person works in it), the mounted daemon moves
  its live layers once they have new work (a person's workspace: written
  to or committed in) and nothing has written to the workspace for 3
  seconds; a move it can't make is tried again 10 seconds later, or once
  you write to the workspace. Each move is a line in the daemon's log, and
  `gitfold layer` shows when each last moved. Your own workspace, once your
  first prompt ran its hook, moves only at your turns.
- **Changed files get a fresh time** at each switch, so incremental builds
  rebuild them.

**For an orchestrator: shared contracts.** Put what the tasks share
(interfaces, schemas, a registry) in a root workspace, and make it a layer
of each child; children then see the contracts as the root writes them, and
can't change them in place (they propose changes, below):

```
gitfold branch agent/contracts --from origin/main
gitfold branch agent/child-1 --from origin/main
ls ~/mnt/repo/agent/child-1 >/dev/null               # opens its workspace
gitfold layer -w agent/child-1 add agent/contracts   # each child
gitfold track-reads agent/child-1 on                 # who read what (shown by consent)
```

The workspace a layer is added to must have been opened (else exit 3); a
layer of a branch without a workspace shows its commits.

Children can also be layers of each other, or of a reviewer's workspace. To
**check the work together**, merge it into a snapshot (`cd $(gitfold
snapshot agent/contracts agent/child-1 agent/child-2)`, see the merge
previews above), or add the children as layers of a workspace that builds
and tests them (`gitfold layer add`, then `gitfold layer remove` each
when done).

### Contracts: proposing a change, consent of the affected

**As a child**, when a contract (a layer's file) must change for your task:

1. `gitfold propose <file>...` (from your workspace). It prints the new
   branch (`<layer>-prop/<you>`, a copy of the contracts' work, uncommitted
   included) and where to edit:
   `<mount>/<layer>-prop/<you>/<file>`. Your workspace now shows the
   proposal in the layer's place. `--from <DIR>` copies `<DIR>/<file>` in
   instead.
2. Edit the files **there** (the path printed; not in your workspace,
   where they stay read-only). Keep the change minimal: others build on
   these files. Your next turn shows the edits in your workspace (or now:
   `gitfold layer add <branch>`); then build and test your own work
   against them.
3. Tell the layer's agent: `gitfold send <layer> -s "proposal" "<branch>:
   <what changed and why>"`. Wait for its answer in your inbox: `contract
   changed` (accepted; your workspace shows the layer again, with it) or
   `proposal rejected` (with a reason; your workspace shows the layer as it
   was). Running `gitfold propose` again adds to the same proposal;
   `gitfold reject <branch>` withdraws it.

**As the root** (the contracts' agent), when told of a proposal:

1. `gitfold proposals` lists them: who, the files changed (diff stat), and
   the last consent check. `gitfold changes <branch>` and `git diff`
   in `<mount>/<branch>` show more.
2. `gitfold consent <branch> --test '<build or test command>'` checks what
   accepting it gives (your work as now with the proposed change) with
   every child showing your workspace: each child's own work merged with
   it, and the command run in the child's view with it (a read-only
   snapshot; `$GITFOLD_WORKSPACE` names the child, `$GITFOLD_SCRATCH` is a
   writable directory for build output, e.g. `CARGO_TARGET_DIR=$GITFOLD_SCRATCH/target
   cargo check`; `--timeout` seconds, default 600). Exit 0: all pass. Exit
   4: the table says who fails and how (`merges` `no`: conflicts with the
   child's own files; `test` `failed`: the exit code and last line, and the
   snapshot to look in), or nobody was checked. Its `read` column says
   whether the child read the proposed files: `yes`, `no`, or `unknown`
   when its reads aren't recorded (`gitfold track-reads`); unknown counts
   as read. Nothing in any workspace changes.
3. If it passes, `gitfold accept <branch>` in your workspace: the files are
   written as your uncommitted changes (commit them as usual), the proposer
   shows your workspace again, every child not known to have skipped the
   files gets a `contract changed` message with the proposed lines, and
   all see it at their next turn. Accept needs a passed consent of the
   proposal and your work as they are now: if either changed since, check
   again (`--force` skips this; only when the failures are expected and
   the children told). Exit 4 with `changed <files> since` means you
   changed those files since the proposal was made: `gitfold reject
   <branch> --reason "<why>"` and ask for a new one. Otherwise reject with
   a reason the proposer can act on.

**A child told `contract changed`** reads the message (the changed lines),
re-reads the files it uses (the message says when your layer shows the
change: a live one at your next prompt, or now with `gitfold layer add
<layer>`; a frozen one when you add it again with `--frozen`), and adapts
its work.

## Editable layers: building on another branch

When your code depends on another branch's code that isn't merged yet
(stacked pull requests, `main <- p1 <- p2`), build on it: make it an
**editable** layer. Your commits then sit on its commits, what it
committed is yours to edit, and what it hasn't committed yet shows on
top. gitfold carries your commits along when it gets new ones.

```
gitfold branch p2 --edit p1             # p2 at p1's tip, built on it
gitfold layer add p1 --edit             # in your workspace: your commits moved onto p1's
gitfold layer add p1 --edit --frozen    # built on p1 as it is now; not followed by itself
gitfold layer add p1                    # p1 see-only again: your commits off p1, onto main
gitfold layer remove p1                 # the same, and p1 no longer shown
gitfold layer                           # p1 ... built on: your 3 commits sit on it
gitfold list                            # each branch under what it builds on, with notes
```

- **When to use `--edit`:** your code calls or changes its code, and your
  pull request can include its commits until it lands. Only to look at its
  work, keep the layer see-only. To change one of its files without
  building on it, propose a change ("Contracts" above) or ask its agent.
  `--edit` takes a branch, not a tag or a commit, nor main (every branch
  sits on main already).
- **The denial for a read-only file** offers `gitfold layer add <A>
  --edit` first, saying `your commits will sit on A's (a pull request for
  your branch then includes A's commits until A lands)`. Building on a
  branch is a decision about your pull request: if the task didn't say
  so, ask the user, or propose the change instead. For a file several
  see-only layers changed, it names every `--edit` needed (one alone
  leaves the others' lines read-only).
- **Its uncommitted lines: your edits to them wait.** A file it changed
  but hasn't committed yet shows its lines, and you may edit them, but
  your branch doesn't have them yet, so git sees only your own lines there
  (`git status`, `git diff` and `git commit -a` leave its lines out). A
  change to its lines (a changed line, or lines added inside its block, a
  method in its new class) is a **waiting edit**: kept in the file, not
  seen by git, and listed (`gitfold layer`, and the turn's notes): `1
  waiting edit in api.rs: it changes A's uncommitted lines; it becomes
  yours to commit once A commits them`. Once A commits those lines and your
  branch moves onto them (your next turn), they are ordinary uncommitted
  changes: commit them as usual. A file A added and hasn't committed is
  hidden from git until then: all your edits to it wait. If A changes the
  lines you edited meanwhile, the file shows git's conflict markers
  (`Conflict: <file>: your waiting edits conflict with A's newer work`):
  edit them out. `gitfold layer add A` (see-only) and `gitfold layer remove
  A` refuse (exit 4) while you have waiting edits of A's lines: undo them,
  or wait for A to commit.
- **`WARNING: commit <id> ... has <A>'s uncommitted lines in <file>`** (in
  `gitfold layer` and the turn's notes) means a commit on your branch was
  made without gitfold's filter and hooks (a GUI built on libgit2) and
  holds another branch's unfinished lines. Take it out before pushing with
  the `git reset --soft <id>^` it gives, and commit again with git; `git
  push` refuses it meanwhile.
- **It says first what it moves** (`moves 3 commits of p2 onto p1`; with
  no commits of your own, `p2 now builds on p1`), then what it did.
  Refused, it says what it would have moved (`would move ...`), then why
  (exit 4). Commits already pushed are refused unless you add `--yes`: ask
  the user first, it rewrites published history.
- **At the start of each of your turns** (the plugin's `UserPromptSubmit`
  hook), your commits are moved onto your editable layers' new commits,
  and you are told the files that changed under you (read them again),
  whether your own commits were rewritten, and the push to run if they
  were pushed. Waiting edits whose lines the layer committed are named:
  they are ordinary changes now, to commit as usual. Never mid-turn. A
  frozen layer isn't followed: `gitfold layer add p1 --edit --frozen`
  again brings it up to date. Until your turn, the listing says `needs
  update: p1 has new commits` and, on a layer's line, `updates at your
  next turn`. In a workspace without an agent (a person's), the daemon
  moves it once nothing has written to it for a few seconds.
- **gitfold never pushes.** After a move that rewrote pushed commits,
  push with the printed `git push --force-with-lease=...` (`list` and
  `gitfold layer` note it too); it never suggests a push onto main.
  Suggest it to the user unless pushing is part of your task.
- **When it can't move,** nothing changes, and you are told once:
  - `blocked: commit <id> conflicts on <paths> with p1`: rebase by hand in
    your workspace (`git rebase --onto p1 <start>`), then `gitfold layer
    add p1 --edit`;
  - `waits: uncommitted changes to <file> overlap the move`: commit or
    stash them; it happens at the next turn (staged changes there wait
    too);
  - `2 commits change p1's lines, so they can't stand without p1: ...`
    (from `layer add p1` or `layer remove p1`): keep p1 editable, or
    change those commits not to depend on its lines;
  - with several editable layers, `p1 and db conflict with each other on
    <paths>: ask one of them to resolve it, or gitfold layer remove db`.
- **Several editable layers:** your commits sit on a merge of their
  commits that gitfold makes. Adding a second warns that a pull request
  for your branch shows all of their commits until they all land: open it
  after they land, or build on one of them.
- **When it lands:** while p1's branch still exists, you are told it
  looks merged (`p1 looks merged into main as <id>, but p1 is still
  there: gitfold layer remove p1 moves p2 onto main with only its own
  commits`). Suggest `gitfold layer remove p1` to the user rather than
  running it: it rewrites your history (and asks `--yes` for pushed
  commits). Once p1's branch is deleted, your next turn moves your commits
  there by itself; you are told, with the branch your pull request should
  now target: suggest the push and then `gh pr edit <BRANCH> --base main`
  to the user;
  don't run them yourself. Merged into another branch (p1 into p0), p0
  becomes your editable layer.
- **Your workspace moves with your commits:** uncommitted work elsewhere
  stays, your other layers stay shown, and files the move changes get a
  fresh time, so builds see them. While a move is under way (or one was
  interrupted), `gitfold layer add` refuses: retry after it, or at your
  next turn.

## When something fails

- `Transport endpoint is not connected`: the mount's daemon is gone. Run
  `gitfold unmount <MOUNTPOINT>`, then `gitfold mount <REPO> <MOUNTPOINT>`.
- `EBUSY` opening a workspace: its branch is checked out elsewhere, or being
  removed or copied. Wait and retry, or use another branch.
- `gitfold version <MOUNTPOINT>` exits 3 if the mount runs a different build
  than the installed one: remount to upgrade.
- The daemon log path is printed by `gitfold mount`, by default
  `~/.local/state/gitfold/`.
