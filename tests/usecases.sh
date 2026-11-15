#!/usr/bin/env bash
# End-to-end use cases of layers, bases and snapshots, each a scenario a
# user or an agent would run (written up in the layers user guide): a child
# building on a lead's unfinished work, live, frozen and commits-only
# layers, a release tag, two layers on one file, a base that moved on since
# the layers started, a shared append-only file, a denied edit, a stack of
# PRs, a frozen base, pushed commits, a base that landed, a base shown as a
# layer too, a child on the lead's branch, a layer and a blended file on a
# branch with a base, snapshots of several agents' work, a proposal, a
# person working without an agent, a child denied editing the lead's
# class and offered to build on it, a list file not marked merge=union,
# a registry entry among another layer's, a method inside another layer's
# class in a blended file.
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
ls "$C" >/dev/null; turn_starts "$C" >/dev/null   # an agent's: its layers move at its turns
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
eq "UC13: a workspace can't show itself: a usage error (exit 2)" "2" "$("$BIN" -C "$W" layer add me >/dev/null 2>&1; echo $?)"
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

# UC26: the base moved on since the layers started. A layer brings only its
# own changes: the base's newer fixes stay, two layers that started from
# different older commits keep all their changes, and a change to a line
# the base changed too conflicts with the base. A branch that shares no
# history with the base can't be a layer.
the_base_moved_on() {
printf 'one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n' >"$REPO/util.rs"; echo 'x = 1' >"$REPO/config.rs"
R add -A; R commit -qm files
R branch me; R branch b
commit_on b util.rs "$(printf 'one\ntwo\nthree\nfour\nfive B\nsix\nseven\neight')"
commit_on b config.rs 'x = 2'
commit_on me util.rs "$(printf 'one FIX\ntwo\nthree\nfour\nfive\nsix\nseven\neight')"
R branch c me
commit_on c util.rs "$(printf 'one FIX\ntwo\nthree C\nfour\nfive\nsix\nseven\neight')"
commit_on me util.rs "$(printf 'one FIX\ntwo\nthree\nfour\nfive\nsix\nseven\neight FIX')"
commit_on me config.rs 'x = 9'
R switch -q --orphan lonely; R rm -rqf .; echo alone >"$REPO/alone.txt"; R add -A; R commit -qm alone
R switch -q main
mount_fs
W=$MNT/me
"$BIN" -C "$W" layer add b >/dev/null
eq "UC26: the base's fix stays, the layer's change shows" "one FIX two three four five B six seven eight FIX" \
    "$(paste -sd' ' "$W/util.rs")"
"$BIN" -C "$W" layer add c >/dev/null
eq "UC26: two layers from different older commits: every change" \
    "one FIX two three C four five B six seven eight FIX" "$(paste -sd' ' "$W/util.rs")"
check "UC26: read-only" fails sh -c "echo x >>'$W/util.rs'"
check "UC26: a change to a line the base changed too: git's markers" grep -q '^<<<<<<< base' "$W/config.rs"
out=$(listing "$W")
check "UC26: listed as a conflict with the base" has "$out" "Conflict: config.rs: b's change conflicts with your base"
check "UC26: not the clean merge" fails has "$out" "util.rs"
check "UC26: the deny hook says so" has "$(edit_hook "$W" config.rs)" "b's change conflicts with your base"
eq "UC26: git sees none of it" "" "$(git -C "$W" status --short)"
out=$("$BIN" -C "$W" layer add lonely 2>&1); rc=$?
eq "UC26: no shared history: refused (exit 4)" "4" "$rc"
check "UC26: saying why" has "$out" "lonely shares no history with me's base"
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
check "UC15: and what updates it" has "$(listing "$U")" "\`gitfold layer base api --frozen\` updates it"
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
R push -q -f origin ui 2>/dev/null
R merge -q --squash api >/dev/null; R commit -qm "api (squash)"
landing=$(R rev-parse main)
out=$(turn_starts "$U")
check "UC17: the turn says api looks landed in main" has "$out" "looks squash- or rebase-merged into main"
check "UC17: and how to follow it: layer base main" has "$out" "\`gitfold layer base main\` moves ui there with only its own commits"
check "UC17: not moved unasked while api is still there" test "$(R rev-parse main)" != "$(R rev-parse ui^)"
check "UC17: list says it too" has "$("$BIN" -C "$REPO" list)" "api looks landed in main: gitfold layer base main moves it there"
commit_on main later.txt later
out=$("$BIN" -C "$U" layer base main 2>&1); rc=$?
eq "UC17: pushed commits: refused without --yes, exit 4" "4" "$rc"
check "UC17: saying so" has "$out" "--yes moves them"
eq "UC17: nothing changed" "api" "$(R config branch.ui.gitfoldParent)"
check "UC17: with --yes, it follows the landing" "$BIN" -C "$U" layer base main --yes
eq "UC17: ui now on the landing commit" "$landing" "$(R rev-parse ui^)"
eq "UC17: with only its own commit" "ui.txt" "$(R diff --name-only "$landing" ui)"
check "UC17: no base left" fails R config branch.ui.gitfoldParent
eq "UC17: the listing: a plain Base: main" "Base: main" "$(listing "$U" | head -1)"
check "UC17: list: ui not under main" fails has "$("$BIN" -C "$REPO" list)" "└─ ui"

# UC18: on its own base again, then the base taken away.
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
U=$MNT/ui A=$MNT/api
echo wip >>"$A/api.txt"
check "UC19: a layer of the base, on a branch built on it" "$BIN" -C "$U" layer add api
eq "UC19: api's uncommitted work on top of its commits" "one|wip" "$(paste -sd'|' "$U/api.txt")"
check "UC19: read-only" fails sh -c "echo x >>'$U/api.txt'"
eq "UC19: git sees nothing of it" "" "$(git -C "$U" status --short)"
echo mine >"$U/ui.txt"; G "$U" add ui.txt; G "$U" commit -qm "ui: own"
eq "UC19: ui's commit holds only its own change" "ui.txt" "$(R diff --name-only api ui)"
# api commits that work and goes on: ui moves onto it at its turn, the
# layer stays, showing only what api hasn't committed.
G "$A" commit -qam "api: wip"; echo more >>"$A/api.txt"
turn_ends "$A"
out=$(turn_starts "$U")
check "UC19: the turn says it moved" has "$out" "was moved onto its base api"
eq "UC19: ui on api's new commit" "$(R rev-parse api)" "$(R rev-parse ui^)"
eq "UC19: its own commit kept, alone" "ui.txt|mine" "$(R diff --name-only api ui)|$(cat "$U/ui.txt")"
eq "UC19: the layer shows api's new work" "one|wip|more" "$(paste -sd'|' "$U/api.txt")"
eq "UC19: still a layer, still read-only" "api all work live, last HH:MM|yes" \
    "$(row "$U" api)|$(sh -c "echo x >>'$U/api.txt'" 2>/dev/null && echo no || echo yes)"
eq "UC19: git still sees nothing of it" "" "$(git -C "$U" status --short)"
check "UC19: the base line" has "$(listing "$U")" "Base: api, live your 1 commit sits on api"
check "UC19: when it last moved" has "$(listing "$U")" "last moved HH:MM"
# Asked for: the same.
G "$A" commit -qam "api: more"
check "UC19: layer base api moves it now" "$BIN" -C "$U" layer base api
eq "UC19: on api's tip, the layer there" "$(R rev-parse api)|one|wip|more" \
    "$(R rev-parse ui^)|$(paste -sd'|' "$U/api.txt")"
eq "UC19: git clean" "" "$(git -C "$U" status --short)"
}

