#!/usr/bin/env bash
# End-to-end use cases of layers and snapshots, each a scenario a user or an
# agent would run, numbered V1-V21 as in the layers design's use cases:
# see-only layers (V1-V5), editable layers: building on another branch,
# waiting edits, git working normally, the guard, landings and pushed
# commits (V6-V17), union files (V18, V19), snapshots and proposals (V20,
# V21).
# usage: tests/usecases.sh (or with the others: tests/run.sh); sections as
# in tests/lib.sh, each on a fresh repository (tests/fixture.sh): SECTION=V7
# runs "V7 ..." only, SECTION="V1 " V1 alone (not V10-V19).
# Where the design doesn't fix a message's words, the checks look for key
# words in it, not whole lines.
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=usecases
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# git as a user runs it in a workspace: never an editor, never stuck.
export GIT_EDITOR=true
G() { timeout 60 git -C "$1" -c user.name=t -c user.email=t@t "${@:2}"; }

# Claude Code's hooks in workspace directory $1: the turn ends (Stop: a
# snapshot of its work, if someone shows it) or starts (UserPromptSubmit:
# its layers move).
turn_ends() { (cd "$1" && echo '{}' | "$BIN" snapshot --claude-hook stop >/dev/null 2>&1); }
turn_starts() { (cd "$1" && "$BIN" layer --claude-hook user-prompt-submit); }
# Makes workspace directory $1 an agent's: its layers move at its turns.
an_agent() { ls "$1" >/dev/null; turn_starts "$1" >/dev/null; }
# Claude Code's Edit of file $2 in workspace directory $1, through the deny hook.
edit_hook() { printf '{"tool_name":"Edit","cwd":"%s","tool_input":{"file_path":"%s"}}' "$1" "$2" |
    "$BIN" layer --claude-hook pre-tool-use; }
# Claude Code's tool call $3 (Edit, Write) of file $2 in workspace
# directory $1, through the deny hook, with the tool's other input $4 (JSON
# fields, strings escaped).
tool_hook() { printf '{"tool_name":"%s","cwd":"%s","tool_input":{"file_path":"%s",%s}}' "$3" "$1" "$2" "$4" |
    "$BIN" layer --claude-hook pre-tool-use; }
# The listing of the workspace at $1, times as HH:MM, spaces squeezed.
listing() { "$BIN" -C "$1" layer | sed -E 's/[0-9]{2}:[0-9]{2}/HH:MM/g' | tr -s ' '; }
# The listing's row of layer $2 in the workspace at $1.
row() { listing "$1" | awk -v l="$2" '$1 == l' ; }
# Commits on branch $1 with the main worktree (left on main): file $2 gets
# content $3. Only for branches whose workspace was never opened.
commit_on() {
    R switch -q "$1" || return; printf '%s\n' "$3" >"$REPO/$2"; R add -A; R commit -qm "$1: $2"; R switch -q main
}
has() { grep -qF -- "$2" <<<"$1"; }
# Text $1 has $2, before $3.
before() {
    [[ $1 == *"$2"* && $1 == *"$3"* ]] || return 1
    local a=${1%%"$2"*} b=${1%%"$3"*}
    [ ${#a} -lt ${#b} ]
}
# File $1 opens for writing (nothing written); `fails can_write` for read-only.
can_write() { (: >>"$1") 2>/dev/null; }
# The parents of commit $1, sorted, on one line.
parents() { R rev-list --parents -n1 "$1" | cut -d' ' -f2- | tr ' ' '\n' | sort | paste -sd' '; }
# Revisions $@ as ids, sorted, on one line.
ids() { for r; do R rev-parse "$r"; done | sort | paste -sd' '; }
# Waits up to 3 seconds for branch $1's file $2 to have $3 lines.
lines_on() { for _ in $(seq 30); do [ "$(R show "$1:$2" | wc -l)" = "$3" ] && return; sleep 0.1; done; }

## See-only layers

# V1: a child shows the lead's unfinished API: it sees the draft, can't
# write it, and is offered to build on the lead first.
v1_see_the_leads_draft() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; printf 'class Billing {\n}\n' >"$REPO/api/billing.rs"
R add -A; R commit -qm api
R branch lead; R branch child; R branch other
mount_fs
L=$MNT/lead C=$MNT/child
an_agent "$C"
printf 'class Billing {\n    fn charge() {}\n}\n' >"$L/api/billing.rs"; G "$L" commit -qam "lead: charge"
echo 'fn draft() {}' >>"$L/api/contract.rs"
check "V1: show the lead's work" "$BIN" -C "$C" layer add lead
check "V1: the child sees the draft" grep -q draft "$C/api/contract.rs"
check "V1: and the lead's commit" grep -q charge "$C/api/billing.rs"
check "V1: and can't write it" fails sh -c "echo x >>'$C/api/contract.rs'"
eq "V1: git sees nothing of it" "" "$(git -C "$C" status --short)"
r=$(row "$C" lead)
check "V1: listed as all work" has "$r" "all work"
check "V1: live" has "$r" "live"
check "V1: not built on" fails has "$r" "built on"
out=$(edit_hook "$C" api/contract.rs)
check "V1: editing the draft is denied" has "$out" '"permissionDecision":"deny"'
check "V1: offering to build on the lead" has "$out" "gitfold layer add lead --edit"
check "V1: first, before a proposal" before "$out" "gitfold layer add lead --edit" "gitfold propose"
check "V1: saying the commits will sit on the lead's" has "$out" "your commits will sit on lead's"
check "V1: and what that means for a pull request" has "$out" "pull request"
check "V1: an uncommitted change: it says so" has "$out" "lead hasn't committed its change to this file yet"
check "V1: and that the edits wait for it" has "$out" "wait"
check "V1: then a proposal" has "$out" "gitfold propose api/contract.rs"
check "V1: asking its agent" has "$out" "gitfold send"
check "V1: and removing the layer" has "$out" "gitfold layer remove"
check "V1: not the old base command" fails has "$out" "layer base"
out=$(edit_hook "$C" api/billing.rs)
check "V1: the lead's committed class: denied too" has "$out" '"permissionDecision":"deny"'
check "V1: offering to build on the lead" has "$out" "gitfold layer add lead --edit"
check "V1: a committed change: no caveat" fails has "$out" "hasn't committed"
R switch -q -c rel main; echo 'fn v3() {}' >>"$REPO/api/contract.rs"; R commit -qam v3; R tag v1.3; R switch -q main
"$BIN" -C "$MNT/other" layer add v1.3 >/dev/null
out=$(edit_hook "$MNT/other" api/contract.rs)
check "V1: a tag's file: denied" has "$out" deny
check "V1: no offer to build on a tag" fails has "$out" "layer add v1.3 --edit"
}

# V2: live follows at the turn start, frozen doesn't, adding again
# refreshes; --commits hides uncommitted work; a tag ahead of the base shows,
# one the base has is refused; a branch with no workspace.
v2_live_frozen_commits_a_tag() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; R add -A; R commit -qm api
R tag v1.2
echo 'fn v2() {}' >>"$REPO/api/contract.rs"; R commit -qam v2
R branch lead; R branch child; R branch lib; R branch feature-x
mount_fs
L=$MNT/lead C=$MNT/child
an_agent "$C"
echo 'fn draft() {}' >>"$L/api/contract.rs"
"$BIN" -C "$C" layer add lead >/dev/null
echo 'lib v1' >"$MNT/lib/lib.txt"
check "V2: a frozen library" "$BIN" -C "$C" layer add lib --frozen
echo 'fn second() {}' >>"$L/api/contract.rs"; echo 'lib v2' >"$MNT/lib/lib.txt"
turn_ends "$L"; turn_ends "$MNT/lib"
out=$(turn_starts "$C")
check "V2: the lead's new work shows after the child's turn starts" grep -q second "$C/api/contract.rs"
eq "V2: the frozen layer didn't move" "lib v1" "$(cat "$C/lib.txt")"
check "V2: the hook names the lead" has "$out" lead
check "V2: and not the frozen layer" fails has "$out" lib
check "V2: adding it again, frozen, refreshes it" "$BIN" -C "$C" layer add lib --frozen
eq "V2: refreshed" "lib v2" "$(cat "$C/lib.txt")"
check "V2: still frozen" has "$(row "$C" lib)" "frozen at HH:MM"

check "V2: show only what the lead committed" "$BIN" -C "$C" layer add lead --commits
check "V2: the uncommitted draft is gone" fails grep -q draft "$C/api/contract.rs"
r=$(row "$C" lead)
check "V2: one row for the lead, commits only" has "$r" "commits only"
eq "V2: re-adding replaced it: one row" "1" "$(listing "$C" | awk '$1 == "lead"' | wc -l)"
G "$L" commit -qam "lead: contract"
turn_starts "$C" >/dev/null
check "V2: the commit shows at the next turn" grep -q draft "$C/api/contract.rs"

out=$("$BIN" -C "$MNT/lib" layer add v1.2 2>&1); rc=$?
eq "V2: a tag the base has: refused (exit 4)" "4" "$rc"
check "V2: pointing to a snapshot of it" has "$out" "gitfold snapshot v1.2"
p=$("$BIN" -C "$REPO" snapshot v1.2)
eq "V2: which shows the tag as it was" "fn v1() {}" "$(cat "$p/api/contract.rs")"
R switch -q -c rel main; echo 'fn v3() {}' >>"$REPO/api/contract.rs"; R commit -qam v3; R tag v1.3; R switch -q main
check "V2: a tag ahead of the base" "$BIN" -C "$MNT/lib" layer add v1.3
check "V2: its change shows" grep -q v3 "$MNT/lib/api/contract.rs"
check "V2: listed as fixed" has "$(row "$MNT/lib" v1.3)" "fixed"
check "V2: removed" "$BIN" -C "$MNT/lib" layer remove v1.3
check "V2: gone" fails grep -q v3 "$MNT/lib/api/contract.rs"

commit_on feature-x fx.txt one
check "V2: a branch with no workspace" "$BIN" -C "$C" layer add feature-x
eq "V2: its commits show" "one" "$(cat "$C/fx.txt")"
check "V2: listed as live" has "$(row "$C" feature-x)" "live"
commit_on feature-x fx.txt two
turn_starts "$C" >/dev/null
eq "V2: its new commit shows at the next turn" "two" "$(cat "$C/fx.txt")"
}

