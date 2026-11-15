#!/usr/bin/env bash
# End-to-end test of layers (other branches' work shown in a workspace,
# proposals to change it) and of snapshots of workspaces' work (snapshot).
# usage: tests/layers.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=layers
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

snapshot_of_one_workspace() {
R branch s1; R branch s2
out=$("$BIN" -C "$REPO" snapshot s1 2>&1 >/dev/null)
eq "unmounted, no workspace yet: the branch's tip, as a commit" "$(R rev-parse s1)|yes" \
    "$("$BIN" -C "$REPO" snapshot s1 2>/dev/null)|$(echo "$out" | grep -q '^not mounted' && echo yes)"
mount_fs
echo s >>"$MNT/s1/a.txt"; echo n >"$MNT/s1/new.txt"; rm "$MNT/s1/d/b.txt"
ln -s a.txt "$MNT/s1/link"; echo x >"$MNT/s1/run.sh"; chmod +x "$MNT/s1/run.sh"
echo '*.o' >>"$MNT/s1/.gitignore"; echo o >"$MNT/s1/build.o"
p=$("$BIN" -C "$MNT/s1" snapshot)
eq "printed as a snapshot path" "$MNT/@" "${p%@*}@"
c=${p##*@}
eq "the parent is the base" "$(R rev-parse s1)" "$(R rev-parse "$c^")"
eq "the tree is what git would commit with add -A" \
    "$(G "$MNT/s1" add -A && G "$MNT/s1" write-tree)" "$(R rev-parse "$c^{tree}")"
eq "ignored files left out" "" "$(R ls-tree --name-only "$c" build.o)"
eq "kept by a ref" "$c" "$(R rev-parse refs/gitfold-snapshot/s1/1)"
out=$("$BIN" -C "$REPO" snapshot s1 2>&1 >/dev/null)
eq "unchanged work: the same snapshot, no new ref, said so" "$p 1|unchanged since its last snapshot" \
    "$("$BIN" -C "$REPO" snapshot s1 2>/dev/null) $(R for-each-ref refs/gitfold-snapshot/s1 | wc -l)|$out"
echo more >>"$MNT/s1/a.txt"
p2=$("$BIN" -C "$REPO" snapshot s1)
check "new work: a new snapshot" test "$p2" != "$p"
eq "numbered on" "${p2##*@}" "$(R rev-parse refs/gitfold-snapshot/s1/2)"
eq "the workspace unchanged" "A	.gitignore
M	a.txt
D	d/b.txt
A	link
A	new.txt
A	run.sh" "$("$BIN" -C "$REPO" changes s1)"
eq "opens as a folder" "a
s
more" "$(cat "$p2/a.txt")"
eq "--commits: only its commits" "$MNT/@$(R rev-parse s1)" "$("$BIN" -C "$REPO" snapshot --commits s1)"
echo again >>"$MNT/s1/a.txt"
eq "the stop hook: nothing printed, nothing new while no workspace shows it" "|2" \
    "$(cd "$MNT/s1" && "$BIN" snapshot --claude-hook stop)|$(R for-each-ref refs/gitfold-snapshot/s1 | wc -l)"
ls "$MNT/s2" >/dev/null; "$BIN" -C "$REPO" layer add s1 -w s2 >/dev/null
echo last >>"$MNT/s1/a.txt"
(cd "$MNT/s1" && "$BIN" snapshot --claude-hook stop)
newest=$(R for-each-ref --sort=-refname --count=1 --format='%(refname)' refs/gitfold-snapshot/s1)
eq "the stop hook: a new one once a layer shows it" "last" "$(R show "$newest:a.txt" | tail -1)"
check "the hook takes no branch" fails "$BIN" -C "$REPO" snapshot s1 --claude-hook stop
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
eq "unmounted: the same work, as a commit" "$(R rev-parse "$newest")" \
    "$("$BIN" -C "$REPO" snapshot s1 2>/dev/null)"
}

snapshot_of_several_merged() {
R branch m1; R branch m2; R branch m3
mount_fs
echo one >>"$MNT/m1/a.txt"; echo n1 >"$MNT/m1/n1.txt"
echo two >>"$MNT/m2/d/c.txt"; rm "$MNT/m2/d/b.txt"
p=$("$BIN" -C "$REPO" snapshot m1 m2)
eq "printed as a snapshot path" "$MNT/@" "${p%@*}@"
eq "both workspaces' work" "a
one|c
two|n1" "$(cat "$p/a.txt")|$(cat "$p/d/c.txt")|$(cat "$p/n1.txt")"
check "a deletion carried" test ! -e "$p/d/b.txt"
eq "the same work: the same snapshot" "$p" "$("$BIN" -C "$REPO" snapshot m1 m2)"
echo three >>"$MNT/m3/a.txt"
out=$("$BIN" -C "$REPO" snapshot m1 m3 2>&1); rc=$?
eq "conflicts: exit 4, the paths listed, what to do instead" "4 a.txt|yes" \
    "$rc $(echo "$out" | sed -n 's/^  //p')|$(echo "$out" | grep -qx '`gitfold snapshot m1` and `gitfold snapshot m3` show each alone' && echo yes)"
G "$MNT/m3" commit -qam three
p=$("$BIN" -C "$REPO" snapshot --commits m1 m3)
eq "--commits: only commits, no uncommitted work" "a
three|" "$(cat "$p/a.txt")|$(ls "$p/n1.txt" 2>/dev/null)"
R switch -q main; echo top >"$REPO/top.txt"; R add top.txt; R commit -qm top
p=$("$BIN" -C "$REPO" snapshot --onto main m2 2>/dev/null)
eq "--onto: onto another revision" "top|c
two" "$(cat "$p/top.txt")|$(cat "$p/d/c.txt")"
eq "a branch without a workspace: its tip" "top" \
    "$(cat "$("$BIN" -C "$REPO" snapshot m1 main)/top.txt")"
out=$("$BIN" -C "$REPO" snapshot m1 nope 2>&1); rc=$?
eq "neither a branch nor a revision (exit 3)" "3 Error: no branch or revision nope" "$rc $out"
eq "nothing changed in the workspaces" "M	a.txt
A	n1.txt" "$("$BIN" -C "$REPO" changes m1)"
}

# A snapshot of workspace $1's work, as its agent's Stop hook makes one: its
# commit.
snap() { "$BIN" -C "$REPO" snapshot "$1" 2>/dev/null | sed "s/.*@//"; }
# The layers the listing of workspace $2 (or the one $1 is in) names, on one
# line.
layer_names() { "$BIN" -C "$1" layer ${2:+-w "$2"} | awk '/^LAYER/{t=1;next} t&&(!NF||/^Your /){exit} t{print $1}' | paste -sd' '; }
# The listing, its times as HH:MM and its columns' spaces squeezed.
listing() { "$BIN" -C "$1" layer | sed -E 's/[0-9]{2}:[0-9]{2}/HH:MM/g' | tr -s ' '; }
# Claude Code's Edit of file $2 in directory $1, through the deny hook.
edit_hook() { printf '{"tool_name":"Edit","cwd":"%s","tool_input":{"file_path":"%s"}}' "$1" "$2" |
    "$BIN" layer --claude-hook pre-tool-use; }

layers_shown_in_a_workspace() {
R branch me; R branch la; R branch lb
mount_fs
W=$MNT/me
echo "la" >>"$MNT/la/a.txt"; echo nla >"$MNT/la/nla.txt"; rm "$MNT/la/d/b.txt"
mkdir "$MNT/la/nd"; echo f >"$MNT/la/nd/f"
echo lb >"$MNT/lb/lb.txt"
echo mine >>"$W/d/c.txt"
cat "$W/a.txt" "$W/d/b.txt" >/dev/null   # cached by the kernel before the switch
out=$("$BIN" -C "$W" layer add la); rc=$?
eq "add a layer: a confirmation, then what the daemon did" "0|layer la added: all work, live, last HH:MM|me: layers shown" \
    "$rc|$(echo "$out" | sed -n 1p | sed -E 's/[0-9]{2}:[0-9]{2}/HH:MM/')|$(echo "$out" | sed -n 2p | sed 's/ (.*//')"
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
out=$(edit_hook "$W" a.txt)
eq "Claude Code's Edit of a layer's file: denied, saying whose and what to do" "deny yes yes yes" \
    "$(echo "$out" | sed -n 's/.*"permissionDecision":"\([a-z]*\)".*/\1/p') $(echo "$out" | grep -q "a.txt shows la's work" && echo yes) $(echo "$out" | grep -q 'gitfold send la <text>' && echo yes) $(echo "$out" | grep -q 'gitfold layer remove la' && echo yes)"
eq "of its own file: allowed" "" "$(edit_hook "$W" "$W/d/c.txt")"
eq "outside a workspace: allowed" "" "$(edit_hook "$W" /tmp/x)"
eq "git sees only own changes" " M d/c.txt
?? own.txt" "$(git -C "$W" status --short)"
check "creating where a layer deleted is refused" fails sh -c "echo x >'$W/d/b.txt'"
eq "gitfold still finds the workspace" "la" "$(layer_names "$W/d")"
check "a second layer" "$BIN" -C "$W" layer add lb
eq "both show" "a
la|lb" "$(cat "$W/a.txt")|$(cat "$W/lb.txt")"
check "remove --all" "$BIN" -C "$W" layer remove --all
eq "the base again, own changes kept" "a|b|c
mine
more|n" "$(cat "$W/a.txt")|$(cat "$W/d/b.txt")|$(cat "$W/d/c.txt")|$(cat "$W/own.txt")"
check "the layers' new files gone" test ! -e "$W/nla.txt" -a ! -e "$W/nd" -a ! -e "$W/lb.txt"
eq "git back, seeing only own changes" " M d/c.txt
?? own.txt" "$(git -C "$W" status --short)"
echo again >>"$MNT/la/a.txt"
check "add it again, frozen" "$BIN" -C "$W" layer add la --frozen
eq "the layer's work as now" "a
la
again" "$(cat "$W/a.txt")"
echo later >>"$MNT/la/a.txt"; snap la >/dev/null
(cd "$W" && "$BIN" layer --claude-hook user-prompt-submit >/dev/null)
eq "frozen: the next turn doesn't move it" "a
la
again" "$(cat "$W/a.txt")"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
mount_fs
eq "shown again after a remount" "a
la
again" "$(cat "$W/a.txt")"
eq "and git still sees only own changes" " M d/c.txt
?? own.txt" "$(git -C "$W" status --short)"
G "$W" update-index --no-skip-worktree a.txt
for _ in $(seq 50); do G "$W" ls-files -v a.txt | grep -q '^S' && break; sleep 0.1; done
eq "a skip-worktree bit git dropped is put back" "S a.txt" "$(G "$W" ls-files -v a.txt)"
check "commit -a" G "$W" commit -qam own
eq "it holds only own changes" "d/c.txt" "$(G "$W" show --name-only --format= HEAD)"
eq "nothing left after it" "?? own.txt" "$(git -C "$W" status --short)"
R config core.sparseCheckout true
check "sparse checkout: adding again still switches" "$BIN" -C "$W" layer add la --frozen
eq "to the layer's work as now" "later" "$(tail -1 "$W/a.txt")"
out=$(git -C "$W" status 2>&1); rc=$?
eq "but git is off, saying why (exit 128)" "128 yes" "$rc $(echo "$out" | grep -q 'LAYERS SHOWN (la): git is off here (sparse checkout is on). Run gitfold layer remove --all' && echo yes)"
R config --unset core.sparseCheckout
check "remove the layer" "$BIN" -C "$W" layer remove la
eq "no layers: git back" "?? own.txt" "$(git -C "$W" status --short)"
for _ in $(seq 50); do [ "$(cat "$COMMON/gitfold/me/base")" = "$(R rev-parse me)" ] && break; sleep 0.1; done
eq "the base caught up with the commit" "$(R rev-parse me)" "$(cat "$COMMON/gitfold/me/base")"
check "removing an unknown layer (exit 3)" sh -c "'$BIN' -C '$W' layer remove nope; [ \$? = 3 ]"
check "remove names a layer or --all (exit 2)" sh -c "'$BIN' -C '$W' layer remove; [ \$? = 2 ]"
admin=$COMMON/worktrees/gitfold-ws-me
touch "$admin/index.lock"
out=$("$BIN" -C "$W" layer add la 2>&1); rc=$?
rm -f "$admin/index.lock"
eq "git busy: refused (exit 4), nothing stored" "4 No layers." "$rc $("$BIN" -C "$W" layer | head -1)"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
eq "unmounted: stored, shown when it opens" "me: not mounted; its layers show when it opens" \
    "$("$BIN" -C "$REPO" layer add la -w me | tail -1)"
mount_fs
for _ in $(seq 50); do [ "$(sed -n 2p "$W/a.txt" 2>/dev/null)" = la ] && break; sleep 0.1; done
eq "shown once mounted" "a
la
again
later" "$(cat "$W/a.txt")"
eq "and git told: only own changes" "?? own.txt" "$(git -C "$W" status --short)"
}

layers_live_frozen_commits_and_revisions() {
R branch me; R branch live; R branch frozen; R branch commits; R branch plain
R switch -q plain; echo p >"$REPO/p.txt"; R add p.txt; R commit -qm p
R switch -q -c tagged main; echo t >"$REPO/t.txt"; R add t.txt; R commit -qm t; R tag v1.2
R switch -q main; R branch -q -D tagged
mount_fs
W=$MNT/me
echo live >"$MNT/live/live.txt"; echo fz >"$MNT/frozen/frozen.txt"
echo cm >"$MNT/commits/cm.txt"; G "$MNT/commits" add cm.txt; G "$MNT/commits" commit -qm cm
echo uncommitted >"$MNT/commits/cu.txt"
ls "$W" >/dev/null; turn_starts "$W" >/dev/null   # an agent's: its layers move at its turns
check "add: live, --frozen, --commits, a branch without a workspace, a tag" sh -c "
    '$BIN' -C '$W' layer add live && '$BIN' -C '$W' layer add frozen --frozen &&
    '$BIN' -C '$W' layer add commits --commits && '$BIN' -C '$W' layer add plain &&
    '$BIN' -C '$W' layer add v1.2"
eq "each one's work shows" "live|fz|cm|no|p|t" \
    "$(cat "$W/live.txt")|$(cat "$W/frozen.txt")|$(cat "$W/cm.txt")|$(test -e "$W/cu.txt" && echo yes || echo no)|$(cat "$W/p.txt")|$(cat "$W/t.txt")"
eq "the listing: what each shows and how it updates, then what your commits sit on" "LAYER SHOWS UPDATES YOUR COMMITS
live all work live, last HH:MM
frozen all work frozen at HH:MM
commits commits only live, last HH:MM
plain commits live, last HH:MM
v1.2 commits fixed
Your commits sit on main." "$(listing "$W")"
echo live2 >>"$MNT/live/live.txt"; echo fz2 >>"$MNT/frozen/frozen.txt"
snap live >/dev/null; snap frozen >/dev/null
echo cm2 >"$MNT/commits/cm2.txt"; G "$MNT/commits" add cm2.txt; G "$MNT/commits" commit -qm cm2
R switch -q plain; echo p2 >>"$REPO/p.txt"; R commit -qam p2; R switch -q main
out=$(cd "$W" && "$BIN" layer --claude-hook user-prompt-submit)
eq "the next turn moves the live ones, saying so" "yes" \
    "$(echo "$out" | grep -q 'moved to their latest work: live (1 file changed), commits (1 file changed), plain (1 file changed)' && echo yes)"
eq "a workspace: its newest snapshot; a branch: its tip; frozen stays" "live
live2|cm2|p
p2|fz" "$(cat "$W/live.txt")|$(cat "$W/cm2.txt")|$(cat "$W/p.txt")|$(cat "$W/frozen.txt")"
eq "a quiet turn says nothing" "" "$(cd "$W" && "$BIN" layer --claude-hook user-prompt-submit)"
check "add the frozen one again, without --frozen" "$BIN" -C "$W" layer add frozen
eq "replaced in place: live, its work as now" "frozen all work live, last HH:MM|fz
fz2" "$(listing "$W" | grep '^frozen')|$(cat "$W/frozen.txt")"
out=$("$BIN" -C "$W" layer add live --frozen)
eq "and a live one --frozen" "layer live updated: all work, frozen at HH:MM|live all work frozen at HH:MM" \
    "$(echo "$out" | head -1 | sed -E 's/[0-9]{2}:[0-9]{2}/HH:MM/')|$(listing "$W" | grep '^live')"
check "--commits dropped: all its work" "$BIN" -C "$W" layer add commits
eq "uncommitted included" "uncommitted" "$(cat "$W/cu.txt")"
eq "still five layers, in the order added" "live frozen commits plain v1.2" "$(layer_names "$W")"
check "remove one" "$BIN" -C "$W" layer remove plain
eq "gone" "live frozen commits v1.2|no" "$(layer_names "$W")|$(test -e "$W/p.txt" && echo yes || echo no)"
check "neither a branch nor a revision (exit 3)" sh -c "'$BIN' -C '$W' layer add nope; [ \$? = 3 ]"
check "remove --all" "$BIN" -C "$W" layer remove --all
eq "no layers" "No layers.
Your commits sit on main." "$("$BIN" -C "$W" layer)"
R branch api plain
echo own >"$W/own.txt"; G "$W" add own.txt; G "$W" commit -qm own
out=$("$BIN" -C "$W" layer add api --edit)
eq "built on api: says what it moves" "moves 1 commit of me onto api" "$(head -1 <<<"$out")"
eq "the listing: built on, your commits" "api commits live, last HH:MM built on: your 1 commit sits on it" \
    "$(listing "$W" | grep '^api')"
check "and not on main" fails grep -q "sit on main" <<<"$(listing "$W")"
check "a see-only layer next to it" "$BIN" -C "$W" layer add live
check "editable layers stored as such" grep -qP '^layer api\t.*\tedit$' "$COMMON/gitfold/me/layers"
R switch -q api; echo two >"$REPO/api2.txt"; R add api2.txt; R commit -qm "api: two"; R switch -q main
eq "api moved on: needs a move" "Note: needs update: its next turn, or \`gitfold layer add <LAYER> --edit\`" \
    "$(listing "$W" | grep '^Note')"
check "moved onto api's new commit with the layer shown" "$BIN" -C "$W" layer add api --edit
eq "on api's tip, with its own commit" "$(R rev-parse api)|own.txt" "$(R rev-parse me^)|$(R diff --name-only api me)"
eq "api's new file shows, the other layer still there" "two|api live" "$(cat "$W/api2.txt")|$(layer_names "$W")"
eq "git: only its own" "" "$(git -C "$W" status --short)"
R pack-refs --all
eq "after the refs are packed, the same" "$(R rev-parse api)" "$(R rev-parse refs/gitfold-own/me)"
}

layers_changing_one_file() {
printf '1\n2\n3\n4\n5\n6\n7\n8\n' >"$REPO/n.txt"; R add n.txt; R commit -qm n
R branch me; R branch x; R branch y; R branch z
mount_fs
W=$MNT/me
printf '1 x\n2\n3\n4\n5\n6\n7\n8\n' >"$MNT/x/n.txt"
printf '1\n2\n3\n4\n5\n6\n7\n8 y\n' >"$MNT/y/n.txt"
printf '1\n2\n3\n4\n5\n6\n7\n8 z\n' >"$MNT/z/n.txt"
cat "$W/n.txt" >/dev/null
check "two layers changing one file" sh -c "'$BIN' -C '$W' layer add x && '$BIN' -C '$W' layer add y"
eq "different lines: merged" "1 x 2 3 4 5 6 7 8 y" "$(tr '\n' ' ' <"$W/n.txt" | sed 's/ $//')"
eq "no conflict listed" "" "$("$BIN" -C "$W" layer | grep Conflict)"
check "read-only" test ! -w "$W/n.txt"
eq "the deny hook names both" "yes yes" \
    "$(edit_hook "$W" n.txt | grep -q "n.txt shows the merge of x and y's work" && echo yes) $(edit_hook "$W" n.txt | grep -q 'gitfold layer remove x' && echo yes)"
out=$("$BIN" -C "$W" layer add z)
eq "a third changing the same line: conflict markers, said, naming the two that conflict" "<<<<<<< base + x + y|>>>>>>> z|yes" \
    "$(grep '^<<<<<<<' "$W/n.txt")|$(grep '^>>>>>>>' "$W/n.txt")|$(echo "$out" | grep -qF "note: Conflict: n.txt is changed by both y and z (it shows git's conflict markers)" && echo yes)"
eq "listed, naming the two that conflict (x merges)" "Conflict: n.txt is changed by both y and z" "$("$BIN" -C "$W" layer | grep Conflict)"
eq "the deny hook names them all" "yes yes" \
    "$(edit_hook "$W" n.txt | grep -q "the merge of x, y and z's work" && echo yes) $(edit_hook "$W" n.txt | grep -q 'Conflict: n.txt is changed by both y and z\.' && echo yes)"
check "still read-only" fails sh -c "echo x >>'$W/n.txt'"
eq "git sees none of it" "" "$(git -C "$W" status --short)"
rm "$MNT/x/d/b.txt"; echo yb >>"$MNT/y/d/b.txt"
check "add both again, now up to date" sh -c "'$BIN' -C '$W' layer add x && '$BIN' -C '$W' layer add y"
eq "a deletion against a change: the base's version" "b" "$(cat "$W/d/b.txt")"
eq "listed as such" "Conflict: d/b.txt is changed by both x and y (the changes can't be merged: it shows your base's version)" \
    "$("$BIN" -C "$W" layer | grep 'Conflict: d/')"
check "remove one" "$BIN" -C "$W" layer remove z
eq "the others merge again" "1 x 2 3 4 5 6 7 8 y" "$(tr '\n' ' ' <"$W/n.txt" | sed 's/ $//')"
}

layers_follow_the_workspace_as_it_is_now() {
R branch me; R branch b; R branch o1; R branch o2; R branch o3; R branch o4
mount_fs
W=$MNT/me
echo "b v1" >"$MNT/b/b.txt"; snap b >/dev/null
ls "$W" >/dev/null; turn_starts "$W" >/dev/null   # an agent's: its layers move at its turns
"$BIN" -C "$W" layer add b >/dev/null
eq "a layer of b's work" "b v1" "$(cat "$W/b.txt")"
"$BIN" -C "$REPO" remove --force b >/dev/null
(cd "$W" && "$BIN" layer --claude-hook user-prompt-submit >/dev/null)
eq "b's workspace removed: the next turn shows its commits" "no|b commits live, last HH:MM" \
    "$(test -e "$W/b.txt" && echo yes || echo no)|$(listing "$W" | grep '^b ')"
ls "$MNT/b" >/dev/null
(cd "$W" && "$BIN" layer --claude-hook user-prompt-submit >/dev/null)
eq "opened again: never the earlier workspace's snapshot" "no|b all work live, last HH:MM" \
    "$(test -e "$W/b.txt" && echo yes || echo no)|$(listing "$W" | grep '^b ')"
echo "b v2" >"$MNT/b/b2.txt"
"$BIN" -C "$REPO" remove --force b >/dev/null; ls "$MNT/b" >/dev/null
echo "b v3" >"$MNT/b/b3.txt"
out=$(cd "$W" && "$BIN" layer --claude-hook user-prompt-submit)
eq "no snapshot of it yet (no turn ended there): one taken at this turn's start" "b v3|no|yes" \
    "$(cat "$W/b3.txt")|$(test -e "$W/b2.txt" && echo yes || echo no)|$(echo "$out" | grep -q 'moved to their latest work: b (1 file changed)' && echo yes)"
for o in o1 o2 o3 o4; do echo "$o" >"$MNT/$o/$o.txt"; done
pids=(); for o in o1 o2 o3 o4; do "$BIN" -C "$W" layer add "$o" >/dev/null & pids+=($!); done; wait "${pids[@]}"
eq "layers added at once: none lost" "b o1 o2 o3 o4" "$(layer_names "$W" | tr ' ' '\n' | sort | paste -sd' ')"
eq "the daemon's message: one path, singular" "me: layers shown (1 path changed)" \
    "$("$BIN" -C "$W" layer remove o1 | sed -n 2p)"
out=$("$BIN" -C "$W" layer add main)
eq "a live layer of the branch the base came from: said when there is nothing to show yet" "yes" \
    "$(echo "$out" | head -1 | grep -q '(nothing to show yet: all of main is in your base already' && echo yes)"
R update-ref refs/remotes/origin/main main; R config gitfold.trunk origin/main
eq "the trunk a remote's branch: named as the branch" "Your commits sit on main." "$(listing "$W" | grep 'sit on')"
R config --unset gitfold.trunk
check "remove --all" "$BIN" -C "$W" layer remove --all
eq "nothing shown" "no|No layers." "$(test -e "$W/o2.txt" && echo yes || echo no)|$("$BIN" -C "$W" layer | head -1)"
}

# Claude Code's hooks in workspace directory $1: its turn ends (Stop) or
# starts (UserPromptSubmit).
turn_ends() { (cd "$1" && "$BIN" snapshot --claude-hook stop); }
turn_starts() { (cd "$1" && "$BIN" layer --claude-hook user-prompt-submit); }

layers_of_an_agent_and_of_a_person() {
R branch me; R branch ag; R branch pe
mount_fs
W=$MNT/me
ls "$W" >/dev/null; turn_starts "$W" >/dev/null   # an agent's: its layers move at its turns
# A person's workspace (no Stop hook of its own runs): no turns, so at each
# of this workspace's turn starts, as it is then.
echo "pe 1" >"$MNT/pe/pe.txt"
"$BIN" -C "$W" layer add pe >/dev/null
eq "a person's work, added" "pe 1" "$(cat "$W/pe.txt")"
for i in 2 3 4; do
    echo "pe $i" >"$MNT/pe/pe.txt"; turn_starts "$W" >/dev/null
    eq "a person's work: at every turn start, as it is then ($i)" "pe $i" "$(cat "$W/pe.txt")"
done
# A record an earlier build wrote (any snapshot's): not an agent's.
echo "$(R rev-parse main)" >"$COMMON/gitfold/pe/snapshotted"
echo "pe 5" >"$MNT/pe/pe.txt"; turn_starts "$W" >/dev/null
eq "an earlier build's record: still followed as it is now" "pe 5" "$(cat "$W/pe.txt")"

# An agent's workspace: as it was at the end of its last turn, never
# half-way through one.
echo "ag 1" >"$MNT/ag/ag.txt"
eq "its turn ends, no layer shows it: no snapshot" "|" "$(turn_ends "$MNT/ag")|$(R for-each-ref refs/gitfold-snapshot/ag)"
"$BIN" -C "$W" layer add ag >/dev/null
eq "added: as it is now (no turn of its ended while shown)" "ag 1" "$(cat "$W/ag.txt")"
echo "ag 2" >"$MNT/ag/ag.txt"; turn_starts "$W" >/dev/null
eq "its turn under way: not shown half-way" "ag 1" "$(cat "$W/ag.txt")"
turn_ends "$MNT/ag"; echo "ag 3, half-way" >"$MNT/ag/ag.txt"
out=$(turn_starts "$W")
eq "its turn ended: shown as it was then, and said" "ag 2|yes" \
    "$(cat "$W/ag.txt")|$(grep -q 'moved to their latest work: ag (1 file changed)' <<<"$out" && echo yes)"
"$BIN" -C "$W" layer add ag >/dev/null
eq "added again: its last turn's end still" "ag 2" "$(cat "$W/ag.txt")"
"$BIN" -C "$W" layer add ag --frozen >/dev/null
eq "added frozen: as it is now" "ag 3, half-way" "$(cat "$W/ag.txt")"
"$BIN" -C "$W" layer add ag >/dev/null
eq "live again: its last turn's end" "ag 2" "$(cat "$W/ag.txt")"

# One layer that can't move doesn't keep the others from moving.
echo "1111111111111111111111111111111111111111 stop" >"$COMMON/gitfold/ag/snapshotted"
echo "pe 6" >"$MNT/pe/pe.txt"; turn_starts "$W" >/dev/null
eq "a layer that can't move: the others still do" "pe 6|ag 2" "$(cat "$W/pe.txt")|$(cat "$W/ag.txt")"
turn_ends "$MNT/ag"; turn_starts "$W" >/dev/null
eq "and it moves once it can" "ag 3, half-way" "$(cat "$W/ag.txt")"

# Snapshots of one workspace at once (its Stop hook, others' turn starts):
# each gets a number of its own, none fails.
pids=()
for i in 1 2 3 4 5 6 7 8; do
    echo "c$i" >"$MNT/pe/c$i.txt"; "$BIN" -C "$REPO" snapshot pe >/dev/null 2>&1 & pids+=($!)
done
rcs=""; for p in "${pids[@]}"; do wait "$p"; rcs+=$?; done
eq "snapshots at once: all made" "00000000" "$rcs"
eq "numbered one after another" "$(R for-each-ref refs/gitfold-snapshot/pe | wc -l)" \
    "$(R for-each-ref --format='%(refname)' refs/gitfold-snapshot/pe | sed 's|.*/||' | sort -n | tail -1)"
}

# Waits up to 15 s for file $1 to hold $2 (its last line); prints the time
# waited in seconds.
wait_for_line() {
    local start=$SECONDS
    for _ in $(seq 150); do [ "$(tail -1 "$1" 2>/dev/null)" = "$2" ] && break; sleep 0.1; done
    echo $((SECONDS - start))
}

layers_move_by_themselves_without_an_agent() {
R branch pe; R branch ag; R branch src; R branch fz; R branch tipb
mount_fs
P=$MNT/pe A=$MNT/ag
ls "$P" "$A" >/dev/null
# No hook ever runs in pe: a person works in it. One ran in ag: an agent's.
turn_starts "$A" >/dev/null
echo "src 1" >"$MNT/src/s.txt"; echo "fz 1" >"$MNT/fz/f.txt"
for w in "$P" "$A"; do
    "$BIN" -C "$w" layer add src >/dev/null; "$BIN" -C "$w" layer add fz --frozen >/dev/null
done
"$BIN" -C "$P" layer add tipb >/dev/null
eq "added" "src 1|src 1|fz 1" "$(cat "$P/s.txt")|$(cat "$A/s.txt")|$(cat "$P/f.txt")"
echo "src 2" >"$MNT/src/s.txt"; echo "fz 2" >"$MNT/fz/f.txt"
wait_for_line "$P/s.txt" "src 2" >/dev/null
eq "a person's workspace: a layer's new work shows by itself, within seconds" "src 2" "$(cat "$P/s.txt")"
eq "a frozen layer doesn't move" "fz 1" "$(cat "$P/f.txt")"
eq "an agent's workspace: not between its turns" "src 1" "$(cat "$A/s.txt")"
eq "the listing says when it moved" "src all work live, last HH:MM" "$(listing "$P" | grep '^src')"
check "the daemon log says so, one line" grep -q 'pe: its layers moved to their latest work (3 s without a write, no agent): src (1 file changed)' "$LOG"
out=$(turn_starts "$A")
eq "the agent's next turn moves it, and says so" "src 2|yes" \
    "$(cat "$A/s.txt")|$(grep -q 'moved to their latest work: src (1 file changed)' <<<"$out" && echo yes)"
eq "frozen there too" "fz 1" "$(cat "$A/f.txt")"

# A branch's tip that moved.
R switch -q tipb; echo "tip 1" >"$REPO/t.txt"; R add t.txt; R commit -qm t; R switch -q main
wait_for_line "$P/t.txt" "tip 1" >/dev/null
eq "a branch's new commit shows by itself" "tip 1" "$(cat "$P/t.txt" 2>/dev/null)"

# Not while the workspace is being written to.
echo "src 3" >"$MNT/src/s.txt"
moved=no
for i in $(seq 12); do
    echo "$i" >>"$P/own.txt"; sleep 0.5
    [ "$(cat "$P/s.txt")" = "src 3" ] && moved=yes
done
eq "being written to: it waits" "no" "$moved"
took=$(wait_for_line "$P/s.txt" "src 3")
eq "then moves once nothing wrote for a few seconds" "src 3" "$(cat "$P/s.txt")"
check "not before ($took s after the last write)" test "$took" -ge 2

# An agent's workspace as a layer: its end-of-turn snapshots only.
"$BIN" -C "$P" layer add ag >/dev/null
echo "ag 1" >"$A/a1.txt"; turn_ends "$A"
wait_for_line "$P/a1.txt" "ag 1" >/dev/null
eq "an agent's turn ended: its work shows by itself" "ag 1" "$(cat "$P/a1.txt" 2>/dev/null)"
echo "ag 2, half-way" >"$A/a1.txt"
sleep 6
eq "never half-way through its turn" "ag 1" "$(cat "$P/a1.txt")"
turn_ends "$A"
wait_for_line "$P/a1.txt" "ag 2, half-way" >/dev/null
eq "its next turn's end shows" "ag 2, half-way" "$(cat "$P/a1.txt")"
eq "the frozen layer still as added" "fz 1" "$(cat "$P/f.txt")"
eq "the agent's workspace: still as its last turn's start" "src 2" "$(cat "$A/s.txt")"
}

union_files_and_the_filter() {
echo 'reg.txt merge=union' >"$REPO/.gitattributes"; printf 'one\ntwo\n' >"$REPO/reg.txt"
R add -A; R commit -qm union-merge
R branch me; R branch la; R branch lb
mount_fs
W=$MNT/me
echo fromA >>"$MNT/la/reg.txt"; echo la >>"$MNT/la/a.txt"
echo fromB >>"$MNT/lb/reg.txt"
cat "$W/reg.txt" >/dev/null
check "add two layers" sh -c "'$BIN' -C '$W' layer add la && '$BIN' -C '$W' layer add lb"
eq "the union file has both layers' lines" "one two fromA fromB" "$(paste -sd' ' "$W/reg.txt")"
eq "not a conflict" "" "$("$BIN" -C "$W" layer | grep Conflict)"
check "and is writable" test -w "$W/reg.txt"
check "a whole file isn't" test ! -w "$W/a.txt"
eq "HEAD stays on the branch" "me" "$(G "$W" branch --show-current)"
eq "git status clean" "" "$(git -C "$W" status --short)"
eq "the filter is the workspace's own" "reg.txt: filter: gitfold-me|reg.txt: filter: unspecified" \
    "$(G "$W" check-attr filter -- reg.txt)|$(R check-attr filter -- reg.txt)"
echo fromMe >>"$W/reg.txt"
eq "git diff: only own lines" "+fromMe" "$(git -C "$W" diff | grep '^+[^+]')"
check "commit" G "$W" commit -qam mine
eq "the branch gets only own lines, at once" "one two fromMe" "$(R show me:reg.txt | paste -sd' ')"
eq "the file keeps the layers' lines" "one two fromA fromB fromMe" "$(paste -sd' ' "$W/reg.txt")"
eq "git status still clean" "" "$(git -C "$W" status --short)"
eq "HEAD~1 is the commit before" "union-merge" "$(G "$W" log -1 --format=%s HEAD~1)"
echo amended >>"$W/reg.txt"
check "amend" G "$W" commit -q --amend -am mine2
eq "the amend is on the branch" "one two fromMe amended|mine2" \
    "$(R show me:reg.txt | paste -sd' ')|$(R log -1 --format=%s me)"
eq "stash keeps only own lines" "one two fromMe amended stashed|one two fromA fromB fromMe amended" \
    "$(echo stashed >>"$W/reg.txt"; G "$W" stash -q; R show 'refs/stash:reg.txt' | paste -sd' ')|$(paste -sd' ' "$W/reg.txt")"
G "$W" stash pop -q
eq "stash pop: own lines back among the layers'" "one two fromA fromB fromMe amended stashed| M reg.txt" \
    "$(paste -sd' ' "$W/reg.txt")|$(git -C "$W" status --short)"
G "$W" checkout -q reg.txt
sed 's/^two$/TWO/' "$W/reg.txt" >"$W/.reg.tmp"
check "an editor's save (a new file renamed over it)" mv "$W/.reg.tmp" "$W/reg.txt"
eq "git sees only that change" "-two +TWO" "$(git -C "$W" diff | grep '^[-+][^-+]' | paste -sd' ')"
G "$W" checkout -q reg.txt
# The guard: commits made without the filter don't reach the branch.
tip=$(R rev-parse me)
blob=$(G "$W" hash-object -w --no-filters reg.txt)
G "$W" update-index --cacheinfo "100644,$blob,reg.txt"
out=$(G "$W" commit -q --no-verify -m leak 2>&1); rc=$?
eq "a commit of the layers' lines, --no-verify: refused, saying why" "1 yes $tip" \
    "$((rc != 0)) $(echo "$out" | grep -q "adds la's uncommitted line \"fromA\" to reg.txt" && echo yes) $(R rev-parse me)"
leak=$(G "$W" commit-tree "$(G "$W" write-tree)" -p HEAD -m leak)
G "$W" reset -q
check "plumbing too: update-ref refused" fails G "$W" update-ref refs/heads/me "$leak"
eq "the branch didn't move" "$tip" "$(R rev-parse me)"
git init -q --bare "$T/remote.git"
out=$(G "$W" push -q "$T/remote.git" "$leak:refs/heads/main" 2>&1); rc=$?
eq "pushing it is refused (pre-push)" "1 yes" "$((rc != 0)) $(echo "$out" | grep -q 'refusing to push refs/heads/main' && echo yes)"
check "pushing the branch isn't" G "$W" push -q "$T/remote.git" me
check "remove one layer" "$BIN" -C "$W" layer remove lb
eq "its lines gone, own ones kept" "one two fromA fromMe amended" "$(paste -sd' ' "$W/reg.txt")"
eq "git status clean after the switch" "" "$(git -C "$W" status --short)"
eq "a snapshot of it: only its own lines" "one two fromMe amended" \
    "$(R show "$(snap me):reg.txt" | paste -sd' ')"
echo moreMe >>"$W/reg.txt"
check "remove --all" "$BIN" -C "$W" layer remove --all
eq "the layer's lines gone, own ones kept" "one two fromMe amended moreMe" "$(paste -sd' ' "$W/reg.txt")"
eq "on the branch, the uncommitted line its change" "me| M reg.txt" \
    "$(G "$W" branch --show-current)|$(git -C "$W" status --short)"
eq "no filter, no guard left" "unspecified|1|1" \
    "$(G "$W" check-attr filter -- reg.txt | sed 's/.*: //')|$(G "$W" config filter.gitfold-me.process >/dev/null; echo $?)|$(G "$W" config core.hooksPath >/dev/null; echo $?)"
}

# Claude Code's read-tracking hook on its Read tool, for each path.
read_file() { for p; do printf '{"tool_name":"Read","cwd":"%s","tool_input":{"file_path":"%s"}}' "$REPO" "$p" |
    "$BIN" track-reads --claude-hook pre-tool-use; done; }

proposals_to_change_a_contract() {
printf 'fn get(id)\n' >"$REPO/api.txt"; R add api.txt; R commit -qm api
for b in contracts lib c1 c2 c3 c4 c5; do R branch $b; done
mount_fs
echo 'fn put(id)' >>"$MNT/contracts/api.txt"   # the root's contract, uncommitted
echo 'fn mine(id)' >"$MNT/c5/api.txt"          # c5's own version, before the layer
echo lib >"$MNT/lib/lib.txt"
for c in c1 c2 c3 c4 c5; do ls "$MNT/$c" >/dev/null; turn_starts "$MNT/$c" >/dev/null; "$BIN" -C "$REPO" layer add contracts -w $c >/dev/null; done
"$BIN" -C "$REPO" layer add lib -w c2 >/dev/null                                  # c2 shows two layers
for c in c1 c2 c3 c5; do "$BIN" -C "$REPO" track-reads $c on >/dev/null; done   # c4: reads unknown
read_file "$MNT/c1/api.txt" "$MNT/c2/api.txt" "$MNT/c3/a.txt" "$MNT/c5/api.txt"
echo 'get(1)' >"$MNT/c1/use.txt"; echo 'put(2)' >"$MNT/c2/use.txt"; echo 'get(4)' >"$MNT/c4/use.txt"
"$BIN" -C "$MNT/c4" propose api.txt >/dev/null                                   # c4 shows a proposal of its own
eq "the deny hook offers building on the layer first, then a proposal" "yes" \
    "$(edit_hook "$MNT/c1" api.txt | grep -q 'build on contracts: `gitfold layer add contracts --edit`.*Or run `gitfold propose api.txt`' && echo yes)"
check "proposing outside a layer's workspace: no such layer (exit 3)" \
    sh -c "cd '$MNT/c1' && '$BIN' propose api.txt --layer c2; [ \$? = 3 ]"
out=$(cd "$MNT/c1" && "$BIN" propose api.txt 2>&1)
eq "propose: a proposal branch, where to edit" "yes yes" \
    "$(echo "$out" | grep -q 'branch contracts-prop/c1' && echo yes) $(echo "$out" | grep -qx "  $MNT/contracts-prop/c1/api.txt" && echo yes)"
eq "recorded as a proposal" "contracts c1" \
    "$(R config branch.contracts-prop/c1.gitfoldProposal) $(R config branch.contracts-prop/c1.gitfoldProposedBy)"
eq "shown in place of the layer" "contracts-prop/c1" "$(layer_names "$MNT/c1")"
eq "a copy of the layer's work, uncommitted included" "fn get(id)
fn put(id)" "$(cat "$MNT/contracts-prop/c1/api.txt")"
printf 'fn fetch(id)\nfn put(id)\n' >"$MNT/contracts-prop/c1/api.txt"; echo 'fetch(1)' >"$MNT/c1/use.txt"
eq "the deny hook: edit the proposal there" "yes" \
    "$(edit_hook "$MNT/c1" api.txt | grep -q "shows your proposal contracts-prop/c1.*$MNT/contracts-prop/c1/api.txt" && echo yes)"
(cd "$MNT/c1" && "$BIN" layer --claude-hook user-prompt-submit >/dev/null)
eq "the next turn shows the edits" "fn fetch(id)" "$(head -1 "$MNT/c1/api.txt")"
mkdir -p "$T/from"; printf 'fn fetch(id)\nfn put(id)\n++ note\n' >"$T/from/api.txt"
check "propose again, --layer naming the layer, --from a directory: the same proposal" \
    sh -c "'$BIN' -C '$REPO' propose -w c1 api.txt --layer contracts --from '$T/from' | grep -q 'copied into it: api.txt'"
out=$("$BIN" -C "$REPO" proposals contracts)
eq "proposals: who, what, consent" "contracts-prop/c1: proposed by c1, to change contracts|yes|  consent: not checked (\`gitfold consent\`)" \
    "$(echo "$out" | grep "^contracts-prop/c1")|$(echo "$out" | grep -q "api.txt | 3 ++-" && echo yes)|$(echo "$out" | sed -n '/^contracts-prop\/c1/,/consent/p' | tail -1)"
eq "proposals of another layer: none" "" "$("$BIN" -C "$REPO" proposals c2)"
echo t >"$MNT/contracts/types.txt"   # the root's work moves on after the proposal
TEST='f=$(cut -d"(" -f1 use.txt 2>/dev/null) || exit 0; grep -q "fn $f(" api.txt || { echo "no fn $f"; exit 1; }'
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 2>&1); rc=$?
eq "consent: a workspace's own version conflicts (exit 4)" "4 c5	yes	no	-	conflicts: api.txt" \
    "$rc $(echo "$out" | grep '^c5')"
"$BIN" -C "$REPO" layer remove contracts -w c5 >/dev/null
eq "--only-readers skips who didn't read the files" "c3	no	-	-" \
    "$("$BIN" -C "$REPO" consent contracts-prop/c1 --only-readers | grep '^c3')"
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 --test "$TEST" 2>&1); rc=$?
eq "consent --test: every workspace showing the layer or a proposal of it (exit 4)" "4|workspace	read	merges	test
c1	yes	yes	passed
c2	yes	yes	passed
c3	no	yes	passed
c4	unknown	yes	failed	exit 1: no fn get" "$rc|$(echo "$out" | sed -n '1,5p' | sed 's/ (.*//')"
eq "the workspaces' own trees untouched" "A	use.txt|get(4)" "$("$BIN" -C "$REPO" changes c4)|$(cat "$MNT/c4/use.txt")"
eq "proposals: the failed consent" "  consent: failed: 1 of 4 failing: c4" \
    "$("$BIN" -C "$REPO" proposals contracts | sed -n '/^contracts-prop\/c1/,/consent/p' | tail -1)"
echo 'put(4)' >"$MNT/c4/use.txt"
TEST2='test -f types.txt && { [ "$GITFOLD_WORKSPACE" != c2 ] || test -f lib.txt; } && echo x >"$GITFOLD_SCRATCH/out"'
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 --test "$TEST2" 2>&1); rc=$?
eq "checked on the layer as now, with each one's other layers, a scratch dir to write" "0" "$rc"
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 --test 'sleep 5' --timeout 1 2>&1); rc=$?
eq "--timeout: the test killed" "4 c1	yes	yes	failed	timed out after 1 s" "$rc $(echo "$out" | grep '^c1' | sed 's/ (.*//')"
eq "no snapshot registrations left" "0" "$(ls "$COMMON/worktrees" | grep -vc '^gitfold-ws-')"
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 --test "$TEST" 2>&1); rc=$?
eq "fixed: consent passes (exit 0)" "0 contracts-prop/c1: passed (4 workspaces checked)" "$rc $(echo "$out" | tail -1)"
echo m >"$MNT/contracts/more.txt"
out=$("$BIN" -C "$MNT/contracts" accept contracts-prop/c1 2>&1); rc=$?
eq "the layer moved since the consent: accept refused (exit 4)" "4 yes" \
    "$rc $(echo "$out" | grep -q 'contracts changed since: check again' && echo yes)"
