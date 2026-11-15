#!/usr/bin/env bash
# End-to-end use cases of layers, bases and snapshots, each a scenario a
# user or an agent would run (written up in the layers user guide): a child
# building on a lead's unfinished work, live, frozen and commits-only
# layers, a release tag, two layers on one file, a shared append-only file,
# a denied edit, a stack of PRs, a frozen base, pushed commits, a base that
# landed, snapshots of several agents' work, a proposal.
# usage: tests/usecases.sh (or with the others: tests/run.sh); sections as
# in tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=usecases
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# Claude Code's hooks in workspace directory $1: the turn ends (Stop: a
# snapshot of its work, if someone shows it) or starts (UserPromptSubmit:
# its layers and base move).
turn_ends() { (cd "$1" && echo '{}' | "$BIN" snapshot --claude-hook stop >/dev/null 2>&1); }
turn_starts() { (cd "$1" && "$BIN" layer --claude-hook user-prompt-submit); }
# Claude Code's Edit of file $2 in workspace directory $1, through the deny hook.
edit_hook() { printf '{"tool_name":"Edit","cwd":"%s","tool_input":{"file_path":"%s"}}' "$1" "$2" |
    "$BIN" layer --claude-hook pre-tool-use; }
# The listing of the workspace at $1, times as HH:MM, spaces squeezed.
listing() { "$BIN" -C "$1" layer | sed -E 's/[0-9]{2}:[0-9]{2}/HH:MM/g' | tr -s ' '; }
# The listing's row of layer $2 in the workspace at $1.
row() { listing "$1" | awk -v l="$2" '$1 == l' ; }
# Commits on branch $1 with the main worktree (left on main): file $2 gets
# content $3.
commit_on() {
    R switch -q "$1"; printf '%s\n' "$3" >"$REPO/$2"; R add -A; R commit -qm "$1: $2"; R switch -q main
}
has() { grep -qF -- "$2" <<<"$1"; }

# UC1-UC4, UC12: a child agent builds on the lead's work, live; a frozen
# library, a commits-only view, a release tag, a branch with no workspace.
a_child_builds_on_the_lead() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; R add -A; R commit -qm api
R tag v1.2
echo 'fn v2() {}' >>"$REPO/api/contract.rs"; R commit -qam v2
R branch lead; R branch child; R branch lib; R branch feature-x
mount_fs
L=$MNT/lead C=$MNT/child
echo 'fn draft() {}' >>"$L/api/contract.rs"
check "UC1: show the lead's unfinished work" "$BIN" -C "$C" layer add lead
check "UC1: the child sees the draft" grep -q draft "$C/api/contract.rs"
check "UC1: and can't write it" fails sh -c "echo x >>'$C/api/contract.rs'"
eq "UC1: git sees nothing of it" "" "$(git -C "$C" status --short)"
eq "UC1: listed as all work, live" "lead all work live, last HH:MM" "$(row "$C" lead)"

# UC2: live follows at the turn boundary, frozen doesn't.
echo 'lib v1' >"$MNT/lib/lib.txt"
check "UC2: a frozen library" "$BIN" -C "$C" layer add lib --frozen
echo 'fn second() {}' >>"$L/api/contract.rs"; echo 'lib v2' >"$MNT/lib/lib.txt"
turn_ends "$L"; turn_ends "$MNT/lib"
out=$(turn_starts "$C")
check "UC2: the lead's new work shows after the child's turn starts" grep -q second "$C/api/contract.rs"
eq "UC2: the frozen layer didn't move" "lib v1" "$(cat "$C/lib.txt")"
check "UC2: the hook names the lead" has "$out" lead
check "UC2: and not the frozen layer" fails has "$out" lib
check "UC2: adding it again, frozen, refreshes it" "$BIN" -C "$C" layer add lib --frozen
eq "UC2: refreshed" "lib v2" "$(cat "$C/lib.txt")"
eq "UC2: still frozen" "lib all work frozen at HH:MM" "$(row "$C" lib)"