# V2: adding again replaces, in the last mode; own files win; mistakes;
# removing every layer.
v2_replacing_own_files_mistakes() {
R branch me; R branch b
mount_fs
W=$MNT/me
echo b >>"$MNT/b/a.txt"; echo mine >>"$W/a.txt"; echo bb >"$MNT/b/b.txt"
for how in "" --frozen --commits ""; do
    "$BIN" -C "$W" layer add b $how >/dev/null
done
eq "V2: re-adding replaces: one row" "1" "$(listing "$W" | awk '$1 == "b"' | wc -l)"
r=$(row "$W" b)
check "V2: in the last mode" bash -c "grep -q 'all work' <<<'$r' && grep -q live <<<'$r'"
eq "V2: my own file wins" "a
mine" "$(cat "$W/a.txt")"
check "V2: and stays writable" sh -c "echo more >>'$W/a.txt'"
eq "V2: git sees only my change" " M a.txt" "$(git -C "$W" status --short)"
eq "V2: an unknown name: exit 3" "3" "$("$BIN" -C "$W" layer add nosuch >/dev/null 2>&1; echo $?)"
eq "V2: a workspace can't show itself: a usage error (exit 2)" "2" "$("$BIN" -C "$W" layer add me >/dev/null 2>&1; echo $?)"
eq "V2: removing what isn't a layer: exit 3" "3" "$("$BIN" -C "$W" layer remove nosuch >/dev/null 2>&1; echo $?)"
check "V2: remove --all" "$BIN" -C "$W" layer remove --all
check "V2: the layer's files are gone" test ! -e "$W/b.txt"
eq "V2: git as before" " M a.txt" "$(git -C "$W" status --short)"
out=$(listing "$W")
check "V2: the listing: your commits sit on main" has "$out" "Your commits sit on main"
eq "V2: and no layer" "" "$(row "$W" b)"
check "V2: no base in the listing" fails has "$out" "Base:"
}

# V3: two see-only layers on one file merge; a same-line conflict shows
# markers and is listed; a deletion against a change shows the base's
# version; a conflict with the workspace's own commits is named; the deny
# hook names both and the --edits that would unlock it; removing one
# clears the conflict.
v3_two_layers_on_one_file() {
printf 'one\ntwo\nthree\nfour\nfive\n' >"$REPO/util.rs"; echo 'x = 1' >"$REPO/config.rs"
echo 'y = 1' >"$REPO/settings.rs"; echo old >"$REPO/old.md"; R add -A; R commit -qm files
R branch me; R branch b; R branch c
R switch -q b; R rm -q old.md; R commit -qm "b: old.md"; R switch -q main
mount_fs
W=$MNT/me
echo 'y = 9' >"$W/settings.rs"; G "$W" commit -qam "me: settings"
printf 'b0\none\ntwo\nthree\nfour\nfive\n' >"$MNT/b/util.rs"
printf 'one\ntwo\nthree\nfour\nfive\nc9\n' >"$MNT/c/util.rs"
echo 'x = 2' >"$MNT/b/config.rs"; echo 'x = 3' >"$MNT/c/config.rs"
echo 'y = 2' >"$MNT/b/settings.rs"
echo changed >"$MNT/c/old.md"
"$BIN" -C "$W" layer add b >/dev/null; "$BIN" -C "$W" layer add c >/dev/null
eq "V3: changes to different lines merge" "b0 one two three four five c9" "$(paste -sd' ' "$W/util.rs")"
check "V3: the merged file is read-only" fails sh -c "echo x >>'$W/util.rs'"
check "V3: a conflict shows git's markers" grep -q '^<<<<<<<' "$W/config.rs"
check "V3: read-only too" fails sh -c "echo x >>'$W/config.rs'"
out=$(listing "$W")
conflicts=$(grep -i conflict <<<"$out")
check "V3: the listing reports the conflict" has "$(grep config.rs <<<"$conflicts")" "b and c"
check "V3: not the clean merge" fails has "$conflicts" "util.rs"
eq "V3: a deletion against a change shows the base's version" "old" "$(cat "$W/old.md")"
check "V3: and is reported" has "$out" "old.md"
check "V3: a conflict with my own commit: markers" grep -q '^<<<<<<<' "$W/settings.rs"
check "V3: named in the listing" has "$(grep settings.rs <<<"$conflicts")" "b"
out=$(edit_hook "$W" config.rs)
check "V3: editing it is denied" has "$out" '"permissionDecision":"deny"'
check "V3: naming both layers" has "$out" "b and c"
check "V3: the --edit for b" has "$out" "gitfold layer add b --edit"
check "V3: and for c" has "$out" "gitfold layer add c --edit"
check "V3: offering a proposal" has "$out" "gitfold propose"
check "V3: and gitfold layer remove" has "$out" "gitfold layer remove"
check "V3: removing one layer" "$BIN" -C "$W" layer remove c
eq "V3: the other's version, no conflict" "x = 2" "$(cat "$W/config.rs")"
check "V3: the conflict line gone" fails has "$(listing "$W" | grep config.rs)" "onflict"
eq "V3: git sees only my own commit's work" "" "$(git -C "$W" status --short)"
}

