---
name: gitfold
description: How to work in a gitfold mount (a repository served as a FUSE filesystem, with read-only @<rev> snapshots and writable per-branch workspaces), and how to run several agents on it, including branches built on other agents' branches. Use when the working directory is under a gitfold mount, when the user mentions gitfold, or when setting up workspaces for parallel agents.
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
gitfold inbox --wait --timeout 600   # none yet? wait up to 10 minutes for one (exit 124 if none came)
gitfold inbox --peek           # list them without taking
gitfold watch agent/b:.        # wait until something in agent/b's task folder changes
```

Each message is taken once. Messages are files in `.task/inbox/`, so anything
can send one: an orchestrator, a script, a person (`gitfold send <BRANCH>
<TEXT>` from the repository, as `user`).

**Delivery without polling, in Claude Code:** the gitfold plugin's hooks
deliver them (see [Claude Code](#claude-code) below). Without the plugin, add
these (the `parent` ones also mark your presence and follow your branch's
parent at turn start)
to `~/.claude/settings.json` (the commands do nothing outside a gitfold
workspace):

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command",
      "command": "gitfold parent --claude-hook session-start" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command",
      "command": "gitfold parent --claude-hook session-end" }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command",
      "command": "gitfold parent --claude-hook subagent-stop" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command",
      "command": "gitfold parent --claude-hook user-prompt-submit",
      "timeout": 600 }, { "type": "command",
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
  mounted it (`gitfold.mountpoint` in its git config), with the flags it had
  then (`gitfold.mountOptions`);
  if none is recorded, creating a worktree fails, saying so: mount it.
- **Other repositories are left alone:** a git worktree in
  `.claude/worktrees/<name>`, as without the plugin.
- **Where gitfold can't run** (not Linux, or no release binary that runs, as
  on NixOS without `nix-ld`: build gitfold and put it on `PATH`), the same,
  except that in a repository used with gitfold creating a worktree fails,
  saying why.
- **Inbox messages arrive in the session** (see [Messages](#messages-between-agents)).
- **Your branch takes its parent's new commits at your turn start**, if it
  is built on a branch and follows it (`gitfold parent`): what moved is said
  then, or why following is paused (in full once, then a one-line reminder
  each turn: resolve it as it says, or ask the user). While your session is
  active in a workspace the daemon doesn't move its branch; once the session
  ends (or no hook ran for 30 minutes) it does, when the workspace is idle.

The hooks are `gitfold branch --claude-hook worktree-create`, `gitfold remove
--claude-hook worktree-remove`, the `inbox --claude-hook` ones above, and
`gitfold parent --claude-hook session-start`, `session-end`, `subagent-stop`
and `user-prompt-submit` (your presence, and following at turn start).
Outside a workspace they do nothing; an error inside one exits 1, shown to
the user, never blocking. `worktree-remove` removes the workspace even while
the repository isn't mounted. A
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

**To build and test several branches together,** start a branch at their
merge: `gitfold branch try --from agent/a --from agent/b` runs `git merge` in
the new workspace (a conflict leaves git's merge in progress there: `git
merge --continue` or `--abort`). `-w` adds each one's uncommitted changes,
and then a merge that doesn't complete leaves nothing.
The new branch is an ordinary one, unrelated to them afterwards.

## Building on another branch

When your work needs another task's commits before they land, build your
branch on that branch, its **parent**:

```
gitfold branch agent/ui --parent agent/api   # starts at api's tip, built on it
gitfold parent                               # what yours is built on, following or not
gitfold parent --update                      # take the parent's new commits now
```

- **Following:** a branch built on another takes the parent's new commits by
  itself (with the plugin, at your turn start, and you are told what moved),
  replaying its own commits onto them, your uncommitted work carried along.
  `--no-follow` (or `gitfold parent --no-follow`) leaves it to `--update`.
- **When it can't:** a conflict moves nothing; you are told the paths and
  how to resolve it once, then reminded each turn. Commits you already
  pushed move only with `gitfold parent --update --yes`, which prints the
  force-push to run; gitfold never pushes.
- **When the parent lands** in the main branch (or in its own parent): once
  its branch is deleted, yours, if it follows, moves there with only its own
  commits; while it still exists, following notes `api looks merged into
  main` (in `gitfold list` too), and `gitfold parent --set main` does that
  move. gitfold never fetches: `git fetch` to let it see a merge done on the
  forge.
- `gitfold parent --set <P>` moves your commits onto another parent;
  `--none` forgets the parent and keeps the history as it is.
- `gitfold list` shows the tree, and notes what needs doing (`conflict on
  PATH`, `following paused: pushed`, `api looks merged into main`, `parent
  gone`).

**Reading the others' work:** `<mount>/@committed/<branch>/` is that branch
merged with every branch built on it, read-only (`git merge-tree`, so
conflicts are in the files as git leaves them); `<mount>/@live/<branch>/` the
same with each workspace's uncommitted work. `.gitfold-conflicts` at its root
lists the conflicts and which branches they are between. To see who else is
changing a file: `gitfold changes -- <PATH>...` (one line per branch,
`committed` or `uncommitted`).

## Running many agents

- **One branch and workspace per task.** Workspaces cost what their changes
  weigh, so dozens are fine. An agent that only reads needs only a snapshot:
  `cd <mount>/@<sha>`.
- **Agents that only read a few files:** `gitfold branch <BRANCH> --prewarm
  none` skips the background walk that lists the whole tree when a workspace
  opens.
- **Watching them:** `gitfold list` shows each workspace's changed, deleted and
  ignored file counts, and how long ago it last changed (a file or a commit):
  an agent idle for long stands out, and the tree of branches built on
  branches. `--stat` adds line totals.
  `gitfold changes <BRANCH>` shows one's changed paths. All read stored
  state, so polling many workspaces is cheap.
- **Is the work stale, or colliding?** `gitfold overlaps` (inside a workspace, or
  `gitfold overlaps <BRANCH>`) lists paths the workspace changed (`W`) or read (`R`) that its
  remote's default branch (`origin/HEAD`, else `main`) changed since they
  diverged. `--against <REV>` picks
  another target. `--against <BRANCH>` of another workspace includes its
  uncommitted changes, and `--against-workspaces` checks every other one: two
  agents editing the same files. It exits 6 if anything overlaps, 0 if not
  (1 if a workspace it couldn't compare with was skipped, said),
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
- **Merge previews:** for branches built on one another, the views show them
  merged (see [Building on another branch](#building-on-another-branch)).
  For any branches, without making a branch or a workspace, make the merge
  commit with git and open it as a snapshot.
  No working tree, index or ref is touched (git 2.38 or later). This is a
  bash function: it runs in a subshell, so the identity doesn't leak into your
  shell.

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

## When something fails

- `Transport endpoint is not connected`: the mount's daemon is gone. Run
  `gitfold unmount <MOUNTPOINT>`, then `gitfold mount <REPO> <MOUNTPOINT>`.
- `EBUSY` opening a workspace: its branch is checked out elsewhere, or being
  removed or copied. Wait and retry, or use another branch.
- `gitfold version <MOUNTPOINT>` exits 3 if the mount runs a different build
  than the installed one: remount to upgrade.
- The daemon log path is printed by `gitfold mount`, by default
  `~/.local/state/gitfold/`.
