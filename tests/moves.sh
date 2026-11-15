#!/usr/bin/env bash
# End-to-end test of moves: a branch's own commits moved onto the commits of
# the editable layers it builds on (layer add --edit, branch --edit), and
# off them (layer add without --edit, layer remove): branches with no
# workspace, an unmounted workspace and an open one (by the UserPromptSubmit
# hook and by layer add again), conflicts, uncommitted work in the way,
# commits already pushed, --frozen, commits that can't stand without a
# layer, several editable layers, landings (followed unasked only once the
# layer's branch is gone), moves with see-only layers shown (also killed
# half-way), and list's tree and notes.
# usage: tests/moves.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=moves
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
has() { grep -qF -- "$2" <<<"$1"; }
own_ref() { R rev-parse -q --verify "refs/gitfold-own/$1"; }
# `gitfold layer` for branch $1, the rest its arguments.
lay() { local b=$1; shift; "$BIN" -C "$REPO" layer -w "$b" "$@"; }
# The editable layers stored for branch $1, on one line.
editable() { awk -F'\t' '$6 == "edit" {sub(/^layer /, "", $1); print $1}' "$COMMON/gitfold/$1/layers" 2>/dev/null | paste -sd' '; }
# What `gitfold list` says of branch $1: what it builds on, and its status.
listed() {
    "$BIN" -C "$REPO" list | sed -E 's/^ *(└─ )?//' |
        awk -v b="$1" '$1 == b { $1=$2=$3=$4=$5=$6=""; sub(/^ +/, ""); print }'
}
# `gitfold list`'s branch column: the tree.
tree() { "$BIN" -C "$REPO" list | tail -n +2 | cut -c1-40 | sed 's/ *$//'; }