check "consent again" "$BIN" -C "$REPO" consent contracts-prop/c1 --test "$TEST"
echo x >"$MNT/contracts-prop/c1/x.txt"
out=$("$BIN" -C "$MNT/contracts" accept contracts-prop/c1 2>&1); rc=$?
eq "the proposal changed since the consent: refused (exit 4)" "4 yes" \
    "$rc $(echo "$out" | grep -q 'the proposal changed since' && echo yes)"
rm "$MNT/contracts-prop/c1/x.txt"
check "accept in another workspace: refused (exit 3)" \
    sh -c "'$BIN' -C '$REPO' accept contracts-prop/c1 -w c2; [ \$? = 3 ]"
out=$("$BIN" -C "$MNT/c1" accept contracts-prop/c1 -w contracts 2>&1); rc=$?
eq "-w from inside another workspace: refused (exit 3)" "3 yes" "$rc $(echo "$out" | grep -q 'you are in workspace c1' && echo yes)"
check "accept of no proposal (exit 3)" sh -c "'$BIN' -C '$REPO' accept c2 -w contracts; [ \$? = 3 ]"
echo 'fn del(id)' >>"$MNT/contracts/api.txt"
out=$("$BIN" -C "$MNT/contracts" accept contracts-prop/c1 2>&1); rc=$?
eq "the layer changed the file since: refused (exit 4)" "4 yes" "$rc $(echo "$out" | grep -q 'contracts changed api.txt since' && echo yes)"
printf 'fn get(id)\nfn put(id)\n' >"$MNT/contracts/api.txt"
out=$("$BIN" -C "$MNT/contracts" accept contracts-prop/c1 2>&1); rc=$?
eq "accept" "0 accepted contracts-prop/c1 into contracts: api.txt" "$rc $(echo "$out" | head -1)"
eq "written into the layer's workspace, uncommitted" "fn fetch(id)
fn put(id)
++ note| M api.txt" "$(cat "$MNT/contracts/api.txt")|$(git -C "$MNT/contracts" status --short -- api.txt)"
eq "the proposer shows the layer again, with the change" "contracts|fn fetch(id)" \
    "$(layer_names "$MNT/c1")|$(head -1 "$MNT/c1/api.txt")"