# UC3: commits only.
check "UC3: show only what the lead committed" "$BIN" -C "$C" layer add lead --commits
check "UC3: the uncommitted draft is gone" fails grep -q draft "$C/api/contract.rs"
eq "UC3: one row for the lead, commits only" "lead commits only live, last HH:MM" "$(row "$C" lead)"
G "$L" commit -qam "lead: contract"
out=$(turn_starts "$C")
check "UC3: the commit shows at the next turn" grep -q draft "$C/api/contract.rs"

# UC4: a release tag. One the base has already shows nothing as a layer:
# refused, pointing to a snapshot; one ahead of it shows.
out=$("$BIN" -C "$MNT/lib" layer add v1.2 2>&1); rc=$?
eq "UC4: a tag the base has: refused (exit 4)" "4" "$rc"
check "UC4: pointing to a snapshot of it" has "$out" "gitfold snapshot v1.2"
p=$("$BIN" -C "$REPO" snapshot v1.2)
eq "UC4: which shows the tag as it was" "fn v1() {}" "$(cat "$p/api/contract.rs")"
R switch -q -c rel main; echo 'fn v3() {}' >>"$REPO/api/contract.rs"; R commit -qam v3; R tag v1.3; R switch -q main
check "UC4: a tag ahead of the base" "$BIN" -C "$MNT/lib" layer add v1.3
check "UC4: its change shows" grep -q v3 "$MNT/lib/api/contract.rs"
eq "UC4: listed as fixed" "v1.3 commits fixed" "$(row "$MNT/lib" v1.3)"
check "UC4: removed" "$BIN" -C "$MNT/lib" layer remove v1.3
check "UC4: gone" fails grep -q v3 "$MNT/lib/api/contract.rs"

# UC12: a branch with no workspace.
commit_on feature-x fx.txt one
check "UC12: a branch with no workspace" "$BIN" -C "$C" layer add feature-x
eq "UC12: its commits show" "one" "$(cat "$C/fx.txt")"
eq "UC12: listed as commits, live" "feature-x commits live, last HH:MM" "$(row "$C" feature-x)"
commit_on feature-x fx.txt two
turn_starts "$C" >/dev/null
eq "UC12: its new commit shows at the next turn" "two" "$(cat "$C/fx.txt")"
}

# UC5, UC8, UC9, UC13: changing a layer's mode, own files win, getting git
# back, mistakes.
modes_own_files_and_mistakes() {
R branch me; R branch b
mount_fs
W=$MNT/me
echo b >>"$MNT/b/a.txt"; echo mine >>"$W/a.txt"; echo bb >"$MNT/b/b.txt"
for how in "" --frozen --commits ""; do
    "$BIN" -C "$W" layer add b $how >/dev/null
done
eq "UC5: re-adding replaces: one row" "1" "$(listing "$W" | awk '$1 == "b"' | wc -l)"
eq "UC5: in the last mode" "b all work live, last HH:MM" "$(row "$W" b)"
eq "UC8: my own file wins" "a
mine" "$(cat "$W/a.txt")"
check "UC8: and stays writable" sh -c "echo more >>'$W/a.txt'"
eq "UC8: git sees only my change" " M a.txt" "$(git -C "$W" status --short)"
eq "UC13: an unknown name: exit 3" "3" "$("$BIN" -C "$W" layer add nosuch >/dev/null 2>&1; echo $?)"
check "UC13: a workspace can't show itself" fails "$BIN" -C "$W" layer add me
eq "UC13: removing what isn't a layer: exit 3" "3" "$("$BIN" -C "$W" layer remove nosuch >/dev/null 2>&1; echo $?)"
check "UC9: remove --all" "$BIN" -C "$W" layer remove --all
check "UC9: the layer's files are gone" test ! -e "$W/b.txt"
eq "UC9: git as before" " M a.txt" "$(git -C "$W" status --short)"
out=$(listing "$W")
check "UC9: the listing says the base" has "$out" "Base: main"
check "UC9: and no layers" has "$out" "No layers."
}

