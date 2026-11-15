#!/usr/bin/env bash
# End-to-end test of bases (stacked branches): layer base, branch --base,
# moves of branches with no workspace, of an unmounted workspace and of an
# open one (by the UserPromptSubmit hook and by layer base again),
# conflicts, uncommitted work in the way, commits already pushed, --frozen,
# taking a branch off the stack, and list's tree and notes.
# usage: tests/stacks.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=stacks
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# Commits on branch $1 with the main worktree (left on main): file $2 gets
# content $3.
commit_on() {
    R switch -q "$1"; printf '%s\n' "$3" >"$REPO/$2"; R add -A; R commit -qm "$1: $2"; R switch -q main
}
# The parent of branch $1's tip, and whether $2 is it.
parent_is() { [ "$(R rev-parse "$1^")" = "$(R rev-parse "$2")" ]; }
# Runs Claude Code's UserPromptSubmit hook in directory $1.
prompt_hook() { (cd "$1" && "$BIN" layer --claude-hook user-prompt-submit); }
stack_ref() { R rev-parse -q --verify "refs/gitfold-stack/$1"; }
# `gitfold layer base` for branch $1, the rest its arguments.
base() { local b=$1; shift; "$BIN" -C "$REPO" layer base -w "$b" "$@"; }
# What `gitfold list` says of branch $1: its base and status columns.
listed() {
    "$BIN" -C "$REPO" list | sed -E 's/^ *(└─ )?//' |
        awk -v b="$1" '$1 == b { $1=$2=$3=$4=$5=$6=""; sub(/^ +/, ""); print }'
}
# `gitfold list`'s branch column: the tree.
tree() { "$BIN" -C "$REPO" list | tail -n +2 | cut -c1-40 | sed 's/ *$//'; }

