#!/usr/bin/env bash
# End-to-end test of workspace rebases (the base following the branch tip) with
# git operations that move the branch: commits, merges and rebases with
# conflicts, stash, amend, resets, bisect, external moves.
# usage: tests/rebase.sh
# Creates /tmp/gf-rb-test/{repo,mnt}; removes them at the end unless KEEP=1.
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
T=/tmp/gf-rb-test
REPO=$T/repo
MNT=$T/mnt
LOG=$T/gitfold.log
. "$(dirname "$0")/lib.sh"

W=$MNT/ws
STATE=$REPO/.git/gitfold/ws
G() { git -C "$W" -c user.name=t -c user.email=t@t -c core.editor=true "$@"; }
R() { git -C "$REPO" -c user.name=t -c user.email=t@t "$@"; }
# Waits until the workspace's base is the branch tip.
wait_base() {
    for _ in $(seq 100); do
        [ "$(cat "$STATE/base" 2>/dev/null)" = "$(R rev-parse ws)" ] && return 0
        sleep 0.1
    done
    return 1
}
# The workspace as seen through the mount: names, types, modes, sizes, content.
view() {
    (cd "$W" && find . -path ./.git -prune -o -printf '%p %y %m %s\n' | sort &&
        find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 md5sum) | md5sum
}
upper_files() { find "$STATE/upper" -type f -o -type l | wc -l; }
fails() { ! "$@"; }
clean() { [ -z "$(G status --porcelain -uall)" ] && [ -z "$(G diff-files --name-only)" ]; }

rm -rf "$T"; mkdir -p "$MNT" "$REPO"
R init -q -b main
(
    cd "$REPO"
    echo a >a.txt; echo b >b.txt; echo del >del.txt
    mkdir -p dir/sub many; echo c >dir/c.txt; echo d >dir/sub/d.txt
    printf '#!/bin/sh\n' >exec.sh; chmod 755 exec.sh; ln -s a.txt link
    for i in $(seq 100); do echo "$i" >"many/f$i.txt"; done
)
R add -A; R commit -qm init
R branch ws
for b in other other2; do # branches whose a.txt conflicts with the workspace's
    R checkout -q -b $b main; echo "$b" >"$REPO/a.txt"; R commit -qam $b
done
R checkout -q main
mount_fs

echo "== commit"
check "workspace opens clean" clean
echo x >>"$W/a.txt"; echo new >"$W/new.txt"; rm "$W/del.txt"; chmod 644 "$W/exec.sh"
G add -A
before=$(stat -c '%i %Y %Z %s %a' "$W/a.txt")
check "commit" G commit -qm one
check "base follows the commit" wait_base
eq "upper emptied (committed files dropped)" 0 "$(upper_files)"
check "clean by stat alone" clean
eq "dropped file keeps its stat" "$before" "$(stat -c '%i %Y %Z %s %a' "$W/a.txt")"
check "deletion still shown" test ! -e "$W/del.txt"
eq "mode change kept" 644 "$(stat -c %a "$W/exec.sh")"
eq "base pinned" "$(R rev-parse ws)" "$(R rev-parse refs/gitfold/ws)"

echo "== chained commands while rebasing"
bad=0
for i in $(seq 20); do
    echo "$i" >>"$W/b.txt" && G commit -qam "c$i" && clean && [ "$(tail -1 "$W/b.txt")" = "$i" ] ||
        bad=$((bad + 1))
    sleep "0.$((RANDOM % 12))"
done
eq "commit && status && cat, 20 times" 0 "$bad"
check "base follows" wait_base
check "clean after the chain" clean
eq "upper empty after the chain" 0 "$(upper_files)"

echo "== merge with a conflict"
base=$(cat "$STATE/base")
check "merge conflicts" fails G merge -q other
sleep 1.5
eq "base unchanged while conflicted" "$base" "$(cat "$STATE/base")"
echo resolved >"$W/a.txt"; G add a.txt
check "commit the merge" G commit -q --no-edit
check "base follows the merge" wait_base
check "clean after the merge" clean
eq "resolved content" resolved "$(cat "$W/a.txt")"
eq "merge has two parents" 3 "$(G cat-file -p HEAD | grep -c '^\(parent\|tree\)')"

