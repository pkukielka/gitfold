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
edit_hook() { printf '{"tool_name":"Edit","cwd":"%s","tool_input":{"file_path":"%s"}}' "$W" "$1" |
    "$BIN" layer --claude-hook pre-tool-use; }
eq "Claude Code's Edit of a layer's file: denied, saying whose" "deny yes" \
    "$(edit_hook a.txt | sed -n 's/.*"permissionDecision":"\([a-z]*\)".*/\1/p') $(edit_hook a.txt | grep -q "workspace la's work" && echo yes)"
eq "of its own file: allowed" "" "$(edit_hook "$W/d/c.txt")"
eq "outside a workspace: allowed" "" "$(edit_hook /tmp/x)"
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
G "$W" update-index --no-skip-worktree a.txt
for _ in $(seq 50); do G "$W" ls-files -v a.txt | grep -q '^S' && break; sleep 0.1; done
eq "a skip-worktree bit git dropped is put back" "S a.txt" "$(G "$W" ls-files -v a.txt)"
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
admin=$COMMON/worktrees/gitfold-ws-me
touch "$admin/index.lock"
out=$("$BIN" -C "$W" layer add la 2>&1); rc=$?
rm -f "$admin/index.lock"
eq "git busy: refused (exit 4), nothing stored" "4 " "$rc $("$BIN" -C "$W" layer list)"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
eq "unmounted: stored, shown when it opens" "me: not mounted; its layers show when it opens" \
    "$("$BIN" -C "$REPO" layer add la -w me)"
mount_fs
for _ in $(seq 50); do [ "$(sed -n 2p "$W/a.txt" 2>/dev/null)" = la ] && break; sleep 0.1; done
eq "shown once mounted" "a
la
again" "$(cat "$W/a.txt")"
eq "and git told: only own changes" "?? own.txt" "$(git -C "$W" status --short)"
}

blended_files_and_the_lens_commit() {
echo 'reg.txt gitfold-blend=hunk' >"$REPO/.gitattributes"; printf 'one\ntwo\n' >"$REPO/reg.txt"
R add -A; R commit -qm blendable
R branch me; R branch la
mount_fs
W=$MNT/me
echo fromA >>"$MNT/la/reg.txt"; echo la >>"$MNT/la/a.txt"
cat "$W/reg.txt" >/dev/null
check "add a layer" "$BIN" -C "$W" layer add la
eq "the blended file has the layer's lines" "one two fromA" "$(tr '\n' ' ' <"$W/reg.txt" | sed 's/ $//')"
check "and is writable" test -w "$W/reg.txt"
check "a whole file isn't" test ! -w "$W/a.txt"
eq "HEAD at the lens commit" "gitfold lens: me + la (commits go to me; never push this commit)" "$(G "$W" log -1 --format=%s)"
eq "git status clean" "" "$(git -C "$W" status --short)"
echo fromMe >>"$W/reg.txt"
eq "git diff: only own lines" "+fromMe" "$(git -C "$W" diff | grep '^+[^+]')"
check "commit" G "$W" commit -qam mine
for _ in $(seq 50); do [ "$(R show me:reg.txt | tail -1)" = fromMe ] && break; sleep 0.1; done
eq "the branch gets only own lines" "one two fromMe" "$(R show me:reg.txt | tr '\n' ' ' | sed 's/ $//')"
eq "git status still clean" "" "$(git -C "$W" status --short)"
eq "HEAD at a new lens commit" "1" "$(G "$W" cat-file -p HEAD | grep -c '^gitfold-lens 1')"
git init -q --bare "$T/remote.git"
check "pushing HEAD is refused" fails G "$W" push -q "$T/remote.git" HEAD:refs/heads/main
check "pushing the branch isn't" G "$W" push -q "$T/remote.git" me
eq "a seal of it: only its own lines" "one two fromMe" \
    "$(R show "$("$BIN" -C "$REPO" seal me):reg.txt" | tr '\n' ' ' | sed 's/ $//')"
echo amended >>"$W/reg.txt"
check "commit, then amend" sh -c "git -C '$W' -c user.name=t -c user.email=t@t commit -qam two &&
    sleep 2 && echo amended2 >>'$W/reg.txt' && git -C '$W' -c user.name=t -c user.email=t@t commit -q --amend -am two"
for _ in $(seq 50); do [ "$(R show me:reg.txt | tail -1)" = amended2 ] && break; sleep 0.1; done
eq "the amend reaches the branch" "one two fromMe amended amended2" "$(R show me:reg.txt | tr '\n' ' ' | sed 's/ $//')"
eq "without the lens mark" "0" "$(R cat-file -p me | grep -c '^gitfold-lens')"
eq "git status still clean" "" "$(git -C "$W" status --short)"
echo moreMe >>"$W/reg.txt"
check "lens off" "$BIN" -C "$W" lens off
eq "the layer's lines gone, own ones kept" "one two fromMe amended amended2 moreMe" "$(tr '\n' ' ' <"$W/reg.txt" | sed 's/ $//')"
eq "back on the branch, the uncommitted line its change" "me| M reg.txt" \
    "$(G "$W" branch --show-current)|$(git -C "$W" status --short)"
}