# UC27: a child shows the lead's uncommitted contract, then builds on the
# lead's branch: its own commits go onto the lead's, the contract still
# shows on top.
a_child_on_the_leads_branch() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; R add -A; R commit -qm api
R branch lead; R branch child
mount_fs
L=$MNT/lead C=$MNT/child
echo 'fn shared() {}' >"$L/api/shared.rs"; G "$L" add -A; G "$L" commit -qm "lead: shared"
echo 'fn draft() {}' >>"$L/api/contract.rs"
"$BIN" -C "$C" layer add lead >/dev/null
echo 'use shared;' >"$C/client.rs"; G "$C" add client.rs; G "$C" commit -qm "child: client"
check "UC27: the child builds on the lead's branch" "$BIN" -C "$C" layer base lead
eq "UC27: its commit on the lead's" "$(R rev-parse lead)|client.rs" "$(R rev-parse child^)|$(R diff --name-only lead child)"
eq "UC27: the contract still shows, uncommitted part too" "fn v1() {}|fn draft() {}" "$(paste -sd'|' "$C/api/contract.rs")"
check "UC27: read-only" fails sh -c "echo x >>'$C/api/contract.rs'"
check "UC27: what the lead committed is the child's base now: writable" sh -c "echo '// ok' >>'$C/api/shared.rs'"
eq "UC27: git sees only that" " M api/shared.rs" "$(git -C "$C" status --short)"
G "$C" checkout -q -- api/shared.rs
check "UC27: the listing: base and layer" sh -c "'$BIN' -C '$C' layer | grep -q '^Base: lead, live' && '$BIN' -C '$C' layer | grep -q '^lead '"
}