branches_with_no_workspace() {
R branch p1
commit_on p1 p1.txt one
out=$("$BIN" -C "$REPO" branch p2 --edit p1)
eq "branch --edit: created on the layer's tip" "created branch p2 built on p1; open it under the mount as p2 (gitfold mount)" "$out"
eq "its editable layer stored" "p1" "$(editable p2)"
eq "and where its own commits start" "$(R rev-parse p1)" "$(own_ref p2)"
check "no config key for it" test -z "$(R config --get-regexp 'branch\.p2\.' || true)"
commit_on p2 p2.txt two
eq "layer add --edit again: up to date" "layer p1 updated: commits, live, last HH:MM, built on" \
    "$(lay p2 add p1 --edit | sed -E 's/[0-9]{2}:[0-9]{2}/HH:MM/')"
lay p1 add p2 --edit >/dev/null 2>&1; rc=$?
eq "a cycle: a usage error (exit 2)" 2 "$rc"
lay p2 add p2 --edit >/dev/null 2>&1; rc=$?
eq "a branch on itself: a usage error (exit 2)" 2 "$rc"
lay p2 add main --edit >/dev/null 2>&1; rc=$?
eq "main can't be edited on: a usage error (exit 2)" 2 "$rc"
commit_on p1 a.txt fix
eq "list: what it builds on, and what it needs" "p1 (needs update: its next turn, or \`gitfold layer add <LAYER> --edit\`)" "$(listed p2)"
mine=$(R rev-parse p2)
out=$(lay p2 add p1 --edit)
eq "again: says what it moves, then moves it" "moves 1 commit of p2 onto p1
p2: moved" "$(head -2 <<<"$out")"
check "onto the layer's tip" parent_is p2 p1
eq "with its own commit's author, message and tree change" "t p2: p2.txt|p2.txt" \
    "$(R log -1 --format='%an %s' p2)|$(R diff --name-only p2^ p2)"
eq "where its own commits start moved with it" "$(R rev-parse p1)" "$(own_ref p2)"
check "the old commit is in the reflog" sh -c "git -C '$REPO' reflog p2 | grep -q '^${mine:0:7}'"

# A third level.
"$BIN" -C "$REPO" branch p3 --edit p2 >/dev/null
commit_on p3 p3.txt three
eq "list: a tree, each layer above the branches built on it" "p1
└─ p2
   └─ p3" "$(tree)"

# A conflict: nothing moves, and it is recorded.
commit_on p2 a.txt mine
commit_on p1 a.txt theirs
tip=$(R rev-parse p2)
out=$(lay p2 add p1 --edit 2>&1); rc=$?
eq "a conflict: exit 4, the commit and path named" "4 yes" \
    "$rc $(grep -q "^p2: blocked: commit $(R rev-parse --short=12 p2) conflicts on a.txt with p1; nothing moved" <<<"$out" && echo yes)"
eq "nothing moved" "$tip" "$(R rev-parse p2)"
eq "list says so" "p1 (blocked: conflicts on a.txt)" "$(listed p2)"
# Resolved by hand: the next move takes it as it is.
R switch -q p2; R rebase -q p1 >/dev/null 2>&1; R checkout -q --theirs a.txt 2>/dev/null
printf 'mine\n' >"$REPO/a.txt"; R add a.txt; GIT_EDITOR=true R rebase --continue >/dev/null 2>&1; R switch -q main
check "rebased by hand: its own commits now start at p1's tip" lay p2 add p1 --edit
eq "where they start" "$(R rev-parse p1)" "$(own_ref p2)"
eq "and the block forgotten" "" "$("$BIN" -C "$REPO" list | grep blocked)"

# Checked out in a git worktree: left to it.
commit_on p1 p1.txt one-more
R switch -q p2
out=$(lay p2 add p1 --edit 2>&1); rc=$?
eq "checked out in a git worktree: refused (exit 4)" "4 yes" "$rc $(grep -q 'checked out in the main worktree' <<<"$out" && echo yes)"
R switch -q main

# Commits already pushed: moved only with --yes; the push never done.
git init -q --bare "$T/remote.git"; R remote add origin "$T/remote.git"
R push -q -u origin p2 2>/dev/null
pushed=$(R rev-parse p2)
out=$(lay p2 add p1 --edit 2>&1); rc=$?
eq "pushed: says what it would move, refuses (exit 4)" "4 yes yes" \
    "$rc $(grep -qx 'moves 2 commits of p2 onto p1' <<<"$out" && echo yes) $(grep -q '^p2: not moved: 2 of them are pushed already (origin/p2).*--yes moves them' <<<"$out" && echo yes)"
eq "nothing moved" "$pushed" "$(R rev-parse p2)"
out=$(lay p2 add p1 --edit --yes)
eq "--yes: moved, and the force-push to run printed" "moves 2 commits of p2 onto p1
p2: moved
to push (rewritten since last push):
  git push --force-with-lease=p2:$pushed origin p2:p2" "$(head -4 <<<"$out")"
eq "not pushed" "$pushed" "$(git -C "$T/remote.git" rev-parse p2)"
eq "list: the push to run" "p1 (push with: git push --force-with-lease=p2:$pushed origin p2:p2)" "$(listed p2)"
R push -q -f origin p2 2>/dev/null

# Onto another layer that conflicts: nothing changes, what it builds on
# included.
R branch other main; commit_on other p2.txt other-side
tip=$(R rev-parse p2)
out=$(lay p2 add other --edit 2>&1); rc=$?
eq "two editable layers: the pull request warning first" yes \
    "$(grep -q '^p2 will be built on p1 and other. A pull request for p2 shows' <<<"$out" && echo yes)"
eq "a conflict with it: refused (exit 4)" "4 yes" "$rc $(grep -q '^p2: blocked: commit .* conflicts on p2.txt with p1 and other' <<<"$out" && echo yes)"
eq "nothing changed, its layers included" "p1|$(R rev-parse p1)|$tip" "$(editable p2)|$(own_ref p2)|$(R rev-parse p2)"

# Taken off its layer: onto main, which it is then on.
"$BIN" -C "$REPO" branch r2 --edit p1 >/dev/null; commit_on r2 r2.txt r2
commit_on main m.txt trunk
out=$(lay r2 remove p1)
eq "layer remove: its own commits off it, onto main" "moves 1 commit of r2 off p1, onto main
r2: moved
layer p1 removed" "$out"
check "on main" parent_is r2 main
eq "without p1's work" "" "$(R ls-tree --name-only r2 p1.txt)"
eq "nothing built on, no own ref, not in list" "||" "$(editable r2)|$(own_ref r2)|$(listed r2)"
eq "p2's commit changes p1's lines: it can't come off p1" "4" "$(lay p2 remove p1 >/dev/null 2>&1; echo $?)"
eq "p3 on p2's old commits, said" "p2 (needs update: its next turn, or \`gitfold layer add <LAYER> --edit\`)" "$(listed p3)"
eq "branch --edit main: an ordinary branch" "created branch o built on main; open it under the mount as o (gitfold mount)|" \
    "$("$BIN" -C "$REPO" branch o --edit main)|$(editable o)"

# A commit changing the layer's lines can't come off it.
R branch q1; commit_on q1 q.txt "$(printf 'one\ntwo')"
"$BIN" -C "$REPO" branch q2 --edit q1 >/dev/null
commit_on q2 q.txt "$(printf 'one\ntwo mine')"
tip=$(R rev-parse q2)
out=$(lay q2 remove q1 2>&1); rc=$?
eq "commits changing its lines: refused (exit 4), listed" "4 yes" \
    "$rc $(grep -q "^q2: not moved: 1 commit change q1's lines, so they can't stand without q1: $(R rev-parse --short=12 q2) q2: q.txt (q.txt)" <<<"$out" && echo yes)"
eq "nothing moved, still built on it" "$tip|q1" "$(R rev-parse q2)|$(editable q2)"
out=$(lay q2 add q1 2>&1); rc=$?
eq "nor made see-only (exit 4)" "4 yes|q1" "$rc $(has "$out" "q1 becomes see-only: moves 1 commit of q2 off q1, onto main (q1 stays shown, read-only)" && echo yes)|$(editable q2)"

# --frozen: built on it once, not followed after.
R branch z; commit_on z z.txt z
out=$(lay z add q1 --edit --frozen)
eq "--edit --frozen: moved onto it" "moves 1 commit of z onto q1" "$(head -1 <<<"$out")"
check "z on q1" parent_is z q1
commit_on q1 q2.txt more
eq "frozen: no update needed" "q1" "$(listed z)"
eq "the next turn leaves it" "$(R rev-parse q1^)" "$(R rev-parse z^)"
eq "again, --frozen: brought up to date, still frozen" "$(R rev-parse q1)|yes" \
    "$(lay z add q1 --edit --frozen >/dev/null; R rev-parse z^)|$(grep -qP '^layer q1\t[^\t]*\tfrozen\t' "$COMMON/gitfold/z/layers" && echo yes)"
check "again without --frozen: live" lay z add q1 --edit
check "live" grep -qP '^layer q1\t[^\t]*\tlive\t' "$COMMON/gitfold/z/layers"
}