# Claude Code's read-tracking hook on its Read tool, for each path.
read_file() { for p; do printf '{"tool_name":"Read","cwd":"%s","tool_input":{"file_path":"%s"}}' "$REPO" "$p" |
    "$BIN" track-reads --claude-hook pre-tool-use; done; }

proposals_to_change_a_contract() {
printf 'fn get(id)\n' >"$REPO/api.txt"; R add api.txt; R commit -qm api
for b in contracts c1 c2 c3 c4 c5; do R branch $b; done
mount_fs
echo 'fn put(id)' >>"$MNT/contracts/api.txt"   # the root's contract, uncommitted
echo 'fn mine(id)' >"$MNT/c5/api.txt"          # c5's own version, before the layer
for c in c1 c2 c3 c4 c5; do ls "$MNT/$c" >/dev/null; "$BIN" -C "$REPO" layer add contracts -w $c >/dev/null; done
for c in c1 c2 c3 c5; do "$BIN" -C "$REPO" track-reads $c on >/dev/null; done   # c4: reads unknown
read_file "$MNT/c1/api.txt" "$MNT/c2/api.txt" "$MNT/c3/a.txt" "$MNT/c5/api.txt"
echo 'get(1)' >"$MNT/c1/use.txt"; echo 'put(2)' >"$MNT/c2/use.txt"; echo 'get(4)' >"$MNT/c4/use.txt"
edit_hook() { printf '{"tool_name":"Edit","cwd":"%s","tool_input":{"file_path":"api.txt"}}' "$1" |
    "$BIN" layer --claude-hook pre-tool-use; }
eq "the deny hook offers a proposal first" "yes" \
    "$(edit_hook "$MNT/c1" | grep -q 'To change it, run `gitfold propose api.txt`' && echo yes)"
check "proposing outside a layer's workspace: no such layer (exit 3)" \
    sh -c "cd '$MNT/c1' && '$BIN' propose api.txt --layer c2; [ \$? = 3 ]"
out=$(cd "$MNT/c1" && "$BIN" propose api.txt 2>&1)
eq "propose: a proposal branch, where to edit" "yes yes" \
    "$(echo "$out" | grep -q 'branch contracts-prop/c1' && echo yes) $(echo "$out" | grep -qx "  $MNT/contracts-prop/c1/api.txt" && echo yes)"
eq "recorded as a proposal" "contracts c1" \
    "$(R config branch.contracts-prop/c1.gitfoldProposal) $(R config branch.contracts-prop/c1.gitfoldProposedBy)"
eq "shown in place of the layer" "on 	contracts-prop/c1" "$("$BIN" -C "$MNT/c1" layer list | cut -f1,2)"
eq "a copy of the layer's work, uncommitted included" "fn get(id)
fn put(id)" "$(cat "$MNT/contracts-prop/c1/api.txt")"
printf 'fn fetch(id)\nfn put(id)\n' >"$MNT/contracts-prop/c1/api.txt"; echo 'fetch(1)' >"$MNT/c1/use.txt"
eq "the deny hook: edit the proposal there" "yes" \
    "$(edit_hook "$MNT/c1" | grep -q "shows your proposal contracts-prop/c1.*$MNT/contracts-prop/c1/api.txt" && echo yes)"
(cd "$MNT/c1" && "$BIN" layer --claude-hook user-prompt-submit >/dev/null)
eq "the next turn shows the edits" "fn fetch(id)" "$(head -1 "$MNT/c1/api.txt")"
mkdir -p "$T/from"; printf 'fn fetch(id)\nfn put(id)\n' >"$T/from/api.txt"
check "propose again, --from a directory: the same proposal" \
    sh -c "'$BIN' -C '$REPO' propose -w c1 api.txt --layer contracts-prop/c1 --from '$T/from' | grep -q 'copied into it: api.txt'"
out=$("$BIN" -C "$REPO" proposals)
eq "proposals: who, what, consent" "contracts-prop/c1: proposed by c1, to change contracts|yes|  consent: not checked (\`gitfold consent\`)" \
    "$(echo "$out" | head -1)|$(echo "$out" | grep -q 'api.txt | 2 +-' && echo yes)|$(echo "$out" | tail -1)"
eq "proposals of another layer: none" "" "$("$BIN" -C "$REPO" proposals c2)"
TEST='f=$(cut -d"(" -f1 use.txt 2>/dev/null) || exit 0; grep -q "fn $f(" api.txt || { echo "no fn $f"; exit 1; }'
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 2>&1); rc=$?
eq "consent: a workspace's own version conflicts (exit 4)" "4 c5	yes	no	-	conflicts: api.txt" \
    "$rc $(echo "$out" | grep '^c5')"
