# gitfold and the alternatives

gitfold mounts your existing git clone and shows every branch and commit as a
folder. Unchanged files are read from git's object store; a branch's workspace
stores only the files you change, and each folder is a real linked git worktree.
It is Linux only and needs no root and no server, but its writes pay FUSE
overhead.

Several tools solve part of the same problem. This page compares them as
fairly as we can: what each does, what a new working copy costs, what it needs,
and when it is the better choice. Facts about other tools link to their own
documentation (sources at the end); "unverified" marks what we couldn't confirm.
gitfold's numbers come from [bench/RESULTS.md](../bench/RESULTS.md). Checked
2026-11-21, against gitfold 0.5.1.

## At a glance

| | Strengths | Weaknesses | Best for |
|---|---|---|---|
| **gitfold** | Any branch or commit as a folder in about a second, almost no disk; real git worktrees; branches follow their parents; merged views and conflict notices across workspaces | Writes through FUSE: slower builds, small files 4-6x slower; Linux only; no Git LFS content, `.gitattributes` filters or submodules | Many working copies of a big repository, such as parallel coding agents |
| `git worktree` | Native speed; full git support; every platform | Each copy writes every file; submodules across worktrees are incomplete | A few copies of a small or medium repository |
| Sparse checkout, partial clone, Scalar | Native speed on a smaller tree; Scalar sets up a big repository with these and background maintenance; every platform | Tools see only the chosen slice; a partial clone still writes each worktree in full and needs its remote | A known part of a huge repository |
| Reflink copies, btrfs snapshots | Native writes; cheap copies of a *built* tree, build output included | Copy-on-write filesystems only; copies an existing checkout (another revision means checking it out in the copy); each copy must be registered, and git re-reads every file once | Copies of built trees |
| overlayfs | Native writes after copy-up | One checkout per base, fixed while mounted; root or a user namespace; no maintained tool we know of sets it up for git | The fastest writes, on a system you control; bases that aren't git revisions |
| EdenFS + Sapling | Files fetched lazily from a server; Linux, macOS, Windows (open-source builds: Linux) | Needs its server, unsupported outside Meta; not plain git (Sapling's `.sl` mode) | Meta, or teams adopting Sapling and its server |
| VFS for Git | Files fetched lazily, for huge repositories | Windows only; needs a GVFS server and Microsoft's git fork; its README recommends Scalar for new deployments | Windows with Azure Repos |
| GitFS (Presslabs) | Versioning by saving files | Commits every change; needs a remote; last release 2019 | People who don't use git |
| jj workspaces | jj's model: first-class conflicts, undo, automatic rebasing; every platform | Full checkouts; no submodules, Git LFS, `.gitattributes` or partial clones | jj users with repositories small enough to check out |
| Stacked-branch tools | Pull-request stacks on a forge (Graphite, git-town, spr, ghstack); every platform | Rebase only when run; `--update-refs` skips branches checked out in a worktree | Review in stacked pull requests |
| Containers per agent | Isolate ports, databases and toolchains too | Each still needs a checkout or a bind mount | Isolating more than files |

## Requirements

| | New working copy (big repo) | Needs a server | Needs root | Platforms | Plain git works | Writes |
|---|---|---|---|---|---|---|
| **gitfold** | 0.87 s, almost no disk (Chromium) | no | no | Linux | yes: a linked worktree | FUSE: kernel build +2.5-4% full, +11-29% incremental |
| `git worktree` | 17 s, 5.5 GiB (Chromium) | no | no | all | yes | native |
| Sparse checkout, partial clone, Scalar | in proportion to what's checked out | partial clone: yes | no | all | yes | native |
| Reflink copies, btrfs snapshots | little disk, copies every inode | no | no | btrfs, XFS, ZFS 2.2+, APFS | once registered as a worktree | native |
| overlayfs | almost free, after one checkout per base | no | root or a user namespace | Linux | once set up as a worktree | native after copy-up |
| EdenFS + Sapling | lazy | yes; unsupported outside Meta | a root helper (setuid or sudo) | Linux, macOS, Windows (open-source builds: Linux) | no (Sapling's `.sl` mode) | unmeasured |
| VFS for Git | lazy | yes (GVFS protocol) | a driver | Windows | through Microsoft's git fork | unmeasured |
| GitFS (Presslabs) | a clone of a remote | yes, to push to | no | Linux, macOS | commits made for you | FUSE (Python) |
| jj workspaces | a full checkout | no | no | all | colocated since jj 0.46 | native |

Stacked-branch tools and containers make no working copies, so they aren't in
this table.

## When not to use gitfold

- **Build-heavy work:** writes go through FUSE. A full Linux kernel build is
  2.5-4% slower than in a worktree, an incremental one 11-29%, a cargo relink
  about 2x, creating many small files 4-6x.
- **Git LFS, `.gitattributes` filters, submodules, real mtimes:** gitfold shows
  LFS files as pointers, applies no filters, doesn't initialise submodules and
  gives unchanged files a fixed time (see the README's "Differences from a
  checkout").
- **Small repositories, or macOS and Windows:** use `git worktree` (or sparse
  checkout, or jj); a checkout takes seconds and writes at native speed. gitfold
  pays off when a checkout costs tens of seconds or gigabytes, or when you open
  many at once.

## Tool by tool

### `git worktree`

git's own way to have several working trees of one repository: each linked
worktree has its own `HEAD` and index and shares the rest [1]. Every tracked
file is written to disk: on Chromium (512k files) that takes 17 s and 5.5 GiB,
on the Linux kernel 3.5 s and 1.8 GB. A branch can be checked out in only one
worktree, and git's documentation calls submodule support across several
checkouts of a superproject incomplete [1].

**gitfold** registers each folder as exactly such a linked worktree, so git and
your tools see the same thing; it reads unchanged files from the object store
instead of writing them out, and stores only what you change (under
`.git/gitfold/`).

**Pick it instead when** a checkout is cheap enough, writes must be native, or
you need what gitfold leaves out (the gaps listed above).

### Sparse checkout, partial clone and Scalar

- **Sparse checkout** writes only chosen directories, per worktree [2]. A new
  worktree costs only that slice, but tools see only those files, and you
  choose the slice up front.
- **Partial clone** (`--filter=blob:none`) leaves file contents out of the
  clone and fetches them on demand from the remote [3]. It shrinks the clone,
  not the checkout: each worktree still fetches and writes every file, and the
  remote must be reachable.
- **Scalar**, part of git since 2.38, sets up a large repository with these
  plus background maintenance and commit-graph settings [4][5].

**gitfold** shows the whole tree in every workspace without writing it. It works
on a full or a partial clone (it fetches a commit's missing objects when a
folder opens), but doesn't apply sparse patterns. Scalar's maintenance settings
help a gitfold clone as much as any other.

**Pick them instead when** you work in a known part of the repository, or on
macOS or Windows.

### Reflink copies and filesystem snapshots

Copy-on-write file copies and btrfs subvolume snapshots copy a checkout while
sharing its data blocks. Reflink copies (`cp --reflink`, the default in GNU `cp`
where the filesystem supports it [9]) work on btrfs, XFS, ZFS since 2.2 [6][7]
and APFS; btrfs snapshots are instantaneous [8]. The copy isn't a git worktree
by itself: it has to be registered (`git worktree add --no-checkout`, then
`git reset` to fill its index), and git re-reads every file once, because every
inode changed [10]. A reflink copy still creates every inode, which takes time
on hundreds of thousands of files (unmeasured here). Tools such as worktrunk
share build caches between worktrees with copy-on-write copies [20].

**gitfold** works on any filesystem, opens any branch or commit straight from
git rather than copying one checkout, and needs no index refresh. It can't share
build output between workspaces (a fork of a workspace copies its changes,
sharing blocks where the filesystem can).

**Pick them instead when** you are on btrfs, XFS, ZFS 2.2+ or APFS and want
copies of a *built* tree (`target/`, `node_modules/`) at native write speed.

### overlayfs and fuse-overlayfs

A union filesystem: a writable upper directory over read-only lower ones, with
a file copied up on its first write [11]; fuse-overlayfs does the same in FUSE,
for rootless containers [12]. For git you still need a checkout for each base
revision as the lower layer, and a worktree registration and index for each
overlay; we know of no maintained tool that sets this up. The kernel's overlayfs
needs root, or since Linux 5.11 an unprivileged user namespace, and processes
outside that namespace don't see its mounts [13]. The lower layer mustn't change
while mounted [11], so a base can't follow its branch. Without `redirect_dir`,
renaming any lower directory fails with EXDEV [11].

**gitfold** is the same idea with git's object store as the lower layer: any
revision works without a base checkout, and a workspace's base moves with its
branch (commits, rebases) without touching the files. gitfold copies up a
renamed lower directory and returns EXDEV only when it holds over 10,000 files
or 100 MiB.

**Pick it instead when** you need the fastest writes and can run as root or in
a user namespace, or the base isn't a git revision.

### EdenFS and Sapling (Meta)

Sapling is Meta's Git-compatible version control system; EdenFS, its virtual
filesystem, fetches working files as they are accessed, from a server such as
Mononoke [14][15]. It runs on Linux, macOS and Windows [16] (open-source builds:
Linux [14]) and starts a helper as root, setuid or through sudo [34]. Outside
Meta, EdenFS and Mononoke are "not yet supported for external usage" [14].
EdenFS works only in Sapling's `.sl` mode, which doesn't support git commands;
even in that mode, EdenFS support is experimental and missing from public
releases [17].

**gitfold** is plain git on a local clone and needs no server, but runs only on
Linux.

**Pick it instead when** you work at Meta, or adopt Sapling with its server.

### VFS for Git (Microsoft)

VFS for Git virtualises a huge repository's working tree on Windows, fetching
files as they are first accessed [18], on top of Windows' Projected File System
[19]. It needs a server that speaks the GVFS protocol (such as Azure Repos) and
Microsoft's fork of git. It still ships releases, but its README strongly
recommends Scalar for new deployments [18]; Microsoft moved to Scalar when macOS
lost the kernel support that virtualisation needed [5].

**gitfold** needs no special server or git build and gives many folders per
clone, but runs only on Linux.

**Pick it instead when** you are on Windows with Azure Repos at that scale,
though its README recommends Scalar for new deployments.

### GitFS and other git filesystems

GitFS (Presslabs) mounts a remote repository's branch and turns every change
into a commit, pushed in batches; history is browsable [21]. Its last release
was in 2019. Read-only history browsers (legitfs, JGitFS, rogitfs, timefs [22])
show branches, tags and commits as folders.

**gitfold**'s workspaces are writable git worktrees where you commit yourself;
nothing is committed for you. Its `@<rev>` snapshots are a read-only history
browser of the same kind.

**Pick them instead when** you want people who don't use git to get versioning
just by saving files (GitFS), or a read-only browser on macOS.

### Jujutsu (jj) workspaces

jj is a different version control system that stores its data in git; by
default a repository is colocated with git, and `jj` and `git` commands can be
mixed [23]. `jj workspace add` makes another working copy of the same
repository [24]; since jj 0.46, extra workspaces can be git worktrees
(`--colocate`) [25]. Each workspace is a full checkout, and rewriting a commit
rebases its descendants automatically. jj doesn't support submodules, Git LFS,
`.gitattributes`, git's sparse checkout or partial clones [23].

**gitfold** stays plain git (branches, `git rebase`, your tools), avoids the
checkout, and moves *branches* onto their parents' new commits (below). Using
both, through colocated workspaces, is untested.

**Pick it instead when** you want jj's model (first-class conflicts, undo,
automatic rebasing) and a cross-platform tool, on a repository small enough to
check out.

### Stacked-branch tools

- **`git rebase --update-refs`** (git 2.38+) updates the branches pointing at
  rebased commits, except "branches that are checked out in a worktree" [26].
- **git-branchless** rebases all stacks without checking them out, with
  `git undo`; it calls itself alpha [27].
- **Graphite** (`gt`) restacks descendants and manages GitHub pull requests [28].
- **git-town** syncs and ships stacked branches on GitHub, GitLab, Bitbucket,
  Gitea and Forgejo [29].
- **spr** and **ghstack** make one GitHub pull request per commit [30][31].

These rebase a stack when you run them. **gitfold** moves a branch onto its
parent's new commits by itself, down the whole tree, at safe moments: when an
agent's turn starts, when a workspace has been quiet for a few seconds, and,
for a branch without a workspace, when its parent moves. In a workspace, git
does the move and carries uncommitted work as `git rebase --autostash` does; if
that work would conflict, the move stops and gitfold reports it. gitfold never
moves pushed commits automatically, and leaves a branch checked out in a plain
git worktree to you. When a parent lands in main (merge, squash or rebase
merge) and its branch is deleted, as forges do after a merge, the child moves
there; gitfold sees the merge once you fetch.

gitfold also offers read-only merged views of a branch and everything built on
it (`@committed`, and `@live` with uncommitted edits, conflicts listed),
`gitfold changes -- PATH` (who is changing a file, across workspaces), and
conflict notices to agents. It manages no pull requests.

**Pick them instead when** you need pull-request stacks on a forge, or work on
macOS or Windows.

### Containers and worktree managers for coding agents

Claude Code's `--worktree` and subagents with `isolation: worktree` use plain git
worktrees, and a `WorktreeCreate` hook can replace them [32]; gitfold's plugin
uses that hook. Worktree managers such as worktrunk [20] script `git worktree`
for parallel agents. Container Use (Dagger) gives each agent a container and a
branch [33], and devcontainers isolate ports, databases and toolchains, which
neither worktrees nor gitfold do; each container still needs a checkout or a
bind mount.

**gitfold** isolates files only, cheaply, and can be the files layer under any of
these (bind-mounting a gitfold folder into a container is untested; gitfold
itself runs inside Docker given `--device /dev/fuse --cap-add SYS_ADMIN`, plus
`--security-opt apparmor=unconfined` under AppArmor).

**Pick them instead when** you need to isolate ports, databases or toolchains.

## Sources

1. git-worktree: https://git-scm.com/docs/git-worktree
2. git-sparse-checkout: https://git-scm.com/docs/git-sparse-checkout
3. Partial clone: https://git-scm.com/docs/partial-clone
4. scalar: https://git-scm.com/docs/scalar
5. GitHub blog, "The story of Scalar": https://github.blog/open-source/git/the-story-of-scalar/
6. ioctl_ficlone(2): https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html
7. OpenZFS 2.2 block cloning: https://www.phoronix.com/news/OpenZFS-2.2-Released
8. btrfs subvolumes: https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html
9. GNU cp: https://www.gnu.org/software/coreutils/manual/html_node/cp-invocation.html
10. git-config `core.checkStat`, `core.trustctime`: https://git-scm.com/docs/git-config
11. overlayfs: https://docs.kernel.org/filesystems/overlayfs.html
12. fuse-overlayfs: https://github.com/containers/fuse-overlayfs
13. Linux 5.11, unprivileged overlayfs mounts: https://kernelnewbies.org/Linux_5.11 · https://lore.kernel.org/lkml/20201217142025.GB1236412@miu.piliscsaba.redhat.com/
14. Sapling: https://github.com/facebook/sapling
15. EdenFS inode storage: https://github.com/facebook/sapling/blob/main/eden/fs/docs/InodeStorage.md
16. EdenFS overview: https://github.com/facebook/sapling/blob/main/eden/fs/docs/Overview.md
17. Sapling's git support modes: https://sapling-scm.com/docs/git/git_support_modes/
18. VFS for Git: https://github.com/microsoft/VFSForGit
19. Projected File System: https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system
20. worktrunk: https://github.com/max-sixty/worktrunk
21. GitFS (Presslabs): https://github.com/presslabs/gitfs
22. legitfs https://github.com/mbr/legitfs/ · JGitFS https://github.com/centic9/JGitFS · rogitfs https://github.com/aw32/rogitfs · timefs https://docs.rs/crate/timefs/0.1.0
23. jj and git: https://docs.jj-vcs.dev/latest/git-compatibility/
24. jj working copies: https://docs.jj-vcs.dev/latest/working-copy/
25. jj changelog (0.46.0): https://github.com/jj-vcs/jj/blob/main/CHANGELOG.md
26. git-rebase `--update-refs`: https://git-scm.com/docs/git-rebase
27. git-branchless: https://github.com/arxanas/git-branchless
28. Graphite: https://graphite.com/docs/command-reference
29. Git Town: https://www.git-town.com/
30. spr: https://github.com/ejoffe/spr
31. ghstack: https://github.com/ezyang/ghstack
32. Claude Code worktrees: https://code.claude.com/docs/en/worktrees.md
33. Container Use: https://github.com/dagger/container-use
34. EdenFS process state (privhelper): https://github.com/facebook/sapling/blob/main/eden/fs/docs/Process_State.md