check "the proposal's branch is gone" fails R rev-parse -q --verify refs/heads/contracts-prop/c1
eq "and its config" "" "$(R config --get-regexp 'branch\.contracts-prop/c1' 2>&1)"
eq "proposals: c4's left" "contracts-prop/c4: proposed by c4, to change contracts" "$("$BIN" -C "$REPO" proposals | head -1)"
msg=$("$BIN" -C "$REPO" inbox c2)
eq "readers told: from the layer, the proposer's changed lines" "from: contracts|subject: contract changed|yes|yes|  -fn get(id)|  +fn fetch(id)|  +++ note" \
    "$(echo "$msg" | sed -n 1p)|$(echo "$msg" | sed -n 3p)|$(echo "$msg" | grep -q '^contract api.txt changed' && echo yes)|$(echo "$msg" | grep -q '^the change, as c1 proposed it:' && echo yes)|$(echo "$msg" | grep -- '-fn')|$(echo "$msg" | grep -- '+fn')|$(echo "$msg" | grep -- '++ note')"
eq "the proposer too (it read it)" "1" "$("$BIN" -C "$REPO" inbox c1 | grep -c '^subject: contract changed')"
eq "and who isn't known not to: c4, not c3" "|1" "$("$BIN" -C "$REPO" inbox c3)|$("$BIN" -C "$REPO" inbox c4 | grep -c '^subject: contract changed')"
snap contracts >/dev/null
(cd "$MNT/c2" && "$BIN" layer --claude-hook user-prompt-submit >/dev/null)
eq "the others see it at their next turn" "fn fetch(id)" "$(head -1 "$MNT/c2/api.txt")"
start=$(R for-each-ref --format='%(refname)' refs/gitfold-proposal/ | head -1); oid=$(R rev-parse "$start")
R update-ref -d "$start"
eq "proposals: one that can't be read is said so" "yes" \
    "$("$BIN" -C "$REPO" proposals | grep -q "^contracts-prop/c4: to change contracts, can't be read" && echo yes)"