# UC28: a layer of another branch while built on api, with a blended file:
# api's new commits move the branch, the layer stays, blends and the
# layers commit come back.
an_unrelated_layer_on_a_base() {
printf 'CHANGELOG.md merge=union\n' >"$REPO/.gitattributes"; printf '# log\n' >"$REPO/CHANGELOG.md"
R add -A; R commit -qm log
R branch api; R branch c
"$BIN" -C "$REPO" branch ui --base api >/dev/null
mount_fs
U=$MNT/ui A=$MNT/api
echo '- c: thing' >>"$MNT/c/CHANGELOG.md"; echo c >"$MNT/c/c.txt"
"$BIN" -C "$U" layer add c >/dev/null
echo '- ui: screen' >>"$U/CHANGELOG.md"; G "$U" commit -qam "ui: log"
for _ in $(seq 30); do [ "$(R show ui:CHANGELOG.md | wc -l)" = 2 ] && break; sleep 0.1; done
eq "UC28: ui's commit holds only its own line" "# log|- ui: screen" "$(R show ui:CHANGELOG.md | paste -sd'|')"
echo api >"$A/api.txt"; G "$A" add api.txt; G "$A" commit -qm "api: more"
out=$(turn_starts "$U")
check "UC28: moved onto api at the turn" has "$out" "was moved onto its base api"
eq "UC28: on api's tip" "$(R rev-parse api)" "$(R rev-parse ui~1)"
eq "UC28: api's file, c's layer" "api|c" "$(cat "$U/api.txt")|$(cat "$U/c.txt")"
eq "UC28: the blend: c's line and ui's" "# log|- c: thing|- ui: screen" "$(LC_ALL=C sort "$U/CHANGELOG.md" | paste -sd'|')"
check "UC28: HEAD at a layers commit again" has "$(git -C "$U" log -1 --format=%s)" "gitfold layers"
eq "UC28: git sees nothing of c" "" "$(git -C "$U" status --short)"
eq "UC28: ui's commits hold only its own changes" "CHANGELOG.md" "$(R diff --name-only api ui)"
echo '- ui: more' >>"$U/CHANGELOG.md"; G "$U" commit -qam "ui: more"
for _ in $(seq 30); do [ "$(R show ui:CHANGELOG.md | wc -l)" = 3 ] && break; sleep 0.1; done
eq "UC28: commits still go onto the branch, own lines only" "# log|- ui: screen|- ui: more" "$(R show ui:CHANGELOG.md | paste -sd'|')"
# api moves on while ui's newest commit is still on the layers commit:
# the move takes it along, at the first try.
echo api2 >"$A/api2.txt"; G "$A" add api2.txt; G "$A" commit -qm "api: again"
echo '- ui: third' >>"$U/CHANGELOG.md"; G "$U" commit -qam "ui: third"
out=$("$BIN" -C "$U" layer base api 2>&1)
check "UC28: moved at once" has "$out" "ui: moved onto api"
eq "UC28: its three commits on api's tip, own lines only" "$(R rev-parse api)|# log|- ui: screen|- ui: more|- ui: third" \
    "$(R rev-parse ui~3)|$(R show ui:CHANGELOG.md | paste -sd'|')"
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
ls "$C" >/dev/null; turn_starts "$C" >/dev/null   # an agent's: its layers move at its turns
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

# UC29: a person works in a workspace with no agent; the lead's agent works
# on. The person's layer of the lead follows by itself, between the
# person's edits.
a_person_without_an_agent() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; R add -A; R commit -qm api
R branch lead; R branch me
mount_fs
L=$MNT/lead P=$MNT/me
echo 'fn draft() {}' >>"$L/api/contract.rs"
"$BIN" -C "$P" layer add lead >/dev/null
check "UC29: the person sees the lead's draft" grep -q draft "$P/api/contract.rs"
echo 'fn second() {}' >>"$L/api/contract.rs"; turn_ends "$L"
for _ in $(seq 150); do grep -q second "$P/api/contract.rs" && break; sleep 0.1; done
check "UC29: the lead's next turn shows within seconds, no command run" grep -q second "$P/api/contract.rs"
eq "UC29: listed as moved" "lead all work live, last HH:MM" "$(row "$P" lead)"
echo 'fn third() {}' >>"$L/api/contract.rs"; turn_ends "$L"
seen=no
for i in $(seq 10); do
    echo "note $i" >>"$P/notes.txt"; sleep 0.5
    grep -q third "$P/api/contract.rs" && seen=yes
done
eq "UC29: not while the person is editing" "no" "$seen"
for _ in $(seq 150); do grep -q third "$P/api/contract.rs" && break; sleep 0.1; done
check "UC29: but once they pause" grep -q third "$P/api/contract.rs"
eq "UC29: git sees only the person's own file" "?? notes.txt" "$(git -C "$P" status --short)"
}