# V4: the base moved on since the layers started: a layer brings only its
# own changes, the base's newer fixes stay, two layers from different older
# commits keep all their changes; a branch with no shared history is refused.
v4_the_base_moved_on() {
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
eq "V4: the base's fix stays, the layer's change shows" "one FIX two three four five B six seven eight FIX" \
    "$(paste -sd' ' "$W/util.rs")"
"$BIN" -C "$W" layer add c >/dev/null
eq "V4: two layers from different older commits: every change" \
    "one FIX two three C four five B six seven eight FIX" "$(paste -sd' ' "$W/util.rs")"
check "V4: read-only" fails sh -c "echo x >>'$W/util.rs'"
check "V4: a change to a line the branch changed too: git's markers" grep -q '^<<<<<<<' "$W/config.rs"
out=$(listing "$W")
check "V4: listed as a conflict" has "$(grep -i conflict <<<"$out" | grep config.rs)" "b"
check "V4: not the clean merge" fails has "$(grep -i conflict <<<"$out")" "util.rs"
check "V4: the deny hook names it" has "$(edit_hook "$W" config.rs)" "b"
eq "V4: git sees none of it" "" "$(git -C "$W" status --short)"
out=$("$BIN" -C "$W" layer add lonely 2>&1); rc=$?
eq "V4: no shared history: refused (exit 4)" "4" "$rc"
check "V4: saying why" has "$out" "shares no history"
eq "V4: and not added" "" "$(row "$W" lonely)"
}

# V5: a person works in a workspace with no agent; the lead's agent works
# on. The person's layer of the lead follows by itself, between the
# person's edits.
v5_a_person_without_an_agent() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; R add -A; R commit -qm api
R branch lead; R branch me
mount_fs
L=$MNT/lead P=$MNT/me
echo 'fn draft() {}' >>"$L/api/contract.rs"
"$BIN" -C "$P" layer add lead >/dev/null
check "V5: the person sees the lead's draft" grep -q draft "$P/api/contract.rs"
echo 'fn second() {}' >>"$L/api/contract.rs"; turn_ends "$L"
for _ in $(seq 150); do grep -q second "$P/api/contract.rs" && break; sleep 0.1; done
check "V5: the lead's next turn shows within seconds, no command run" grep -q second "$P/api/contract.rs"
check "V5: listed as moved" has "$(row "$P" lead)" "live, last HH:MM"
echo 'fn third() {}' >>"$L/api/contract.rs"; turn_ends "$L"
seen=no
for i in $(seq 10); do
    echo "note $i" >>"$P/notes.txt"; sleep 0.5
    grep -q third "$P/api/contract.rs" && seen=yes
done
eq "V5: not while the person is editing" "no" "$seen"
for _ in $(seq 150); do grep -q third "$P/api/contract.rs" && break; sleep 0.1; done
check "V5: but once they pause" grep -q third "$P/api/contract.rs"
eq "V5: git sees only the person's own file" "?? notes.txt" "$(git -C "$P" status --short)"
}

## Editable layers

# V6: a child built on the lead: its commits sit on the lead's, the lead's
# committed files are its to edit, the lead's uncommitted work shows, the
# listing says "built on", and it follows the lead's new commits.
v6_build_on_the_lead() {
R branch lead
commit_on lead api.rs 'class Api {}'
R tag v1.2 lead
R branch other; commit_on other other.txt mine
out=$("$BIN" -C "$REPO" branch child --edit lead 2>&1); rc=$?
eq "V6: branch child --edit lead" "0" "$rc"
eq "V6: the new branch starts at the lead's tip" "$(R rev-parse lead)" "$(R rev-parse child 2>&1)"
out=$("$BIN" -C "$REPO" layer add lead --edit -w other 2>&1); rc=$?
eq "V6: layer add lead --edit on an existing branch" "0" "$rc"
check "V6: saying what it moves" has "$out" "moves 1 commit of other onto lead"
eq "V6: its commit on the lead's" "$(R rev-parse lead)|other.txt" "$(R rev-parse other^)|$(R diff --name-only lead other)"
out=$("$BIN" -C "$REPO" layer add v1.2 --edit -w other 2>&1); rc=$?
check "V6: a tag can't be edited on" test "$rc" != 0
check "V6: saying to use a branch" has "$out" "branch"
mount_fs
L=$MNT/lead C=$MNT/child
an_agent "$C"
echo 'fn draft() {}' >"$L/draft.rs"; turn_ends "$L"
turn_starts "$C" >/dev/null
eq "V6: the lead's committed file" "class Api {}" "$(cat "$C/api.rs" 2>&1)"
eq "V6: the lead's uncommitted work shows" "fn draft() {}" "$(cat "$C/draft.rs" 2>&1)"
eq "V6: git sees none of it" "" "$(git -C "$C" status --short)"
check "V6: the lead's committed file is writable" sh -c "echo '// mine' >>'$C/api.rs'"
eq "V6: as the child's own change" " M api.rs" "$(git -C "$C" status --short)"
G "$C" checkout -q -- api.rs
r=$(row "$C" lead)
check "V6: the listing says built on" has "$r" "built on"
check "V6: all work" has "$r" "all work"
check "V6: and live" has "$r" "live"
check "V6: list shows the child under the lead" has "$("$BIN" -C "$REPO" list)" "└─ child"
echo own >"$C/own.rs"; G "$C" add own.rs; G "$C" commit -qm "child: own"
eq "V6: the child's commit sits on the lead's" "$(R rev-parse lead)" "$(R rev-parse child^)"
eq "V6: holding only its own change" "own.rs" "$(R diff --name-only lead child)"
G "$L" add draft.rs; G "$L" commit -qm "lead: draft"; turn_ends "$L"
out=$(turn_starts "$C")
eq "V6: the lead's new commit: the child moves onto it at its turn" "$(R rev-parse lead)" "$(R rev-parse child^)"
check "V6: the hook says so" has "$out" "lead"
eq "V6: still only its own change" "own.rs" "$(R diff --name-only lead child)"
eq "V6: git clean" "" "$(git -C "$C" status --short)"
}

# The lead with an uncommitted class in api.rs, the child built on the lead,
# both agents: L, C. $1: the use case, for the failure.
the_lead_drafts_a_class() {
printf '// api\n' >"$REPO/api.rs"; R add -A; R commit -qm api
R branch lead
"$BIN" -C "$REPO" branch child --edit lead >/dev/null || fail "$1: branch child --edit lead"
mount_fs
L=$MNT/lead C=$MNT/child
an_agent "$C"
printf '// api\nclass Api {\n    fn a() {}\n}\n' >"$L/api.rs"
turn_ends "$L"; turn_starts "$C" >/dev/null
}
# Workspace $1's git status and diffs say nothing of api.rs (the lead's
# uncommitted class, the child's waiting edits).
git_blind_to_api() {
    ! git -C "$1" status --short | grep -q api.rs &&
        ! git -C "$1" diff HEAD | grep -q -e 'class Api' -e 'fn m()' &&
        ! git -C "$1" diff --cached | grep -q 'class Api'
}