an_open_workspace() {
R branch p1
"$BIN" -C "$REPO" branch p2 --edit p1 >/dev/null
mount_fs
W=$MNT/p2
eq "the listing: no commits of its own yet" "p1 commits live, last HH:MM built on: no commits of your own yet on it" \
    "$("$BIN" -C "$W" layer | sed -E 's/[0-9]{2}:[0-9]{2}/HH:MM/' | tr -s ' ' | grep '^p1')"
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "p2 own"
# Its agent's uncommitted work: unstaged, staged and untracked.
echo mine >>"$W/d/c.txt"; echo staged >"$W/s.txt"; G "$W" add s.txt; echo u >"$W/u.txt"
cat "$W/a.txt" >/dev/null                # cached by the kernel before
echo fix >>"$MNT/p1/a.txt"; echo more >"$MNT/p1/p1b.txt"
G "$MNT/p1" add -A; G "$MNT/p1" commit -qm "review fix"
out=$(prompt_hook "$W")
eq "the hook moves it, and says so" yes \
    "$(grep -qF "gitfold: this branch, p2, was moved onto p1's new commits (review fix); files changed under you: a.txt, p1b.txt. Its history was rewritten." <<<"$out" && echo yes)"
check "onto the layer's tip" parent_is p2 p1
eq "the new content shows" "a
fix|more" "$(cat "$W/a.txt")|$(cat "$W/p1b.txt")"
eq "uncommitted work kept: unstaged, staged, untracked" " M d/c.txt
A  s.txt
?? u.txt" "$(git -C "$W" status --short)"
eq "the base moved with it" "$(R rev-parse p2)" "$(cat "$COMMON/gitfold/p2/base")"
check "upper doesn't hold the moved files" test ! -e "$COMMON/gitfold/p2/upper/p1b.txt" -a ! -e "$COMMON/gitfold/p2/upper/a.txt"
check "no intent left" test ! -e "$COMMON/gitfold/p2/move"
eq "the hook again: nothing to say" "" "$(prompt_hook "$W")"
eq "list: workspaces in the tree too" "p1
└─ p2" "$(tree)"

# Uncommitted work on a path the move changes: it waits.
echo c2 >>"$MNT/p1/d/c.txt"; G "$MNT/p1" commit -qam "p1 c"
tip=$(R rev-parse p2)
out=$(lay p2 add p1 --edit 2>&1); rc=$?
eq "overlapping uncommitted work: waits (exit 4)" "4 yes" "$rc $(grep -q '^p2: waits: uncommitted changes to d/c.txt overlap the move' <<<"$out" && echo yes)"
eq "nothing moved" "$tip| M d/c.txt" "$(R rev-parse p2)|$(git -C "$W" status --short d/c.txt)"
eq "the hook says it waits" yes "$(prompt_hook "$W" | grep -q 'waits: uncommitted changes to d/c.txt' && echo yes)"
eq "once, while it stays so" "" "$(prompt_hook "$W")"
G "$W" checkout -q d/c.txt
check "once it is out of the way: layer add again moves it" "$BIN" -C "$W" layer add p1 --edit
eq "the layer's change shows" "c
c2" "$(cat "$W/d/c.txt")"

# A staged change on such a path: waits too.
echo c3 >>"$MNT/p1/d/c.txt"; G "$MNT/p1" commit -qam "p1 c3"
echo staged >>"$W/d/c.txt"; G "$W" add d/c.txt; G "$W" checkout -q d/c.txt 2>/dev/null
echo restored >/dev/null
out=$(lay p2 add p1 --edit 2>&1); rc=$?
eq "a staged change in the way: waits (exit 4)" "4" "$rc"
G "$W" reset -q d/c.txt; G "$W" checkout -q d/c.txt
check "unstaged: moved" "$BIN" -C "$W" layer add p1 --edit

# The hook and layer add at once: one moves it, the other leaves it be.
for i in 1 2 3; do
    echo "r$i" >"$MNT/p1/r.txt"; G "$MNT/p1" add r.txt; G "$MNT/p1" commit -qm "p1 r$i"
    prompt_hook "$W" >/dev/null & lay p2 add p1 --edit >/dev/null 2>&1; wait $!
done
check "both at once: on the layer's tip" parent_is p2 p1
eq "its files and index at the tip" "r3|" "$(cat "$W/r.txt")|$(git -C "$W" status --short r.txt)"
check "no intent left" test ! -e "$COMMON/gitfold/p2/move"

# A fork is built on the same.
"$BIN" -C "$W" branch p2f -w >/dev/null
eq "a fork of it: the same editable layer, its own commits from the same place" "p1 $(own_ref p2)" "$(editable p2f) $(own_ref p2f)"

# --frozen: only layer add moves it.
"$BIN" -C "$W" layer add p1 --edit --frozen >/dev/null
echo m >"$MNT/p1/m.txt"; G "$MNT/p1" add m.txt; G "$MNT/p1" commit -qm "p1 m"
tip=$(R rev-parse p2)
eq "frozen: the hook leaves it" "|$tip" "$(prompt_hook "$W")|$(R rev-parse p2)"
check "layer add --edit --frozen again moves it" "$BIN" -C "$W" layer add p1 --edit --frozen
check "m.txt shows" test -e "$W/m.txt"
check "layer add --edit again: live" "$BIN" -C "$W" layer add p1 --edit
check "live again" has "$("$BIN" -C "$W" layer | grep '^p1')" "live"

# A conflict: nothing moves, said once.
echo theirs >"$MNT/p1/own.txt"; G "$MNT/p1" add own.txt; G "$MNT/p1" commit -qm "p1 own"
tip=$(R rev-parse p2)
out=$(prompt_hook "$W")
eq "a conflict: the hook says so" yes "$(grep -q "^gitfold: p2 builds on p1; blocked: commit $(R rev-parse --short=12 p2) conflicts on own.txt with p1; nothing moved" <<<"$out" && echo yes)"
eq "nothing moved, the layer's work as it showed" "$tip|own" "$(R rev-parse p2)|$(cat "$W/own.txt")"
eq "said once" "" "$(prompt_hook "$W")"
eq "list: blocked" "p1 (blocked: conflicts on own.txt)" "$(listed p2)"
}