branches_with_no_workspace() {
R branch p1
commit_on p1 p1.txt one
out=$("$BIN" -C "$REPO" branch p2 --base p1)
eq "branch --base: created on the base's tip" "created branch p2 on p1; open it as <mountpoint>/p2" "$out"
eq "its base recorded" "p1" "$(R config branch.p2.gitfoldParent)"
eq "and where its own commits start" "$(R rev-parse p1)" "$(stack_ref p2)"
commit_on p2 p2.txt two
eq "layer base p1 again: up to date" "p2: up to date with p1" "$(base p2 p1)"
check "a cycle is refused" fails base p1 p2
check "so is a branch on itself" fails base p2 p2
old=$(R rev-parse p1)
commit_on p1 a.txt fix
eq "list: the tree, with what it needs" "p1 (needs update: gitfold layer base p1)" "$(listed p2)"
mine=$(R rev-parse p2)
out=$(base p2 p1)
eq "again: says what it moves, then moves it" "moves 1 commit of p2 onto p1
p2: moved onto p1 (1 new commit)" "$out"
check "onto the base's tip" parent_is p2 p1
eq "with its own commit's author, message and tree change" "t p2: p2.txt|p2.txt" \
    "$(R log -1 --format='%an %s' p2)|$(R diff --name-only p2^ p2)"
eq "where its own commits start moved with it" "$(R rev-parse p1)" "$(stack_ref p2)"
check "the old commit is in the reflog" sh -c "git -C '$REPO' reflog p2 | grep -q '^${mine:0:7}'"
eq "nothing left to do" "p2: up to date with p1" "$(base p2 p1)"

# A third level, moved bottom-up with --all.
"$BIN" -C "$REPO" branch p3 --base p2 >/dev/null
commit_on p3 p3.txt three
commit_on p1 a.txt fix2
eq "list: a tree, each base above the branches on it" "p1
└─ p2
   └─ p3" "$(tree)"
out=$("$BIN" -C "$REPO" layer base --all)
eq "--all: bottom-up" "moves 1 commit of p2 onto p1
p2: moved onto p1 (1 new commit)
moves 1 commit of p3 onto p2
p3: moved onto p2 (2 new commits)" "$out"
check "p3 on p2" parent_is p3 p2
check "p2 on p1" parent_is p2 p1

# A conflict: nothing moves, and it is recorded.
commit_on p2 a.txt mine
commit_on p1 a.txt theirs
tip=$(R rev-parse p2)
out=$(base p2 p1 2>&1); rc=$?
eq "a conflict: exit 4, the commit and path named" "4 yes" \
    "$rc $(grep -q "^p2: blocked: commit $(R rev-parse --short=12 p2) conflicts on a.txt with p1; nothing moved" <<<"$out" && echo yes)"
eq "nothing moved" "$tip" "$(R rev-parse p2)"
eq "list says so" "p1 (blocked: conflicts on a.txt)" "$(listed p2)"
# Resolved by hand: the next update takes it as it is.
R switch -q p2; R rebase -q p1 >/dev/null 2>&1; R checkout -q --theirs a.txt 2>/dev/null
printf 'mine\n' >"$REPO/a.txt"; R add a.txt; GIT_EDITOR=true R rebase --continue >/dev/null 2>&1; R switch -q main
eq "rebased by hand: where its own commits start is moved" "p2: has p1's tip already; its own commits now start there" \
    "$(base p2 p1)"
eq "and the block forgotten" "" "$("$BIN" -C "$REPO" list | grep blocked)"

# Checked out in a git worktree: left to it.
commit_on p1 p1.txt one-more
R switch -q p2
out=$(base p2 p1 2>&1); rc=$?
eq "checked out in a git worktree: refused (exit 4)" "4 yes" "$rc $(grep -q 'checked out in the main worktree' <<<"$out" && echo yes)"
R switch -q main

# Commits already pushed: moved only with --yes; the push never done.
git init -q --bare "$T/remote.git"; R remote add origin "$T/remote.git"
R push -q -u origin p2 2>/dev/null
pushed=$(R rev-parse p2)
out=$(base p2 p1 2>&1); rc=$?
eq "pushed: says what it would move, refuses (exit 4)" "4 yes yes" \
    "$rc $(grep -qx 'moves 2 commits of p2 onto p1' <<<"$out" && echo yes) $(grep -q '^p2: not moved: 2 of them are pushed already (origin/p2).*--yes moves them' <<<"$out" && echo yes)"
eq "nothing moved" "$pushed" "$(R rev-parse p2)"
out=$(base p2 p1 --yes)
eq "--yes: moved, and the force-push to run printed" "moves 2 commits of p2 onto p1
p2: moved onto p1 (1 new commit)
to push (rewritten since last push):
  git push --force-with-lease=p2:$pushed origin p2:p2" "$out"
eq "not pushed" "$pushed" "$(git -C "$T/remote.git" rev-parse p2)"
eq "list: the push to run" "p1 (push with: git push --force-with-lease origin p2)" "$(listed p2)"

# The base rewound behind where its own commits start: its dropped commit
# goes from p2 too.
commit_on p1 x.txt dropped-later
base p2 p1 --yes >/dev/null
R branch -f p1 p1~1
eq "base rewound: moved without the dropped commit" "p2: moved onto p1 (0 new commits)|" \
    "$(base p2 p1 --yes 2>&1 | grep '^p2:')|$(R ls-tree --name-only p2 x.txt)"
check "onto the base's tip, its two commits on it" test "$(R rev-parse p2~2)" = "$(R rev-parse p1)"

# --frozen: built on its base once, not followed after.
R branch z; commit_on z z.txt z
new=$(R rev-list --count main..p1)
out=$(base z p1 --frozen)
eq "a branch not stacked: its commits moved onto it" "moves 1 commit of z onto p1
z: moved onto p1 ($new new commits)
frozen: z doesn't follow p1's new commits; \`gitfold layer base p1 --frozen\` brings it up to date again" "$out"
check "z on p1" parent_is z p1
eq "frozen: stored as manual, listed so" "manual|p1 (frozen)" "$(R config branch.z.gitfoldRestack)|$(listed z)"
commit_on p1 y.txt y
eq "again, --frozen: up to date, still frozen" "manual" "$(base z p1 --frozen >/dev/null; R config branch.z.gitfoldRestack)"
check "on p1's new tip" parent_is z p1
base z p1 >/dev/null
eq "again without --frozen: live" "" "$(R config branch.z.gitfoldRestack)"

# The trunk as its base: off the stack, its own commits moved onto it.
commit_on main m.txt trunk
out=$(base z main)
eq "layer base main: its own commits moved onto main, off the stack" "moves 1 commit of z onto main
z: moved onto main (1 new commit)
z is off the stack (it was built on p1); its base is main" "$out"
check "on main" parent_is z main
eq "without p1's work" "" "$(R ls-tree --name-only z p1.txt)"
eq "forgotten, ref and all" "|||" "$(R config branch.z.gitfoldParent)|$(stack_ref z)|$(R config branch.z.gitfoldRestack)|$(listed z)"
eq "again: nothing to do" "z isn't built on another branch; nothing to do|$(R rev-parse z)" \
    "$(base z main)|$(R rev-parse z)"
eq "branch --base main: an ordinary branch" "created branch o on main; open it as <mountpoint>/o|" \
    "$("$BIN" -C "$REPO" branch o --base main)|$(R config branch.o.gitfoldParent)"
}

an_open_workspace() {
R branch p1
"$BIN" -C "$REPO" branch p2 --base p1 >/dev/null
mount_fs
W=$MNT/p2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "p2 own"
# Its agent's uncommitted work: unstaged, staged and untracked.
echo mine >>"$W/d/c.txt"; echo staged >"$W/s.txt"; G "$W" add s.txt; echo u >"$W/u.txt"
cat "$W/a.txt" >/dev/null                # cached by the kernel before
echo fix >>"$MNT/p1/a.txt"; echo more >"$MNT/p1/p1b.txt"
G "$MNT/p1" add -A; G "$MNT/p1" commit -qm "review fix"
out=$(prompt_hook "$W")
eq "the hook moves it, and says so" "gitfold: this branch, p2, was moved onto its base p1's 1 new commit (review fix); files changed under you: a.txt, p1b.txt. Its history was rewritten: push it with --force-with-lease when you push." "$out"
check "onto the base's tip" parent_is p2 p1
eq "the new content shows" "a
fix|more" "$(cat "$W/a.txt")|$(cat "$W/p1b.txt")"
eq "uncommitted work kept: unstaged, staged, untracked" " M d/c.txt
A  s.txt
?? u.txt" "$(git -C "$W" status --short)"
for _ in $(seq 30); do [ "$(cat "$COMMON/gitfold/p2/base")" = "$(R rev-parse p2)" ] && break; sleep 0.1; done
eq "the base follows" "$(R rev-parse p2)" "$(cat "$COMMON/gitfold/p2/base")"
for _ in $(seq 30); do [ ! -e "$COMMON/gitfold/p2/upper/p1b.txt" ] && break; sleep 0.1; done
check "and upper no longer holds the moved files" test ! -e "$COMMON/gitfold/p2/upper/p1b.txt" -a ! -e "$COMMON/gitfold/p2/upper/a.txt"
check "no intent left" test ! -e "$COMMON/gitfold/p2/restack"
eq "the hook again: nothing to say" "" "$(prompt_hook "$W")"
eq "list: workspaces in the tree too" "p1
└─ p2" "$(tree)"

# Uncommitted work on a path the move changes: it waits.
echo c2 >>"$MNT/p1/d/c.txt"; G "$MNT/p1" commit -qam "p1 c"
tip=$(R rev-parse p2)
out=$(base p2 p1 2>&1); rc=$?
eq "overlapping uncommitted work: waits (exit 4)" "4 yes" "$rc $(grep -q '^p2: waits: uncommitted changes to d/c.txt overlap the move' <<<"$out" && echo yes)"
eq "nothing moved" "$tip| M d/c.txt" "$(R rev-parse p2)|$(git -C "$W" status --short d/c.txt)"
eq "list: the base, and what it needs" "p1 (needs update: gitfold layer base p1)" "$(listed p2)"
eq "the hook says it waits" yes "$(prompt_hook "$W" | grep -q 'waits: uncommitted changes to d/c.txt' && echo yes)"
eq "once, while it stays so" "" "$(prompt_hook "$W")"
G "$W" checkout -q d/c.txt
check "once it is out of the way: layer base again moves it" "$BIN" -C "$W" layer base p1
eq "the base's change shows" "c
c2" "$(cat "$W/d/c.txt")"

# The hook and layer base at once: one moves it, the other leaves it be
# (it used to put the files and index back under the new tip).
for i in 1 2 3; do
    echo "r$i" >"$MNT/p1/r.txt"; G "$MNT/p1" add r.txt; G "$MNT/p1" commit -qm "p1 r$i"
    prompt_hook "$W" >/dev/null & base p2 p1 >/dev/null 2>&1; wait $!
done
check "both at once: on the base's tip" parent_is p2 p1
eq "its files and index at the tip" "r3|" "$(cat "$W/r.txt")|$(git -C "$W" status --short r.txt)"
check "no intent left" test ! -e "$COMMON/gitfold/p2/restack"

# A fork keeps the base.
"$BIN" -C "$W" branch p2f -w >/dev/null
eq "a fork of it: the same base, its own commits from the same place" "p1 $(stack_ref p2)" "$(R config branch.p2f.gitfoldParent) $(stack_ref p2f)"

# --frozen: only layer base moves it.
"$BIN" -C "$W" layer base p1 --frozen >/dev/null
echo m >"$MNT/p1/m.txt"; G "$MNT/p1" add m.txt; G "$MNT/p1" commit -qm "p1 m"
tip=$(R rev-parse p2)
eq "frozen: the hook leaves it" "|$tip" "$(prompt_hook "$W")|$(R rev-parse p2)"
check "layer base --frozen again moves it" "$BIN" -C "$W" layer base p1 --frozen
check "m.txt shows" test -e "$W/m.txt"
check "layer base again: live" "$BIN" -C "$W" layer base p1
eq "not manual any more" "" "$(R config branch.p2.gitfoldRestack)"

# A conflict: nothing moves, said once.
echo theirs >"$MNT/p1/own.txt"; G "$MNT/p1" add own.txt; G "$MNT/p1" commit -qm "p1 own"
tip=$(R rev-parse p2)
out=$(prompt_hook "$W")
eq "a conflict: the hook says so" yes "$(grep -q "^gitfold: p2 is built on p1, which has new commits; blocked: commit $(R rev-parse --short=12 p2) conflicts on own.txt with p1; nothing moved" <<<"$out" && echo yes)"
eq "nothing moved" "$tip|own" "$(R rev-parse p2)|$(cat "$W/own.txt")"
eq "said once" "" "$(prompt_hook "$W")"
for _ in $(seq 30); do [ "$(cat "$COMMON/gitfold/p2/base")" = "$(R rev-parse p2)" ] && break; sleep 0.1; done
eq "list: blocked" "p1 (blocked: conflicts on own.txt)" "$(listed p2)"
}

layers_or_an_unmounted_workspace() {
R branch p1; R branch la
"$BIN" -C "$REPO" branch p2 --base p1 >/dev/null
mount_fs
W=$MNT/p2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "p2 own"
echo la >>"$MNT/la/a.txt"
"$BIN" -C "$W" layer add la >/dev/null
echo n >"$MNT/p1/n.txt"; G "$MNT/p1" add n.txt; G "$MNT/p1" commit -qm "p1 n"
out=$(base p2 p1 2>&1); rc=$?
eq "layers shown: waits (exit 4)" "4 yes" "$rc $(grep -q 'its workspace shows layers' <<<"$out" && echo yes)"
"$BIN" -C "$W" lens off >/dev/null
for _ in $(seq 30); do [ "$(cat "$COMMON/gitfold/p2/base")" = "$(R rev-parse p2)" ] && break; sleep 0.1; done
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
out=$(base p2 p1 | grep '^p2:')
eq "unmounted, nothing uncommitted: moved" "p2: moved onto p1 (1 new commit); 1 file changed in its workspace" "$out"
eq "its base moved too" "$(R rev-parse p2)" "$(cat "$COMMON/gitfold/p2/base")"
mount_fs
eq "the new file shows" "n" "$(cat "$W/n.txt")"
eq "git status clean" "" "$(git -C "$W" status --short)"
echo dirty >>"$W/a.txt"
echo n2 >"$MNT/p1/n2.txt"; G "$MNT/p1" add n2.txt; G "$MNT/p1" commit -qm "p1 n2"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
tip=$(R rev-parse p2)
out=$(base p2 p1 2>&1); rc=$?
eq "unmounted, uncommitted changes: waits for the mount (exit 4)" "4 yes $tip" \
    "$rc $(grep -q 'moved once it is mounted' <<<"$out" && echo yes) $(R rev-parse p2)"
mount_fs
out=$(prompt_hook "$W")
eq "mounted: the hook moves it" yes "$(grep -q 'was moved onto its base p1' <<<"$out" && echo yes)"
eq "own change kept" " M a.txt" "$(git -C "$W" status --short)"
# An intent recovery can't read: layer switches wait, taking it off the
# stack forgets it.
echo junk >"$COMMON/gitfold/p2/restack"
out=$("$BIN" -C "$W" layer on la 2>&1); rc=$?
eq "an intent left: layer switches refused (exit 4)" "4 yes" "$rc $(grep -q 'p2 is being moved onto its base' <<<"$out" && echo yes)"
out=$(base p2 main)
eq "layer base main forgets it, nothing moved" "p2 is off the stack (it was built on p1); its base is main; nothing moved, and an interrupted move's intent is forgotten|" \
    "$out|$(R config branch.p2.gitfoldParent)"
check "then layers switch again" "$BIN" -C "$W" layer on la
}

# Branches $1 (on main, file $1.txt) and $2 built on it (file $2.txt).
pair() {
    R branch "$1"; commit_on "$1" "$1.txt" "$1"
    "$BIN" -C "$REPO" branch "$2" --base "$1" >/dev/null; commit_on "$2" "$2.txt" "$2"
}
# Squash-merges branch $1 into main as one commit; prints it.
squash() {
    R merge -q --squash "$1" >/dev/null 2>&1; R commit -qm "$1 (#1)"; R rev-parse main
}
# Says whether branch $1 sits right on commit $2 with its one own commit,
# built on $3, its stack ref at $2.
landed_on() {
    [ "$(R rev-parse "$1^")" = "$2" ] && [ "$(R config "branch.$1.gitfoldParent")" = "$3" ] &&
        [ "$(stack_ref "$1")" = "$2" ]
}

a_parent_that_landed() {
commit_on main m.txt moved-on
# A squash merge, the base deleted after it.
pair p1 p2
s=$(squash p1)
eq "list: the base looks landed" "p1 (p1 looks landed in main: needs update: gitfold layer base p1)" "$(listed p2)"
R branch -q -D p1
out=$(base p2 p1)
eq "a squash merge: moved onto the landing commit, saying so first" "moves 1 commit of p2 onto main (p1 looks squash- or rebase-merged into main as ${s:0:12})
p2: p1 looks squash- or rebase-merged into main as ${s:0:12}; moved there with its own 1 commit and now built on main. Its pull request's base should now be main: push it, then \`gh pr edit p2 --base main\`" "$out"
check "on the squash, built on main, its stack ref there" landed_on p2 "$s" main
eq "with only its own change" "p2.txt" "$(R diff --name-only "$s" p2)"
commit_on main later.txt later
eq "main moving on: no note (following it is on request)" "main" "$(listed p2)"
eq "layer base main follows it on request, off the stack" "p2: moved onto main (1 new commit)|" \
    "$(base p2 main | grep '^p2:')|$(R config branch.p2.gitfoldParent)"

# A rebase merge: onto its last commit.
pair r1 r2
commit_on main m.txt before-r1
R cherry-pick main..r1 >/dev/null; last=$(R rev-parse main)
base r2 r1 >/dev/null
check "a rebase merge: onto its last commit" landed_on r2 "$last" main

# A squash the maintainer tweaked.
pair t1 t2
R merge -q --squash t1 >/dev/null; echo tweaked >"$REPO/a.txt"; R commit -qam "t1, tweaked"
base t2 t1 >/dev/null
check "a tweaked squash: found too" landed_on t2 "$(R rev-parse main)" main

# A merge commit, into a stacked base's base.
R branch p0; commit_on p0 p0.txt p0
"$BIN" -C "$REPO" branch q1 --base p0 >/dev/null; commit_on q1 q1.txt q1
"$BIN" -C "$REPO" branch q2 --base q1 >/dev/null; commit_on q2 q2.txt q2
commit_on p0 p0b.txt more
R switch -q p0; R merge -q --no-ff --no-edit q1; R switch -q main
out=$(base q2 q1)
eq "a merge into the base's own base: onto the merge" yes \
    "$(grep -q "^q2: q1 was merged into p0 by $(R rev-parse --short=12 p0);" <<<"$out" && echo yes)"
check "built on p0 now" landed_on q2 "$(R rev-parse p0)" p0

# A merge of the base after it got more commits than the child's.
pair x1 x2
commit_on x1 x1b.txt later
R merge -q --no-ff --no-edit x1
base x2 x1 >/dev/null
check "a merge of a later base tip: onto the merge" landed_on x2 "$(R rev-parse main)" main

# A move onto a landing interrupted after the refs (the base not yet set):
# the next update finishes it, rather than going back onto the base.
pair k1 k2
s=$(squash k1)
R rebase -q --onto "$s" k1 k2; R switch -q main
R update-ref refs/gitfold-stack/k2 "$s"; R config branch.k2.gitfoldLanding "main $s"
eq "interrupted move onto a landing: finished" "k2: up to date with main|main|" \
    "$(base k2 k1)|$(R config branch.k2.gitfoldParent)|$(R config branch.k2.gitfoldLanding)"

# A deleted base: its own base's name went with it, so it is named.
R branch g0; commit_on g0 g0.txt g0
"$BIN" -C "$REPO" branch g1 --base g0 >/dev/null; commit_on g1 g1.txt g1
"$BIN" -C "$REPO" branch g2 --base g1 >/dev/null; commit_on g2 g2.txt g2
R switch -q g0; R merge -q --squash g1 >/dev/null 2>&1; R commit -qm "g1 rewritten" -m x; R switch -q main
R branch -q -D g1
out=$(base g2 g1 2>&1); rc=$?
eq "the base gone: says to name another (exit 4)" "4 yes" "$rc $(grep -q 'its base g1 is missing' <<<"$out" && echo yes)"
base g2 g0 >/dev/null
check "layer base g0: only its own commits onto g0" landed_on g2 "$(R rev-parse g0)" g0

# A fast-forward, told apart from a fresh base by what was recorded when
# it was set: off main then.
pair f1 f2
f1=$(R rev-parse f1)
eq "recorded off main" "$f1 main no" "$(R config branch.f2.gitfoldBaseOnTarget)"
R merge -q --ff-only f1
eq "a fast-forward: moved onto it" "f2: f1 looks fast-forwarded into main up to ${f1:0:12}; moved there with its own 1 commit and now built on main. Its pull request's base should now be main: push it, then \`gh pr edit f2 --base main\`" \
    "$(base f2 f1 | grep '^f2:')"
check "built on main at f1's commit" landed_on f2 "$f1" main
# A fresh base: it was on main all along.
R branch e1; "$BIN" -C "$REPO" branch e2 --base e1 >/dev/null; commit_on e2 e2.txt e2
eq "a fresh base: recorded on main" "$(R rev-parse e1) main yes" "$(R config branch.e2.gitfoldBaseOnTarget)"
commit_on main m.txt after-e
eq "a fresh base: not taken for a fast-forward" "e2: up to date with e1|e1" \
    "$(base e2 e1)|$(R config branch.e2.gitfoldParent)"

# Not merged: nothing said, nothing moved.
pair u1 u2
commit_on main m.txt moved-again
tip=$(R rev-parse u2)
eq "unmerged: up to date, still on its base" "u2: up to date with u1|$tip|u1" \
    "$(base u2 u1)|$(R rev-parse u2)|$(R config branch.u2.gitfoldParent)"

# Merged in a way detection can't tell: moved by taking it off the stack.
pair v1 v2
R merge -q --squash v1 >/dev/null; echo rewritten >"$REPO/v1.txt"; R commit -qam "v1, rewritten"
eq "a squash rewritten when merged: not found" "v2: up to date with v1" "$(base v2 v1)"
base v2 main >/dev/null
eq "layer base main: its own commit onto main's tip, off the stack" "$(R rev-parse main)|v2.txt|" \
    "$(R rev-parse v2^)|$(R diff --name-only main v2)|$(R config branch.v2.gitfoldParent)"
}

a_branch_on_the_local_trunk() {
# The trunk is origin/main; c is built on the local main tracking it (as
# after a landing).
git init -q --bare "$T/origin.git"; R remote add origin "$T/origin.git"
R push -q -u origin main; R remote set-head origin main
commit_on main x.txt local      # not pushed
R branch c; R config branch.c.gitfoldParent main; R update-ref refs/gitfold-stack/c main
commit_on c c.txt c
# Someone else pushes; pulling with --rebase rewrites the local commit.
R switch -q --detach origin/main; echo y >"$REPO/y.txt"; R add y.txt; R commit -qm theirs
R push -q origin HEAD:main; R switch -q main; R fetch -q origin; R rebase -q origin/main
eq "list: no note for a branch on the trunk's local branch" "main" "$(listed c)"
eq "the rewritten local commit isn't taken for a landing" "c: moved onto main (2 new commits)|" \
    "$(base c main | grep '^c:')|$(R config branch.c.gitfoldParent)"
check "on main's tip" parent_is c main
}

a_landed_parent_in_an_open_workspace() {
R branch h1; commit_on h1 h1.txt h1
"$BIN" -C "$REPO" branch h2 --base h1 >/dev/null
mount_fs
W=$MNT/h2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "h2 own"
echo mine >>"$W/a.txt"
commit_on main m.txt moved-on
s=$(squash h1)
tip=$(R rev-parse h2)
eq "it only looks merged, h1 still there: the hook says so, moves nothing" "gitfold: h2's base h1 looks squash- or rebase-merged into main as ${s:0:12}, but h1 is still there; \`gitfold layer base h1\` moves it there; suggest it to the user rather than running it yourself: it rewrites h2's history.|$tip" \
    "$(prompt_hook "$W")|$(R rev-parse h2)"
eq "once" "" "$(prompt_hook "$W")"
R branch -q -D h1
out=$(prompt_hook "$W")
eq "h1 gone: the hook moves it onto main, and says so" "gitfold: this branch's base h1 looks squash- or rebase-merged into main as ${s:0:12}; h2 is now built on main with only its own 1 commit; files changed under you: m.txt. Its history was rewritten, so it needs a push with --force-with-lease, and then its pull request's base should be main: suggest \`gh pr edit h2 --base main\` to the user after that push rather than running it yourself." "$out"
check "on the squash" landed_on h2 "$s" main
eq "main's file shows, own work kept" "moved-on| M a.txt" "$(cat "$W/m.txt")|$(git -C "$W" status --short)"
commit_on main later.txt later
eq "main moving on: the hook leaves it" "|$(R rev-parse h2)" "$(prompt_hook "$W")|$(R rev-parse h2)"
}

section "stacks: branches with no workspace" branches_with_no_workspace
section "stacks: a parent that landed" a_parent_that_landed
section "stacks: a branch on the local trunk" a_branch_on_the_local_trunk
section "stacks: a landed parent, an open workspace" a_landed_parent_in_an_open_workspace
section "stacks: an open workspace" an_open_workspace
section "stacks: layers, an unmounted workspace" layers_or_an_unmounted_workspace
finish