# Claude Code's tool call $3 (Edit, Write) of file $2 in workspace
# directory $1, through the deny hook, with the tool's other input $4 (JSON
# fields, strings escaped).
tool_hook() { printf '{"tool_name":"%s","cwd":"%s","tool_input":{"file_path":"%s",%s}}' "$3" "$1" "$2" "$4" |
    "$BIN" layer --claude-hook pre-tool-use; }

# UC30: a child edits a class in the lead's work: denied, and offered to
# build on the lead (`layer base lead`), which makes what the lead
# committed the child's to edit; what the lead hasn't committed stays the
# lead's.
editing_the_leads_class() {
mkdir "$REPO/api"; printf 'class Billing {\n}\n' >"$REPO/api/billing.rs"; echo 'fn v1() {}' >"$REPO/api/contract.rs"
R add -A; R commit -qm api
R branch lead; R branch child; R branch other; R tag v1.2
"$BIN" -C "$REPO" branch child2 --base other >/dev/null
mount_fs
L=$MNT/lead C=$MNT/child
printf 'class Billing {\n  fn charge() {}\n}\n' >"$L/api/billing.rs"; G "$L" commit -qam "lead: charge"
echo 'fn draft() {}' >>"$L/api/contract.rs"
"$BIN" -C "$C" layer add lead >/dev/null
out=$(edit_hook "$C" api/billing.rs)
check "UC30: editing the lead's class is denied" has "$out" '"permissionDecision":"deny"'
check "UC30: offering to build on the lead first" has "$out" "build on lead: \`gitfold layer base lead\`: your commits will sit on lead's, and lead's committed code becomes yours to edit"
check "UC30: then a proposal, asking, removing" has "$(sed 's/.*layer base lead//' <<<"$out")" "gitfold propose api/billing.rs"
check "UC30: the change is committed: no caveat" fails has "$out" "hasn't committed"
out=$(edit_hook "$C" api/contract.rs)
check "UC30: an uncommitted change: offered too" has "$out" "gitfold layer base lead"
check "UC30: saying it stays the lead's until committed" has "$out" "lead hasn't committed its change to this file yet: after \`layer base lead\` it stays lead's (read-only) until lead commits it"
"$BIN" -C "$MNT/child2" layer add lead >/dev/null
check "UC30: on a branch with another base: it would replace it" \
    has "$(edit_hook "$MNT/child2" api/billing.rs)" "(it replaces your base other: a branch has one base)"
check "UC30: the child builds on the lead" "$BIN" -C "$C" layer base lead
check "UC30: the committed class is the child's to edit now" sh -c "echo '// mine' >>'$C/api/billing.rs'"
check "UC30: and not denied" fails has "$(edit_hook "$C" api/billing.rs)" deny
out=$(edit_hook "$C" api/contract.rs)
check "UC30: the uncommitted change still the lead's" has "$out" '"permissionDecision":"deny"'
check "UC30: no offer to build on what it is built on" fails has "$out" "layer base lead"
R switch -q -c rel main; echo 'fn v3() {}' >>"$REPO/api/contract.rs"; R commit -qam v3; R tag v1.3; R switch -q main
"$BIN" -C "$MNT/other" layer add v1.3 >/dev/null
out=$(edit_hook "$MNT/other" api/contract.rs)
check "UC30: a tag's file: denied" has "$out" deny
check "UC30: no offer to build on a tag" fails has "$out" "layer base"
}

