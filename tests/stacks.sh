#!/usr/bin/env bash
# End-to-end test of stacked branches: stack on/off/sync/list, branch --on,
# restacks of branches with no workspace, of an unmounted workspace and of
# an open one (by the UserPromptSubmit hook and by stack sync), conflicts,
# uncommitted work in the way, manual mode, and list's notes.
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

branches_with_no_workspace() {
R branch p1
commit_on p1 p1.txt one
out=$("$BIN" -C "$REPO" branch p2 --on p1)
eq "branch --on: created on the parent's tip" "created branch p2 on p1; open it as <mountpoint>/p2" "$out"
eq "its parent recorded" "p1" "$(R config branch.p2.gitfoldParent)"
eq "and its parent base" "$(R rev-parse p1)" "$(stack_ref p2)"
commit_on p2 p2.txt two
eq "stack on p1 again: up to date" "p2 is on p1 (base $(R rev-parse --short=12 p1)); up to date" \
    "$("$BIN" -C "$REPO" stack on p1 -w p2)"
check "a cycle is refused" fails "$BIN" -C "$REPO" stack on p2 -w p1
check "so is a branch on itself" fails "$BIN" -C "$REPO" stack on p2 -w p2
old=$(R rev-parse p1)
commit_on p1 a.txt fix
eq "stack list: the tree, with what it needs" "p1
  p2  (needs restack (1))" "$("$BIN" -C "$REPO" stack list)"
out=$("$BIN" -C "$REPO" stack sync p2 --dry-run)
eq "--dry-run: says so, moves nothing" "p2	would be restacked onto p1 (1 new commit)|$old" "$out|$(R rev-parse p2^)"
mine=$(R rev-parse p2)
out=$("$BIN" -C "$REPO" stack sync p2)
eq "sync: restacked" "p2	restacked onto p1 (1 new commit)" "$out"
check "onto the parent's tip" parent_is p2 p1
eq "with its own commit's author, message and tree change" "t p2: p2.txt|p2.txt" \
    "$(R log -1 --format='%an %s' p2)|$(R diff --name-only p2^ p2)"
eq "the parent base moved with it" "$(R rev-parse p1)" "$(stack_ref p2)"
check "the old commit is in the reflog" sh -c "git -C '$REPO' reflog p2 | grep -q '^${mine:0:7}'"
eq "nothing left to do" "p2	up to date with p1" "$("$BIN" -C "$REPO" stack sync p2)"

# A third level, synced bottom-up with --all.
"$BIN" -C "$REPO" branch p3 --on p2 >/dev/null
commit_on p3 p3.txt three
commit_on p1 a.txt fix2
out=$("$BIN" -C "$REPO" stack sync --all)
eq "--all: bottom-up" "p2	restacked onto p1 (1 new commit)
p3	restacked onto p2 (2 new commits)" "$out"
check "p3 on p2" parent_is p3 p2
check "p2 on p1" parent_is p2 p1

# A conflict: nothing moves, and it is recorded.
commit_on p2 a.txt mine
commit_on p1 a.txt theirs
tip=$(R rev-parse p2)
out=$("$BIN" -C "$REPO" stack sync p2 2>&1); rc=$?
eq "a conflict: exit 4, the commit and path named" "4 yes" \
    "$rc $(grep -q "^p2	blocked: commit $(R rev-parse --short=12 p2) conflicts on a.txt with p1; nothing moved" <<<"$out" && echo yes)"
eq "nothing moved" "$tip" "$(R rev-parse p2)"
eq "list says so" "  p2  (restack blocked)" "$("$BIN" -C "$REPO" stack list | grep '^  p2')"
# Resolved by hand: the next sync takes it as it is.
R switch -q p2; R rebase -q p1 >/dev/null 2>&1; R checkout -q --theirs a.txt 2>/dev/null
printf 'mine\n' >"$REPO/a.txt"; R add a.txt; GIT_EDITOR=true R rebase --continue >/dev/null 2>&1; R switch -q main
eq "rebased by hand: its base is moved" "p2	has p1's tip already; its parent base moved there" \
    "$("$BIN" -C "$REPO" stack sync p2)"
eq "and the block forgotten" "" "$("$BIN" -C "$REPO" stack list | grep blocked)"

# Checked out in a git worktree: left to it.
commit_on p1 p1.txt one-more
R switch -q p2
out=$("$BIN" -C "$REPO" stack sync p2 2>&1); rc=$?
eq "checked out in a git worktree: refused (exit 4)" "4 yes" "$rc $(grep -q 'checked out in the main worktree' <<<"$out" && echo yes)"
R switch -q main

# The push it needs, never done.
git init -q --bare "$T/remote.git"; R remote add origin "$T/remote.git"
R push -q -u origin p2 2>/dev/null
pushed=$(R rev-parse p2)
out=$("$BIN" -C "$REPO" stack sync p2)
eq "restacked, and the force-push to run printed" "p2	restacked onto p1 (1 new commit)
to push (rewritten since last push):
  git push --force-with-lease=p2:$pushed origin p2:p2" "$out"
eq "not pushed" "$pushed" "$(git -C "$T/remote.git" rev-parse p2)"
eq "list: needs force-push" "  p2  (needs force-push)" "$("$BIN" -C "$REPO" stack list | grep '^  p2')"

# The parent rewound behind the base: its dropped commit goes from p2 too.
commit_on p1 x.txt dropped-later
"$BIN" -C "$REPO" stack sync p2 >/dev/null
R branch -f p1 p1~1
eq "parent rewound: restacked without the dropped commit" "p2	restacked onto p1 (0 new commits)|" \
    "$("$BIN" -C "$REPO" stack sync p2 2>&1 | head -1)|$(R ls-tree --name-only p2 x.txt)"
check "onto the parent's tip, its two commits on it" test "$(R rev-parse p2~2)" = "$(R rev-parse p1)"

check "stack off" "$BIN" -C "$REPO" stack off -w p2
eq "forgotten, ref and all" "||" "$(R config branch.p2.gitfoldParent)|$(stack_ref p2)|$("$BIN" -C "$REPO" stack list | grep '^  p2 ')"
check "not stacked: sync refused (exit 3)" sh -c "'$BIN' -C '$REPO' stack sync p2; [ \$? = 3 ]"
}