# V7: the child adds a method inside the lead's uncommitted class: a
# waiting edit, which git doesn't see, until the lead commits the class.
v7_waiting_edits() {
the_lead_drafts_a_class V7
check "V7: the child sees the lead's class" grep -q 'class Api' "$C/api.rs"
check "V7: and can add a method inside it" sh -c "printf '// api\nclass Api {\n    fn a() {}\n    fn m() {}\n}\n' >'$C/api.rs'"
eq "V7: the file shows both" "// api|class Api {|    fn a() {}|    fn m() {}|}" "$(paste -sd'|' "$C/api.rs")"
echo more >>"$C/a.txt"
eq "V7: git status: only the child's own change" " M a.txt" "$(git -C "$C" status --short)"
check "V7: git diff: neither the class nor the method" fails sh -c "git -C '$C' diff | grep -q -e 'class Api' -e 'fn m'"
check "V7: the listing shows the waiting edit" has "$(listing "$C")" "1 waiting edit in api.rs"
check "V7: and the turn's notes" has "$(turn_starts "$C")" "waiting edit"
G "$C" commit -qam "child: a.txt"
eq "V7: git commit -a commits the rest" "a.txt" "$(R diff --name-only lead child)"
eq "V7: and leaves the method waiting" "// api" "$(R show child:api.rs)"
check "V7: still in the view" grep -q 'fn m' "$C/api.rs"
eq "V7: git clean" "" "$(git -C "$C" status --short)"
G "$L" commit -qam "lead: class"; turn_ends "$L"
turn_starts "$C" >/dev/null
eq "V7: the lead committed the class: the child moves onto it" "$(R rev-parse lead)" "$(R rev-parse child^)"
eq "V7: the method is an ordinary change now" " M api.rs" "$(git -C "$C" status --short)"
eq "V7: git diff shows only the method" "+    fn m() {}" "$(git -C "$C" diff | grep '^+[^+]')"
check "V7: no waiting edit listed" fails has "$(listing "$C")" "waiting edit"
G "$C" commit -qam "child: method"
eq "V7: committed on top of the lead's commit" "$(R rev-parse lead)" "$(R rev-parse child~2)"
eq "V7: the branch has the class and the method" "// api|class Api {|    fn a() {}|    fn m() {}|}" \
    "$(R show child:api.rs | paste -sd'|')"
eq "V7: git clean" "" "$(git -C "$C" status --short)"
}

# V8: git works normally in a workspace built on the lead with its
# uncommitted class shown and a waiting edit in it.
v8_git_works_normally() {
the_lead_drafts_a_class V8
printf '// api\nclass Api {\n    fn a() {}\n    fn m() {}\n}\n' >"$C/api.rs"
echo one >"$C/f1.txt"; G "$C" add f1.txt; G "$C" commit -qm "child: f1"
echo two >"$C/f2.txt"; G "$C" add f2.txt; G "$C" commit -qm "child: f2"
eq "V8: HEAD is on the child's branch" "refs/heads/child" "$(git -C "$C" symbolic-ref HEAD 2>&1)"
eq "V8: HEAD is the branch's tip" "$(R rev-parse child)" "$(git -C "$C" rev-parse HEAD)"
eq "V8: HEAD~1 is the previous own commit" "child: f1" "$(git -C "$C" log -1 --format=%s HEAD~1)"
check "V8: git status clean" test -z "$(git -C "$C" status --short)"
echo three >"$C/f3.txt"; G "$C" add f3.txt
out=$(G "$C" commit -m "child: f3")
id=$(sed -nE 's/^\[child ([0-9a-f]+)\].*/\1/p' <<<"$out")
check "V8: the commit id printed is the branch's" sh -c "[ -n '$id' ] && case '$(R rev-parse child)' in '$id'*) true;; *) false;; esac"
check "V8: blind to the lead's lines" git_blind_to_api "$C"

G "$C" commit -q --amend -m "child: f3, amended"
eq "V8: --amend amends the branch's commit" "child: f3, amended|child: f2" "$(R log -2 --format=%s child | paste -sd'|')"
check "V8: blind after --amend" git_blind_to_api "$C"
G "$C" reset -q HEAD~1
eq "V8: reset HEAD~1" "child: f2" "$(R log -1 --format=%s child)"
eq "V8: the commit's file left as a change" "?? f3.txt" "$(git -C "$C" status --short)"
check "V8: blind after reset" git_blind_to_api "$C"
G "$C" add f3.txt; G "$C" commit -qm "child: f3"

echo more >>"$C/a.txt"
check "V8: git stash" G "$C" stash -q
eq "V8: stashed: clean" "" "$(git -C "$C" status --short)"
eq "V8: the stash holds only a.txt" "a.txt" "$(git -C "$C" stash show --name-only 2>&1)"
check "V8: the view keeps the class and the method" sh -c "grep -q 'class Api' '$C/api.rs' && grep -q 'fn m' '$C/api.rs'"
check "V8: git stash pop" G "$C" stash pop -q
eq "V8: popped" " M a.txt" "$(git -C "$C" status --short)"

echo f1-more >>"$C/f1.txt"
printf 'y\ny\ny\ny\n' | G "$C" add -p >/dev/null 2>&1
eq "V8: add -p offers only own changes" "a.txt f1.txt" "$(git -C "$C" diff --cached --name-only | paste -sd' ')"
G "$C" commit -qm "child: more"
eq "V8: committed" "" "$(git -C "$C" status --short)"

n=$(R rev-list --count lead..child)
check "V8: rebase -i of own commits" env GIT_SEQUENCE_EDITOR="sed -i '2s/^pick/fixup/'" timeout 60 \
    git -C "$C" -c user.name=t -c user.email=t@t rebase -q -i HEAD~3
eq "V8: two commits squashed into one" "$((n - 1))" "$(R rev-list --count lead..child)"
eq "V8: still on the branch" "refs/heads/child" "$(git -C "$C" symbolic-ref HEAD 2>&1)"
eq "V8: the same files" "a.txt f1.txt f2.txt f3.txt" "$(R diff --name-only lead child | paste -sd' ')"
check "V8: blind after rebase" git_blind_to_api "$C"

check "V8: switch away" G "$C" switch -q -c side
check "V8: blind on the other branch" git_blind_to_api "$C"
check "V8: and back" G "$C" switch -q child
eq "V8: on the branch again" "refs/heads/child" "$(git -C "$C" symbolic-ref HEAD 2>&1)"
check "V8: blind" git_blind_to_api "$C"
eq "V8: the view: the lead's class with the waiting method" "// api|class Api {|    fn a() {}|    fn m() {}|}" \
    "$(paste -sd'|' "$C/api.rs")"
eq "V8: the branch never got the lead's lines" "// api" "$(R show child:api.rs)"
}

# V9: a commit with the lead's uncommitted lines can't reach the child's
# branch: not by git commit (--no-verify), not by plumbing.
v9_the_guard() {
the_lead_drafts_a_class V9
echo own >"$C/own.txt"; G "$C" add own.txt; G "$C" commit -qm "child: own"
tip=$(R rev-parse child)
blob=$(git -C "$C" hash-object -w --no-filters api.rs)
G "$C" update-index --cacheinfo "100644,$blob,api.rs"
out=$(G "$C" commit -qm "child: sneaky" --no-verify 2>&1); rc=$?
check "V9: committing the lead's lines fails, even with --no-verify" test "$rc" != 0
check "V9: saying why: the lead's uncommitted lines" has "$out" "lead"
eq "V9: the branch didn't move" "$tip" "$(R rev-parse child)"
tree=$(G "$C" write-tree)
c=$(G "$C" commit-tree "$tree" -p HEAD -m "child: plumbing")
out=$(G "$C" update-ref refs/heads/child "$c" 2>&1); rc=$?
check "V9: update-ref with it fails" test "$rc" != 0
check "V9: saying why" has "$out" "lead"
eq "V9: the branch didn't move" "$tip" "$(R rev-parse child)"
out=$(G "$C" update-ref HEAD "$c" 2>&1); rc=$?
check "V9: through HEAD too" test "$rc" != 0
eq "V9: the branch still didn't move" "$tip" "$(R rev-parse child)"
eq "V9: the branch never got the lead's lines" "// api" "$(R show child:api.rs)"
G "$C" reset -q
eq "V9: after a reset, git is clean" "" "$(git -C "$C" status --short)"
check "V9: the class still shows" grep -q 'class Api' "$C/api.rs"
}

