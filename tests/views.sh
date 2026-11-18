#!/usr/bin/env bash
# End-to-end test of the read-only views `<mount>/@committed/N/` and
# `<mount>/@live/N/` (the workspaces as they are): branch N
# merged with the branches built on it, kept current as their commits and
# the tree of branches change, with conflicts in the files and the
# summary file; the reserved names under `@`.
# usage: tests/views.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=views
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# Commits FILE=CONTENT pairs to branch $1 (made from $2 if new), without
# touching the main worktree: through a temporary index.
commit_on() {
    local b=$1 from=$2; shift 2
    local idx=$T/idx.$$ tree c parent
    parent=$(R rev-parse -q --verify "refs/heads/$b" || R rev-parse "$from")
    GIT_INDEX_FILE=$idx R read-tree "$parent"
    for kv in "$@"; do
        local f=${kv%%=*} v=${kv#*=}
        if [ "$v" = "-" ]; then
            GIT_INDEX_FILE=$idx R update-index --force-remove "$f"
        else
            local o; o=$(printf '%s' "$v" | R hash-object -w --stdin)
            GIT_INDEX_FILE=$idx R update-index --add --cacheinfo "100644,$o,$f"
        fi
    done
    tree=$(GIT_INDEX_FILE=$idx R write-tree)
    c=$(R commit-tree "$tree" -p "$parent" -m "$b")
    R update-ref "refs/heads/$b" "$c"
    rm -f "$idx"
}
# Makes branch $1 built on $2.
built_on() { R config "branch.$1.gitfoldParent" "refs/heads/$2"; }
# Waits (up to 10 s) until command "$@" succeeds; its result.
until_ok() { for _ in $(seq 100); do "$@" >/dev/null 2>&1 && return 0; sleep 0.1; done; "$@"; }
has() { [ "$(cat "$1" 2>/dev/null)" = "$2" ]; }

V=@committed/api

merged_with_whats_built_on_it() {
commit_on api main a.txt=$'a\n'
commit_on ui api $'ui.txt=ui\n' $'d/b.txt=b\nui\n'
commit_on web api $'web.txt=web\n' d/c.txt=-
built_on ui api; built_on web api
mount_fs
eq "ui's file" "ui" "$(cat "$MNT/$V/ui.txt")"
eq "web's file" "web" "$(cat "$MNT/$V/web.txt")"
eq "ui's change" $'b\nui' "$(cat "$MNT/$V/d/b.txt")"
check "web's deletion" fails test -e "$MNT/$V/d/c.txt"
eq "the listing" ".gitfold-conflicts a.txt d ui.txt web.txt" "$(LC_ALL=C ls -A "$MNT/$V" | tr '\n' ' ' | sed 's/ $//')"
eq "no conflicts" "" "$(cat "$MNT/$V/.gitfold-conflicts")"
check "read-only" fails touch "$MNT/$V/new"
check "read-only files" fails sh -c "echo x >>'$MNT/$V/a.txt'"
check "no repository" fails test -e "$MNT/$V/.git"
eq "listed under @committed once opened" "api" "$(ls "$MNT/@committed")"
eq "the branch itself unchanged" "" "$(R show api:ui.txt 2>/dev/null)"
}

conflicts() {
commit_on api main $'gone.txt=g\n'
commit_on ui api $'a.txt=ui\n' 'gone.txt=-'
commit_on web api $'a.txt=web\n' $'gone.txt=web\n'
built_on ui api; built_on web api
mount_fs
check "markers name the sides by id" grep -qE '^<<<<<<< [0-9a-f]{40}$' "$MNT/$V/a.txt"
eq "the member by its commit" $'ui\n=======\nweb\n>>>>>>> '"$(R rev-parse web)" "$(sed 1d "$MNT/$V/a.txt")"
eq "modify/delete: the changed version, as git leaves it" "web" "$(cat "$MNT/$V/gone.txt")"
eq "the summary, as git says it" $'CONFLICT (contents)\tapi+ui, web\ta.txt\nCONFLICT (modify/delete)\tapi+ui, web\tgone.txt' \
    "$(cat "$MNT/$V/.gitfold-conflicts")"
}

gits_merge() {
commit_on api main $'CHANGES=c\n' $'DRIVEN=d\n' $'.gitattributes=CHANGES merge=union\nDRIVEN merge=mine\n'
commit_on ui api a.txt=- $'renamed.txt=a\n' $'CHANGES=c\nui\n' $'DRIVEN=ui\n'
commit_on web api $'a.txt=a\nweb\n' $'CHANGES=c\nweb\n' $'DRIVEN=web\n'
# The repository's merge driver, by a path relative to its top, in settings
# it includes for itself only.
mkdir -p "$REPO/tools"
printf '#!/bin/sh\necho driven >"$1"\n' >"$REPO/tools/drive.sh"
chmod +x "$REPO/tools/drive.sh"
printf '[merge "mine"]\n\tdriver = tools/drive.sh %%A\n' >"$T/drivers"
R config "includeIf.gitdir:$REPO/.path" "$T/drivers"
built_on ui api; built_on web api
refs_before=$(R for-each-ref | wc -l)
objects_before=$(R count-objects -v | sed -n 's/^count: //p')
mount_fs
eq "an edit follows a rename" $'a\nweb' "$(cat "$MNT/$V/renamed.txt")"
eq "merge=union" $'c\nui\nweb' "$(cat "$MNT/$V/CHANGES")"
eq "the repository's merge driver" "driven" "$(cat "$MNT/$V/DRIVEN")"
eq "no ref in the repository" "$refs_before" "$(R for-each-ref | wc -l)"
eq "no object in the repository" "$objects_before" "$(R count-objects -v | sed -n 's/^count: //p')"
check "the private repository's" test -d "$COMMON/gitfold/.views.git"
}

kept_current() {
commit_on ui main $'ui.txt=1\n'
built_on ui main
R branch api main; built_on ui api
mount_fs
eq "at first" "1" "$(cat "$MNT/$V/ui.txt")"
stat "$MNT/$V/d/b.txt" >/dev/null; ls "$MNT/$V/d" >/dev/null   # cached by the kernel
commit_on ui api $'ui.txt=2\n' $'d/b.txt=changed\n' $'d/new.txt=new\n'
check "a new commit shows" until_ok has "$MNT/$V/ui.txt" 2
eq "a file the kernel had cached" "changed" "$(cat "$MNT/$V/d/b.txt")"
eq "its size too" 8 "$(stat -c %s "$MNT/$V/d/b.txt")"
eq "a new file in a listed directory" "b.txt c.txt new.txt" "$(ls "$MNT/$V/d" | tr '\n' ' ' | sed 's/ $//')"
commit_on ui api d/new.txt=- d/c.txt=- d/b.txt=-
check "a directory whose files all went" until_ok fails test -e "$MNT/$V/d"
commit_on web main $'web.txt=web\n'
built_on web api
check "a branch built on it later" until_ok has "$MNT/$V/web.txt" web
R config --unset branch.web.gitfoldParent
check "and gone when it isn't any more" until_ok fails test -e "$MNT/$V/web.txt"
commit_on api main $'a.txt=api\n'
check "the branch's own commit" until_ok has "$MNT/$V/a.txt" api
check "with a new time" test "$(stat -c %Y "$MNT/$V/a.txt")" -gt 1577836800
stat "$MNT/$V/ui.txt" >/dev/null
commit_on ui api ui.txt=- $'ui.txt/x=x\n'
check "a file now a directory" until_ok test -f "$MNT/$V/ui.txt/x"
check "seen as one" test -d "$MNT/$V/ui.txt"
}

names() {
R branch feature/x main
R branch committed main
R branch api main
mount_fs
check "@committed is there" test -d "$MNT/@committed"
check "@live is there" test -d "$MNT/@live"
check "a branch with a slash" test -f "$MNT/@committed/feature/x/a.txt"
eq "its namespace lists it" "x" "$(ls "$MNT/@committed/feature")"
check "no such branch" fails test -e "$MNT/@committed/nosuch"
check "a branch named committed: its snapshot by full name" test -f "$MNT/@refs/heads/committed/a.txt"
check "@committed is no snapshot of it" fails test -e "$MNT/@committed/a.txt"
check "a view of the root" test -f "$MNT/@committed/main/a.txt"
}

root_shows_opened_branches() {
commit_on api main $'api.txt=api\n'
built_on api main
mount_fs
check "a branch on main without a workspace isn't in main's view" fails test -e "$MNT/@committed/main/api.txt"
ls "$MNT/api" >/dev/null    # opens its workspace
check "once opened, it is" until_ok test -f "$MNT/@committed/main/api.txt"
}

# Many refreshes while readers walk the view: the files must end as the
# branches say, never as an older view cached for good.
refresh_under_readers() {
R branch api main
commit_on ui api $'n.txt=0\n'
built_on ui api
mount_fs
( while :; do cat "$MNT/$V"/*.txt "$MNT/$V/d"/* >/dev/null 2>&1; ls -lR "$MNT/$V" >/dev/null 2>&1; done ) &
READER=$!
for i in $(seq 20); do
    commit_on ui api "n.txt=$i" "f$i.txt=$i" "d/b.txt=$i"
    until_ok has "$MNT/$V/n.txt" "$i" || fail "refresh $i"
done
kill $READER; wait $READER 2>/dev/null
eq "the last content" 20 "$(cat "$MNT/$V/d/b.txt")"
eq "every file listed" 20 "$(ls "$MNT/$V" | grep -c '^f[0-9]*\.txt$')"
for i in 1 7 20; do eq "f$i.txt" "$i" "$(cat "$MNT/$V/f$i.txt")"; done
}

live_views() {
commit_on api main a.txt=$'a\n' $'.gitignore=*.o\n'
commit_on ui api $'ui.txt=ui\n'
built_on ui api
mount_fs
L=@live/api
eq "the tips while no workspace is open" "ui" "$(cat "$MNT/$L/ui.txt")"
echo edited >"$MNT/ui/ui.txt"
echo new >"$MNT/ui/new.txt"
echo obj >"$MNT/ui/x.o"
rm "$MNT/ui/a.txt"
check "an uncommitted edit" until_ok has "$MNT/$L/ui.txt" edited
check "a new file" has "$MNT/$L/new.txt" new
check "an ignored file left out" fails test -e "$MNT/$L/x.o"
check "a deletion" fails test -e "$MNT/$L/a.txt"
eq "@committed has the tips" "ui" "$(cat "$MNT/@committed/api/ui.txt")"
echo mine >"$MNT/api/api.txt"
check "the view's own workspace" until_ok has "$MNT/$L/api.txt" mine
check "read-only" fails touch "$MNT/$L/other"
eq "listed under @live once opened" "api" "$(ls "$MNT/@live")"
P() { git -C "$COMMON/gitfold/.views.git" "$@"; }
check "a workspace's live tree kept" P rev-parse -q --verify refs/gitfold/live/ui
"$BIN" -C "$REPO" remove --force ui >/dev/null
check "dropped when it is removed" fails P rev-parse -q --verify refs/gitfold/live/ui
check "the view shows the tip again" until_ok has "$MNT/$L/ui.txt" ui
}

resting_views() {
commit_on api main a.txt=$'a\n'
commit_on ui api $'ui.txt=ui\n'
commit_on web api $'web.txt=web\n'
built_on ui api; built_on web api
GITFOLD_VIEW_REST_SECS=2 mount_fs
echo e1 >"$MNT/ui/ui.txt"
check "awake: current" until_ok has "$MNT/@live/api/ui.txt" e1
eq "@committed opened" "web" "$(cat "$MNT/@committed/api/web.txt")"
echo e2 >"$MNT/ui/ui.txt"
commit_on web api $'web.txt=web2\n'
sleep 5
eq "a resting @live view wakes current" "e2" "$(cat "$MNT/@live/api/ui.txt")"
eq "a resting @committed view wakes current" "web2" "$(cat "$MNT/@committed/api/web.txt")"
}

live_trees_side_by_side() {
commit_on api main a.txt=$'a\n'
for b in ui web db; do commit_on "$b" api "$b.txt=$b"; built_on "$b" api; done
# git making a live tree takes 2 s: one after another, the three would
# take 6 s, past the 5 s the first access waits.
mkdir "$T/slow"
printf '#!/bin/sh\ncase " $* " in *" add -A "*) sleep 2 ;; esac\nexec %s "$@"\n' \
    "$(command -v git)" >"$T/slow/git"
chmod +x "$T/slow/git"
PATH=$T/slow:$PATH mount_fs
for b in ui web db; do echo "$b: edited" >"$MNT/$b/$b.txt"; done
s=$(date +%s%N)
cat "$MNT/@live/api/.gitfold-conflicts" >/dev/null
took=$(( ($(date +%s%N) - s) / 1000000 ))
for b in ui web db; do
    eq "the first access shows $b's edit" "$b: edited" "$(cat "$MNT/@live/api/$b.txt")"
done
check "made side by side (took $took ms)" test "$took" -lt 4500
}

unrelated_history() {
commit_on api main a.txt=$'a\n'
R update-ref refs/heads/lone "$(R commit-tree "$(R mktree </dev/null)" -m lone)"
built_on lone api
mount_fs
eq "left out, noted" "skipped	lone	no history in common with refs/heads/api" "$(grep lone "$MNT/@committed/api/.gitfold-conflicts")"
}

# The private repository garbage-collected in a partial clone (blob:none, of
# a local remote): a blob its trees name that the clone hasn't fetched (one
# no view opened, here by hand) is the promisor remote's, left alone.
gc_in_a_partial_clone() {
commit_on api main a.txt=$'a\n'
commit_on ui api $'ui.txt=ui\n' $'a.txt=ui\na\n'
commit_on web api $'web.txt=web\n' $'a.txt=a\nweb\n'
commit_on far main $'far.txt=far\n'
git clone -q --bare "$REPO" "$T/remote.git"
git -C "$T/remote.git" config uploadpack.allowFilter true
rm -rf "$REPO"
git clone -q --no-checkout --filter=blob:none "file://$T/remote.git" "$REPO"
for b in api ui web; do R branch -q "$b" "origin/$b"; done
built_on ui api; built_on web api
mount_fs
eq "the view, merged" $'ui\na\nweb' "$(cat "$MNT/$V/a.txt")"
unmount_fs
# git in the private repository with the mount's settings (no hook, no lazy
# fetch).
P() { GIT_NO_LAZY_FETCH=1 git -C "$COMMON/gitfold/.views.git" -c core.hooksPath=/dev/null "$@"; }
far=$(R rev-parse origin/far:far.txt)
check "a blob not fetched" fails env GIT_NO_LAZY_FETCH=1 git -C "$REPO" cat-file -e "$far"
P update-ref refs/gitfold/views/far "$(P commit-tree -m far "$(printf '100644 blob %s\tfar.txt\n' "$far" | P mktree --missing)")"
# What the mount's `gc --auto` runs past its thresholds: an incremental
# repack (loose objects), and a full gc (too many packs), pruning at once.
check "an incremental repack there" P repack -d -l -q
check "a full gc there" P -c gc.pruneExpire=now gc --quiet
eq "the view's tree kept" $'ui\na\nweb' "$(P show refs/gitfold/views/committed/api:a.txt)"
}

# Conflict notices: a commit in a workspace, checked against the work of the
# branches its work meets; messages from gitfold in their inboxes.

# How many messages there are in the inbox of branch $1.
notices() { ls "$COMMON/gitfold/.tasks/$1/inbox" 2>/dev/null | grep -vc '^\.'; }
# The messages of branch $1, oldest first, each ending with `---`, without
# their `time:` header.
said() {
    local d=$COMMON/gitfold/.tasks/$1/inbox m
    for m in $(ls "$d" 2>/dev/null); do cat "$d/$m"; echo ---; done | grep -v '^time: '
}
# Waits (up to 10 s) until branch $1 has $2 messages.
until_told() { for _ in $(seq 100); do [ "$(notices "$1")" = "$2" ] && return 0; sleep 0.1; done; return 1; }
# Waits long enough for a notice that should not come: two sync rounds and
# a live tree.
settle() { sleep 4; }
# Commits what the workspace of $1 changed (`git commit -am`).
commit_in() { G "$MNT/$1" commit -qam "$1" >/dev/null; }
# Takes every message of the branches $@ (deletes them).
clear_inbox() { for b in "$@"; do rm -f "$COMMON/gitfold/.tasks/$b/inbox"/[0-9]*; done; }
# Opens the workspaces $@, and waits past their first sync, which only
# records where they are.
open() { for b in "$@"; do ls "$MNT/$b" >/dev/null; done; sleep 2; }
# api, and ui built on it (made if it isn't yet; following api with --follow),
# mounted, both open.
api_ui() {
    R branch api main
    R rev-parse -q --verify refs/heads/ui >/dev/null || R branch ui main
    built_on ui api
    [ "${1:-}" = --follow ] || R config branch.ui.gitfoldFollow false
    mount_fs
    open api ui
}

notices_uncommitted_side() {
api_ui --follow
echo ui >"$MNT/ui/a.txt"
echo api >"$MNT/api/a.txt"; commit_in api
short=$(R rev-parse --short api)
text="from: gitfold
subject: conflict: a.txt

api committed $short; it conflicts with ui's uncommitted a.txt.
If it needs resolving, ui's change is the one still open, so that's the cheapest place to adjust.
ui follows api: it takes the commit once they don't conflict (until then following says nothing moved).
---"
check "the other side is told" until_told ui 1
eq "what it is told" "$text" "$(said ui)"
eq "the committer too, the same (it is where they land, too)" "$text" "$(said api)"
eq "nothing moved" "$(R rev-parse main)" "$(R rev-parse ui)"
}

notices_committer_parents_uncommitted() {
api_ui --follow
echo api >"$MNT/api/a.txt"
echo ui >"$MNT/ui/a.txt"; commit_in ui
short=$(R rev-parse --short ui)
check "the parent, whose work is uncommitted, is told" until_told api 1
check "so is the committer" until_told ui 1
eq "what" "ui committed $short; it conflicts with api's uncommitted a.txt." "$(said api | sed -n 4p)"
eq "the same to both" "$(said api)" "$(said ui)"
}

notices_siblings() {
R branch shop main
R branch api main; commit_on web main $'d/b.txt=web\n'
for b in api web; do built_on "$b" shop; R config "branch.$b.gitfoldFollow" false; done
mount_fs
open shop api web
echo web >"$MNT/web/a.txt"
echo api >"$MNT/api/a.txt"; echo api >"$MNT/api/d/b.txt"; commit_in api
short=$(R rev-parse --short api)
check "the sibling is told" until_told web 1
check "the parent too" until_told shop 1
check "the committer too" until_told api 1
eq "uncommitted, then committed" "api committed $short; it conflicts with web's uncommitted a.txt.
If it needs resolving, web's change is the one still open, so that's the cheapest place to adjust.

api committed $short; it conflicts with web's committed d/b.txt, so the two will conflict when they land in shop.
If it needs resolving, api committed last." "$(said shop | sed '1,3d;$d')"
eq "the same to each" "$(said shop)" "$(said web)"
eq "and to the committer" "$(said shop)" "$(said api)"
eq "the subject names the paths" "subject: conflict: a.txt, d/b.txt" "$(said shop | sed -n 2p)"
}

notices_both_committed_following() {
commit_on ui main $'a.txt=ui\n'
api_ui --follow
echo api >"$MNT/api/a.txt"; commit_in api
short=$(R rev-parse --short api)
check "the committer is told" until_told api 1
check "so is the other side" until_told ui 1
eq "they land in the committer; the child is where it gets resolved" "api committed $short; it conflicts with ui's committed a.txt, so the two will conflict when they land in api.
If it needs resolving, ui is built on api: that's where it gets resolved (following is paused on it)." "$(said api | sed '1,3d;$d')"
}

notices_once() {
api_ui
echo ui >"$MNT/ui/a.txt"
echo api >"$MNT/api/a.txt"; commit_in api
check "told" until_told ui 1
clear_inbox ui api
echo api2 >"$MNT/api/a.txt"; commit_in api
echo x >"$MNT/api/x.txt"; G "$MNT/api" add x.txt; commit_in api
settle
eq "a conflict already there isn't told again" "0 0" "$(notices ui) $(notices api)"
echo a >"$MNT/api/a.txt"; commit_in api
settle
eq "nor gone" 0 "$(notices ui)"
echo api3 >"$MNT/api/a.txt"; commit_in api
check "back: told again" until_told ui 1
}

notices_not_for_moves() {
R config user.name t; R config user.email t@t   # git rebase commits
R branch api main
R branch web main; built_on web api; R config branch.web.gitfoldFollow false
commit_on ui main $'ui.txt=ui\n'; built_on ui api
mount_fs
open api web ui
echo web >"$MNT/web/a.txt"
echo api >"$MNT/api/a.txt"; commit_in api
check "web is told of api's commit" until_told web 1
clear_inbox web api
check "ui follows api" until_ok R merge-base --is-ancestor api ui
settle
eq "ui moving onto api brings api's change: not told again" "0 0" "$(notices web) $(notices api)"
}

notices_not_for_the_parents_commits() {
api_ui
R branch web main; built_on web api; R config branch.web.gitfoldFollow false
open web
echo web >"$MNT/web/a.txt"
echo ui >"$MNT/ui/ui.txt"; G "$MNT/ui" add ui.txt; commit_in ui
echo api >"$MNT/api/a.txt"; commit_in api
check "web is told of api's commit" until_told web 1
clear_inbox web api
G "$MNT/ui" rebase -q api
settle
eq "ui rebased onto api by hand: api's change isn't ui's" "0 0" "$(notices web) $(notices api)"
echo web >"$MNT/web/d/b.txt"
echo api >"$MNT/api/d/b.txt"; commit_in api
check "web is told of api's next commit" until_told web 1
clear_inbox web api
G "$MNT/ui" merge -q --no-edit api
settle
eq "nor ui merging api" "0 0" "$(notices web) $(notices api)"
}

notices_one_step() {
api_ui
echo ui >"$MNT/ui/a.txt"; echo ui >"$MNT/ui/d/b.txt"
commit_on two api $'a.txt=api\n'
c1=$(R rev-parse two)
commit_on two api $'d/b.txt=api\n'
R update-ref refs/heads/api "$c1"; R update-ref refs/heads/api two
check "told" until_told ui 1
settle
eq "once, for both commits" 1 "$(notices ui)"
eq "both paths" "subject: conflict: a.txt, d/b.txt" "$(said ui | sed -n 2p)"
}

notices_not_at_first_sync() {
api_ui
echo ui >"$MNT/ui/a.txt"
unmount_fs
commit_on api main $'a.txt=api\n'
mount_fs
open api ui
echo api2 >"$MNT/api/d/b.txt"; echo ui >"$MNT/ui/d/b.txt"; G "$MNT/api" commit -qm api -- d/b.txt
check "the next commit is told" until_told ui 1
eq "not the one made while unmounted" "subject: conflict: d/b.txt" "$(said ui | sed -n 2p)"
}

# Each commit is checked after the one before: the last one told says what
# those before did.
notices_off() {
api_ui
R config gitfold.conflictNotices false
echo ui >"$MNT/ui/a.txt"; echo api >"$MNT/api/a.txt"; commit_in api
settle   # checked with the settings as they are then
R config branch.ui.gitfoldConflictNotices true
echo ui >"$MNT/ui/d/b.txt"; echo api >"$MNT/api/d/b.txt"; commit_in api
check "on for one branch only: it is told" until_told ui 1
eq "off for the repository: not before, and not the others" "subject: conflict: d/b.txt 0" "$(said ui | sed -n 2p) $(notices api)"
R config --unset gitfold.conflictNotices; R config branch.ui.gitfoldConflictNotices false
clear_inbox ui
echo ui >"$MNT/ui/c.txt"; G "$MNT/ui" add c.txt; echo api >"$MNT/api/c.txt"; G "$MNT/api" add c.txt; commit_in api
check "off for one branch: the others are told" until_told api 1
eq "it isn't" 0 "$(notices ui)"
}

notices_binary() {
printf 'bin\0ary\n' >"$REPO/b.dat"; R add b.dat; R commit -qm bin
api_ui
printf 'ui\0\n' >"$MNT/ui/b.dat"; printf 'api\0\n' >"$MNT/api/b.dat"; commit_in api
check "a binary file" until_told ui 1
check "named" grep -q "uncommitted b.dat" <(said ui)
}

# An origin whose main is the repository's: branches made from origin/main.
with_origin() {
    git init -q --bare "$T/origin.git"
    R remote add origin "$T/origin.git"; R push -q origin main; R fetch -q origin
    R remote set-head origin main >/dev/null
}

notices_without_parents() {
with_origin
R branch -q --no-track api origin/main; commit_on web origin/main $'d/b.txt=web\n'
mount_fs
open api web
echo web >"$MNT/web/a.txt"
echo api >"$MNT/api/a.txt"; echo api >"$MNT/api/d/b.txt"; commit_in api
short=$(R rev-parse --short api)
check "the other branch without a parent is told" until_told web 1
check "so is the committer" until_told api 1
eq "siblings under main" "api committed $short; it conflicts with web's uncommitted a.txt.
If it needs resolving, web's change is the one still open, so that's the cheapest place to adjust.

api committed $short; it conflicts with web's committed d/b.txt, so the two will conflict when they land in main.
If it needs resolving, api committed last." "$(said web | sed '1,3d;$d')"
eq "the same to both" "$(said web)" "$(said api)"
}

notices_without_a_parent_and_built_on_main() {
with_origin
R checkout -q --detach   # main free for its workspace
R branch -q --no-track api origin/main
R branch -q web main; built_on web main; R config branch.web.gitfoldFollow false
mount_fs
open main api web
echo web >"$MNT/web/a.txt"
echo api >"$MNT/api/a.txt"; commit_in api
check "a branch built on main is told" until_told web 1
check "main too, where they land" until_told main 1
check "and the committer" until_told api 1
eq "the same to each" "$(said web)" "$(said main)"
clear_inbox web main api
echo main >"$MNT/main/d/b.txt"; echo api >"$MNT/api/d/b.txt"; commit_in main
check "main's own commit: the branch without a parent is told" until_told api 1
eq "main is told too, once" 1 "$(notices main)"
}

section "views: merged with what is built on it" merged_with_whats_built_on_it
section "views: conflicts" conflicts
section "views: git's merge" gits_merge
section "views: kept current" kept_current
section "views: names under @" names
section "views: the root shows opened branches" root_shows_opened_branches
section "views: refreshes under readers" refresh_under_readers
section "views: @live, the workspaces as they are" live_views
section "views: resting, and woken current" resting_views
section "views: @live makes its live trees side by side" live_trees_side_by_side
section "views: a branch with unrelated history left out" unrelated_history
section "views: garbage collection in a partial clone" gc_in_a_partial_clone
section "notices: the uncommitted side" notices_uncommitted_side
section "notices: a child's commit, its parent's uncommitted work" notices_committer_parents_uncommitted
section "notices: siblings, landing in their parent" notices_siblings
section "notices: both committed, a child following the committer" notices_both_committed_following
section "notices: each conflict once" notices_once
section "notices: not for gitfold's moves" notices_not_for_moves
section "notices: not for the parent's commits" notices_not_for_the_parents_commits
section "notices: several commits at once, one notice" notices_one_step
section "notices: not at the first sync" notices_not_at_first_sync
section "notices: off for the repository, or for a branch" notices_off
section "notices: a binary file" notices_binary
section "notices: branches without a parent, siblings under main" notices_without_parents
section "notices: a branch without a parent and one built on main" notices_without_a_parent_and_built_on_main
finish