an_open_workspace() {
R branch p1
"$BIN" -C "$REPO" branch p2 --on p1 >/dev/null
mount_fs
W=$MNT/p2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "p2 own"
# Its agent's uncommitted work: unstaged, staged and untracked.
echo mine >>"$W/d/c.txt"; echo staged >"$W/s.txt"; G "$W" add s.txt; echo u >"$W/u.txt"
cat "$W/a.txt" >/dev/null                # cached by the kernel before
echo fix >>"$MNT/p1/a.txt"; echo more >"$MNT/p1/p1b.txt"
G "$MNT/p1" add -A; G "$MNT/p1" commit -qm "review fix"
out=$(prompt_hook "$W")
eq "the hook restacks it, and says so" "gitfold: this branch, p2, was restacked onto p1's 1 new commit (review fix); files changed under you: a.txt, p1b.txt. Its history was rewritten: push it with --force-with-lease when you push." "$out"
check "onto the parent's tip" parent_is p2 p1
eq "the new content shows" "a
fix|more" "$(cat "$W/a.txt")|$(cat "$W/p1b.txt")"
eq "uncommitted work kept: unstaged, staged, untracked" " M d/c.txt
A  s.txt
?? u.txt" "$(git -C "$W" status --short)"
for _ in $(seq 30); do [ "$(cat "$COMMON/gitfold/p2/base")" = "$(R rev-parse p2)" ] && break; sleep 0.1; done
eq "the base follows" "$(R rev-parse p2)" "$(cat "$COMMON/gitfold/p2/base")"
for _ in $(seq 30); do [ ! -e "$COMMON/gitfold/p2/upper/p1b.txt" ] && break; sleep 0.1; done
check "and upper no longer holds the restacked files" test ! -e "$COMMON/gitfold/p2/upper/p1b.txt" -a ! -e "$COMMON/gitfold/p2/upper/a.txt"
check "no intent left" test ! -e "$COMMON/gitfold/p2/restack"
eq "the hook again: nothing to say" "" "$(prompt_hook "$W")"

# Uncommitted work on a path the restack changes: it waits.
echo c2 >>"$MNT/p1/d/c.txt"; G "$MNT/p1" commit -qam "p1 c"
tip=$(R rev-parse p2)
out=$("$BIN" -C "$REPO" stack sync p2 2>&1); rc=$?
eq "overlapping uncommitted work: waits (exit 4)" "4 yes" "$rc $(grep -q '^p2	waits: uncommitted changes to d/c.txt overlap the restack' <<<"$out" && echo yes)"
eq "nothing moved" "$tip| M d/c.txt" "$(R rev-parse p2)|$(git -C "$W" status --short d/c.txt)"
eq "list: the parent, and what it needs" "p1 (needs restack (1))" \
    "$("$BIN" -C "$REPO" list | awk '$1 == "p2" { $1=$2=$3=$4=$5=$6=""; sub(/^ +/, ""); print }')"
eq "the hook says it waits" yes "$(prompt_hook "$W" | grep -q 'waits: uncommitted changes to d/c.txt' && echo yes)"
eq "once, while it stays so" "" "$(prompt_hook "$W")"
G "$W" checkout -q d/c.txt
check "once it is out of the way: synced" "$BIN" -C "$W" stack sync
eq "the parent's change shows" "c
c2" "$(cat "$W/d/c.txt")"

# The hook and stack sync at once: one restacks, the other leaves it be
# (it used to put the files and index back under the new tip).
for i in 1 2 3; do
    echo "r$i" >"$MNT/p1/r.txt"; G "$MNT/p1" add r.txt; G "$MNT/p1" commit -qm "p1 r$i"
    prompt_hook "$W" >/dev/null & "$BIN" -C "$REPO" stack sync p2 >/dev/null 2>&1; wait $!
done
check "both at once: on the parent's tip" parent_is p2 p1
eq "its files and index at the tip" "r3|" "$(cat "$W/r.txt")|$(git -C "$W" status --short r.txt)"
check "no intent left" test ! -e "$COMMON/gitfold/p2/restack"

# A fork keeps the parent base.
"$BIN" -C "$W" branch p2f -w >/dev/null
eq "a fork of it: the same parent and base" "p1 $(stack_ref p2)" "$(R config branch.p2f.gitfoldParent) $(stack_ref p2f)"

# Manual mode: only stack sync restacks.
R config branch.p2.gitfoldRestack manual
echo m >"$MNT/p1/m.txt"; G "$MNT/p1" add m.txt; G "$MNT/p1" commit -qm "p1 m"
tip=$(R rev-parse p2)
eq "manual: the hook leaves it" "|$tip" "$(prompt_hook "$W")|$(R rev-parse p2)"
check "stack sync does it" "$BIN" -C "$REPO" stack sync p2
check "m.txt shows" test -e "$W/m.txt"
R config --unset branch.p2.gitfoldRestack

# A conflict: nothing moves, said once.
echo theirs >"$MNT/p1/own.txt"; G "$MNT/p1" add own.txt; G "$MNT/p1" commit -qm "p1 own"
tip=$(R rev-parse p2)
out=$(prompt_hook "$W")
eq "a conflict: the hook says so" yes "$(grep -q "^gitfold: p2 is stacked on p1, which has new commits; blocked: commit $(R rev-parse --short=12 p2) conflicts on own.txt with p1; nothing moved" <<<"$out" && echo yes)"
eq "nothing moved" "$tip|own" "$(R rev-parse p2)|$(cat "$W/own.txt")"
eq "said once" "" "$(prompt_hook "$W")"
for _ in $(seq 30); do [ "$(cat "$COMMON/gitfold/p2/base")" = "$(R rev-parse p2)" ] && break; sleep 0.1; done
eq "list: blocked" "p1 (restack blocked)" \
    "$("$BIN" -C "$REPO" list | awk '$1 == "p2" { $1=$2=$3=$4=$5=$6=""; sub(/^ +/, ""); print }')"
}