R update-ref "$start" "$oid"
check "the proposer withdraws its own" "$BIN" -C "$MNT/c4" reject contracts-prop/c4
eq "it shows the layer again" "contracts" "$(layer_names "$MNT/c4")"
"$BIN" -C "$MNT/c2" propose api.txt >/dev/null
check "reject in a third workspace: refused (exit 3)" \
    sh -c "'$BIN' -C '$REPO' reject contracts-prop/c2 -w c3; [ \$? = 3 ]"
check "reject" "$BIN" -C "$MNT/contracts" reject contracts-prop/c2 --reason "keep fetch"
eq "the layer shown again, the proposer told why, the proposal gone" "contracts|yes|" \
    "$(layer_names "$MNT/c2" | tr " " "\n" | grep contracts)|$("$BIN" -C "$REPO" inbox c2 | grep -q 'was rejected: keep fetch' && echo yes)|$(R branch --list 'contracts-prop/*')"
eq "no snapshot or start refs of removed proposals left" "" \
    "$(R for-each-ref --format='%(refname)' refs/gitfold-proposal/ 'refs/gitfold-snapshot/contracts-prop*')"
(cd "$MNT/c2" && "$BIN" propose lib.txt --layer lib >/dev/null)
out=$("$BIN" -C "$REPO" consent lib-prop/c2 --only-readers 2>&1); rc=$?
eq "nobody checked: not a pass (exit 4)" "4 lib-prop/c2: unchecked" "$rc $(echo "$out" | tail -1 | sed 's/ (.*//')"
check "accept without a passed consent: refused" fails "$BIN" -C "$MNT/lib" accept lib-prop/c2
echo lib2 >"$MNT/lib-prop/c2/lib.txt"
(cd "$MNT/lib-prop/c2" && exec sleep 30) & SLEEPER=$!
sleep 0.5
out=$("$BIN" -C "$MNT/lib" accept lib-prop/c2 --force 2>&1); rc=$?
kill $SLEEPER 2>/dev/null; wait $SLEEPER 2>/dev/null
eq "--force; its workspace in use: kept, no longer a proposal" "0 yes |lib2" \
    "$rc $(echo "$out" | grep -q "its workspace wasn't removed" && echo yes) $("$BIN" -C "$REPO" proposals)|$(cat "$MNT/lib/lib.txt")"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
