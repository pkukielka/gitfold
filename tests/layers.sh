#!/usr/bin/env bash
# End-to-end test of layers: workspaces' uncommitted work as commits (seal),
# merged into snapshots (mix).
# usage: tests/layers.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=layers
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

seal_uncommitted_work_as_a_commit() {
R branch s1
check "no workspace yet: nothing to seal (exit 3)" sh -c "'$BIN' -C '$REPO' seal s1; [ \$? = 3 ]"
mount_fs
echo s >>"$MNT/s1/a.txt"; echo n >"$MNT/s1/new.txt"; rm "$MNT/s1/d/b.txt"
ln -s a.txt "$MNT/s1/link"; echo x >"$MNT/s1/run.sh"; chmod +x "$MNT/s1/run.sh"
echo '*.o' >>"$MNT/s1/.gitignore"; echo o >"$MNT/s1/build.o"
c=$("$BIN" -C "$MNT/s1" seal)
eq "the parent is the base" "$(R rev-parse s1)" "$(R rev-parse "$c^")"
eq "the tree is what git would commit with add -A" \
    "$(G "$MNT/s1" add -A && G "$MNT/s1" write-tree)" "$(R rev-parse "$c^{tree}")"
eq "ignored files left out" "" "$(R ls-tree --name-only "$c" build.o)"
eq "kept by a ref" "$c" "$(R rev-parse refs/gitfold-seal/s1/1)"
eq "unchanged work: the same commit, no new ref" "$c 1" \
    "$("$BIN" -C "$REPO" seal s1) $(R for-each-ref refs/gitfold-seal/s1 | wc -l)"
echo more >>"$MNT/s1/a.txt"
c2=$("$BIN" -C "$REPO" seal s1)
check "new work: a new seal" test "$c2" != "$c"
eq "numbered on" "$c2" "$(R rev-parse refs/gitfold-seal/s1/2)"
eq "the workspace unchanged" "A	.gitignore
M	a.txt
D	d/b.txt
A	link
A	new.txt
A	run.sh" "$("$BIN" -C "$REPO" changes s1)"
eq "opens as a snapshot" "a
s
more" "$(cat "$MNT/@$c2/a.txt")"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
eq "unmounted: the same seal" "$c2" "$("$BIN" -C "$REPO" seal s1)"
}

mix_workspaces_into_a_snapshot() {
R branch m1; R branch m2; R branch m3
mount_fs
echo one >>"$MNT/m1/a.txt"; echo n1 >"$MNT/m1/n1.txt"
echo two >>"$MNT/m2/d/c.txt"; rm "$MNT/m2/d/b.txt"
p=$("$BIN" -C "$REPO" mix m1 m2)
eq "printed as a snapshot path" "$MNT/@" "${p%@*}@"
eq "both workspaces' work" "a
one|c
two|n1" "$(cat "$p/a.txt")|$(cat "$p/d/c.txt")|$(cat "$p/n1.txt")"
check "a deletion carried" test ! -e "$p/d/b.txt"
eq "the same work: the same snapshot" "$p" "$("$BIN" -C "$REPO" mix m1 m2)"
eq "--commit prints the commit" "${p##*@}" "$("$BIN" -C "$REPO" mix --commit m1 m2)"
echo three >>"$MNT/m3/a.txt"
out=$("$BIN" -C "$REPO" mix m1 m3 2>&1); rc=$?
eq "conflicts: exit 4, the paths listed" "4 a.txt" "$rc $(echo "$out" | sed -n 's/^  //p')"
R switch -q main; echo top >"$REPO/top.txt"; R add top.txt; R commit -qm top
p=$("$BIN" -C "$REPO" mix --onto main m2)
eq "--onto: onto another revision" "top|c
two" "$(cat "$p/top.txt")|$(cat "$p/d/c.txt")"
eq "a branch without a workspace: its tip" "top" \
    "$(cat "$("$BIN" -C "$REPO" mix m1 main)/top.txt")"
check "neither a workspace nor a revision" fails "$BIN" -C "$REPO" mix m1 nope
eq "nothing changed in the workspaces" "M	a.txt
A	n1.txt" "$("$BIN" -C "$REPO" changes m1)"
}

section "seal: uncommitted work as a commit" seal_uncommitted_work_as_a_commit
section "mix: workspaces merged into a snapshot" mix_workspaces_into_a_snapshot
finish