# UC31: a denied edit that only adds lines to a list file not marked
# merge=union: the hint to mark it.
appending_to_an_unmarked_list() {
printf 'core\nnet\n' >"$REPO/modules.txt"; R add -A; R commit -qm modules
R branch lead; R branch child
mount_fs
L=$MNT/lead C=$MNT/child
printf 'core\nnet\nbilling\n' >"$L/modules.txt"
"$BIN" -C "$C" layer add lead >/dev/null
hint="mark it \`merge=union\` in .gitattributes, and every workspace can add lines to it"
out=$(tool_hook "$C" modules.txt Edit '"old_string":"net\n","new_string":"net\nui\n"')
check "UC31: an appended line: denied" has "$out" deny
check "UC31: with the merge=union hint" has "$out" "$hint"
out=$(tool_hook "$C" modules.txt Write '"content":"core\nnet\nbilling\nui\n"')
check "UC31: Write adding a line: the hint" has "$out" "$hint"
out=$(tool_hook "$C" modules.txt MultiEdit '"edits":[{"old_string":"core","new_string":"auth\ncore"}]')
check "UC31: MultiEdit adding a line: the hint" has "$out" "$hint"
out=$(tool_hook "$C" modules.txt Edit '"old_string":"net\n","new_string":"network\n"')
check "UC31: a changed line: denied" has "$out" deny
check "UC31: without the hint" fails has "$out" merge=union
out=$(tool_hook "$C" modules.txt Edit '"old_string":"net","new_string":"net2"')
check "UC31: text added inside a line: no hint" fails has "$out" merge=union
}