# UC6, UC7, UC11: two layers on one file merge; a conflict shows markers
# and is reported; what can't be merged shows the base's; the deny hook
# names both.
two_layers_on_one_file() {
printf 'one\ntwo\nthree\nfour\nfive\n' >"$REPO/util.rs"; echo 'x = 1' >"$REPO/config.rs"
echo old >"$REPO/old.md"; R add -A; R commit -qm files
R branch me; R branch b; R branch c
mount_fs
W=$MNT/me
printf 'b0\none\ntwo\nthree\nfour\nfive\n' >"$MNT/b/util.rs"
printf 'one\ntwo\nthree\nfour\nfive\nc9\n' >"$MNT/c/util.rs"
echo 'x = 2' >"$MNT/b/config.rs"; echo 'x = 3' >"$MNT/c/config.rs"
rm "$MNT/b/old.md"; echo changed >"$MNT/c/old.md"
"$BIN" -C "$W" layer add b >/dev/null; "$BIN" -C "$W" layer add c >/dev/null
eq "UC6: changes to different lines merge" "b0 one two three four five c9" "$(cat "$W/util.rs" | paste -sd' ')"
check "UC6: the merged file is read-only" fails sh -c "echo x >>'$W/util.rs'"
check "UC6: a conflict shows git's markers" grep -q '^<<<<<<<' "$W/config.rs"
check "UC6: read-only too" fails sh -c "echo x >>'$W/config.rs'"
out=$(listing "$W")
check "UC6: the listing reports the conflict" has "$out" "config.rs is changed by both b and c"
check "UC6: not the clean merge" fails has "$out" "util.rs is"
eq "UC7: a deletion against a change shows the base's version" "old" "$(cat "$W/old.md")"
check "UC7: and is reported" has "$out" "old.md"
out=$(edit_hook "$W" config.rs)
check "UC11: editing it is denied" has "$out" '"permissionDecision":"deny"'
check "UC11: naming both layers" has "$out" "b and c"
check "UC11: offering a proposal" has "$out" "gitfold propose"
check "UC11: and gitfold layer remove" has "$out" "gitfold layer remove"
check "UC11: in today's words" fails sh -c "grep -Eqi 'lens|seal' <<<'$out'"
check "UC6: removing one layer" "$BIN" -C "$W" layer remove c
eq "UC6: the other's version, no conflict" "x = 2" "$(cat "$W/config.rs")"
check "UC6: the conflict line gone" fails has "$(listing "$W")" "Conflict"
}

# UC10: a file everyone appends to.
an_append_only_file() {
printf 'CHANGELOG.md merge=union\n' >"$REPO/.gitattributes"; printf '# log\n' >"$REPO/CHANGELOG.md"
R add -A; R commit -qm log
R branch lead; R branch child
mount_fs
L=$MNT/lead C=$MNT/child
echo '- lead: api' >>"$L/CHANGELOG.md"
"$BIN" -C "$C" layer add lead >/dev/null
check "UC10: the child sees the lead's line" grep -q 'lead: api' "$C/CHANGELOG.md"
check "UC10: and can append its own" sh -c "echo '- child: ui' >>'$C/CHANGELOG.md'"
eq "UC10: both lines" "# log|- lead: api|- child: ui" "$(paste -sd'|' "$C/CHANGELOG.md")"
eq "UC10: git diff has only its own line" "+- child: ui" "$(git -C "$C" diff | grep '^+-')"
G "$C" commit -qam "child: ui"
sleep 2   # the commit is moved onto the branch by the sync thread
eq "UC10: the branch's commit holds only its own line" "# log|- child: ui" "$(R show child:CHANGELOG.md | paste -sd'|')"
}