echo "== merge --abort"
v=$(view)
check "merge conflicts" fails G merge -q other2
check "merge --abort" G merge --abort
check "clean after abort" clean
eq "view unchanged after abort" "$v" "$(view)"

echo "== rebase with a conflict"
# other3: the workspace's tip with a conflicting b.txt, so only "mine" is replayed
R checkout -q -b other3 ws; echo other3 >"$REPO/b.txt"; R commit -qam other3; R checkout -q main
echo mine >"$W/b.txt"; G commit -qam mine; wait_base
check "rebase conflicts" fails G rebase -q other3
sleep 1.5
check "no rebase of the base mid-rebase" test "$(cat "$STATE/base")" = "$(R rev-parse ws)"
echo merged >"$W/b.txt"; G add b.txt
check "rebase --continue" G rebase --continue
check "base follows the rebase" wait_base
check "clean after the rebase" clean
eq "rebased content" merged "$(cat "$W/b.txt")"
check "other3 is an ancestor" G merge-base --is-ancestor other3 HEAD

echo "== rebase --abort"
echo again >"$W/b.txt"; G commit -qam again; wait_base
tip=$(R rev-parse ws); v=$(view)
check "rebase conflicts" fails G rebase -q other2
check "rebase --abort" G rebase --abort
check "clean after abort" clean
eq "tip unchanged" "$tip" "$(R rev-parse ws)"
eq "view unchanged after abort" "$v" "$(view)"

echo "== stash"
echo stashed >>"$W/a.txt"; echo untracked >"$W/u.txt"
check "stash -u" G stash -q -u
check "clean after stash" clean
check "untracked file stashed" test ! -e "$W/u.txt"
check "stash pop" G stash pop -q
eq "stashed change back" stashed "$(tail -1 "$W/a.txt")"
check "untracked file back" test -e "$W/u.txt"
G checkout -q -- a.txt; rm "$W/u.txt"
check "clean after checkout" clean

echo "== amend"
echo amend >>"$W/a.txt"; G commit -qam am1; wait_base
old=$(R rev-parse ws)
check "amend" G commit -q --amend -m am2
check "base follows the amend" wait_base
check "clean after amend" clean
eq "pin moved off the replaced commit" "$(R rev-parse ws)" "$(R rev-parse refs/gitfold/ws)"
check "base is not the replaced commit" test "$(cat "$STATE/base")" != "$old"

echo "== reset --soft (view kept: copies up and whiteouts)"
echo f >"$W/F.txt"; rm "$W/many/f1.txt"; echo changed >"$W/dir/sub/d.txt"
G add -A; G commit -qm addF; wait_base
v=$(view)
check "reset --soft HEAD~2" G reset -q --soft HEAD~2
check "base follows the reset" wait_base
eq "view unchanged by the reset" "$v" "$(view)"
eq "status shows the undone commits" "A  F.txt|M  a.txt|M  dir/sub/d.txt|D  many/f1.txt" \
    "$(G status --porcelain | paste -sd'|')"
check "recommit" G commit -qm recommit
check "base follows" wait_base
check "clean after recommit" clean
eq "upper empty again" 0 "$(upper_files)"

echo "== reset --hard"
check "reset --hard HEAD~1" G reset -q --hard HEAD~1
check "base follows the reset" wait_base
check "clean after reset --hard" clean
check "file gone with the commit" test ! -e "$W/F.txt"
check "file back with the commit" test -e "$W/many/f1.txt"
eq "content back" d "$(cat "$W/dir/sub/d.txt")"

echo "== directory deleted and recreated (opaque)"
rm -r "$W/many"; mkdir "$W/many"; for i in $(seq 100); do echo "$i" >"$W/many/f$i.txt"; done
echo extra >"$W/many/new.txt"; rm "$W/many/f50.txt"
G add -A; G commit -qm regenerated; wait_base
check "clean after committing it" clean
eq "upper empty again" 0 "$(upper_files)"
check "deleted file stays deleted" test ! -e "$W/many/f50.txt"
eq "listing" 100 "$(ls "$W/many" | wc -l)"