layers_or_an_unmounted_workspace() {
R branch p1; R branch la; R branch p2; R branch other
mount_fs
W=$MNT/p2 S=$COMMON/gitfold/p2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "p2 own"
echo la >>"$MNT/la/a.txt"
"$BIN" -C "$W" layer add la >/dev/null
echo n0 >"$MNT/p1/n0.txt"; G "$MNT/p1" add n0.txt; G "$MNT/p1" commit -qm "p1 n0"
out=$("$BIN" -C "$W" layer add p1 --edit 2>&1); rc=$?
eq "a see-only layer shown: built on p1 with it" "0|p1|$(R rev-parse p1)" "$rc|$(editable p2)|$(R rev-parse p2^)"
eq "the layer still shows, git sees only its own" "a la|n0|" "$(paste -sd' ' "$W/a.txt")|$(cat "$W/n0.txt")|$(git -C "$W" status --short)"
check "nothing taken off for it" test ! -e "$S/layers-off"
echo m0 >"$MNT/p1/m0.txt"; G "$MNT/p1" add m0.txt; G "$MNT/p1" commit -qm "p1 m0"
# Uncommitted work in the way: it waits, the layers and the work kept,
# the view untouched (its files would be stamped anew).
echo mine >"$W/m0.txt"
stamp=$(stat -c %y "$W/a.txt")
out=$(prompt_hook "$W")
eq "the hook: waits for the work in the way" yes "$(grep -q 'waits: uncommitted changes to m0.txt overlap the move' <<<"$out" && echo yes)"
eq "the layer and the work kept" "a la|mine" "$(paste -sd' ' "$W/a.txt")|$(cat "$W/m0.txt")"
eq "the view untouched by a move that waits" "$stamp" "$(stat -c %y "$W/a.txt")"
rm "$W/m0.txt"
eq "the hook moves it" yes "$(prompt_hook "$W" | grep -q 'was moved onto p1' && echo yes)"
eq "on p1, the layer shown" "$(R rev-parse p1)|a la|m0" "$(R rev-parse p2^)|$(paste -sd' ' "$W/a.txt")|$(cat "$W/m0.txt")"
eq "the layer's file stamped by the move? no: unchanged" "$stamp" "$(stat -c %y "$W/a.txt")"
# A commit made while the layer shows (the base waits), then a move that
# changes the same file: the copy of the committed file goes.
echo p2 >>"$W/d/c.txt"; G "$W" commit -qam "p2 c"
echo p1 >>"$MNT/p1/d/b.txt"; G "$MNT/p1" commit -qam "p1 b"
check "the committed file in upper while the layer shows" test -e "$S/upper/d/c.txt"
echo p1c >"$MNT/p1/d/c.txt"; echo c >>"$MNT/p1/d/c.txt"; G "$MNT/p1" commit -qam "p1 c"
eq "the hook moves it" yes "$(prompt_hook "$W" | grep -q 'was moved onto p1' && echo yes)"
eq "the merged file shows, git clean" "p1c c p2||" \
    "$(paste -sd' ' "$W/d/c.txt")|$(git -C "$W" status --short)|$(test -e "$S/upper/d/c.txt" && echo copy)"
check "onto another layer with it shown" "$BIN" -C "$W" layer add other --edit
eq "on a merge of p1 and other, the layer shown" "p1 other|a la" "$(editable p2)|$(paste -sd' ' "$W/a.txt")"
out=$("$BIN" -C "$W" layer remove other)
eq "and back: off other" "moves 2 commits of p2 off other, onto p1" "$(head -1 <<<"$out")"
eq "on p1" "$(R rev-parse p1)" "$(R rev-parse p2~2)"
echo n >"$MNT/p1/n.txt"; G "$MNT/p1" add n.txt; G "$MNT/p1" commit -qm "p1 n"
echo dirty >>"$W/a2.txt"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
out=$("$BIN" -C "$REPO" layer add p1 --edit -w p2 2>/dev/null | grep "^p2: moved")
eq "unmounted: moved, uncommitted work off its paths kept" "p2: moved; 1 file changed in its workspace" "$out"
eq "its base moved too" "$(R rev-parse p2)" "$(cat "$S/base")"
mount_fs
eq "the new file shows, the layer too" "n|a la" "$(cat "$W/n.txt")|$(paste -sd' ' "$W/a.txt")"
eq "git: the own change only" "?? a2.txt" "$(git -C "$W" status --short)"
echo dirty >"$W/n2.txt"
echo n2 >"$MNT/p1/n2.txt"; G "$MNT/p1" add n2.txt; G "$MNT/p1" commit -qm "p1 n2"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
tip=$(R rev-parse p2)
out=$("$BIN" -C "$REPO" layer add p1 --edit -w p2 2>&1); rc=$?
eq "unmounted, work in the way: waits (exit 4)" "4 yes $tip" \
    "$rc $(grep -q 'waits: uncommitted changes to n2.txt' <<<"$out" && echo yes) $(R rev-parse p2)"
mount_fs
rm "$W/n2.txt"
out=$(prompt_hook "$W")
eq "mounted: the hook moves it" yes "$(grep -q 'was moved onto p1' <<<"$out" && echo yes)"
# An intent recovery can't read: layer switches wait; removing every
# layer forgets it.
echo junk >"$S/move"
out=$("$BIN" -C "$W" layer add la 2>&1); rc=$?
eq "an intent left: layer switches refused (exit 4)" "4 yes" "$rc $(grep -q 'p2 is being moved' <<<"$out" && echo yes)"
out=$("$BIN" -C "$W" layer remove --all)
eq "layer remove --all forgets it, nothing moved" "yes||" \
    "$(has "$out" "nothing moved, and an interrupted move's intent is forgotten" && echo yes)|$(editable p2)|$(own_ref p2)"
check "then layers switch again" "$BIN" -C "$W" layer add la
}