printf 'fn fetch(id)\nfn put(id)\nfn list()\n' >"$T/from/api.txt"
check "unmounted: propose --from" "$BIN" -C "$REPO" propose -w c2 api.txt --from "$T/from"
eq "shown in place of the layer" "contracts-prop/c2" "$(layer_names "$REPO" c2 | tr " " "\n" | grep contracts)"
check "consent, without a test" "$BIN" -C "$REPO" consent contracts-prop/c2
check "and accept" "$BIN" -C "$REPO" accept contracts-prop/c2 -w contracts
eq "written into the stored workspace" "fn list()" "$(R show "$(snap contracts 2>/dev/null):api.txt" | tail -1)"
eq "the layer back" "contracts" "$(layer_names "$REPO" c2 | tr " " "\n" | grep contracts)"
}

proposals_keep_the_layer_as_it_was() {
printf 'fn get(id)\n' >"$REPO/api.txt"; R add api.txt; R commit -qm api
R branch lead; R branch me; R branch rd; R branch plain
R switch -q plain; echo p >"$REPO/p.txt"; R add p.txt; R commit -qm p; R switch -q main
mount_fs
W=$MNT/me
echo 'fn put(id)' >>"$MNT/lead/api.txt"; G "$MNT/lead" commit -qam put
echo 'fn wip(id)' >>"$MNT/lead/api.txt"
ls "$W" "$MNT/rd" >/dev/null; turn_starts "$W" >/dev/null; turn_starts "$MNT/rd" >/dev/null
"$BIN" -C "$W" layer add lead --commits >/dev/null
out=$("$BIN" -C "$W" propose api.txt)
eq "propose: a copy of the layer's work; the next turn shows the edits" "yes yes" \
    "$(echo "$out" | grep -q "branch lead-prop/me, a copy of lead's work, uncommitted included" && echo yes) $(echo "$out" | grep -q 'your next turn shows your edits here (or now: `gitfold layer add lead-prop/me`)' && echo yes)"
eq "the proposal shows as all work" "lead-prop/me all work live, last HH:MM" "$(listing "$W" | grep '^lead')"
ls "$MNT/lead-prop" >/dev/null
check "rejected" "$BIN" -C "$W" reject lead-prop/me
eq "the layer back as it was: commits only" "lead commits only live, last HH:MM" "$(listing "$W" | grep '^lead')"
eq "no empty folder of the proposal left" "" "$(ls "$MNT" | grep lead-prop)"
check "nor can it be entered" test ! -e "$MNT/lead-prop"
"$BIN" -C "$W" layer add lead --frozen >/dev/null
"$BIN" -C "$MNT/rd" layer add lead --frozen >/dev/null
"$BIN" -C "$W" propose api.txt >/dev/null
echo 'fn wanted(id)' >>"$MNT/lead-prop/me/api.txt"
check "consent" "$BIN" -C "$REPO" consent lead-prop/me
check "accept" "$BIN" -C "$MNT/lead" accept lead-prop/me
eq "accepted: the layer back, still frozen, taken with the change" "lead all work frozen at HH:MM|fn wanted(id)" \
    "$(listing "$W" | grep '^lead')|$(tail -1 "$W/api.txt")"
eq "a reader with a frozen layer: told how to take the change" "yes" \
    "$("$BIN" -C "$REPO" inbox rd | grep -q 'your layer of lead is frozen: `gitfold layer add lead --frozen` takes its work as it is now' && echo yes)"
"$BIN" -C "$W" propose api.txt >/dev/null
out=$("$BIN" -C "$W" layer add lead)
eq "add the layer while its proposal shows: replaced in place, the proposal kept" "lead|yes|lead-prop/me" \
    "$(layer_names "$W")|$(echo "$out" | head -1 | grep -q '(in place of your proposal lead-prop/me, which stays' && echo yes)|$(R for-each-ref --format='%(refname:short)' 'refs/heads/lead-prop/')"
check "add the proposal again: in the same place" sh -c "'$BIN' -C '$W' layer add lead-prop/me >/dev/null && [ \"\$('$BIN' -C '$W' layer | grep -c '^lead')\" = 1 ]"
"$BIN" -C "$W" layer add lead --frozen >/dev/null; "$BIN" -C "$W" layer add lead-prop/me >/dev/null
out=$("$BIN" -C "$W" layer remove lead-prop/me)
eq "remove the proposal: the layer it stood in for shown again as it was, the proposal kept" \
    "layer lead-prop/me removed; lead shown again as it was; the proposal stays: \`gitfold reject lead-prop/me\` drops it|lead all work frozen at HH:MM|lead-prop/me" \
    "$(echo "$out" | head -1)|$(listing "$W" | grep '^lead')|$(R for-each-ref --format='%(refname:short)' 'refs/heads/lead-prop/')"
"$BIN" -C "$W" layer add lead-prop/me >/dev/null
out=$("$BIN" -C "$W" layer remove lead)
eq "remove the layer while its proposal shows: removed, saying the proposal stays" "yes|No layers." \
    "$(echo "$out" | head -1 | grep -q "it showed your proposal lead-prop/me, which stays: \`gitfold reject lead-prop/me\` drops it" && echo yes)|$("$BIN" -C "$W" layer | head -1)"
"$BIN" -C "$W" reject lead-prop/me >/dev/null
"$BIN" -C "$W" layer add plain >/dev/null
out=$("$BIN" -C "$W" propose p.txt)
eq "a layer without a workspace: a copy of its commits, shown as all work" "yes|plain-prop/me all work live, last HH:MM" \
    "$(echo "$out" | grep -q "a copy of plain's commits" && echo yes)|$(listing "$W" | grep '^plain')"
}


# A layer whose work is built on this workspace's own commits, made while
# its base waits for the layers to go: its change is taken against the
# commit the view is built on, so the lines this workspace committed and
# the layer changed since don't conflict.
layers_on_the_workspaces_own_commits() {
printf '1\n2\n3\n4\n5\n6\n7\n8\n' >"$REPO/p.txt"; cp "$REPO/p.txt" "$REPO/q.txt"; R add -A; R commit -qm p
R branch api
"$BIN" -C "$REPO" branch web --edit api >/dev/null
mount_fs
A=$MNT/api W=$MNT/web
ls "$A" "$W" >/dev/null
turn_starts "$A" >/dev/null   # an agent's: its layers move at its turns
echo w >"$W/w.txt"
"$BIN" -C "$A" layer add web >/dev/null
sed -i 's/^3$/3 api/' "$A/p.txt"; G "$A" commit -qam "api 3"
eq "api committed; its base waits for the layers" "$(R rev-parse api^)" "$(cat "$COMMON/gitfold/api/base")"
"$BIN" -C "$REPO" layer add api --edit -w web >/dev/null
sed -i 's/^3 api$/3 web/' "$W/p.txt"
out=$(turn_starts "$A")
eq "web's new work shows in api" yes "$(grep -q 'moved to their latest work: web' <<<"$out" && echo yes)"
eq "no conflict said" "" "$(grep -i conflict <<<"$out")"
eq "nor listed" "" "$("$BIN" -C "$A" layer | grep -i conflict)"
eq "api's own line is its own" "3 api" "$(sed -n 3p "$A/p.txt")"
# The layers removed and added again: the base catches up first, the same.
"$BIN" -C "$A" layer remove --all >/dev/null
for _ in $(seq 30); do [ "$(cat "$COMMON/gitfold/api/base")" = "$(R rev-parse api)" ] && break; sleep 0.1; done
out=$("$BIN" -C "$A" layer add web)
eq "added again, on the caught-up base: no conflict" "" "$(grep -i conflict <<<"$out")"
eq "web's line shows now (api's is in the base)" "3 web|w" "$(sed -n 3p "$A/p.txt")|$(cat "$A/w.txt")"

# A snapshot's parent: the tip only when the workspace has the tip's
# changes (committed through it), else its base.
base_was=$(cat "$COMMON/gitfold/api/base")
sed -i 's/^2$/2 api/' "$A/q.txt"; G "$A" commit -qam "api q"
eq "committed with the layers shown: the base waits" "$base_was" "$(cat "$COMMON/gitfold/api/base")"
c=$(snap api)
eq "committed through the workspace: on the tip" "$(R rev-parse api)" "$(R rev-parse "$c^")"
sed -i 's/^7$/7 more/' "$A/q.txt"
c=$(snap api)
eq "and edited further: still on the tip" "$(R rev-parse api)" "$(R rev-parse "$c^")"
base=$(cat "$COMMON/gitfold/api/base")
blob=$(echo pushed | R hash-object -w --stdin)
export GIT_INDEX_FILE=$T/x.index
R read-tree api; R update-index --add --cacheinfo "100644,$blob,pushed.txt"
R update-ref refs/heads/api "$(R commit-tree "$(R write-tree)" -p api -m "pushed in")"
unset GIT_INDEX_FILE
c=$(snap api)
eq "the tip moved without its files (a push into it): on the base" "$base|" \
    "$(R rev-parse "$c^")|$(R ls-tree --name-only "$c" pushed.txt)"
}

# The snapshot of layer $2 stored for workspace $1.
stored_snapshot() { awk -F'\t' -v l="layer $2" '$1 == l {print $5}' "$COMMON/gitfold/$1/layers"; }

# A person's workspace with a union file (filtered for git): the daemon
# moves its layers there too, sees a source's commits, and backs off from a
# switch it can't make.
layers_move_by_themselves_in_a_workspace_with_union_files() {
echo 'reg.txt merge=union' >"$REPO/.gitattributes"; printf 'one\ntwo\n' >"$REPO/reg.txt"
R add -A; R commit -qm union-merge
R branch pe; R branch src; R branch other
mount_fs
P=$MNT/pe S=$MNT/src
ls "$P" "$S" "$MNT/other" >/dev/null
echo fromSrc >>"$S/reg.txt"; echo "s 1" >"$S/s.txt"
"$BIN" -C "$P" layer add src >/dev/null
eq "union: HEAD on the branch" "one two fromSrc|pe" \
    "$(paste -sd' ' "$P/reg.txt")|$(G "$P" branch --show-current)"
echo "s 2" >"$S/s.txt"
wait_for_line "$P/s.txt" "s 2" >/dev/null
eq "a source's new work shows by itself" "s 2" "$(cat "$P/s.txt")"
# The source shows a layer itself (its base waits) and commits: nothing
# written through the mount, its base the same; its tip moved.
echo o >"$MNT/other/o.txt"; "$BIN" -C "$S" layer add other >/dev/null
G "$S" add s.txt; G "$S" commit -qm "src s"
for _ in $(seq 100); do [ "$(R rev-parse "$(stored_snapshot pe src)^")" = "$(R rev-parse src)" ] && break; sleep 0.1; done
eq "a source's commit is seen: its layer moves onto the snapshot on its tip" "$(R rev-parse src)" \
    "$(R rev-parse "$(stored_snapshot pe src)^")"
# A snapshot of the source that fails (a file it can't read): tried again
# later, though nothing is written to it meanwhile.
echo "u 1" >"$S/u.txt"; chmod 000 "$COMMON/gitfold/src/upper/u.txt"
sleep 5
chmod 644 "$COMMON/gitfold/src/upper/u.txt"
wait_for_line "$P/u.txt" "u 1" >/dev/null
eq "a layer that couldn't move is tried again" "u 1" "$(cat "$P/u.txt" 2>/dev/null)"
# git is told just after the view shows the move.
for _ in $(seq 30); do [ -z "$(git -C "$P" status --short)" ] && break; sleep 0.1; done
eq "git sees no change after the moves" "" "$(git -C "$P" status --short)"
echo fromSrc2 >>"$S/reg.txt"
wait_for_line "$P/reg.txt" "fromSrc2" >/dev/null
# (git is told just after the file changes: a status meanwhile may see it.)
for _ in $(seq 20); do [ -z "$(git -C "$P" status --short)" ] && break; sleep 0.1; done
eq "a union line moves in by itself, git still sees no change" "one two fromSrc fromSrc2|" \
    "$(paste -sd' ' "$P/reg.txt")|$(git -C "$P" status --short)"
# pe's HEAD away from its branch (a bisect, say): its layers don't move
# until it is back.
G "$P" switch -q --detach
echo "s 3" >"$S/s.txt"
sleep 7
eq "HEAD away: no move" "s 2" "$(cat "$P/s.txt")"
G "$P" switch -q pe
wait_for_line "$P/s.txt" "s 3" >/dev/null
eq "back on the branch: it moves" "s 3" "$(cat "$P/s.txt")"
}

has() { grep -qF -- "$2" <<<"$1"; }
# Waits up to $2 tenths of a second for command $1 (a string) to succeed.
wait_until() { for _ in $(seq "$2"); do eval "$1" && return; sleep 0.1; done; return 1; }

# Waiting edits beyond the use cases: an own line next to one, a file the
# editable layer added, the child's snapshot, `layer remove --all`, the
# safety net for a commit made without hooks, and what happens once the
# lead commits.
waiting_edits_in_more_files() {
printf '// head\n// api\n// tail\n' >"$REPO/api.rs"; R add -A; R commit -qm api
R branch lead
"$BIN" -C "$REPO" branch child --edit lead >/dev/null || fail "branch child --edit lead"
mount_fs
L=$MNT/lead C=$MNT/child
turn_starts "$C" >/dev/null
printf '// head\n// api\nclass Api {\n    fn a() {}\n}\n// tail\n' >"$L/api.rs"
printf 'fn draft() {}\n' >"$L/new.rs"
turn_ends "$L" >/dev/null; turn_starts "$C" >/dev/null
eq "the lead's uncommitted work shows" "fn draft() {}" "$(cat "$C/new.rs" 2>&1)"
eq "git status: clean (its stat of the files recorded)" "" "$(git -C "$C" status --short)"
printf '// head\n// api\nclass Api {\n    fn a() { mine }\n}\n// tail\n// mine\n' >"$C/api.rs"
eq "an own line next to a waiting edit: git sees the own line only" "+// mine" \
    "$(git -C "$C" diff | grep '^+[^+]')"
echo 'fn mine() {}' >>"$C/new.rs"
eq "a file only the lead has, edited: hidden from git" " M api.rs" "$(git -C "$C" status --short)"
out=$(listing "$C")
check "listed: the waiting edit in api.rs, whose lines it changes" has "$out" \
    "1 waiting edit in api.rs: it changes lead's uncommitted lines; it becomes yours to commit once lead commits them"
check "and the one in the lead's new file" has "$out" "1 waiting edit in new.rs"
c=$(snap child)
eq "the child's snapshot: its own lines only" "// head|// api|// tail|// mine" "$(R show "$c:api.rs" | paste -sd'|')"
eq "without the lead's file" "" "$(R ls-tree --name-only "$c" new.rs)"
out=$("$BIN" -C "$C" layer remove --all 2>&1); rc=$?
eq "layer remove --all with waiting edits: refused (exit 4)" 4 "$rc"
check "naming them" has "$out" "api.rs (1 waiting edit), new.rs (1 waiting edit)"
G "$C" commit -qam "child: mine"
eq "committed: the own line only" "// head|// api|// tail|// mine" "$(R show child:api.rs | paste -sd'|')"

# A commit made without the filter and the hooks (as a libgit2 tool makes
# it): it lands, and the daemon says so.
tip=$(R rev-parse child)
blob=$(git -C "$C" hash-object -w --no-filters api.rs)
G "$C" update-index --cacheinfo "100644,$blob,api.rs"
G "$C" -c core.hooksPath=/dev/null commit -qm "child: by libgit2"
eq "without hooks the commit lands" "$tip" "$(R rev-parse child^)"
bad=$(R rev-parse --short=12 child)
wait_until 'has "$(listing "$C")" WARNING' 50
out=$(listing "$C")
check "the listing warns: the commit" has "$out" "WARNING: commit $bad (child: by libgit2) on child"
check "whose lines, in which file" has "$out" "adds lead's uncommitted line \"class Api {\" to api.rs"
check "and how to undo it: a mixed reset" has "$out" "git reset $bad^\` (your files"
check "not a soft one (the guard refuses its commit again)" fails has "$out" "--soft"
check "the turn's notes too" has "$(turn_starts "$C")" "WARNING: commit $bad"
check "and the log" grep -q "commit $bad (child: by libgit2)" "$LOG"
# The advice, followed: the commit goes, and git sees the file as before
# (the own line committed, the lead's lines and the waiting edit not).
G "$C" reset -q "$bad^"
eq "git reset <c>^: the commit is gone" "$tip" "$(R rev-parse child)"
wait_until '! has "$(listing "$C")" WARNING' 50
check "taken out: no warning" fails has "$(listing "$C")" WARNING
eq "git sees the waiting edits as before" "" "$(git -C "$C" status --short)"

G "$L" add -A; G "$L" commit -qm "lead: class, new"; turn_ends "$L" >/dev/null
turn_starts "$C" >/dev/null
eq "the lead committed them: the child moves onto it" "$(R rev-parse lead)" "$(R rev-parse child^)"
eq "its waiting edits are ordinary changes" " M api.rs| M new.rs" "$(git -C "$C" status --short | paste -sd'|')"
eq "with what they changed" "-    fn a() {}|+    fn a() { mine }|+fn mine() {}" \
    "$(git -C "$C" diff | grep '^[-+][^-+]' | paste -sd'|')"
check "none listed" fails has "$(listing "$C")" "waiting edit"
}

# A workspace without an agent: the daemon moves its branch onto the
# lead's commit once it is idle, and the waiting edit becomes an ordinary
# change.
waiting_edits_without_an_agent() {
printf '// api\n' >"$REPO/api.rs"; R add -A; R commit -qm api
R branch lead
"$BIN" -C "$REPO" branch pc --edit lead >/dev/null || fail "branch pc --edit lead"
mount_fs
L=$MNT/lead P=$MNT/pc
ls "$P" >/dev/null
printf '// api\nclass Api {\n    fn a() {}\n}\n' >"$L/api.rs"
wait_for_line "$P/api.rs" "}" >/dev/null
eq "the lead's class shows by itself" "// api|class Api {|    fn a() {}|}" "$(paste -sd'|' "$P/api.rs")"
# git told, and its stat of the file recorded: a write must make it read the
# file again.
wait_until '[ -z "$(git -C "$P" status --short)" ]' 30
printf '// api\nclass Api {\n    fn a() {}\n    fn m() {}\n}\n' >"$P/api.rs"
eq "a waiting edit: git sees nothing" "" "$(git -C "$P" status --short)"
check "listed" has "$(listing "$P")" "1 waiting edit in api.rs"
G "$L" commit -qam "lead: class"
wait_until '[ "$(git -C "$P" status --short)" = " M api.rs" ]' 200
eq "the lead committed: the daemon moved the branch onto it" "$(R rev-parse lead)" "$(R rev-parse pc)"
eq "the waiting edit is an ordinary change" "+    fn m() {}" "$(git -C "$P" diff | grep '^+[^+]')"
check "none listed" fails has "$(listing "$P")" "waiting edit"
}

## The review of waiting edits and the guard (reviews/v2-round1): content
## safety. No loss (I1): nothing git does loses the workspace's own lines,
## its waiting edits or a layer's lines shown. No leak (I2): no commit on
## the branch brings an editable or union layer's uncommitted work.

# The lead's main version of api.rs is $1; the child is built on the lead,
# an agent's (its layers move at its turns): L, C.
built_on_the_lead() {
printf "$1" >"$REPO/api.rs"; R add -A; R commit -qm api
R branch lead
"$BIN" -C "$REPO" branch child --edit lead >/dev/null || fail "branch child --edit lead"
mount_fs
L=$MNT/lead C=$MNT/child
turn_starts "$C" >/dev/null
}
# The lead's api.rs becomes $1 (uncommitted), shown to the child at its turn.
lead_drafts() { printf "$1" >"$L/api.rs"; turn_ends "$L" >/dev/null; turn_starts "$C" >/dev/null; }
# git's view of api.rs in the child: its diff's changed lines, one line.
git_sees() { git -C "$C" diff -- "${1:-api.rs}" | grep '^[-+][^-+]' | paste -sd'|'; }

# Stash and pop of an own change next to the lead's line keep the lead's
# line (correctness 1); a layer's lines moved or reordered wait, never
# reaching git as the child's, never deleting its own (correctness 4).
waiting_edits_survive_git() {
printf 'use b;\nuse c;\n\nfn a() {\n    x();\n}\n\nfn c() {\n}\n' >"$REPO/api2.rs"
built_on_the_lead 'a\nb\nc\n'
lead_drafts 'a\nb\nL\nc\n'
printf 'a\nB\nL\nc\n' >"$C/api.rs"
eq "an own change next to the lead's line: git sees it alone" "-b|+B" "$(git_sees)"
check "git stash" G "$C" stash -q
eq "stashed: the lead's line stays" "a|b|L|c" "$(paste -sd'|' "$C/api.rs")"
check "git stash pop" G "$C" stash pop -q
eq "popped: the own change back, the lead's line kept" "a|B|L|c" "$(paste -sd'|' "$C/api.rs")"
eq "git sees the own change alone" "-b|+B" "$(git_sees)"
check "no waiting edit made of it" fails has "$(listing "$C")" "waiting edit"
G "$C" checkout -q -- api.rs

# Imports sorted with the lead's among them; the lead's function moved
# above the child's.
printf 'use b;\nuse c;\nuse a;\n\nfn a() {\n    x();\n}\n\nfn b() {\n    let q = 1;\n}\n\nfn c() {\n}\n' >"$L/api2.rs"
turn_ends "$L" >/dev/null; turn_starts "$C" >/dev/null
printf 'use a;\nuse b;\nuse c;\n\nfn b() {\n    let q = 1;\n}\n\nfn a() {\n    x();\n}\n\nfn c() {\n}\n' >"$C/api2.rs"
eq "imports sorted, the lead's function moved first: git sees nothing" "" "$(git_sees api2.rs)"
check "listed as waiting" has "$(listing "$C")" "waiting edits in api2.rs"
G "$C" commit -qam "child: whatever git sees" 2>/dev/null
check "the branch never gets the lead's import" fails grep -q 'use a;' <(R show child:api2.rs)
check "nor its function" fails grep -q 'fn b()' <(R show child:api2.rs)
check "and keeps the child's" grep -q 'fn a()' <(R show child:api2.rs)
}

# Conflicts: markers and what is between them never reach git, whether the
# child's side is a waiting edit or its own committed line (correctness 3,
# the UX review's serious item); listed; edited out, git sees no revert.
conflicts_never_reach_git() {
built_on_the_lead '// api\nfn keep() {}\n'
lead_drafts '// api\nfn keep() {}\nfn a() { v1 }\n'
printf '// api\nfn keep() {}\nfn a() { child }\n' >"$C/api.rs"
lead_drafts '// api\nfn keep() {}\nfn a() { v2 }\n'
check "the waiting edit conflicts: markers" grep -q '^<<<<<<<' "$C/api.rs"
check "labelled yours, not as waiting edits" grep -q '^<<<<<<< yours' "$C/api.rs"
echo own >"$C/own.txt"; G "$C" add own.txt
eq "git sees nothing of the conflict" "A  own.txt" "$(git -C "$C" status --short)"
G "$C" commit -qam "child: own"
check "the commit holds no marker" fails grep -q '<<<<<<<' <(R show child:api.rs)
eq "nor the lead's lines" "// api|fn keep() {}" "$(R show child:api.rs | paste -sd'|')"
out=$(listing "$C")
check "listed as a conflict" has "$out" "Conflict: api.rs: your changes conflict with lead's newer work"
check "saying git sees none of it" has "$out" "git sees none of your changes to it"
# A committed own line the lead's newer work changes too.
printf '// api\nfn keep() { mine }\nfn a() { v2 }\n' >"$C/api.rs"
G "$C" commit -qam "child: keep"
lead_drafts '// api\nfn keep() { lead }\nfn a() { v2 }\n'
check "the committed line conflicts with the lead's: markers" grep -q '^<<<<<<<' "$C/api.rs"
eq "git sees no change: not the markers, not a revert of its commit" "" "$(git -C "$C" status --short)"
G "$C" reset -q --hard
check "reset --hard: the conflict stays, for the child to resolve" grep -q '^<<<<<<<' "$C/api.rs"
printf '// api\nfn keep() { mine }\nfn a() { v2 }\n' >"$C/api.rs"
eq "resolved for its own line: git sees no revert of it" "" "$(git_sees)"
check "the line, changing the lead's, waits" has "$(listing "$C")" "1 waiting edit in api.rs"
check "no conflict listed" fails has "$(listing "$C")" "Conflict"
}

# Deleting a file the lead changed: git sees it, the guard refuses
# committing it, `git checkout` brings it back (correctness 13), which is
# also how waiting edits are dropped. The lead's new file can't go, nor a
# file of the lead's be moved elsewhere.
deleting_the_leads_files() {
built_on_the_lead '// api\n'
printf 'fn draft() {}\n' >"$L/new.rs"
lead_drafts '// api\nclass Api {\n    fn a() {}\n}\n'
tip=$(R rev-parse child)
rm "$C/api.rs"
eq "deleted: git sees it" " D api.rs" "$(git -C "$C" status --short)"
out=$(G "$C" commit -qam "child: no api" 2>&1); rc=$?
check "committing the deletion: refused" test "$rc" != 0
check "saying why" has "$out" "deletes api.rs, which has lead's uncommitted work"
eq "the branch didn't move" "$tip" "$(R rev-parse child)"
check "listed" has "$(listing "$C")" "api.rs is deleted here, but it has lead's uncommitted work"
check "git checkout brings it back" G "$C" checkout -- api.rs
eq "with the lead's lines" "// api|class Api {|    fn a() {}|}" "$(paste -sd'|' "$C/api.rs")"
eq "git clean" "" "$(git -C "$C" status --short)"
# A waiting edit dropped the same way.
printf '// api\nclass Api {\n    fn a() { mine }\n}\n' >"$C/api.rs"
check "a waiting edit" has "$(listing "$C")" "1 waiting edit in api.rs"
rm "$C/api.rs"; G "$C" checkout -- api.rs
eq "rm, git checkout: as the lead has it" "// api|class Api {|    fn a() {}|}" "$(paste -sd'|' "$C/api.rs")"
check "the waiting edit dropped" fails has "$(listing "$C")" "waiting edit"
check "the lead's new file can't be deleted" fails rm "$C/new.rs"
check "it's still there" test -f "$C/new.rs"
check "nor a file of the lead's moved away" fails mv "$C/api.rs" "$C/other.rs"
eq "git clean" "" "$(git -C "$C" status --short)"
}

# The guard as the authority (correctness 5, 9, 10): a formatter's rewrite
# of the lead's line, by plumbing, is refused with the right words; the
# lead's own commits pass, a fast-forward and a cherry-pick of a frozen
# layer's; a layer adding only new files gets a guard.
the_guard_is_the_authority() {
built_on_the_lead '// api\n'
lead_drafts "// api\n    let total = compute('a', 'b');\n"
tip=$(R rev-parse child)
printf '// api\n    let total = compute("a", "b",);\n' >"$T/fmt.rs"
blob=$(git -C "$C" hash-object -w --no-filters "$T/fmt.rs")
G "$C" update-index --cacheinfo "100644,$blob,api.rs"
out=$(G "$C" commit -qm "child: formatted" 2>&1); rc=$?
check "a formatter's rewrite of the lead's line: refused" test "$rc" != 0
check "naming it" has "$out" "adds lead's uncommitted line"
check "not claiming a tool without the filter only" has "$out" "or a change to lead's lines gitfold took for yours"
eq "the branch didn't move" "$tip" "$(R rev-parse child)"
G "$C" reset -q
# The lead commits: a fast-forward onto it passes.
G "$L" commit -qam "lead: total"
out=$(G "$C" merge -q --ff-only lead 2>&1); rc=$?
eq "git merge --ff-only lead: done" "0" "$rc"
eq "the branch on the lead's commit" "$(R rev-parse lead)" "$(R rev-parse child)"
eq "nothing half-done" "" "$(git -C "$C" status --short)"
turn_starts "$C" >/dev/null

# A frozen layer whose branch committed since: its commit, cherry-picked,
# isn't held against it.
printf '// api\n    let total = compute(1);\nfn later() {}\n' >"$L/api.rs"
turn_ends "$L" >/dev/null
"$BIN" -C "$C" layer add lead --edit --frozen >/dev/null || fail "layer add lead --edit --frozen"
G "$L" commit -qam "lead: later"
out=$(G "$C" cherry-pick lead 2>&1); rc=$?
eq "a cherry-pick of the frozen layer's newer commit: allowed" "0" "$rc"; [ "$rc" = 0 ] || echo "$out"
check "it holds the lead's line" grep -q 'fn later' <(R show child:api.rs)
}

# A layer that only adds new files: the guard is installed all the same.
a_layer_of_new_files_only() {
built_on_the_lead '// api\n'
printf 'fn draft() {}\n' >"$L/new.rs"; turn_ends "$L" >/dev/null; turn_starts "$C" >/dev/null
check "the lead's new file shows" test -f "$C/new.rs"
check "the guard's hooks are installed" test -n "$(git -C "$C" config --worktree core.hooksPath)"
tip=$(R rev-parse child)
out=$(G "$C" add -f new.rs 2>&1; G "$C" commit -qm "child: the lead's file" 2>&1); rc=$?
check "committing the lead's new file: refused" test "$rc" != 0
check "saying so" has "$out" "new.rs"
eq "the branch didn't move" "$tip" "$(R rev-parse child)"
G "$C" reset -q
}

# The lead made see-only: the child's own change to a file holding the
# lead's uncommitted line stays, the lead's line doesn't reach git
# (correctness 2).
see_only_keeps_the_layers_lines_out() {
printf 'fn one() {}\n' >"$REPO/lead.rs"; printf 'x1\nx2\nx3\n' >"$REPO/x.rs"; R add -A; R commit -qm files
R branch lead
commit_on() { R switch -q "$1"; printf '%s\n' "$3" >"$REPO/$2"; R add -A; R commit -qm "$1: $2"; R switch -q main; }
commit_on lead lead.rs "$(printf 'fn one() {}\nfn two() {}')"
"$BIN" -C "$REPO" branch child --edit lead >/dev/null || fail "branch child --edit lead"
commit_on child c.txt mine
mount_fs
L=$MNT/lead C=$MNT/child
turn_starts "$C" >/dev/null
printf 'x1\nx2\nx3\nlead draft\n' >"$L/x.rs"; turn_ends "$L" >/dev/null; turn_starts "$C" >/dev/null
printf 'x1 mine\nx2\nx3\nlead draft\n' >"$C/x.rs"
eq "built on the lead: git sees the own change only" "-x1|+x1 mine" "$(git_sees x.rs)"
out=$("$BIN" -C "$C" layer add lead 2>&1); rc=$?
eq "the lead made see-only" "0" "$rc"
check "git never sees the lead's draft line" fails grep -q 'lead draft' <(git -C "$C" diff)
eq "the own change kept" "-x1|+x1 mine" "$(git_sees x.rs)"
G "$C" commit -qam "child: x"
eq "committed: the own change only" "x1 mine|x2|x3" "$(R show child:x.rs | paste -sd'|')"
}

# Two editable layers on one file: a waiting edit is the lead's or the db's
# by the lines it changes (correctness 14).
whose_waiting_edits() {
printf 'a\nb\nc\nd\ne\nf\n' >"$REPO/api.rs"; R add -A; R commit -qm api
R branch lead; R branch db
"$BIN" -C "$REPO" branch child --edit lead >/dev/null || fail "branch child --edit lead"
mount_fs
L=$MNT/lead D=$MNT/db C=$MNT/child
turn_starts "$C" >/dev/null
"$BIN" -C "$C" layer add db --edit >/dev/null 2>&1 || fail "layer add db --edit"
printf 'a\nlead line\nb\nc\nd\ne\nf\n' >"$L/api.rs"; turn_ends "$L" >/dev/null
printf 'a\nb\nc\nd\ne\ndb line\nf\n' >"$D/api.rs"; turn_ends "$D" >/dev/null
turn_starts "$C" >/dev/null
eq "both lines show" "a|lead line|b|c|d|e|db line|f" "$(paste -sd'|' "$C/api.rs")"
printf 'a\nlead line changed\nb\nc\nd\ne\ndb line\nf\n' >"$C/api.rs"
out=$(listing "$C")
check "the waiting edit is the lead's" has "$out" "1 waiting edit in api.rs: it changes lead's uncommitted lines"
check "not the db's" fails has "$out" "db's uncommitted"
}

# A union file's memo: what git checked out from another branch, then
# reset in the index, is cleaned again (correctness 8); and a branch made
# with --edit shows the layer's work at its first look (surprise 1); and
# the deny hook survives its reader going away (surprise 7).
memo_at_once_and_a_closed_pipe() {
echo 'CHANGELOG merge=union' >"$REPO/.gitattributes"; printf 'v0\n' >"$REPO/CHANGELOG"
R add -A; R commit -qm changelog
R branch api; R branch child
R switch -q api; printf 'v0\nc1\n' >"$REPO/CHANGELOG"; R commit -qam "api: c1"; R switch -q main
mount_fs
A=$MNT/api C=$MNT/child
echo u1 >>"$A/CHANGELOG"
"$BIN" -C "$C" layer add api >/dev/null || fail "layer add api"
eq "the child sees api's lines" "v0|c1|u1" "$(paste -sd'|' "$C/CHANGELOG")"
G "$C" checkout -q api -- CHANGELOG
G "$C" reset -q CHANGELOG
# As in plain git, the file keeps api's committed line as a change; the
# memo, made for the index then, doesn't speak for it now: api's
# uncommitted line stays out.
eq "checked out from api, then reset: git sees the checked-out line only" "+c1" "$(git_sees CHANGELOG)"
G "$C" checkout -q -- CHANGELOG
eq "checked out from the index again: git sees nothing" "" "$(git -C "$C" status --short)"
eq "and the file shows api's lines once each" "v0|c1|u1" "$(paste -sd'|' "$C/CHANGELOG")"

echo 'fn draft() {}' >"$A/draft.rs"
"$BIN" -C "$REPO" branch kid --edit api >/dev/null || fail "branch kid --edit api"
eq "a branch built on api: its work shows at the first look" "fn draft() {}" "$(cat "$MNT/kid/draft.rs" 2>&1)"

echo x >>"$A/a.txt"
"$BIN" -C "$C" layer add api >/dev/null
printf '{"tool_name":"Edit","cwd":"%s","tool_input":{"file_path":"%s"}}' "$C" a.txt |
    "$BIN" layer --claude-hook pre-tool-use 2>"$T/hook.err" | true
check "the deny hook, its reader gone: no panic" fails grep -q panicked "$T/hook.err"
}


section "layers: shown in a workspace" layers_shown_in_a_workspace
section "layers: live, frozen, commits only, revisions; the listing" layers_live_frozen_commits_and_revisions
section "layers: several changing one file" layers_changing_one_file
section "layers: following a workspace as it is now; at once" layers_follow_the_workspace_as_it_is_now
section "layers: of an agent and of a person" layers_of_an_agent_and_of_a_person
section "layers: moving by themselves without an agent" layers_move_by_themselves_without_an_agent
section "layers: union files, the filter and the guard" union_files_and_the_filter
section "snapshot: one workspace's work" snapshot_of_one_workspace
section "snapshot: several merged" snapshot_of_several_merged
section "proposals: a contract changed by consent" proposals_to_change_a_contract
section "proposals: the layer kept as it was" proposals_keep_the_layer_as_it_was
section "layers: on the workspace's own commits" layers_on_the_workspaces_own_commits
section "layers: moving by themselves in a workspace with union files" layers_move_by_themselves_in_a_workspace_with_union_files
section "layers: waiting edits in more files, the safety net" waiting_edits_in_more_files
section "layers: waiting edits without an agent" waiting_edits_without_an_agent
section "safety: waiting edits survive git (stash, reorders)" waiting_edits_survive_git
section "safety: conflicts never reach git" conflicts_never_reach_git
section "safety: deleting the lead's files" deleting_the_leads_files
section "safety: the guard is the authority" the_guard_is_the_authority
section "safety: a layer of new files only" a_layer_of_new_files_only
section "safety: see-only keeps the layer's lines out" see_only_keeps_the_layers_lines_out
section "safety: whose waiting edits" whose_waiting_edits
section "safety: the memo, at once, a closed pipe" memo_at_once_and_a_closed_pipe
finish