echo "== type changes (file <-> directory)"
rm -r "$W/dir/sub"; echo file >"$W/dir/sub"; G add -A; G commit -qm sub-file; wait_base
subfile=$(R rev-parse ws); v=$(view)
check "reset --soft over a dir -> file change" G reset -q --soft HEAD~1
check "base follows" wait_base
eq "view keeps the file" "$v" "$(view)"
check "reset --hard back to the directory" G reset -q --hard HEAD
check "base follows" wait_base
check "clean with the directory" clean
v=$(view)
check "reset --soft over a file -> dir change" G reset -q --soft "$subfile"
check "base follows" wait_base
eq "view keeps the directory" "$v" "$(view)"
check "directory listing intact" test -f "$W/dir/sub/d.txt"
check "reset --hard to the file" G reset -q --hard HEAD
check "base follows" wait_base
check "clean with the file" clean
eq "file content" file "$(cat "$W/dir/sub")"

echo "== rebase retried after a failed copy-up"
rm -f "$W/dir/sub"; mkdir "$W/dir/sub"; for f in d e g; do echo $f >"$W/dir/sub/$f.txt"; done
G add -A; G commit -qm sub-dir; wait_base
rm -r "$W/dir/sub"; echo file >"$W/dir/sub"; G add -A; G commit -qm sub-file-2; wait_base
x=$(R rev-parse ws)
G reset -q --hard HEAD~1; wait_base
sleep 4                                # git's rewritten files dropped: lower again
v=$(view)
chmod 555 "$STATE/tmp"                 # copy-ups fail
R update-ref refs/heads/ws "$x"        # dir/sub becomes a file in the base
sleep 1.5
check "rebase failed and was logged" grep -q "copying up dir/sub" "$LOG"
chmod 755 "$STATE/tmp"
check "rebase succeeds on retry" wait_base
unmount_fs; mount_fs; ls "$W" >/dev/null
eq "view intact after the failed attempt (remounted)" "$v" "$(view)"
G reset -q --hard "$x"; wait_base
sed -i '/copying up dir\/sub/d' "$LOG"  # expected error, not for the final check

echo "== bisect"
base=$(cat "$STATE/base")
check "bisect start" G bisect start HEAD HEAD~3
sleep 1.5
eq "base unchanged while bisecting" "$base" "$(cat "$STATE/base")"
check "bisect reset" G bisect reset
check "clean after bisect" clean

echo "== branch moved from outside"
v=$(view); tip=$(R rev-parse ws)
R update-ref refs/heads/ws ws~1
check "base follows the external move" wait_base
eq "view unchanged" "$v" "$(view)"
check "status shows the difference" test -n "$(G status --porcelain)"
G reset -q --soft "$tip"; wait_base
check "clean after moving back" clean

echo "== branch being rebased in another worktree"
R checkout -q -b busy main; echo busy >"$REPO/a.txt"; R commit -qam busy
R rebase -q other >/dev/null 2>&1
check "main worktree mid-rebase (detached)" test -d "$REPO/.git/rebase-merge"
check "branch refused while being rebased" test ! -e "$MNT/busy"
R rebase --abort; R checkout -q main

echo "== stale opaque marker over a file (crash residue)"
mkdir "$W/stale"; echo s >"$W/stale/x"; G add -A; G commit -qm stale-dir; wait_base
rm -r "$W/stale"; echo file >"$W/stale"   # a file where the directory was
unmount_fs
printf 'Ostale\0' >>"$STATE/markers"      # as if a crash in rmdir left the marker
mount_fs; ls "$W" >/dev/null
echo y >>"$W/a.txt"; G commit -qm other a.txt   # the base keeps the directory
check "rebase not blocked by the stale marker" wait_base
eq "file content" file "$(cat "$W/stale")"
G add -A; G commit -qm stale-file; wait_base
check "clean" clean

echo "== persistence"
echo x >>"$W/b.txt"; G commit -qam last
unmount_fs; mount_fs        # likely before the poll: rebased after the remount
ls "$W" >/dev/null         # opens the workspace
check "base follows after remount" wait_base
st=$(stat -c '%i %Y %Z %s' "$W/a.txt")
check "clean after remount" clean
unmount_fs; mount_fs
eq "stamps survive remount" "$st" "$(stat -c '%i %Y %Z %s' "$W/a.txt")"
check "clean after second remount" clean
check "no sync errors logged" sh -c "! grep -q 'moving the base' '$LOG'"

unmount_fs
echo "== $FAILS failure(s)"
[ -n "${KEEP:-}" ] || rm -rf "$T"
exit $((FAILS > 0))