layers_or_an_unmounted_workspace() {
R branch p1; R branch la
"$BIN" -C "$REPO" branch p2 --on p1 >/dev/null
mount_fs
W=$MNT/p2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "p2 own"
echo la >>"$MNT/la/a.txt"
"$BIN" -C "$W" layer add la >/dev/null
echo n >"$MNT/p1/n.txt"; G "$MNT/p1" add n.txt; G "$MNT/p1" commit -qm "p1 n"
out=$("$BIN" -C "$REPO" stack sync p2 2>&1); rc=$?
eq "layers shown: waits (exit 4)" "4 yes" "$rc $(grep -q 'layers are shown in its workspace' <<<"$out" && echo yes)"
"$BIN" -C "$W" lens off >/dev/null
for _ in $(seq 30); do [ "$(cat "$COMMON/gitfold/p2/base")" = "$(R rev-parse p2)" ] && break; sleep 0.1; done
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
out=$("$BIN" -C "$REPO" stack sync p2)
eq "unmounted, nothing uncommitted: restacked" "p2	restacked onto p1 (1 new commit); 1 file changed in its workspace" "$out"
eq "its base moved too" "$(R rev-parse p2)" "$(cat "$COMMON/gitfold/p2/base")"
mount_fs
eq "the new file shows" "n" "$(cat "$W/n.txt")"
eq "git status clean" "" "$(git -C "$W" status --short)"
echo dirty >>"$W/a.txt"
echo n2 >"$MNT/p1/n2.txt"; G "$MNT/p1" add n2.txt; G "$MNT/p1" commit -qm "p1 n2"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
tip=$(R rev-parse p2)
out=$("$BIN" -C "$REPO" stack sync p2 2>&1); rc=$?
eq "unmounted, uncommitted changes: waits for the mount (exit 4)" "4 yes $tip" \
    "$rc $(grep -q 'restacked once it is mounted' <<<"$out" && echo yes) $(R rev-parse p2)"
mount_fs
out=$(prompt_hook "$W")
eq "mounted: the hook restacks it" yes "$(grep -q 'restacked onto p1' <<<"$out" && echo yes)"
eq "own change kept" " M a.txt" "$(git -C "$W" status --short)"
# An intent recovery can't read: layer switches wait, stack off forgets it.
echo junk >"$COMMON/gitfold/p2/restack"
out=$("$BIN" -C "$W" layer on la 2>&1); rc=$?
eq "an intent left: layer switches refused (exit 4)" "4 yes" "$rc $(grep -q 'a restack of p2 is under way' <<<"$out" && echo yes)"
echo "$out" >&2
check "stack off forgets it" sh -c "'$BIN' -C '$REPO' stack off -w p2 | grep -q 'intent is forgotten'"
check "then layers switch again" "$BIN" -C "$W" layer on la
}

