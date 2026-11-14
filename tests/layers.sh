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

layers_shown_in_a_workspace() {
R branch me; R branch la; R branch lb
mount_fs
W=$MNT/me
echo "la" >>"$MNT/la/a.txt"; echo nla >"$MNT/la/nla.txt"; rm "$MNT/la/d/b.txt"
mkdir "$MNT/la/nd"; echo f >"$MNT/la/nd/f"
echo "lb" >>"$MNT/lb/a.txt"; echo lb >"$MNT/lb/d/c.txt"
echo mine >>"$W/d/c.txt"
cat "$W/a.txt" "$W/d/b.txt" >/dev/null   # cached by the kernel before the switch
check "add a layer" "$BIN" -C "$W" layer add la
eq "its changes show" "a
la|nla|f" "$(cat "$W/a.txt")|$(cat "$W/nla.txt")|$(cat "$W/nd/f")"
check "its deletions too" test ! -e "$W/d/b.txt"
eq "own changes win" "c
mine" "$(cat "$W/d/c.txt")"
eq "listed" "a.txt d nd nla.txt" "$(ls "$W" | tr '\n' ' ' | sed 's/ $//')"
check "a layer's file has no write bits" test ! -w "$W/a.txt"
check "writing it is refused" fails sh -c "echo x >>'$W/a.txt'"
check "removing it is refused" fails rm -f "$W/nla.txt"
check "renaming it is refused" fails mv "$W/a.txt" "$W/z.txt"
check "renaming a directory holding one is refused" fails mv "$W/nd" "$W/nd2"
check "chmod is refused" fails chmod +x "$W/nla.txt"
check "own files stay writable" sh -c "echo more >>'$W/d/c.txt' && echo n >'$W/own.txt'"
eq "git sees only own changes" " M d/c.txt
?? own.txt" "$(git -C "$W" status --short)"
check "creating where a layer deleted is refused" fails sh -c "echo x >'$W/d/b.txt'"
eq "gitfold still finds the workspace" "on 	la" "$("$BIN" -C "$W/d" layer list | cut -c1-6)"
check "a second layer on top" "$BIN" -C "$W" layer add lb
eq "the higher layer wins a path" "a
lb" "$(cat "$W/a.txt")"
check "hide it" "$BIN" -C "$W" layer off lb
eq "the lower one shows again" "a
la" "$(cat "$W/a.txt")"
check "lens off" "$BIN" -C "$W" lens off
eq "the base again, own changes kept" "a|b|c
mine
more|n" "$(cat "$W/a.txt")|$(cat "$W/d/b.txt")|$(cat "$W/d/c.txt")|$(cat "$W/own.txt")"
check "the layer's new files gone" test ! -e "$W/nla.txt" -a ! -e "$W/nd"
eq "git back, seeing only own changes" " M d/c.txt
?? own.txt" "$(git -C "$W" status --short)"
echo again >>"$MNT/la/a.txt"
check "layer on again" "$BIN" -C "$W" layer on la
eq "still the layer's work as added" "a
la" "$(cat "$W/a.txt")"
check "update: the layer's work as now" "$BIN" -C "$W" layer update
eq "the newer work shows" "a
la
again" "$(cat "$W/a.txt")"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
mount_fs
eq "shown again after a remount" "a
la
again" "$(cat "$W/a.txt")"
eq "and git still sees only own changes" " M d/c.txt
?? own.txt" "$(git -C "$W" status --short)"
check "commit -a" G "$W" commit -qam own
eq "it holds only own changes" "d/c.txt" "$(G "$W" show --name-only --format= HEAD)"
eq "nothing left after it" "?? own.txt" "$(git -C "$W" status --short)"
R config core.sparseCheckout true
check "sparse checkout: layers still switch" "$BIN" -C "$W" layer update
out=$(git -C "$W" status 2>&1); rc=$?
eq "but git is off, saying why (exit 128)" "128 yes" "$rc $(echo "$out" | grep -q 'LENS ON (la): git is off here (sparse checkout is on)' && echo yes)"
R config --unset core.sparseCheckout
check "rm the layer" "$BIN" -C "$W" layer rm la
check "rm the other" "$BIN" -C "$W" layer rm lb
eq "no layers: git back" "?? own.txt" "$(git -C "$W" status --short)"
for _ in $(seq 50); do [ "$(cat "$COMMON/gitfold/me/base")" = "$(R rev-parse me)" ] && break; sleep 0.1; done
eq "the base caught up with the commit" "$(R rev-parse me)" "$(cat "$COMMON/gitfold/me/base")"
check "an unknown layer (exit 3)" sh -c "'$BIN' -C '$W' layer off nope; [ \$? = 3 ]"
}

section "layers: shown in a workspace" layers_shown_in_a_workspace
section "seal: uncommitted work as a commit" seal_uncommitted_work_as_a_commit
section "mix: workspaces merged into a snapshot" mix_workspaces_into_a_snapshot
finish