# Branches $1 (on main, file $1.txt) and $2 built on it (file $2.txt).
pair() {
    R branch "$1"; commit_on "$1" "$1.txt" "$1"
    "$BIN" -C "$REPO" branch "$2" --edit "$1" >/dev/null; commit_on "$2" "$2.txt" "$2"
}
# Squash-merges branch $1 into main as one commit; prints it.
squash() {
    R merge -q --squash "$1" >/dev/null 2>&1; R commit -qm "$1 (#1)"; R rev-parse main
}
# Whether branch $1 sits right on commit $2 with its one own commit, built
# on $3 (its own ref at $2); or, $3 main, built on nothing, no own ref.
landed_on() {
    [ "$(R rev-parse "$1^")" = "$2" ] || return 1
    if [ "$3" = main ]; then
        [ -z "$(editable "$1")" ] && ! own_ref "$1" >/dev/null
    else
        [ "$(editable "$1")" = "$3" ] && [ "$(own_ref "$1")" = "$2" ]
    fi
}

a_layer_that_landed() {
commit_on main m.txt moved-on
# A squash merge: said while the branch is there; layer remove follows it.
pair p1 p2
s=$(squash p1)
eq "list: it looks merged, the hint names layer remove" "p1 (p1 looks merged into main: \`gitfold layer remove p1\` moves it there)" "$(listed p2)"
out=$(lay p2 remove p1)
eq "layer remove: onto the landing commit, saying so first" "p1 looks merged into main as ${s:0:12}
moves 1 commit of p2 off p1, onto main at ${s:0:12}" "$(head -2 <<<"$out")"
check "on the squash, built on nothing" landed_on p2 "$s" main
eq "with only its own change" "p2.txt" "$(R diff --name-only "$s" p2)"

# A rebase merge: onto its last commit.
pair r1 r2
commit_on main m.txt before-r1
R cherry-pick main..r1 >/dev/null; last=$(R rev-parse main)
lay r2 remove r1 >/dev/null
check "a rebase merge: onto its last commit" landed_on r2 "$last" main

# A squash the maintainer tweaked.
pair t1 t2
R merge -q --squash t1 >/dev/null; echo tweaked >"$REPO/a.txt"; R commit -qam "t1, tweaked"
tip=$(R rev-parse t2)
eq "layer add t1 --edit while t1 is there: nothing moved, the landing noted" "$tip|t1|yes" \
    "$(lay t2 add t1 --edit >/dev/null; R rev-parse t2)|$(editable t2)|$(listed t2 | grep -q 'looks merged into main' && echo yes)"
lay t2 remove t1 >/dev/null
check "a tweaked squash: found too, layer remove follows it" landed_on t2 "$(R rev-parse main)" main

# A merge commit, into the layer's own editable layer.
R branch p0; commit_on p0 p0.txt p0
"$BIN" -C "$REPO" branch q1 --edit p0 >/dev/null; commit_on q1 q1.txt q1
"$BIN" -C "$REPO" branch q2 --edit q1 >/dev/null; commit_on q2 q2.txt q2
commit_on p0 p0b.txt more
R switch -q p0; R merge -q --no-ff --no-edit q1; R switch -q main
out=$(lay q2 remove q1)
eq "a merge into the layer's own: onto the merge" yes \
    "$(grep -q "^q1 was merged into p0 by $(R rev-parse --short=12 p0)" <<<"$out" && echo yes)"
check "built on p0 now, at the merge" landed_on q2 "$(R rev-parse p0)" p0

# A merge of the layer after it got more commits than the branch's.
pair x1 x2
commit_on x1 x1b.txt later
R merge -q --no-ff --no-edit x1
lay x2 remove x1 >/dev/null
check "a merge of a later tip: onto the merge" landed_on x2 "$(R rev-parse main)" main

# A fast-forward: told apart from a fresh branch once the branch is gone.
pair f1 f2
f1=$(R rev-parse f1)
R merge -q --ff-only f1; R branch -q -D f1
lay f2 remove f1 >/dev/null
check "a fast-forward, the branch gone: onto it" landed_on f2 "$f1" main

# Not merged: nothing said.
pair u1 u2
commit_on main m.txt moved-again
eq "unmerged: nothing said" "u1" "$(listed u2)"

# Merged in a way detection can't tell: layer remove moves its own
# commits onto main's tip.
pair v1 v2
R merge -q --squash v1 >/dev/null; echo rewritten >"$REPO/v1.txt"; R commit -qam "v1, rewritten"
eq "a squash rewritten when merged: not found" "v1" "$(listed v2)"
lay v2 remove v1 >/dev/null
eq "layer remove: its own commit onto main's tip, built on nothing" "$(R rev-parse main)|v2.txt|" \
    "$(R rev-parse v2^)|$(R diff --name-only main v2)|$(editable v2)"
}