"$BIN" -C "$REPO" layer rm contracts -w c5 >/dev/null
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 --test "$TEST" 2>&1); rc=$?
eq "consent --test: who read it, merged, tested (exit 4)" "4|workspace	read	merges	test
c1	yes	yes	passed
c2	yes	yes	passed
c3	no	-	-
c4	unknown	yes	failed	exit 1: no fn get" "$rc|$(echo "$out" | sed -n '1,5p' | sed 's/ (.*//')"
eq "the workspaces' own trees untouched" "A	use.txt|get(4)" "$("$BIN" -C "$REPO" changes c4)|$(cat "$MNT/c4/use.txt")"
eq "proposals: the failed consent" "  consent: failed: 1 of 3 failing: c4" "$("$BIN" -C "$REPO" proposals contracts | tail -1)"
echo 'put(4)' >"$MNT/c4/use.txt"
out=$("$BIN" -C "$REPO" consent contracts-prop/c1 --test "$TEST" 2>&1); rc=$?
eq "fixed: consent passes (exit 0)" "0 contracts-prop/c1: passed (3 workspaces checked)" "$rc $(echo "$out" | tail -1)"
check "accept in another workspace: refused (exit 3)" \
    sh -c "'$BIN' -C '$REPO' accept contracts-prop/c1 -w c2; [ \$? = 3 ]"