# UC32: a registry entry one workspace adds between another layer's
# entries keeps its place where both show; its branch's commits and its
# snapshot hold only its own line; without the other layer, it is placed
# by the base's lines, and a note says so.
an_entry_among_anothers() {
printf 'registry.txt merge=union\n' >"$REPO/.gitattributes"; printf 'alpha\nzulu\n' >"$REPO/registry.txt"
R add -A; R commit -qm registry
R branch a; R branch b; R branch c
mount_fs
A=$MNT/a B=$MNT/b C=$MNT/c
printf 'alpha\nbravo\ndelta\nzulu\n' >"$A/registry.txt"
"$BIN" -C "$B" layer add a >/dev/null
printf 'alpha\nbravo\ncharlie\ndelta\nzulu\n' >"$B/registry.txt"
eq "UC32: b's file, its line among a's" "alpha bravo charlie delta zulu" "$(paste -sd' ' "$B/registry.txt")"
p=$("$BIN" -C "$REPO" snapshot b)
eq "UC32: b's snapshot holds only its own line" "alpha charlie zulu" "$(paste -sd' ' "$p/registry.txt")"
"$BIN" -C "$C" layer add a >/dev/null; "$BIN" -C "$C" layer add b >/dev/null
eq "UC32: shown with a's, b's line keeps its place" "alpha bravo charlie delta zulu" "$(paste -sd' ' "$C/registry.txt")"
"$BIN" -C "$C" layer remove a >/dev/null; "$BIN" -C "$C" layer add a >/dev/null
eq "UC32: in either order" "alpha bravo charlie delta zulu" "$(paste -sd' ' "$C/registry.txt")"
out=$("$BIN" -C "$C" layer remove a)
eq "UC32: without a: placed by the base's lines" "alpha charlie zulu" "$(paste -sd' ' "$C/registry.txt")"
check "UC32: a note says so" has "$out" "note: b's lines in registry.txt are placed by your base's lines"
check "UC32: the listing too" has "$(listing "$C")" "Note: b's lines in registry.txt are placed by your base's lines"
G "$B" commit -qam "b: charlie"
for _ in $(seq 30); do [ "$(R show b:registry.txt | wc -l)" = 3 ] && break; sleep 0.1; done
eq "UC32: b's commit holds only its own line (placed as extraction does)" "alpha charlie zulu" "$(R show b:registry.txt | LC_ALL=C sort | paste -sd' ')"
}

# UC33: a method added inside another layer's class, in a file marked
# merge=union: blended, with a warning that the file may not be a list.
a_method_inside_anothers_class() {
printf 'shapes.rs merge=union\n' >"$REPO/.gitattributes"; printf '// shapes\n' >"$REPO/shapes.rs"
R add -A; R commit -qm shapes
R branch a; R branch b; R branch c
mount_fs
A=$MNT/a B=$MNT/b C=$MNT/c
printf '// shapes\nimpl Circle {\n    fn area() {}\n}\n' >"$A/shapes.rs"
"$BIN" -C "$B" layer add a >/dev/null
printf '// shapes\nimpl Circle {\n    fn area() {}\n    fn radius() {}\n}\n' >"$B/shapes.rs"
warning="sit among a's: this file may not be a list (it's marked merge=union)"
check "UC33: the author's listing warns" has "$(listing "$B")" "Warning: your lines in shapes.rs $warning"
"$BIN" -C "$C" layer add a >/dev/null
out=$("$BIN" -C "$C" layer add b)
check "UC33: the method shows inside the class" has "$(paste -sd'|' "$C/shapes.rs")" "fn area() {}|    fn radius() {}|}"
check "UC33: a note warns" has "$out" "note: b's lines in shapes.rs $warning"
check "UC33: the listing too" has "$(listing "$C")" "Warning: b's lines in shapes.rs $warning"
printf '// shapes\nimpl Circle {\n    fn area() {}\n}\nfn helper() {}\n' >"$B/shapes.rs"
"$BIN" -C "$C" layer add b >/dev/null
check "UC33: a function after a's block, at the end: no warning" fails has "$(listing "$C")" Warning
}

section "use cases: a child builds on the lead" a_child_builds_on_the_lead
section "use cases: modes, own files, mistakes" modes_own_files_and_mistakes
section "use cases: two layers on one file" two_layers_on_one_file
section "use cases: the base moved on" the_base_moved_on
section "use cases: an append-only file" an_append_only_file
section "use cases: a stack of PRs" a_stack_of_prs
section "use cases: a base and a layer" a_base_and_a_layer
section "use cases: a child on the lead's branch" a_child_on_the_leads_branch
section "use cases: an unrelated layer on a base" an_unrelated_layer_on_a_base
section "use cases: snapshots" snapshots_of_work
section "use cases: a proposal" a_proposal
section "use cases: a person without an agent" a_person_without_an_agent
section "use cases: editing the lead's class" editing_the_leads_class
section "use cases: appending to an unmarked list" appending_to_an_unmarked_list
section "use cases: an entry among another's" an_entry_among_anothers
section "use cases: a method inside another's class" a_method_inside_anothers_class
finish
