# gitfold user guide

Coding agents work best one task per branch, each in its own checkout. On a
small repository that is easy: `git worktree add` and go. On a large one it
isn't. A repository with a million files takes minutes and tens of gigabytes
to check out again, so you end up with a few shared checkouts, agents waiting
for each other, and branches switched under someone's feet.

gitfold mounts your existing clone as a folder in which every branch is a
workspace and every commit a read-only snapshot. Opening one takes about a
second however big the repository, and costs only the files you change. Each
is a real git worktree, so agents, editors and git itself work there as in
any checkout. This guide walks through the situations where that pays off.
For every command and option, see the [README](../README.md); for the advice
gitfold gives agents, the [skill](../skills/gitfold/SKILL.md).

To install gitfold (or its Claude Code plugin), see the README's [Quick
start](../README.md#quick-start). The examples use a small repository, `shop`,
cloned at `~/src/shop` and mounted at `~/mnt/shop`:

```
$ mkdir -p ~/mnt/shop
$ gitfold mount ~/src/shop ~/mnt/shop
mounted at /home/you/mnt/shop (log: /home/you/.local/state/gitfold/...)
unmount with: gitfold unmount /home/you/mnt/shop
```

The mount runs in the background until you unmount it. gitfold commands
work on the repository you are in, like git: run them in `~/src/shop` or in
any workspace.

Contents:
[Many agents, one big repository](#1-many-agents-one-big-repository) ·
[One agent building on another](#2-one-agent-building-on-another) ·
[Keeping an eye on a team](#3-keeping-an-eye-on-a-team) ·
[Trying agents' work together](#4-trying-agents-work-together) ·
[Agents talking to each other](#5-agents-talking-to-each-other) ·
[Old revisions and cleaning up](#6-old-revisions-and-cleaning-up) ·
[When something goes wrong](#when-something-goes-wrong)

## 1. Many agents, one big repository

**The situation.** You want three agents on three tasks at once: fix a tax
bug, add search, refresh the docs.

**With plain git**, each agent needs its own checkout. On a big repository
every `git worktree add` copies the whole tree: on Chromium that is 17
seconds and 5.5 GiB each, before the agent has read a line.

**With gitfold**, create a branch per task and open its folder:

```
$ cd ~/src/shop
$ gitfold branch agent/fix-tax --from origin/main
created branch agent/fix-tax; open it as /home/you/mnt/shop/agent/fix-tax
$ gitfold branch agent/add-search --from origin/main
$ gitfold branch agent/docs-refresh --from origin/main
$ cd ~/mnt/shop/agent/fix-tax
$ ls -a
.  ..  .git  .task  README.md  docs  src
$ git status
On branch agent/fix-tax
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean
```

Start an agent in each folder. They edit, build, commit and push as in any
checkout. `gitfold list` shows them all at a glance: how many files each has
changed, when it last did something, and the line it keeps in
`.task/status`:

```
$ gitfold list
branch                                    changed  deleted  ignored  base           last  follow  status
agent/add-search                                1        0        0  a14c44764575     3s
agent/docs-refresh                              1        0        0  b291a8fad806     4s
agent/fix-tax                                   1        0        0  b291a8fad806     4s          fixing tax rounding
```

**With Claude Code**, the gitfold plugin does this for you: in a mounted
repository, `claude -w review-pr` (and every subagent with
`isolation: worktree`) gets a workspace on a new branch `worktree-review-pr`,
at `~/mnt/shop/worktree-review-pr`, instead of a full worktree under
`.claude/worktrees/`.

**Why it works.** Nothing is copied: unchanged files are read straight from
git's object database, and only the files an agent writes are stored, under
`.git/gitfold/`. A Chromium workspace opens in under a second and takes no
more disk than its changes.

## 2. One agent building on another

**The situation.** One agent builds an API (`api`); another builds the UI
that uses it (`ui`) and needs the API's commits before they are merged.

**With plain git**, the UI agent branches off `api`, and every time `api`
gets a new commit someone has to rebase `ui` onto it. When `api` is
squash-merged on GitHub, `ui` still carries `api`'s original commits and
needs a careful `git rebase --onto`.

**With gitfold**, build `ui` on `api`:

```
$ gitfold branch api --from origin/main
$ cd ~/mnt/shop/api
$ ...                                    # the API agent works and commits
$ gitfold branch ui --parent api
created branch ui on api (following it); open it as /home/you/mnt/shop/ui
$ cd ~/mnt/shop/ui
$ gitfold parent
ui is built on api, following it
```

`ui` now follows `api`. When the API agent commits again, `ui` takes the new
commit by itself, its own commits replayed on top:

```
$ cd ~/mnt/shop/ui && git log --oneline
4130972 UI: show discount
f095881 API: discount per user
39a53c6 API: discount endpoint
b291a8f Initial shop
```

With Claude Code, this happens at the UI agent's next turn, and it is told
what moved:

```
gitfold: since your last turn, ui took its parent's new commits:
moved 1 commit of ui onto api
```

Without an agent in it, the workspace moves once nothing has been written in
it for a few seconds.

**When they conflict.** If both agents change the same lines, nothing moves.
The UI agent is told, and `gitfold list` shows it:

```
gitfold: following paused: ui's commits conflict with api's on src/api/discount.rs. Rebase by hand (git rebase api ui), or wait for api to change
```

```
$ gitfold list
branch                                    changed  deleted  ignored  base           last  follow  status
api                                             0        0        0  b2387787e780     5s
└─ ui                                           0        0        0  fe69db0f8f38     5s  on      (conflict on src/api/discount.rs)
```

Resolve it as any rebase, in the `ui` workspace:

```
$ git rebase api ui
CONFLICT (content): Merge conflict in src/api/discount.rs
$ vi src/api/discount.rs && git add src/api/discount.rs
$ git rebase --continue
Successfully rebased and updated refs/heads/ui.
```

Following resumes by itself. Or don't: once `api` changes again, gitfold
tries again.

**When `api` is merged.** `api`'s PR is squash-merged on GitHub. gitfold
never fetches, so fetch to let it see the merge:

```
$ git fetch
$ gitfold parent ui
ui is built on api, following it
api looks merged into origin/main: `gitfold parent ui --set origin/main` moves ui there with only its own commits (or delete api, `git branch -D api`, and ui moves there by itself)
```

Do either. Deleting `api` (first its workspace, then the branch) moves `ui`
by itself, and the agent is told:

```
$ gitfold remove api
removed workspace api
$ git branch -D api
Deleted branch api (was b238778).
```

```
gitfold: api landed in origin/main; ui is now built on origin/main
moved 1 commit of ui onto origin/main
```

```
$ git log --oneline
d1352bc UI: show discount
623fe50 API: discounts (#12)
b291a8f Initial shop
```

**Why it works.** gitfold remembers where `ui`'s own commits start, so it can
always replay just those with `git rebase --onto`, whether `api` grew or was
squashed into main. It moves a branch only at safe moments, never moves
commits you have pushed without asking, and leaves conflicts to you.

## 3. Keeping an eye on a team

**The situation.** A lead agent (or you) owns a feature branch, `checkout`.
Two subagents work on it: `checkout-api` and `checkout-ui`. You want to see
the feature as a whole while they work, and catch collisions before anyone
merges.

**With plain git**, you can't see the combined result until someone merges,
and you can't see uncommitted work at all. Collisions show up at merge time.

**With gitfold**, build both on the feature branch:

```
$ gitfold branch checkout --from origin/main
$ gitfold branch checkout-api --parent checkout
$ gitfold branch checkout-ui --parent checkout
```

The lead works in `~/mnt/shop/checkout/`, each subagent in its own folder.
Then read the feature through a view. `~/mnt/shop/@live/checkout/` is
`checkout` merged with every branch built on it, uncommitted work included,
and kept up to date as they write. `@committed/checkout/` is the same with
committed work only.

```
$ ls ~/mnt/shop/@live/checkout/src/api
checkout.rs  discount.rs  prices.rs
$ cat ~/mnt/shop/@live/checkout/.gitfold-conflicts
CONFLICT (contents)	checkout+checkout-api, checkout-ui	README.md
$ cat ~/mnt/shop/@live/checkout/README.md
# Shop

<<<<<<< ce19712154e3b26ae34f92d9dbb61f2304959aff
Checkout: POST /checkout
=======
Checkout: click Buy
>>>>>>> a5a3b2c48b69067c3b99189cb8c8c87cdf5c874b
```

Both agents are documenting checkout in `README.md`, and one of them hasn't
even committed yet. To ask who is touching a file:

```
$ gitfold changes -- README.md
checkout-api	committed	README.md
checkout-ui	uncommitted	README.md
```

`gitfold overlaps` asks the same from one agent's side: which of the paths it
changed someone else changed too. Here, against every other workspace (it
exits 6 when something overlaps):

```
$ cd ~/mnt/shop/checkout-ui
$ gitfold overlaps --against-workspaces
note: no read log for checkout-ui (reads aren't tracked, or it hasn't been opened since): writes only
W	checkout-api	README.md
W	agent/add-search	src/ui/index.html
W	ui	src/ui/index.html
```

Run without `--against...`, it checks against the main branch: has
something the agent changed (or, with read tracking on, read) changed there
since it started?

**Told without looking.** You don't have to keep reading the views: when a
commit in one workspace conflicts with another's work, gitfold leaves a
message in the inboxes of everyone involved: both sides, and the branch they
land in if it has a workspace. When `checkout-api` commits its `README.md`:

```
$ gitfold inbox checkout-ui
from: gitfold
time: 2026-11-18T10:12:31Z
subject: conflict: README.md

checkout-api committed 0724067; it conflicts with checkout-ui's uncommitted README.md.
If it needs resolving, checkout-ui's change is the one still open, so that's the cheapest place to adjust.
```

`checkout-api` and the lead's `checkout` get the same message; with Claude
Code, it arrives in each agent's conversation by itself, and wakes an idle one. A
notice only informs: whether it matters (not for a throwaway workspace, say)
is for the agents, or for you. Who is told, the hint and the settings: see
[the README](../README.md#branches-built-on-branches).

**Why it works.** The views are made by `git merge-tree`, git's own merge,
without touching any branch, index or working tree; conflicts look exactly
as a real merge would leave them. `gitfold changes` and `overlaps` read what
each workspace stored, so polling a dozen agents costs next to nothing.

## 4. Trying agents' work together

**The situation.** Two agents finished separate tasks. Before merging either,
you want to build and test them together, uncommitted bits included. Or an
agent is halfway through and wants to try a second approach without losing
the first.

**With plain git**, that's a new checkout, a merge, and copying uncommitted
files by hand (or committing half-done work).

**With gitfold**, start a branch at their merge. `-w` brings each one's
uncommitted changes along:

```
$ gitfold branch try -w --from agent/fix-tax --from agent/add-search
Updating b291a8f..a14c447
Fast-forward
 src/api/search.rs | 1 +
 src/ui/index.html | 1 +
 2 files changed, 2 insertions(+)
 create mode 100644 src/api/search.rs
created branch try; open it as /home/you/mnt/shop/try
$ cd ~/mnt/shop/try && git status --short
 M src/api/prices.rs
 M src/api/search.rs
```

If the branches conflict, you get git's merge in progress, as if you had run
`git merge` yourself:

```
$ gitfold branch try-checkout --from checkout-api --from checkout-ui
Auto-merging README.md
CONFLICT (content): Merge conflict in README.md
Automatic merge failed; fix conflicts and then commit the result.
Error: created branch try-checkout with its merge in progress (see above): finish it in /home/you/mnt/shop/try-checkout with `git merge --continue`, or `git merge --abort`
$ cd ~/mnt/shop/try-checkout
$ vi README.md && git add README.md
$ git merge --continue
[try-checkout c8e479d] Merge branch 'checkout-ui' into try-checkout
```

(With `-w`, a merge that conflicts is refused instead, and nothing is
created: the uncommitted changes need a finished merge to go on.)

In a terminal, git opens your editor for the merge message, as `git merge`
does; `GIT_MERGE_AUTOEDIT=no` skips it.

To fork a single workspace, half-done state and all, run this inside it:

```
$ cd ~/mnt/shop/agent/fix-tax
$ gitfold branch -w agent/fix-tax-b
created branch agent/fix-tax-b at agent/fix-tax, with its uncommitted changes; open it as /home/you/mnt/shop/agent/fix-tax-b
```

`try` and `agent/fix-tax-b` are ordinary branches: keep them, push them, or
`gitfold remove` them and delete the branch.

**Why it works.** Only the changed files are copied, so a fork or a trial
merge costs what the changes weigh, not the tree. The merge is git's own,
run in the new workspace.

## 5. Agents talking to each other

**The situation.** The lead wants to tell `checkout-ui` to leave `README.md`
to the API agent, and wants to know when `checkout-api` hands off.

**With plain git** there is nowhere to put that: not in the branch (it
would be committed), not in the checkout (`git clean` would delete it).

**With gitfold**, every workspace has a task folder, `.task`, which git
doesn't see and which outlives the workspace. Agents keep their spec,
notes, status line and handoff there. Messages go into its inbox:

```
$ cd ~/src/shop
$ gitfold send checkout-ui -s README "checkout-api documents /checkout in README.md; leave that file to them"
sent to checkout-ui
```

The agent in `checkout-ui` takes its messages; each is delivered once:

```
$ gitfold inbox
from: user
time: 2026-11-17T16:42:47Z
subject: README

checkout-api documents /checkout in README.md; leave that file to them
```

With the Claude Code plugin you don't run `inbox` at all: messages appear in
the agent's conversation by themselves, and wake it if it is idle. Other
agents can run `gitfold inbox --wait` between tasks.

To wait for a handoff, watch the other agent's task folder. This waits until
a file in it changes and says which (or, after ten minutes without a change,
prints nothing and exits 124, as `timeout` does; so does `inbox --wait
--timeout`):

```
$ gitfold watch checkout-api:. --timeout 600
C	handoff.md
$ cat ~/mnt/shop/checkout-api/.task/handoff.md
Endpoint done; README section is mine; UI can call POST /checkout
```

**Why it works.** The task folders are plain files under `.git/gitfold/`,
outside the mount, so they are written at disk speed and anything can read
them: a script, an orchestrator, you. Messages are one file each, so sending
one needs nothing but a file write.

## 6. Old revisions and cleaning up

**The situation.** You want to look at last week's code, or compare a file
across revisions, and later clean up the workspaces of finished tasks.

**With plain git**, looking at an old revision means stashing and checking
out, or another worktree.

**With gitfold**, any revision is a read-only folder: `@` and a commit, tag
or branch.

```
$ cd ~/mnt/shop/@origin/main~1
$ git log --oneline -1
b291a8f Initial shop
$ echo x >> README.md
bash: README.md: Permission denied
$ diff ~/mnt/shop/@origin/main/src/api/prices.rs ~/mnt/shop/agent/fix-tax/src/api/prices.rs
2c2
<     id * 10
---
>     id * 10 + id / 5
```

A name like `@origin/main` stays at the commit it showed when first opened.
For the latest, open it by SHA: `cd ~/mnt/shop/@$(git rev-parse origin/main)`.

When a task is done (pushed or merged), remove its workspace. Its commits
stay on the branch. gitfold won't throw away uncommitted work unless you say
`--force`:

```
$ gitfold remove agent/fix-tax
Error: workspace agent/fix-tax has uncommitted changes (1 changed, 0 deleted):
changed:
  src/api/prices.rs
commit them, or use --force to discard them
```

`gitfold remove @origin/main~1` closes a snapshot, and `gitfold unmount
~/mnt/shop` stops the mount. Workspaces keep their changes across unmounts:
mount again and they are where you left them.

**Why it works.** Snapshots are read straight from git and share the kernel's
cache with each other, so opening many costs little. A workspace's whole
state is its stored changes, so removing it is quick and its branch stays.

## When something goes wrong

**`following paused: ... conflict ... on PATH`** (from `gitfold parent`, in
`gitfold list`, or at an agent's turn start). The branch's commits conflict
with its parent's new ones, so nothing moved. Run the rebase it prints in the
branch's workspace and resolve as usual (see
[section 2](#2-one-agent-building-on-another)), or leave it: gitfold tries
again when the parent changes.

**`nothing moved: ...'s uncommitted work on PATH conflicts with ...'s new commits`**
(exit 4).

```
Error: nothing moved: checkout-ui's uncommitted work on docs/api.md conflicts with checkout's new commits. Resolve it either way: commit it, then `gitfold parent checkout-ui --update` reports the conflict, with the git rebase to run; or in its workspace `git stash`, `gitfold parent checkout-ui --update`, then `git stash pop` and resolve the conflict there
```

The parent changed a file you have uncommitted edits in, and they conflict.
Committing or stashing alone doesn't help: the conflict is still there to
resolve. Either commit, run `--update`, and run the rebase it prints (the case
above); or stash, run `--update`, and resolve the conflict `git stash pop`
leaves in the workspace.

**`following paused: pushed`.** The branch's commits are already on its
remote branch, and moving them would rewrite pushed history, so gitfold asks
first (`parent --update` and `--set` move nothing then: exit 4):

```
$ gitfold parent
ui is built on origin/main, following it
following paused: 1 commit is pushed. `gitfold parent ui --update --yes` moves them; then force-push with: git push --force-with-lease=ui:d1352bcd66ae9f4cd51479d4239fda3ec65aaae6 origin ui
$ gitfold parent ui --update --yes
moved 1 commit of ui onto origin/main
its history was rewritten; force-push with: git push --force-with-lease=ui:d1352bcd66ae9f4cd51479d4239fda3ec65aaae6 origin ui
```

Run the printed push yourself; gitfold never pushes.

**`parent gone`** in `gitfold list`. The parent branch was deleted without
landing in the main branch, so gitfold doesn't know where to move your
commits. Pick a new parent (only your own commits move), or forget it with
`--none`:

```
$ gitfold parent checkout-api --set origin/main
checkout-api is now built on origin/main: moved 1 commit
```

**`... is in use; close or cd out of it first`** (exit 5) from `gitfold
remove`. A process still has its working directory or a file in the
workspace; the message names it. Close it, or let `remove` wait:

```
$ gitfold remove worktree-review-pr --wait 10
worktree-review-pr is in use; close or `cd` out of it first:
  3439093 sleep 4
waiting (up to 10 s)...
removed workspace worktree-review-pr
```

**`Transport endpoint is not connected`.** The mount's daemon is gone (killed
or crashed); `gitfold mount` says so, and refuses until the old mount is
cleaned up. Unmount, then mount again. Workspaces keep their changes:

```
$ cat ~/mnt/shop/@HEAD/README.md
cat: /home/you/mnt/shop/@HEAD/README.md: Transport endpoint is not connected
$ gitfold mount ~/src/shop ~/mnt/shop
Error: /home/you/mnt/shop is a stale gitfold mount (its daemon is gone): `gitfold unmount /home/you/mnt/shop`, then mount again
$ gitfold unmount ~/mnt/shop
unmounted /home/you/mnt/shop
$ gitfold mount ~/src/shop ~/mnt/shop
```

**A folder you expected isn't there.** The mount root lists only what has
been opened, so `ls ~/mnt/shop` starts empty: open folders by name.
Workspaces open existing local branches only (create one with `gitfold
branch` first), and a branch checked out elsewhere, say in `~/src/shop`,
fails with `Device or resource busy`: branch from it instead.