# V10: the lead changes the line a waiting edit changed: a conflict, with
# markers, listed.
v10_the_lead_changes_a_waiting_line() {
the_lead_drafts_a_class V10
printf '// api\nclass Api {\n    fn a() { child }\n}\n' >"$C/api.rs"
check "V10: a waiting edit" has "$(listing "$C")" "waiting edit"
printf '// api\nclass Api {\n    fn a() { lead }\n}\n' >"$L/api.rs"; turn_ends "$L"
turn_starts "$C" >/dev/null
check "V10: the waiting edit conflicts: git's markers" grep -q '^<<<<<<<' "$C/api.rs"
check "V10: both versions in it" sh -c "grep -q 'a() { child }' '$C/api.rs' && grep -q 'a() { lead }' '$C/api.rs'"
check "V10: listed as a conflict" has "$(listing "$C" | grep -i conflict)" "api.rs"
check "V10: git still sees nothing of it" git_blind_to_api "$C"
}

# V11: a file a see-only layer and an editable one both changed stays
# read-only; the deny hook names the --edit that would unlock it.
v11_see_only_and_editable_on_one_file() {
printf 'one\ntwo\nthree\nfour\nfive\n' >"$REPO/util.rs"; echo 'x' >"$REPO/api-only.rs"
R add -A; R commit -qm files
R branch api; R branch db; R branch child
mount_fs
C=$MNT/child
an_agent "$C"
printf 'one API\ntwo\nthree\nfour\nfive\n' >"$MNT/api/util.rs"; echo 'x API' >"$MNT/api/api-only.rs"
printf 'one\ntwo\nthree\nfour\nfive DB\n' >"$MNT/db/util.rs"
check "V11: build on api" "$BIN" -C "$C" layer add api --edit
check "V11: show db" "$BIN" -C "$C" layer add db
eq "V11: both changes show" "one API two three four five DB" "$(paste -sd' ' "$C/util.rs")"
check "V11: a file only api changed: writable" can_write "$C/api-only.rs"
check "V11: the file both changed: read-only" fails sh -c "echo x >>'$C/util.rs'"
out=$(edit_hook "$C" util.rs)
check "V11: denied" has "$out" '"permissionDecision":"deny"'
check "V11: naming the --edit that unlocks it" has "$out" "gitfold layer add db --edit"
check "V11: not api's, already editable" fails has "$out" "layer add api --edit"
check "V11: making db editable too" "$BIN" -C "$C" layer add db --edit
check "V11: the file is writable" can_write "$C/util.rs"
check "V11: and not denied" fails has "$(edit_hook "$C" util.rs)" deny
}

# V12, V13: taking the child's commits off the lead it is built on, by
# `layer $1 lead` (add: it becomes see-only; remove: it goes). It says what
# it moves; refused when commits or waiting edits change the lead's lines;
# --yes when pushed.
taking_the_commits_off() {
how=$1 V=$2
printf 'fn one() {}\n' >"$REPO/lead.rs"; R add -A; R commit -qm lead.rs
R branch lead
commit_on lead lead.rs "$(printf 'fn one() {}\nfn two() {}')"
"$BIN" -C "$REPO" branch child --edit lead >/dev/null || fail "$V: branch child --edit lead"
commit_on child c.txt mine
mount_fs
L=$MNT/lead C=$MNT/child
an_agent "$C"
printf 'fn one() {}\nfn two() {}\nfn draft() {}\n' >"$L/lead.rs"; turn_ends "$L"
turn_starts "$C" >/dev/null
out=$("$BIN" -C "$C" layer $how lead 2>&1); rc=$?
eq "$V: layer $how lead: done" "0" "$rc"
check "$V: saying what it moves" has "$out" "moves 1 commit of child"
check "$V: onto main" has "$out" "main"
eq "$V: the child's commit on main" "$(R rev-parse main)" "$(R rev-parse child^)"
eq "$V: holding only its own change" "c.txt" "$(R diff --name-only main child)"
r=$(row "$C" lead)
check "$V: not built on any more" fails has "$r" "built on"
check "$V: the listing: your commits sit on main" has "$(listing "$C")" "sit on main"
if [ "$how" = add ]; then
    check "$V: saying it becomes see-only" has "$out" "see-only"
    check "$V: still shown" has "$r" "all work"
    eq "$V: the lead's work, committed and not" "fn one() {}|fn two() {}|fn draft() {}" "$(paste -sd'|' "$C/lead.rs")"
    check "$V: read-only now" fails sh -c "echo x >>'$C/lead.rs'"
else
    eq "$V: the layer is gone" "" "$r"
    eq "$V: so is the lead's work" "fn one() {}" "$(cat "$C/lead.rs")"
fi
eq "$V: git clean" "" "$(git -C "$C" status --short)"

check "$V: built on the lead again" "$BIN" -C "$C" layer add lead --edit
eq "$V: on the lead" "$(R rev-parse lead)" "$(R rev-parse child^)"
printf 'fn one() {}\nfn two() { mine }\nfn draft() {}\n' >"$C/lead.rs"; G "$C" commit -qam "child: change two"
tip=$(R rev-parse child)
out=$("$BIN" -C "$C" layer $how lead 2>&1); rc=$?
eq "$V: a commit changing the lead's lines: refused (exit 4)" "4" "$rc"
check "$V: listing it" has "$out" "child: change two"
eq "$V: nothing moved" "$tip" "$(R rev-parse child)"
check "$V: still built on" has "$(row "$C" lead)" "built on"
G "$C" reset -q --hard HEAD~1

printf 'fn one() {}\nfn two() {}\nfn draft() { mine }\n' >"$C/lead.rs"
tip=$(R rev-parse child)
out=$("$BIN" -C "$C" layer $how lead 2>&1); rc=$?
eq "$V: a waiting edit of the lead's lines: refused (exit 4)" "4" "$rc"
check "$V: naming its file" has "$out" "lead.rs"
check "$V: as a waiting edit" has "$out" "waiting"
eq "$V: nothing moved" "$tip" "$(R rev-parse child)"
printf 'fn one() {}\nfn two() {}\nfn draft() {}\n' >"$C/lead.rs"
check "$V: undone, the edit no longer waits" fails has "$(listing "$C")" "waiting edit"

git init -q --bare "$T/remote.git"; R remote add origin "$T/remote.git"
R push -q origin child 2>/dev/null
out=$("$BIN" -C "$C" layer $how lead 2>&1); rc=$?
eq "$V: pushed commits: refused (exit 4)" "4" "$rc"
check "$V: saying how to go on" has "$out" "--yes"
eq "$V: nothing moved" "$tip" "$(R rev-parse child)"
out=$("$BIN" -C "$C" layer $how lead --yes 2>&1); rc=$?
eq "$V: with --yes" "0" "$rc"
eq "$V: moved onto main" "$(R rev-parse main)" "$(R rev-parse child^)"
check "$V: printing the force-push" has "$out" "git push --force-with-lease"
eq "$V: never pushed" "$tip" "$(git -C "$T/remote.git" rev-parse child)"
}
v12_editable_back_to_see_only() { taking_the_commits_off add V12; }
v13_remove_an_editable_layer() { taking_the_commits_off remove V13; }