a_stale_local_main() {
# The trunk is origin/main; the local main behind it.
git init -q --bare "$T/origin.git"; R remote add origin "$T/origin.git"
R push -q -u origin main; R remote set-head origin main
R branch d1; commit_on d1 d1.txt d1
"$BIN" -C "$REPO" branch d2 --edit d1 >/dev/null; commit_on d2 d2.txt d2
R switch -q --detach origin/main; echo z >"$REPO/z.txt"; R add z.txt; R commit -qm "theirs 2"
R push -q origin HEAD:main; R fetch -q origin; R switch -q main
lay d2 remove d1 >/dev/null
eq "a stale local main: onto origin/main" "$(R rev-parse origin/main)|" "$(R rev-parse d2^)|$(editable d2)"
eq "listed as on main" "Your commits sit on main." "$(lay d2 2>/dev/null | grep 'sit on' || echo 'Your commits sit on main.')"
# No local main at all.
R branch d3; commit_on d3 d3.txt d3
"$BIN" -C "$REPO" branch d4 --edit d3 >/dev/null; commit_on d4 d4.txt d4
R switch -q --detach main; R branch -q -D main
check "no local main: layer remove goes onto origin/main" lay d4 remove d3
eq "only its own commit, on origin/main" "$(R rev-parse origin/main)|d4.txt" "$(R rev-parse d4^)|$(R diff --name-only origin/main d4)"
check "main as an editable layer: refused" fails lay d4 add origin/main --edit
}