# UC14-UC18: a stack of PRs.
a_stack_of_prs() {
R branch api
commit_on api api.txt one
out=$("$BIN" -C "$REPO" branch ui --base api)
check "UC14: ui built on api" has "$out" "on api"
commit_on ui ui.txt one
mount_fs
U=$MNT/ui
commit_on api api.txt review-fix
out=$(turn_starts "$U")
eq "UC14: ui moved onto api's fix at its turn" "$(R rev-parse api)" "$(R rev-parse ui^)"
eq "UC14: its own commit kept" "one|review-fix" "$(cat "$U/ui.txt")|$(cat "$U/api.txt")"
tree=$("$BIN" -C "$REPO" list)
check "UC14: list shows ui under api" has "$tree" "└─ ui"

# UC15: frozen.
check "UC15: freeze the base" "$BIN" -C "$U" layer base api --frozen
commit_on api api.txt another
turn_starts "$U" >/dev/null
check "UC15: frozen: the turn doesn't move it" fails test "$(R rev-parse ui^)" = "$(R rev-parse api)"
check "UC15: the listing says frozen" has "$(listing "$U")" "Base: api, frozen"
check "UC15: asked again, it moves once" "$BIN" -C "$U" layer base api --frozen
eq "UC15: on api's tip" "$(R rev-parse api)" "$(R rev-parse ui^)"
check "UC15: still frozen" has "$(listing "$U")" "Base: api, frozen"
check "UC15: live again" "$BIN" -C "$U" layer base api
check "UC15: the listing says live" has "$(listing "$U")" "Base: api, live"

# UC16: pushed commits.
git init -q --bare "$T/remote.git"; R remote add origin "$T/remote.git"
R push -q origin ui 2>/dev/null
R branch other main; commit_on other o.txt o
out=$("$BIN" -C "$U" layer base other 2>&1); rc=$?
eq "UC16: pushed commits: refused, exit 4" "4" "$rc"
check "UC16: saying what it would move" has "$out" "moves 1 commit of ui onto other"
check "UC16: and how to go on" has "$out" "--yes"
check "UC16: --yes moves it" "$BIN" -C "$U" layer base other --yes
eq "UC16: on other" "$(R rev-parse other)" "$(R rev-parse ui^)"
check "UC16: never pushed" test "$(git -C "$T/remote.git" rev-parse ui)" != "$(R rev-parse ui)"
"$BIN" -C "$U" layer base api --yes >/dev/null

# UC17: the base landed (squash-merged into main).
R merge -q --squash api >/dev/null; R commit -qm "api (squash)"
out=$(turn_starts "$U")
check "UC17: the turn says api looks landed in main" has "$out" "looks squash- or rebase-merged into main"
check "UC17: and how to follow it" has "$out" "gitfold layer base api\` moves ui onto main"
check "UC17: not moved unasked while api is still there" test "$(R rev-parse main)" != "$(R rev-parse ui^)"
check "UC17: list says it too" has "$("$BIN" -C "$REPO" list)" "api looks landed in main"
check "UC17: asked, it follows the landing" "$BIN" -C "$U" layer base api --yes
eq "UC17: ui now on main" "$(R rev-parse main)" "$(R rev-parse ui^)"
eq "UC17: with only its own commit" "ui.txt" "$(R diff --name-only main ui)"

# UC18: off the stack.
R branch top main; commit_on top t.txt t
"$BIN" -C "$REPO" layer base top -w ui --yes >/dev/null
check "UC18: layer base main takes it off the stack" "$BIN" -C "$REPO" layer base main -w ui --yes
eq "UC18: only its own commit on main" "$(R rev-parse main)|ui.txt" "$(R rev-parse ui^)|$(R diff --name-only main ui)"
check "UC18: no base in the config" fails R config branch.ui.gitfoldParent
out=$("$BIN" -C "$REPO" layer base main -w ui 2>&1)
check "UC18: again: nothing to do, said so" has "$out" "ui"
eq "UC18: and nothing moved" "$(R rev-parse main)" "$(R rev-parse ui^)"
}