# V14: two editable layers: a warning about pull requests, the child's
# commits on a merge of both, either moving rebuilds it, a conflict
# between them moves nothing.
v14_two_editable_layers() {
R branch api; R branch db; R branch child
commit_on api api.rs api; commit_on db db.rs db; commit_on child c.txt mine
mount_fs
C=$MNT/child A=$MNT/api D=$MNT/db
an_agent "$C"
out=$("$BIN" -C "$C" layer add api --edit 2>&1)
check "V14: the first: no pull request warning" fails has "$out" "pull request"
eq "V14: on api" "$(R rev-parse api)" "$(R rev-parse child^)"
out=$("$BIN" -C "$C" layer add db --edit 2>&1); rc=$?
eq "V14: the second" "0" "$rc"
check "V14: warns: built on api and db" has "$out" "built on api and db"
check "V14: about the pull request" has "$out" "pull request"
eq "V14: the child's commit on a merge of both" "$(ids api db)" "$(parents child^)"
eq "V14: holding only its own change" "c.txt" "$(R diff --name-only child^ child)"
eq "V14: both shown" "api|db|mine" "$(cat "$C/api.rs")|$(cat "$C/db.rs")|$(cat "$C/c.txt")"
check "V14: both writable" sh -c "(: >>'$C/api.rs') && (: >>'$C/db.rs')"
r=$(listing "$C")
check "V14: both listed as built on" bash -c "grep '^api ' <<<'$r' | grep -q 'built on' && grep '^db ' <<<'$r' | grep -q 'built on'"
echo api2 >"$A/api2.txt"; G "$A" add api2.txt; G "$A" commit -qm "api: more"
turn_starts "$C" >/dev/null
eq "V14: api moved: the merge rebuilt" "$(ids api db)" "$(parents child^)"
eq "V14: its new file shows" "api2" "$(cat "$C/api2.txt" 2>&1)"
echo db2 >"$D/db2.txt"; G "$D" add db2.txt; G "$D" commit -qm "db: more"
turn_starts "$C" >/dev/null
eq "V14: db moved: rebuilt too" "$(ids api db)" "$(parents child^)"
eq "V14: still only its own change" "c.txt" "$(R diff --name-only child^ child)"
echo api >"$A/x.txt"; G "$A" add x.txt; G "$A" commit -qm "api: x"
turn_starts "$C" >/dev/null
echo db >"$D/x.txt"; G "$D" add x.txt; G "$D" commit -qm "db: x"
tip=$(R rev-parse child)
out=$(turn_starts "$C")
eq "V14: api and db conflict: nothing moves" "$tip" "$(R rev-parse child)"
check "V14: the turn names the path" has "$out" "x.txt"
out=$("$BIN" -C "$C" layer add db --edit 2>&1); rc=$?
check "V14: asked: refused" test "$rc" != 0
check "V14: naming the path" has "$out" "x.txt"
eq "V14: nothing moved" "$tip" "$(R rev-parse child)"
eq "V14: the view as it was" "api" "$(cat "$C/x.txt" 2>&1)"
}

# V15: landings, one editable layer at a time: reported while the branch
# exists, followed by `layer remove`; followed by itself once the branch is
# deleted; landing one of two leaves the other.
v15_landing() {
R branch api; commit_on api api.rs api
"$BIN" -C "$REPO" branch ui --edit api >/dev/null || fail "V15: branch ui --edit api"
commit_on ui ui.txt mine
R branch db; commit_on db db.rs db
R branch ex; commit_on ex ex.rs ex
mount_fs
U=$MNT/ui
an_agent "$U"
R merge -q --squash api >/dev/null; R commit -qm "api (squash)"
out=$(turn_starts "$U")
check "V15: the turn says api looks merged" has "$out" "api looks merged into main"
check "V15: and how to follow it" has "$out" "gitfold layer remove api"
eq "V15: not moved while api's branch is there" "$(R rev-parse api)" "$(R rev-parse ui^)"
check "V15: list says it too" has "$("$BIN" -C "$REPO" list)" "merged"
out=$("$BIN" -C "$U" layer remove api 2>&1); rc=$?
eq "V15: layer remove api" "0" "$rc"
eq "V15: follows the landing: ui on main" "$(R rev-parse main)" "$(R rev-parse ui^)"
eq "V15: with only its own commit" "ui.txt" "$(R diff --name-only main ui)"
eq "V15: api's layer gone" "" "$(row "$U" api)"
check "V15: the listing: your commits sit on main" has "$(listing "$U")" "sit on main"

"$BIN" -C "$U" layer add db --edit >/dev/null 2>&1
"$BIN" -C "$U" layer add ex --edit >/dev/null 2>&1
eq "V15: two editable layers" "$(ids db ex)" "$(parents ui^)"
R merge -q --squash db >/dev/null; R commit -qm "db (squash)"
check "V15: db's branch deleted" R branch -q -D db
turn_starts "$U" >/dev/null
eq "V15: db landed and deleted: its layer gone" "" "$(row "$U" db)"
check "V15: ex stays built on" has "$(row "$U" ex)" "built on"
check "V15: ui on ex" R merge-base --is-ancestor ex ui
eq "V15: its own commit kept" "mine" "$(cat "$U/ui.txt")"

R merge -q --squash ex >/dev/null; R commit -qm "ex (squash)"
check "V15: ex's branch deleted" R branch -q -D ex
turn_starts "$U" >/dev/null
eq "V15: ex landed and deleted: ui on where it landed" "$(R rev-parse main)" "$(R rev-parse ui^)"
eq "V15: with only its own commit" "ui.txt" "$(R diff --name-only main ui)"
eq "V15: no layer left" "" "$(row "$U" ex)"
check "V15: the listing: your commits sit on main" has "$(listing "$U")" "sit on main"
eq "V15: git clean" "" "$(git -C "$U" status --short)"
}

# V16: pushed commits: the turn's live following moves them unasked and
# prints the force-push, which is right; each asked move needs --yes.
v16_pushed_commits() {
R branch api; commit_on api api.rs api
"$BIN" -C "$REPO" branch ui --edit api >/dev/null || fail "V16: branch ui --edit api"
commit_on ui ui.txt mine
R branch other; commit_on other other.txt o
git init -q --bare "$T/remote.git"; R remote add origin "$T/remote.git"
R push -q origin ui 2>/dev/null
mount_fs
U=$MNT/ui
an_agent "$U"
old=$(R rev-parse ui)
commit_on api api.rs api2
out=$(turn_starts "$U")
eq "V16: the turn moves pushed commits unasked" "$(R rev-parse api)" "$(R rev-parse ui^)"
check "V16: printing the force-push" has "$out" "git push --force-with-lease"
cmd=$(grep -oE "git push --force-with-lease[^ ]* [^ ]+ [^ \`'\"]+" <<<"$out" | head -1)
check "V16: with a lease on the pushed tip" has "$cmd" "${old:0:7}"
check "V16: the printed force-push works" sh -c "cd '$REPO' && $cmd -q 2>/dev/null"
eq "V16: and pushes ui as it is now" "$(R rev-parse ui)" "$(git -C "$T/remote.git" rev-parse ui 2>&1)"
pushed=$(R rev-parse ui)
remote_tip=$(git -C "$T/remote.git" rev-parse ui)
for args in "layer add other --edit" "layer add api" "layer remove api"; do
    out=$("$BIN" -C "$U" $args 2>&1); rc=$?
    eq "V16: $args: refused (exit 4)" "4" "$rc"
    check "V16: $args: saying what it would move" has "$out" "moves 1 commit of ui"
    check "V16: $args: and --yes" has "$out" "--yes"
    eq "V16: $args: nothing moved" "$pushed" "$(R rev-parse ui)"
done
out=$("$BIN" -C "$U" layer add other --edit --yes 2>&1); rc=$?
eq "V16: --yes moves it" "0" "$rc"
check "V16: onto api and other" R merge-base --is-ancestor other ui
check "V16: printing the force-push" has "$out" "git push --force-with-lease"
eq "V16: never pushed" "$remote_tip" "$(git -C "$T/remote.git" rev-parse ui)"
}

