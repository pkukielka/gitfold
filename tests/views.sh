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
export GIT_NO_LAZY_FETCH=1
P() { git -C "$COMMON/gitfold/.views.git" "$@"; }
far=$(R rev-parse origin/far:far.txt)
check "a blob not fetched" fails R cat-file -e "$far"
P update-ref refs/gitfold/views/far "$(P commit-tree -m far "$(printf '100644 blob %s\tfar.txt\n' "$far" | P mktree --missing)")"
# As the mount runs it, whatever the number of loose objects.
check "git gc there" P -c core.hooksPath=/dev/null -c gc.pruneExpire=now gc --quiet
eq "the view's tree kept" $'ui\na\nweb' "$(P show refs/gitfold/views/committed/api:a.txt)"
check "nor fetched by it" fails R cat-file -e "$far"
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
section "views: a branch with unrelated history left out" unrelated_history
section "views: garbage collection in a partial clone" gc_in_a_partial_clone
finish