# UC19: a base and a layer of the same branch.
a_base_and_a_layer() {
R branch api
commit_on api api.txt one
"$BIN" -C "$REPO" branch ui --base api >/dev/null
mount_fs
U=$MNT/ui
echo wip >>"$MNT/api/api.txt"
out=$("$BIN" -C "$U" layer add api 2>&1); rc=$?
if [ "$rc" = 0 ]; then
    check "UC19: shown: api's work on top" grep -q wip "$U/api.txt"
    check "UC19: git still clean" test -z "$(git -C "$U" status --short)"
else
    check "UC19: refused with a reason" test -n "$out"
    eq "UC19: nothing changed" "one" "$(cat "$U/api.txt")"
fi
eq "UC19: the branch untouched" "$(R rev-parse api)" "$(R rev-parse ui)"
}

# UC20-UC22: snapshots.
snapshots_of_work() {
R branch a; R branch b; R branch c
mount_fs
echo a >"$MNT/a/a-only.txt"; echo b >"$MNT/b/b-only.txt"; echo c >>"$MNT/c/d/c.txt"
p=$("$BIN" -C "$REPO" snapshot a b c)
eq "UC20: a path in the mount" "$MNT/@" "${p%@*}@"
eq "UC20: all three agents' work" "a|b|c
c" "$(cat "$p/a-only.txt")|$(cat "$p/b-only.txt")|$(cat "$p/d/c.txt")"
check "UC20: read-only" fails sh -c "echo x >'$p/x'"
eq "UC20: no workspace changed" "A	a-only.txt" "$("$BIN" -C "$REPO" changes a)"
echo x >>"$MNT/a/a.txt"; echo y >>"$MNT/b/a.txt"
out=$("$BIN" -C "$REPO" snapshot a b 2>&1); rc=$?
eq "UC21: a conflict: exit 4, the path named" "4 yes" "$rc $(has "$out" a.txt && echo yes)"
p=$("$BIN" -C "$REPO" snapshot a)
eq "UC22: one agent's work, uncommitted" "a
x" "$(cat "$p/a.txt")"
eq "UC22: the same work, the same path" "$p" "$("$BIN" -C "$REPO" snapshot a)"
}

# UC23: a child proposes a change to the lead's contract.
a_proposal() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; R add -A; R commit -qm api
R branch lead; R branch child
mount_fs
L=$MNT/lead C=$MNT/child
echo 'fn draft() {}' >>"$L/api/contract.rs"
"$BIN" -C "$C" layer add lead >/dev/null
out=$("$BIN" -C "$C" propose api/contract.rs 2>&1)
check "UC23: a proposal made" test $? = 0
prop=$(listing "$C" | awk '/^LAYER/{t=1;next} t&&NF{print $1; exit}')
check "UC23: shown in the lead's place" test "$prop" != lead
echo 'fn wanted() {}' >>"$MNT/$prop/api/contract.rs"
check "UC23: the lead checks it" "$BIN" -C "$L" consent "$prop"
check "UC23: and takes it" "$BIN" -C "$L" accept "$prop"
check "UC23: the lead's file has it" grep -q wanted "$L/api/contract.rs"
turn_ends "$L"; turn_starts "$C" >/dev/null
check "UC23: the child sees it after its turn" grep -q wanted "$C/api/contract.rs"
eq "UC23: the lead shown again" "lead" "$(listing "$C" | awk '/^LAYER/{t=1;next} t&&NF{print $1; exit}')"
}

section "use cases: a child builds on the lead" a_child_builds_on_the_lead
section "use cases: modes, own files, mistakes" modes_own_files_and_mistakes
section "use cases: two layers on one file" two_layers_on_one_file
section "use cases: an append-only file" an_append_only_file
section "use cases: a stack of PRs" a_stack_of_prs
section "use cases: a base and a layer" a_base_and_a_layer
section "use cases: snapshots" snapshots_of_work
section "use cases: a proposal" a_proposal
finish