# V17: a stack of pull requests: phase 2 built on phase 1 follows its
# review fixes, and once phase 1 lands, sits on main with only its own
# commits.
v17_a_stack_of_prs() {
R branch p1; commit_on p1 p1.txt one
check "V17: branch p2 --edit p1" "$BIN" -C "$REPO" branch p2 --edit p1
eq "V17: p2 at p1's tip" "$(R rev-parse p1)" "$(R rev-parse p2 2>&1)"
commit_on p2 p2.txt one
mount_fs
P=$MNT/p2
an_agent "$P"
commit_on p1 p1.txt review-fix
turn_starts "$P" >/dev/null
eq "V17: p2 moved onto p1's fix at its turn" "$(R rev-parse p1)" "$(R rev-parse p2^)"
eq "V17: its own commit kept" "one|review-fix" "$(cat "$P/p2.txt")|$(cat "$P/p1.txt")"
check "V17: list shows p2 under p1" has "$("$BIN" -C "$REPO" list)" "└─ p2"
R merge -q --squash p1 >/dev/null; R commit -qm "p1 (squash)"
R branch -q -D p1
out=$(turn_starts "$P")
eq "V17: p1 landed: p2 on main" "$(R rev-parse main)" "$(R rev-parse p2^)"
eq "V17: its pull request against main: only its own commit" "p2: p2.txt" "$(R log --format=%s main..p2)"
eq "V17: only its own change" "p2.txt" "$(R diff --name-only main p2)"
check "V17: the turn says the pull request's base is main now" has "$out" "main"
check "V17: the listing: your commits sit on main" has "$(listing "$P")" "sit on main"
check "V17: list: p2 no longer under another" fails has "$("$BIN" -C "$REPO" list)" "└─ p2"
eq "V17: git clean" "" "$(git -C "$P" status --short)"
}

## Union files

# V18: a changelog everyone appends to (merge=union): writable by everyone,
# lines added at the same spot all kept, commits hold only own lines, HEAD
# on the branch.
v18_a_changelog_everyone_appends_to() {
printf 'CHANGELOG.md merge=union\n' >"$REPO/.gitattributes"; printf '# log\n' >"$REPO/CHANGELOG.md"
R add -A; R commit -qm log
R branch lead; R branch db; R branch child
mount_fs
L=$MNT/lead C=$MNT/child
echo '- lead: api' >>"$L/CHANGELOG.md"; echo '- db: tables' >>"$MNT/db/CHANGELOG.md"
"$BIN" -C "$C" layer add lead >/dev/null; "$BIN" -C "$C" layer add db >/dev/null
eq "V18: both layers' lines, added at the same spot, kept" "# log|- db: tables|- lead: api" \
    "$(LC_ALL=C sort "$C/CHANGELOG.md" | paste -sd'|')"
check "V18: the child can append its own" sh -c "echo '- child: ui' >>'$C/CHANGELOG.md'"
eq "V18: all three, its own last" "# log|- child: ui|- db: tables|- lead: api|- child: ui" \
    "$(LC_ALL=C sort "$C/CHANGELOG.md" | paste -sd'|')|$(tail -1 "$C/CHANGELOG.md")"
eq "V18: git diff has only its own line" "+- child: ui" "$(git -C "$C" diff | grep '^+-')"
eq "V18: HEAD on the branch" "refs/heads/child" "$(git -C "$C" symbolic-ref HEAD 2>&1)"
G "$C" commit -qam "child: ui"
lines_on child CHANGELOG.md 2
eq "V18: the branch's commit holds only its own line" "# log|- child: ui" "$(R show child:CHANGELOG.md | paste -sd'|')"
check "V18: the lead can append too" sh -c "echo '- lead: more' >>'$L/CHANGELOG.md'"
eq "V18: git clean in the child" "" "$(git -C "$C" status --short)"
}

# V18: a registry entry one workspace adds between another layer's entries
# keeps its place where both show; its branch's commits and its snapshot
# hold only its own line; without the other layer, it is placed by the
# base's lines, and a note says so.
v18_an_entry_among_anothers() {
printf 'registry.txt merge=union\n' >"$REPO/.gitattributes"; printf 'alpha\nzulu\n' >"$REPO/registry.txt"
R add -A; R commit -qm registry
R branch a; R branch b; R branch c
mount_fs
A=$MNT/a B=$MNT/b C=$MNT/c
printf 'alpha\nbravo\ndelta\nzulu\n' >"$A/registry.txt"
"$BIN" -C "$B" layer add a >/dev/null
printf 'alpha\nbravo\ncharlie\ndelta\nzulu\n' >"$B/registry.txt"
eq "V18: b's file, its line among a's" "alpha bravo charlie delta zulu" "$(paste -sd' ' "$B/registry.txt")"
p=$("$BIN" -C "$REPO" snapshot b)
eq "V18: b's snapshot holds only its own line" "alpha charlie zulu" "$(paste -sd' ' "$p/registry.txt")"
"$BIN" -C "$C" layer add a >/dev/null; "$BIN" -C "$C" layer add b >/dev/null
eq "V18: shown with a's, b's line keeps its place" "alpha bravo charlie delta zulu" "$(paste -sd' ' "$C/registry.txt")"
"$BIN" -C "$C" layer remove a >/dev/null; "$BIN" -C "$C" layer add a >/dev/null
eq "V18: in either order" "alpha bravo charlie delta zulu" "$(paste -sd' ' "$C/registry.txt")"
out=$("$BIN" -C "$C" layer remove a)
eq "V18: without a: placed by the base's lines" "alpha charlie zulu" "$(paste -sd' ' "$C/registry.txt")"
check "V18: a note says so" has "$out" "b's lines in registry.txt are placed by"
check "V18: the listing too" has "$(listing "$C")" "b's lines in registry.txt are placed by"
G "$B" commit -qam "b: charlie"
lines_on b registry.txt 3
eq "V18: b's commit holds only its own line" "alpha charlie zulu" "$(R show b:registry.txt | LC_ALL=C sort | paste -sd' ')"
}

# V18: a method added inside another layer's class, in a file marked
# merge=union: union-merged, with a warning that the file may not be a list.
v18_a_method_inside_anothers_class() {
printf 'shapes.rs merge=union\n' >"$REPO/.gitattributes"; printf '// shapes\n' >"$REPO/shapes.rs"
R add -A; R commit -qm shapes
R branch a; R branch b; R branch c
mount_fs
A=$MNT/a B=$MNT/b C=$MNT/c
printf '// shapes\nimpl Circle {\n    fn area() {}\n}\n' >"$A/shapes.rs"
"$BIN" -C "$B" layer add a >/dev/null
printf '// shapes\nimpl Circle {\n    fn area() {}\n    fn radius() {}\n}\n' >"$B/shapes.rs"
warning="sit among a's: this file may not be a list"
check "V18: the author's listing warns" has "$(listing "$B")" "your lines in shapes.rs $warning"
"$BIN" -C "$C" layer add a >/dev/null
out=$("$BIN" -C "$C" layer add b)
check "V18: the method shows inside the class" has "$(paste -sd'|' "$C/shapes.rs")" "fn area() {}|    fn radius() {}|}"
check "V18: a note warns" has "$out" "b's lines in shapes.rs $warning"
check "V18: the listing too" has "$(listing "$C")" "b's lines in shapes.rs $warning"
printf '// shapes\nimpl Circle {\n    fn area() {}\n}\nfn helper() {}\n' >"$B/shapes.rs"
"$BIN" -C "$C" layer add b >/dev/null
check "V18: a function after a's block, at the end: no warning" fails has "$(listing "$C")" Warning
}

