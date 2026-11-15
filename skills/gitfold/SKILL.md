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
- **Layers move between turns** (see [Layers](#layers)).

The hooks are `gitfold branch --claude-hook worktree-create`, `gitfold remove
--claude-hook worktree-remove`, the `inbox --claude-hook` ones above, and for
layers `gitfold seal --claude-hook stop` (`Stop`), `gitfold layer
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
  cd $(gitfold mix main agent/task-17 agent/task-18)   # build and test there
  ```

  It merges each workspace's work (its uncommitted changes as a commit,
  `gitfold seal`), or a branch's or revision's commit, in order onto the
  first (or `--onto <REV>`), and prints the merge as a snapshot path. No
  workspace, index or branch changes. It exits 4 on conflicts, listing the
  paths. The same work gives the same snapshot.

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

A **layer** is another workspace's work (uncommitted included) shown in
yours: its changed files over your base, under your own changes. Several
agents building parts of one change can then build and test against each
other's work as it is, without merging or committing anything.

```
gitfold layer list                # bottom first: on/off, the branch, its seal
gitfold layer add agent/api       # show agent/api's work here, as now
gitfold layer off agent/api       # hide it (kept; `on` shows it again)
gitfold layer update              # every layer to its workspace's work as now
gitfold lens off                  # hide every layer
```

- **A layer's files are read-only here.** They are another agent's work in
  progress; a change made here would become your own copy of the file,
  hiding theirs from you and landing in your commits. Writing one fails
  with `EACCES` (Claude Code's edit tools are denied, saying whose it is).
  Two ways out:
  - **ask its agent** to change it: `gitfold send <BRANCH> -s "<file>" "<what
    and why>"`; it shows here once they have made it (next turn);
  - or **`gitfold layer off <BRANCH>`**, then edit your own version (a file
    of yours at a layer's path always wins over the layer, and stays yours).
- **Whole files:** a file both you and a layer changed shows your version,
  unless it is blended (below).
- **`git status` shows only your work.** The layers' files are hidden from
  git (skip-worktree, and excluded if new): `status`, `diff`, `add -A`,
  `commit -a` see and commit only your changes, as do `gitfold changes` and
  `list`. Commit and push normally (with blended files, see below). If git
  refuses to touch a file you didn't change (a merge or rebase over a
  layer's file), run `gitfold lens off` and retry.
- **Blended files: a file several agents add to.** For a registry, a
  manifest or a module list that you and a layer both append to, mark it in
  the base's attributes so it shows everyone's lines instead of one whole
  file: add `<path> gitfold-blend=hunk` (e.g. `registry.txt
  gitfold-blend=hunk`) to `.gitattributes` and commit it before adding the
  layers (your base doesn't move while layers show), or, uncommitted, to
  `$(git rev-parse --git-common-dir)/info/attributes`, then `gitfold layer
  update`. Only files your base has are blended. A blended file shows the
  base's version with every layer's lines and stays writable: add your
  lines anywhere (lines both sides add at one place are all kept). A
  `note:` after `gitfold layer` says if a layer's change to it couldn't be
  blended in.
- **With blended files, git shows and commits only your lines.** HEAD is
  then detached at a lens commit (`git log -1`: `gitfold lens: <you> +
  <layers> (commits go to <you>; never push this commit)`), and `git
  status`/`git diff` show only your lines in a blended file. **Commit
  normally**: within about a second gitfold puts each commit onto your
  branch with only your changes and moves HEAD to a new lens commit, so
  the commit id you saw changes (`git log <your-branch>` has the real one).
  `git log -1` showing `gitfold lens:` again means it is on your branch.
- **Never push HEAD from a lens** (`git push origin HEAD:<branch>`, or
  `--no-verify`): the lens commit holds other agents' uncommitted work, and
  a push guard refuses it. **`git push origin <your-branch>` is fine.**
- **A commit that waits: it depends on a layer's lines.** If `git log -1`
  still shows your commit (not `gitfold lens:`) after a few seconds, or
  `gitfold layer`/`gitfold lens off` exit 4 with `commit <id> can't go onto
  the branch alone (it changes lines of a layer's)`, that commit removed or
  changed another agent's lines in a blended file. It can't go onto your
  branch without their uncommitted work, so it and any later commit stay
  off your branch. Put their lines back as the file showed them (`git diff
  HEAD~1 -- <file>` shows what you changed), keep only your own additions,
  and `git commit -a --amend` (if it is your last commit; if not, stop and
  tell the user). To change their lines, ask their agent with `gitfold send`.
- **Don't `git switch`, `checkout <branch>`, `reset --soft` or `rebase`
  while lensed**, nor `--amend` once `git log -1` shows `gitfold lens:`
  (that amends the lens commit): HEAD leaving the lens commit's
  line turns git off (below). Run **`gitfold lens off`** first (HEAD goes
  back on your branch; your uncommitted lines stay as changes).
- **`fatal: not a git repository: /LENS ON (<layers>): git is off here
  (<why>)`** means gitfold couldn't hide the layers from git here (sparse
  checkout, a split or sparse index, HEAD taken off the lens commit), so it
  turned git off rather than let you commit others' work. The files still
  show. Run `gitfold lens off` to use git again; `gitfold` commands keep
  working meanwhile.
- **Layers move between turns, not during them.** With the gitfold plugin, a
  workspace shown as a layer is sealed when its session stops, and your
  layers move to the newest seals when your next prompt starts; you are
  then told which layers changed (files you read from them may have
  changed: read them again). Without the plugin, or for a workspace no
  session runs in, `gitfold layer update`.
- **Changed files get a fresh time** at each switch, so incremental builds
  rebuild them.

**For an orchestrator: shared contracts.** Put what the tasks share
(interfaces, schemas, a registry) in a root workspace, and make it a layer
of each child; children then see the contracts as the root writes them, and
can't change them (they ask the root with `gitfold send`):

```
gitfold branch agent/contracts --from origin/main
gitfold branch agent/child-1 --from origin/main
ls ~/mnt/repo/agent/child-1 >/dev/null               # opens its workspace
gitfold layer -w agent/child-1 add agent/contracts   # each child
```

The workspace a layer is added to must have been opened (else exit 3); a
layer's branch without a workspace shows its last commit.

Children can also be layers of each other, or of a reviewer's workspace. To
**check the work together**, mix it into a snapshot (`cd $(gitfold mix
agent/contracts agent/child-1 agent/child-2)`, see the merge previews
above), or add the children as layers of a workspace that builds and tests
them (`gitfold layer add`, then `gitfold lens off` when done). `gitfold seal
<BRANCH>` prints one workspace's uncommitted work as a commit, to open as
`<mount>/@<sha>`.

## Stacked branches

For stacked pull requests (`main <- p1 <- p2`), build the child on the
parent's commits and let gitfold carry it along when the parent changes:

```
gitfold branch p2 --on p1         # p2 at p1's tip, stacked on it
gitfold stack on p1 -w p2         # or stack an existing branch
gitfold stack sync                # your workspace onto its parent's new commits
gitfold stack sync --all          # every stacked branch, parents first
gitfold stack list                # the stacks, with what each needs
```

- When p1 gets new commits (review fixes, a rebase), p2's own commits are
  copied onto them: at the start of p2's agent's next turn (the plugin's
  `UserPromptSubmit` hook, which tells you which files changed under you),
  or by `gitfold stack sync`. Your uncommitted work stays; never mid-turn.
- A commit that conflicts with the parent's new ones leaves your branch as
  it was, and you are told once (`list`: `restack blocked`). Rebase by hand
  in your workspace, `git rebase --onto p1 <base>` (the line names it), then
  `gitfold stack sync`.
- Uncommitted changes to a file the restack changes: it waits, and tells
  you which; commit or stash them, and it happens at the next turn.
- It is refused while layers are shown (`gitfold lens off` first).
- When the parent is merged into main (a merge, squash or rebase merge),
  your branch moves onto that merge with only its own commits and is
  stacked on main from then on; you are told, with the `gh pr edit
  <BRANCH> --base main` to run. It doesn't follow main's later commits by
  itself: `gitfold stack sync` does, when you want them. A merge gitfold
  can't recognise (a squash rewritten when merged): `gitfold stack merged
  <PARENT>`.
- gitfold never pushes. After a restack your branch's history is rewritten:
  push with `--force-with-lease` (`stack sync` prints the command).
- `git config branch.<BRANCH>.gitfoldRestack manual`: only `stack sync`
  restacks it.

## When something fails

- `Transport endpoint is not connected`: the mount's daemon is gone. Run
  `gitfold unmount <MOUNTPOINT>`, then `gitfold mount <REPO> <MOUNTPOINT>`.
- `EBUSY` opening a workspace: its branch is checked out elsewhere, or being
  removed or copied. Wait and retry, or use another branch.
- `gitfold version <MOUNTPOINT>` exits 3 if the mount runs a different build
  than the installed one: remount to upgrade.
- The daemon log path is printed by `gitfold mount`, by default
  `~/.local/state/gitfold/`.