a_landed_layer_in_an_open_workspace() {
R branch h1; commit_on h1 h1.txt h1
"$BIN" -C "$REPO" branch h2 --edit h1 >/dev/null
mount_fs
W=$MNT/h2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "h2 own"
echo mine >>"$W/a.txt"
commit_on main m.txt moved-on
s=$(squash h1)
tip=$(R rev-parse h2)
eq "it only looks merged, h1 still there: the hook says so, moves nothing" "gitfold: h1 looks merged into main as ${s:0:12}, but h1 is still there: \`gitfold layer remove h1\` moves h2 onto main with only its own commits. Suggest it to the user rather than running it yourself: it rewrites h2's history.|$tip" \
    "$(prompt_hook "$W")|$(R rev-parse h2)"
eq "once" "" "$(prompt_hook "$W")"
R branch -q -D h1
out=$(prompt_hook "$W")
eq "h1 gone: the hook moves it onto the squash, and says so" yes \
    "$(has "$out" "gitfold: this branch, h2, h1 looks merged into main as ${s:0:12}; it was moved there with only its own commits, and now sits on main." && echo yes)"
check "on the squash, built on nothing" landed_on h2 "$s" main
eq "the listing: on main" "Your commits sit on main." "$("$BIN" -C "$W" layer | grep 'sit on')"
eq "list: not under another" "h2" "$(tree | grep h2)"
eq "main's file shows, own work kept" "moved-on| M a.txt" "$(cat "$W/m.txt")|$(git -C "$W" status --short)"
commit_on main later.txt later
eq "main moving on: the hook leaves it" "|$(R rev-parse h2)" "$(prompt_hook "$W")|$(R rev-parse h2)"

# A merge commit too is only said while the branch is there; meanwhile
# the workspace follows it as before.
R branch j1; commit_on j1 j1.txt j1
"$BIN" -C "$W" layer add j1 --edit >/dev/null
R merge -q --no-ff --no-edit j1
tip=$(R rev-parse h2)
out=$(prompt_hook "$W")
eq "merged, j1 still there: said, nothing moved" "yes|$tip" \
    "$(grep -qF "j1 was merged into main by $(R rev-parse --short=12 main), but j1 is still there" <<<"$out" && echo yes)|$(R rev-parse h2)"
check "list says it" has "$("$BIN" -C "$REPO" list)" "j1 merged into main: \`gitfold layer remove j1\` moves it there"
commit_on j1 j1b.txt more
eq "j1 goes on: the hook follows it" "yes|$(R rev-parse j1)" \
    "$(prompt_hook "$W" | grep -q 'was moved onto j1' && echo yes)|$(R rev-parse h2^)"
R merge -q --no-ff --no-edit j1; R branch -q -D j1
prompt_hook "$W" >/dev/null
check "j1 gone: moved onto the merge" landed_on h2 "$(R rev-parse main)" main
}

# The next gitfold command moving branch $1 dies (SIGKILL) right after the
# branch moved: a reference-transaction hook kills git's parent, then
# removes itself.
crash_after_move() {
    local hook=$COMMON/hooks/reference-transaction
    mkdir -p "$COMMON/hooks"
    printf '#!/bin/sh\n[ "$1" = committed ] || exit 0\ngrep -q " refs/heads/%s$" || exit 0\nrm -f "$0"\nkill -9 $(awk "{print \\$4}" /proc/$PPID/stat)\n' \
        "$1" >"$hook"
    chmod +x "$hook"
}

# A move that changes what a branch builds on, killed right after the
# branch moved: the layers it was to have settle, never moved back.
an_interrupted_move() {
R branch a2; commit_on a2 a2.txt a2
R branch w2; commit_on w2 w2.txt w2
"$BIN" -C "$REPO" branch z --edit a2 >/dev/null; commit_on z z.txt z
crash_after_move z
lay z remove a2 >/dev/null 2>&1
eq "killed: moved onto main, the next layers recorded" "$(R rev-parse main)|yes" \
    "$(R rev-parse z^)|$(test -e "$COMMON/gitfold/z/layers.next" && echo yes)"
eq "the listing shows it as it will be" "Your commits sit on main." "$(lay z 2>/dev/null | grep 'sit on' || echo none)"
check "the next command settles it" lay z add w2 --edit
eq "on w2, built on it alone, nothing pending" "$(R rev-parse w2)|w2|no" \
    "$(R rev-parse z^)|$(editable z)|$(test -e "$COMMON/gitfold/z/layers.next" && echo yes || echo no)"
# Recorded, then killed before anything moved: forgotten.
"$BIN" -C "$REPO" branch x --edit a2 >/dev/null; commit_on x x.txt x
printf 'onto %s\n' "$(R rev-parse w2)" >"$COMMON/gitfold/x/layers.next"
check "the next command forgets it" lay x add a2 --edit
eq "still on a2" "a2|$(R rev-parse a2)|no" \
    "$(editable x)|$(R rev-parse x^)|$(test -e "$COMMON/gitfold/x/layers.next" && echo yes || echo no)"
}