# V19: two layers appending at the same spot of a file not marked
# merge=union conflict (git's rule); an edit only adding lines to it gets
# the hint to mark it.
v19_the_same_spot_without_the_marker() {
printf 'core\nnet\n' >"$REPO/modules.txt"; R add -A; R commit -qm modules
R branch a; R branch b; R branch child; R branch solo
mount_fs
C=$MNT/child S=$MNT/solo
printf 'core\nnet\nbilling\n' >"$MNT/a/modules.txt"
printf 'core\nnet\nui\n' >"$MNT/b/modules.txt"
"$BIN" -C "$C" layer add a >/dev/null; "$BIN" -C "$C" layer add b >/dev/null
check "V19: same spot, no marker: git's markers" grep -q '^<<<<<<<' "$C/modules.txt"
check "V19: both lines in it" sh -c "grep -qx billing '$C/modules.txt' && grep -qx ui '$C/modules.txt'"
check "V19: listed as a conflict" has "$(listing "$C" | grep -i conflict)" "modules.txt"
check "V19: read-only" fails sh -c "echo x >>'$C/modules.txt'"
eq "V19: git sees nothing of it" "" "$(git -C "$C" status --short)"
"$BIN" -C "$S" layer add a >/dev/null
hint="mark it \`merge=union\` in .gitattributes"
out=$(tool_hook "$S" modules.txt Edit '"old_string":"net\n","new_string":"net\nui\n"')
check "V19: appending a line: denied" has "$out" deny
check "V19: with the merge=union hint" has "$out" "$hint"
out=$(tool_hook "$S" modules.txt Write '"content":"core\nnet\nbilling\nui\n"')
check "V19: Write adding a line: the hint" has "$out" "$hint"
out=$(tool_hook "$S" modules.txt Edit '"old_string":"net\n","new_string":"network\n"')
check "V19: a changed line: denied" has "$out" deny
check "V19: without the hint" fails has "$out" merge=union
}

## Snapshots and proposals

# V20: snapshots of several agents' work: a path, conflicts exit 4,
# --commits.
v20_snapshots() {
R branch a; R branch b; R branch c
mount_fs
echo a >"$MNT/a/a-only.txt"; echo b >"$MNT/b/b-only.txt"; echo c >>"$MNT/c/d/c.txt"
G "$MNT/c" commit -qam "c: committed"; echo c2 >"$MNT/c/c-wip.txt"
p=$("$BIN" -C "$REPO" snapshot a b c)
eq "V20: a path in the mount" "$MNT/@" "${p%@*}@"
eq "V20: all three agents' work" "a|b|c
c|c2" "$(cat "$p/a-only.txt")|$(cat "$p/b-only.txt")|$(cat "$p/d/c.txt")|$(cat "$p/c-wip.txt")"
check "V20: read-only" fails sh -c "echo x >'$p/x'"
eq "V20: no workspace changed" "A	a-only.txt" "$("$BIN" -C "$REPO" changes a)"
p=$("$BIN" -C "$REPO" snapshot c --commits)
eq "V20: --commits: only the committed work" "c
c|no" "$(cat "$p/d/c.txt")|$([ -e "$p/c-wip.txt" ] && echo yes || echo no)"
echo x >>"$MNT/a/a.txt"; echo y >>"$MNT/b/a.txt"
out=$("$BIN" -C "$REPO" snapshot a b 2>&1); rc=$?
eq "V20: a conflict: exit 4, the path named" "4 yes" "$rc $(has "$out" a.txt && echo yes)"
p=$("$BIN" -C "$REPO" snapshot a)
eq "V20: one agent's work, uncommitted" "a
x" "$(cat "$p/a.txt")"
eq "V20: the same work, the same path" "$p" "$("$BIN" -C "$REPO" snapshot a)"
}

# V21: a child proposes a change to a see-only layer's contract; the lead
# checks it and takes it; the child sees it at its next turn.
v21_a_proposal() {
mkdir "$REPO/api"; echo 'fn v1() {}' >"$REPO/api/contract.rs"; R add -A; R commit -qm api
R branch lead; R branch child
mount_fs
L=$MNT/lead C=$MNT/child
an_agent "$C"
echo 'fn draft() {}' >>"$L/api/contract.rs"
"$BIN" -C "$C" layer add lead >/dev/null
out=$("$BIN" -C "$C" propose api/contract.rs 2>&1)
check "V21: a proposal made" test $? = 0
prop=$(listing "$C" | awk '/^LAYER/{t=1;next} t&&NF{print $1; exit}')
check "V21: shown in the lead's place" test "$prop" != lead
echo 'fn wanted() {}' >>"$MNT/$prop/api/contract.rs"
check "V21: the lead checks it" "$BIN" -C "$L" consent "$prop"
check "V21: and takes it" "$BIN" -C "$L" accept "$prop"
check "V21: the lead's file has it" grep -q wanted "$L/api/contract.rs"
turn_ends "$L"; turn_starts "$C" >/dev/null
check "V21: the child sees it after its turn" grep -q wanted "$C/api/contract.rs"
eq "V21: the lead shown again" "lead" "$(listing "$C" | awk '/^LAYER/{t=1;next} t&&NF{print $1; exit}')"
}

section "use cases: V1 a child sees the lead's draft" v1_see_the_leads_draft
section "use cases: V2 live, frozen, commits-only, a tag" v2_live_frozen_commits_a_tag
section "use cases: V2 re-adding, own files, mistakes" v2_replacing_own_files_mistakes
section "use cases: V3 two see-only layers on one file" v3_two_layers_on_one_file
section "use cases: V4 the base moved on" v4_the_base_moved_on
section "use cases: V5 a person without an agent" v5_a_person_without_an_agent
section "use cases: V6 build on the lead" v6_build_on_the_lead
section "use cases: V7 waiting edits" v7_waiting_edits
section "use cases: V8 git works normally" v8_git_works_normally
section "use cases: V9 the guard" v9_the_guard
section "use cases: V10 the lead changes a waiting line" v10_the_lead_changes_a_waiting_line
section "use cases: V11 see-only and editable on one file" v11_see_only_and_editable_on_one_file
section "use cases: V12 editable back to see-only" v12_editable_back_to_see_only
section "use cases: V13 remove an editable layer" v13_remove_an_editable_layer
section "use cases: V14 two editable layers" v14_two_editable_layers
section "use cases: V15 landing" v15_landing
section "use cases: V16 pushed commits" v16_pushed_commits
section "use cases: V17 a stack of PRs" v17_a_stack_of_prs
section "use cases: V18 a changelog everyone appends to" v18_a_changelog_everyone_appends_to
section "use cases: V18 an entry among another's" v18_an_entry_among_anothers
section "use cases: V18 a method inside another's class" v18_a_method_inside_anothers_class
section "use cases: V19 the same spot without the marker" v19_the_same_spot_without_the_marker
section "use cases: V20 snapshots" v20_snapshots
section "use cases: V21 a proposal" v21_a_proposal
finish