# Branches $1 (on main, file $1.txt) and $2 stacked on it (file $2.txt).
pair() {
    R branch "$1"; commit_on "$1" "$1.txt" "$1"
    "$BIN" -C "$REPO" branch "$2" --on "$1" >/dev/null; commit_on "$2" "$2.txt" "$2"
}
# Squash-merges branch $1 into main as one commit; prints it.
squash() {
    R merge -q --squash "$1" >/dev/null 2>&1; R commit -qm "$1 (#1)"; R rev-parse main
}
# Says whether branch $1 sits right on commit $2 with its one own commit,
# stacked on $3, its stack ref at $2.
landed_on() {
    [ "$(R rev-parse "$1^")" = "$2" ] && [ "$(R config "branch.$1.gitfoldParent")" = "$3" ] &&
        [ "$(stack_ref "$1")" = "$2" ]
}

a_parent_that_landed() {
commit_on main m.txt moved-on
# A squash merge, the parent deleted after it.
pair p1 p2
s=$(squash p1)
eq "list: the parent looks merged" "  p2  (parent looks merged: re-parent with stack sync)" \
    "$("$BIN" -C "$REPO" stack list | grep '^  p2')"
eq "--dry-run: says so, moves nothing" "p2	p1 looks squash- or rebase-merged into main as ${s:0:12}; would move there and be stacked on main|p1" \
    "$("$BIN" -C "$REPO" stack sync p2 --dry-run)|$(R config branch.p2.gitfoldParent)"
R branch -q -D p1
out=$("$BIN" -C "$REPO" stack sync p2)
eq "a squash merge: re-parented onto the landing commit" "p2	p1 looks squash- or rebase-merged into main as ${s:0:12}; moved there with its own 1 commit and now stacked on main. Its pull request's base should now be main: push it, then \`gh pr edit p2 --base main\`" "$out"
check "on the squash, stacked on main, its stack ref there" landed_on p2 "$s" main
eq "with only its own change" "p2.txt" "$(R diff --name-only "$s" p2)"
commit_on main later.txt later
eq "main moving on: no note (following it is on request)" "  p2" "$("$BIN" -C "$REPO" stack list | grep '^  p2')"
eq "stack sync follows it on request" "p2	restacked onto main (1 new commit)" "$("$BIN" -C "$REPO" stack sync p2)"

# A rebase merge: onto its last commit.
pair r1 r2
commit_on main m.txt before-r1
R cherry-pick main..r1 >/dev/null; last=$(R rev-parse main)
"$BIN" -C "$REPO" stack sync r2 >/dev/null
check "a rebase merge: onto its last commit" landed_on r2 "$last" main

# A squash the maintainer tweaked.
pair t1 t2
R merge -q --squash t1 >/dev/null; echo tweaked >"$REPO/a.txt"; R commit -qam "t1, tweaked"
"$BIN" -C "$REPO" stack sync t2 >/dev/null
check "a tweaked squash: found too" landed_on t2 "$(R rev-parse main)" main

# A merge commit, into a stacked parent's parent.
R branch p0; commit_on p0 p0.txt p0
"$BIN" -C "$REPO" branch q1 --on p0 >/dev/null; commit_on q1 q1.txt q1
"$BIN" -C "$REPO" branch q2 --on q1 >/dev/null; commit_on q2 q2.txt q2
commit_on p0 p0b.txt more
R switch -q p0; R merge -q --no-ff --no-edit q1; R switch -q main
out=$("$BIN" -C "$REPO" stack sync q2)
eq "a merge into the parent's own parent: onto the merge" yes \
    "$(grep -q "^q2	q1 was merged into p0 by $(R rev-parse --short=12 p0);" <<<"$out" && echo yes)"
check "stacked on p0 now" landed_on q2 "$(R rev-parse p0)" p0

# A merge of the parent after it got more commits than the child's base.
pair x1 x2
commit_on x1 x1b.txt later
R merge -q --no-ff --no-edit x1
"$BIN" -C "$REPO" stack sync x2 >/dev/null
check "a merge of a later parent tip: onto the merge" landed_on x2 "$(R rev-parse main)" main

# A move onto a landing interrupted after the refs (the parent not yet
# set): the next sync finishes it, rather than going back onto the parent.
pair k1 k2
s=$(squash k1)
R rebase -q --onto "$s" k1 k2; R switch -q main
R update-ref refs/gitfold-stack/k2 "$s"; R config branch.k2.gitfoldLanding "main $s"
eq "interrupted re-parent: finished" "k2	up to date with main|main|" \
    "$("$BIN" -C "$REPO" stack sync k2)|$(R config branch.k2.gitfoldParent)|$(R config branch.k2.gitfoldLanding)"

# stack merged on a deleted parent: its own parent's name went with it.
R branch g0; commit_on g0 g0.txt g0
"$BIN" -C "$REPO" branch g1 --on g0 >/dev/null; commit_on g1 g1.txt g1
"$BIN" -C "$REPO" branch g2 --on g1 >/dev/null; commit_on g2 g2.txt g2
R switch -q g0; R merge -q --squash g1 >/dev/null 2>&1; R commit -qm "g1 rewritten" -m x; R switch -q main
R branch -q -D g1
check "stack merged, the parent gone: --onto needed (exit 3)" sh -c "'$BIN' -C '$REPO' stack merged g1; [ \$? = 3 ]"
"$BIN" -C "$REPO" stack merged g1 --onto g0 >/dev/null
check "--onto: onto it" landed_on g2 "$(R rev-parse g0)" g0

# Not merged: nothing said, nothing moved.
pair u1 u2
commit_on main m.txt moved-again
tip=$(R rev-parse u2)
eq "unmerged: up to date, still on its parent" "u2	up to date with u1|$tip|u1" \
    "$("$BIN" -C "$REPO" stack sync u2)|$(R rev-parse u2)|$(R config branch.u2.gitfoldParent)"

# Merged in a way detection can't tell: said so.
pair v1 v2
R merge -q --squash v1 >/dev/null; echo rewritten >"$REPO/v1.txt"; R commit -qam "v1, rewritten"
eq "a squash rewritten when merged: not found" "v2	up to date with v1" "$("$BIN" -C "$REPO" stack sync v2)"
"$BIN" -C "$REPO" stack merged v1 >/dev/null
check "stack merged: onto main's tip" landed_on v2 "$(R rev-parse main)" main
}