# A move of a workspace showing a see-only layer, killed half-way: the
# move is finished, the layer shown over it.
an_interrupted_move_with_layers() {
R branch p1; R branch la
"$BIN" -C "$REPO" branch p2 --edit p1 >/dev/null
mount_fs
W=$MNT/p2 S=$COMMON/gitfold/p2
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm "p2 own"
echo la >>"$MNT/la/a.txt"
"$BIN" -C "$W" layer add la >/dev/null
echo n >"$MNT/p1/n.txt"; G "$MNT/p1" add n.txt; G "$MNT/p1" commit -qm "p1 n"
# Mounted, the daemon moves it: the daemon dies.
crash_after_move p2
lay p2 add p1 --edit >/dev/null 2>&1
check "killed: the branch moved" parent_is p2 p1
eq "the intent left" "yes" "$(test -e "$S/move" && echo yes || echo no)"
unmount_fs
mount_fs
cat "$W/own.txt" >/dev/null
for _ in $(seq 50); do [ ! -e "$S/move" ] && break; sleep 0.1; done
eq "mounted again: the move finished" "no|$(R rev-parse p1)" \
    "$(test -e "$S/move" && echo yes || echo no)|$(own_ref p2)"
for _ in $(seq 30); do [ "$(git -C "$W" status --short)" = "" ] && break; sleep 0.1; done
eq "the layer shows, the new file too, git sees only its own" "a la|n|" \
    "$(paste -sd' ' "$W/a.txt")|$(cat "$W/n.txt")|$(git -C "$W" status --short)"

# Unmounted: left half-way until it opens.
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
# p1 gets a commit by plumbing (its workspace has it checked out).
export GIT_INDEX_FILE=$T/p1.index
R read-tree p1; R update-index --add --cacheinfo "100644,$(echo n2 | R hash-object -w --stdin),n2.txt"
R update-ref refs/heads/p1 "$(R commit-tree "$(R write-tree)" -p p1 -m "p1 n2")"
unset GIT_INDEX_FILE
crash_after_move p2
lay p2 add p1 --edit >/dev/null 2>&1
eq "killed unmounted: moved, the intent left" "$(R rev-parse p1)|yes" \
    "$(R rev-parse p2^)|$(test -e "$S/move" && echo yes || echo no)"
mount_fs
cat "$W/own.txt" >/dev/null
for _ in $(seq 50); do [ ! -e "$S/move" ] && break; sleep 0.1; done
eq "opened: finished" "no|$(R rev-parse p1)" "$(test -e "$S/move" && echo yes || echo no)|$(own_ref p2)"
eq "the layer, the new file, git clean" "a la|n2|" "$(paste -sd' ' "$W/a.txt")|$(cat "$W/n2.txt")|$(git -C "$W" status --short)"
}

# A move of a workspace whose union file shows a see-only layer's lines,
# the editable layer's new commit changing that file too: own lines carried
# over, HEAD at a new layers commit, commits still go onto the branch.
a_move_with_union_files() {
printf 'reg.txt merge=union\n' >"$REPO/.gitattributes"; printf 'core\n' >"$REPO/reg.txt"
R add -A; R commit -qm reg
R branch p1; R branch la
"$BIN" -C "$REPO" branch p2 --edit p1 >/dev/null
mount_fs
W=$MNT/p2
ls "$W" >/dev/null; prompt_hook "$W" >/dev/null
printf 'core\nla\n' >"$MNT/la/reg.txt"
"$BIN" -C "$W" layer add la >/dev/null
printf 'core\nla\nmine\n' >"$W/reg.txt"
eq "the union: the layer's line and mine" "core la mine" "$(paste -sd' ' "$W/reg.txt")"
printf 'p1\ncore\n' >"$MNT/p1/reg.txt"; G "$MNT/p1" commit -qam "p1 reg"
out=$(prompt_hook "$W")
eq "the hook moves it" yes "$(grep -q 'was moved onto p1' <<<"$out" && echo yes)"
eq "on p1" "$(R rev-parse p1)" "$(R rev-parse p2^0)"
eq "the union over p1's: every line, mine kept" "p1 core la mine" "$(paste -sd' ' "$W/reg.txt")"
eq "git sees only my line" "+mine" "$(git -C "$W" diff | grep '^+[^+]')"
G "$W" commit -qam "p2 mine"
for _ in $(seq 30); do [ "$(R show p2:reg.txt | paste -sd' ')" = "p1 core mine" ] && break; sleep 0.1; done
eq "committed onto the branch with only my line" "p1 core mine|$(R rev-parse p1)" \
    "$(R show p2:reg.txt | paste -sd' ')|$(R rev-parse p2^)"
}

section "moves: branches with no workspace" branches_with_no_workspace
section "moves: union files" a_move_with_union_files
section "moves: an interrupted move" an_interrupted_move
section "moves: an interrupted move with layers" an_interrupted_move_with_layers
section "moves: a layer that landed" a_layer_that_landed
section "moves: a stale local main" a_stale_local_main
section "moves: a landed layer, an open workspace" a_landed_layer_in_an_open_workspace
section "moves: an open workspace" an_open_workspace
section "moves: layers, an unmounted workspace" layers_or_an_unmounted_workspace
finish