check "accept of no proposal (exit 3)" sh -c "'$BIN' -C '$REPO' accept c2 -w contracts; [ \$? = 3 ]"
echo 'fn del(id)' >>"$MNT/contracts/api.txt"
out=$("$BIN" -C "$MNT/contracts" accept contracts-prop/c1 2>&1); rc=$?
eq "the layer changed the file since: refused (exit 4)" "4 yes" "$rc $(echo "$out" | grep -q 'contracts changed api.txt since' && echo yes)"
printf 'fn get(id)\nfn put(id)\n' >"$MNT/contracts/api.txt"
out=$("$BIN" -C "$MNT/contracts" accept contracts-prop/c1 2>&1); rc=$?
eq "accept" "0 accepted contracts-prop/c1 into contracts: api.txt" "$rc $(echo "$out" | head -1)"
eq "written into the layer's workspace, uncommitted" "fn fetch(id)
fn put(id)| M api.txt" "$(cat "$MNT/contracts/api.txt")|$(git -C "$MNT/contracts" status --short)"
eq "the proposer shows the layer again, with the change" "on 	contracts|fn fetch(id)" \
    "$("$BIN" -C "$MNT/c1" layer list | cut -f1,2)|$(head -1 "$MNT/c1/api.txt")"
check "the proposal's branch is gone" fails R rev-parse -q --verify refs/heads/contracts-prop/c1
eq "and its config" "" "$(R config --get-regexp 'branch\.contracts-prop' 2>&1)"
eq "proposals: none left" "" "$("$BIN" -C "$REPO" proposals)"
msg=$("$BIN" -C "$REPO" inbox c2)
eq "readers told: from the layer, the changed lines" "from: contracts|subject: contract changed|yes|  -fn get(id)|  +fn fetch(id)" \
    "$(echo "$msg" | sed -n 1p)|$(echo "$msg" | sed -n 3p)|$(echo "$msg" | grep -q '^contract api.txt changed' && echo yes)|$(echo "$msg" | grep -- '-fn')|$(echo "$msg" | grep -- '+fn')"
eq "the proposer too (it read it)" "1" "$("$BIN" -C "$REPO" inbox c1 | grep -c '^subject: contract changed')"
eq "not who didn't read it, or isn't known to" "|" "$("$BIN" -C "$REPO" inbox c3)|$("$BIN" -C "$REPO" inbox c4)"
(cd "$MNT/contracts" && "$BIN" seal --claude-hook stop)
(cd "$MNT/c2" && "$BIN" layer --claude-hook user-prompt-submit >/dev/null)
eq "the others see it at their next turn" "fn fetch(id)" "$(head -1 "$MNT/c2/api.txt")"
"$BIN" -C "$MNT/c2" propose api.txt >/dev/null
check "reject in a third workspace: refused (exit 3)" \
    sh -c "'$BIN' -C '$REPO' reject contracts-prop/c2 -w c3; [ \$? = 3 ]"
check "reject" "$BIN" -C "$MNT/contracts" reject contracts-prop/c2 --reason "keep fetch"
eq "the layer shown again, the proposer told why, the proposal gone" "on 	contracts|yes|" \
    "$("$BIN" -C "$MNT/c2" layer list | cut -f1,2)|$("$BIN" -C "$REPO" inbox c2 | grep -q 'was rejected: keep fetch' && echo yes)|$(R branch --list 'contracts-prop/*')"
"$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
printf 'fn fetch(id)\nfn put(id)\nfn list()\n' >"$T/from/api.txt"
check "unmounted: propose --from" "$BIN" -C "$REPO" propose -w c2 api.txt --from "$T/from"
eq "shown in place of the layer" "on 	contracts-prop/c2" "$("$BIN" -C "$REPO" layer list -w c2 | cut -f1,2)"
check "and accept" "$BIN" -C "$REPO" accept contracts-prop/c2 -w contracts
eq "written into the stored workspace" "fn list()" "$(R show "$("$BIN" -C "$REPO" seal contracts 2>/dev/null):api.txt" | tail -1)"
eq "the layer back" "contracts" "$("$BIN" -C "$REPO" layer list -w c2 | cut -f2)"
}

section "layers: shown in a workspace" layers_shown_in_a_workspace
section "layers: blended files, the lens commit" blended_files_and_the_lens_commit
section "seal: uncommitted work as a commit" seal_uncommitted_work_as_a_commit
section "mix: workspaces merged into a snapshot" mix_workspaces_into_a_snapshot
section "proposals: a contract changed by consent" proposals_to_change_a_contract
finish