a_branch_on_the_local_trunk() {
# The trunk is origin/main; c is stacked on the local main tracking it.
git init -q --bare "$T/origin.git"; R remote add origin "$T/origin.git"
R push -q -u origin main; R remote set-head origin main
"$BIN" -C "$REPO" branch c --on main >/dev/null; commit_on c c.txt c
commit_on main x.txt local      # not pushed
"$BIN" -C "$REPO" stack sync c >/dev/null
# Someone else pushes; pulling with --rebase rewrites the local commit.
R switch -q --detach origin/main; echo y >"$REPO/y.txt"; R add y.txt; R commit -qm theirs
R push -q origin HEAD:main; R switch -q main; R fetch -q origin; R rebase -q origin/main
eq "list: no note for a branch on the trunk's local branch" "  c" "$("$BIN" -C "$REPO" stack list | grep '^  c')"
eq "the rewritten local commit isn't taken for a landing" "c	restacked onto main (2 new commits)|main" \
    "$("$BIN" -C "$REPO" stack sync c)|$(R config branch.c.gitfoldParent)"
}

a_landed_parent_in_an_open_workspace() {
R branch h1; commit_on h1 h1.txt h1
"$BIN" -C "$REPO" branch h2 --on h1 >/dev/null
mount_fs
W=$MNT/h2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "h2 own"
echo mine >>"$W/a.txt"
commit_on main m.txt moved-on
s=$(squash h1)
tip=$(R rev-parse h2)
eq "it only looks merged, h1 still there: the hook says so, moves nothing" "gitfold: h2's parent h1 looks squash- or rebase-merged into main as ${s:0:12}, but h1 is still there; \`gitfold stack sync h2\` moves it there; suggest it to the user rather than running it yourself: it rewrites h2's history.|$tip" \
    "$(prompt_hook "$W")|$(R rev-parse h2)"
eq "once" "" "$(prompt_hook "$W")"
R branch -q -D h1
out=$(prompt_hook "$W")
eq "h1 gone: the hook re-parents it, and says so" "gitfold: this branch's parent h1 looks squash- or rebase-merged into main as ${s:0:12}. h2 now sits there with only its own 1 commit, stacked on main; files changed under you: m.txt. Its history was rewritten, so it needs a push with --force-with-lease, and then its pull request's base should be main: suggest \`gh pr edit h2 --base main\` to the user after that push rather than running it yourself." "$out"
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
