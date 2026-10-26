#!/usr/bin/env bash
# End-to-end test of writable workspaces on a scratch clone of a repository.
# usage: tests/workspace.sh <source-repo>   (e.g. ~/work/spark)
# Creates /tmp/gf-ws-test/{repo,mnt}; removes them at the end unless KEEP=1.
set -uo pipefail

SRC=$(realpath "$1")
BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
TESTS=$(realpath "$(dirname "$0")")
T=/tmp/gf-ws-test
REPO=$T/repo
MNT=$T/mnt
LOG=$T/gitfold.log
. "$(dirname "$0")/lib.sh"

rm -rf "$T"; mkdir -p "$MNT"
git clone -q "$SRC" "$REPO"
git -C "$REPO" branch test1
git -C "$REPO" branch feature/x
MAIN=$(git -C "$REPO" branch --show-current)
mount_fs
W=$MNT/test1

echo "== workspace basics"
check "workspace opens" test -d "$W/core"
eq "git status clean" "" "$(git -C "$W" status --porcelain)"
eq "branch" "test1" "$(git -C "$W" branch --show-current)"
check "README matches checkout" cmp "$W/README.md" "$REPO/README.md"

echo "== edits"
echo "appended" >>"$W/README.md"
eq "edited content" "appended" "$(tail -1 "$W/README.md")"
eq "status shows modification" " M README.md" "$(git -C "$W" status --porcelain)"
check "checkout untouched" cmp <(git -C "$REPO" show HEAD:README.md) "$REPO/README.md"
mkdir -p "$W/newdir/sub"; echo new >"$W/newdir/sub/f.txt"
eq "new file content" "new" "$(cat "$W/newdir/sub/f.txt")"
check "new dir listed" sh -c "ls '$W' | grep -qx newdir"
: >"$W/LICENSE"
eq "truncate via >" "0" "$(stat -c %s "$W/LICENSE")"
chmod +x "$W/pom.xml"
check "chmod +x" test -x "$W/pom.xml"
ln -s README.md "$W/readme-link"
eq "symlink" "README.md" "$(readlink "$W/readme-link")"
touch -d "2001-02-03 04:05:06" "$W/NOTICE"
eq "touch -d" "2001" "$(date -r "$W/NOTICE" +%Y)"

echo "== deletions"
rm "$W/NOTICE"
check "deleted file gone" test ! -e "$W/NOTICE"
check "deleted file not listed" sh -c "! ls '$W' | grep -qx NOTICE"
rm -r "$W/docs"
check "deleted dir gone" test ! -e "$W/docs"
mkdir "$W/docs"
eq "recreated dir is empty" "" "$(ls -A "$W/docs")"
echo again >"$W/docs/x"
eq "file in recreated dir" "again" "$(cat "$W/docs/x")"

echo "== renames"
mv "$W/CONTRIBUTING.md" "$W/CONTRIB2.md"
check "renamed file at new path" cmp "$W/CONTRIB2.md" "$REPO/CONTRIBUTING.md"
check "renamed file gone from old path" test ! -e "$W/CONTRIBUTING.md"
mv "$W/dev" "$W/dev2"
check "renamed lower dir (EXDEV fallback)" test -d "$W/dev2" -a ! -e "$W/dev"
mv "$W/newdir" "$W/newdir2"
eq "renamed upper dir keeps content" "new" "$(cat "$W/newdir2/sub/f.txt")"

echo "== git inside the workspace"
git -C "$W" add -A >/dev/null 2>&1
check "git commit" git -C "$W" -c user.name=t -c user.email=t@t commit -q -m "workspace test"
eq "status clean after commit" "" "$(git -C "$W" status --porcelain)"
eq "branch moved in repo" "$(git -C "$W" rev-parse HEAD)" "$(git -C "$REPO" rev-parse test1)"
eq "commit content" "appended" "$(git -C "$REPO" show test1:README.md | tail -1)"
check "deletion committed" sh -c "! git -C '$REPO' cat-file -e test1:NOTICE"

echo "== persistence across remount"
unmount_fs; mount_fs
eq "edit persisted" "appended" "$(tail -1 "$W/README.md")"
check "deletion persisted" test ! -e "$W/NOTICE"
eq "recreated dir persisted" "x" "$(ls "$W/docs")"
eq "status clean after remount" "" "$(git -C "$W" status --porcelain)"

echo "== review fixes"
GOLDEN=sql/hive/src/test/resources/golden
s=$(date +%s%N); rm -r "$W/$GOLDEN"; ms=$(( ($(date +%s%N) - s) / 1000000 ))
check "rm -r of a 7.5k-entry lower dir (${ms} ms)" test ! -e "$W/$GOLDEN" -a "$ms" -lt 20000
STATE=$REPO/.git/gitfold/test1
check "marker log bounded (compacted in process)" test "$(stat -c %s "$STATE/markers")" -lt 400000
echo outside >"$T/outside.txt"; touch -d "2001-01-01" "$T/outside.txt"
ln -s "$T/outside.txt" "$W/outlink"; touch -h -d "2010-01-01" "$W/outlink"
eq "touch -h leaves the link target alone" "2001" "$(date -r "$T/outside.txt" +%Y)"
eq "touch -h sets the link's own time" "2010" "$(stat -c %y "$W/outlink" | cut -c1-4)"
echo w >"$W/wonly"; chmod 200 "$W/wonly"
check "touch on a write-only file" touch "$W/wonly"
chmod 644 "$W/wonly" # git must be able to read it below
check "fsync" dd if=/dev/zero of="$W/synced" bs=4k count=1 conv=fsync status=none
check "no temp files in upper" sh -c "! find '$STATE/upper' -name '.gitfold*' | grep -q ."
ino=$(stat -c %i "$W/pom.xml"); mv "$W/pom.xml" "$T/pom.saved"; cp "$T/pom.saved" "$W/pom.xml"
eq "recreated file gets its path-derived inode back" "$ino" "$(stat -c %i "$W/pom.xml")"
mkdir "$W/stress"
( while :; do ls "$W/stress" >/dev/null; done ) & LS=$!
WRITERS=()
for j in $(seq 8); do ( for i in $(seq 50); do echo "$j" >"$W/stress/f$j-$i"; done ) & WRITERS+=($!); done
wait "${WRITERS[@]}"; kill $LS 2>/dev/null; wait $LS 2>/dev/null
eq "concurrent creates all listed" "400" "$(ls "$W/stress" | wc -l)"
rm -r "$W/stress"
git -C "$W" add -A >/dev/null 2>&1
check "second commit" git -C "$W" -c user.name=t -c user.email=t@t commit -q -m "review fixes"
unmount_fs
rm -rf "$REPO/.git/worktrees/gitfold-ws-test1"   # registration lost, diff kept
mount_fs
eq "status clean after registration recreated at the moved tip" "" "$(git -C "$W" status --porcelain)"

echo "== third review fixes"
eq "base pinned by a ref" "$(cat "$STATE/base")" "$(git -C "$REPO" rev-parse refs/gitfold/test1)"
mkdir "$W/churn"
( for i in $(seq 300); do touch "$W/churn/f$i"; rm "$W/churn/f$i"; done ) & CHURN=$!
( while kill -0 $CHURN 2>/dev/null; do ls -l "$W/churn" >/dev/null 2>&1; done ) & LISTER=$!
wait $CHURN; wait $LISTER 2>/dev/null
phantoms=0; for n in $(ls "$W/churn"); do stat "$W/churn/$n" >/dev/null 2>&1 || phantoms=$((phantoms + 1)); done
eq "no phantom entries after delete/list races" "0 0" "$phantoms $(ls "$W/churn" | wc -l)"
rmdir "$W/churn"
check "rm of the virtual .git refused" sh -c "! rm '$W/.git' 2>/dev/null"
check "mkfifo in a snapshot refused" sh -c "! mkfifo '$MNT/@HEAD/fifo'"
check "chmod in a snapshot is EROFS" sh -c "chmod 600 '$MNT/@HEAD/README.md' 2>&1 | grep -qi read-only"

echo "== crash leftovers and umask"
unmount_fs
echo stale >"$STATE/tmp/0"                           # copy-up a crash interrupted
( umask 077; mount_fs; echo "$PID" >"$T/pid" )       # daemon with a strict umask
PID=$(cat "$T/pid")
echo more >>"$W/build.sbt"
check "copy-up works with a stale tmp file" grep -q more "$W/build.sbt"
eq "copy-up keeps the mode (daemon umask 077)" "644" "$(stat -c %a "$W/build.sbt")"
mkdir -m 775 "$W/m775"
eq "mkdir mode without the daemon's umask" "775" "$(stat -c %a "$W/m775")"
rm "$W/scalastyle-config.xml"
unmount_fs; mount_fs
check "deletion survives remount" test ! -e "$W/scalastyle-config.xml"

echo "== read-only files"
( exec 3>"$W/ro1"; chmod 444 "$W/ro1"; echo data >&3 )        # written after chmod
eq "write through a descriptor opened before chmod 444" data "$(cat "$W/ro1")"
eq "mode kept" 444 "$(stat -c %a "$W/ro1")"
mkdir -p "$T/rosrc/sub"; echo r >"$T/rosrc/sub/r"; chmod 444 "$T/rosrc/sub/r"
tar -C "$T/rosrc" -cf "$T/ro.tar" sub
check "tar extracts read-only files" tar -C "$W" -xf "$T/ro.tar"
eq "their content" r "$(cat "$W/sub/r")"
check "a read-only file can't be opened for writing" sh -c "! echo x >>'$W/ro1'"
chmod 644 "$W/ro1" "$W/sub/r"; rm -r "$W/ro1" "$W/sub"
printf data >"$W/wo"; ( exec 3<"$W/wo"; chmod 200 "$W/wo"; cat <&3 >"$T/wo.out" )
eq "read through a descriptor opened before chmod 200" data "$(cat "$T/wo.out")"
eq "write-only mode kept" 200 "$(stat -c %a "$W/wo")"
eq "the lifted mode is never visible to others" "0 444" \
    "$(perl "$TESTS/race-modes.pl" "$W/race")"
chmod 644 "$W/wo" "$W/race"; rm "$W/wo" "$W/race"

echo "== names and protection"
check "branch with slash" test -d "$MNT/feature/x/core"
check "namespace listed" sh -c "ls '$MNT' | grep -qx feature"
check "slashed rev snapshot" test -d "$MNT/@origin/$MAIN/core"
check "branch checked out in main worktree refused" test ! -e "$MNT/$MAIN"
check "refusal logged" grep -q "already checked out" "$LOG"
check "snapshot read-only" sh -c "! touch '$MNT/@HEAD/x'"
check "snapshot files not reported writable" test ! -w "$MNT/@HEAD/README.md"
check "git status works in a snapshot" git -C "$MNT/@HEAD" status --porcelain
check "root read-only" sh -c "! mkdir '$MNT/newbranch'"
check "unknown branch absent" test ! -e "$MNT/no-such-branch"

unmount_fs
check "no stale mount" sh -c "! mountpoint -q '$MNT'"
echo "== $FAILS failure(s)"
[ -n "${KEEP:-}" ] || rm -rf "$T"
exit $((FAILS > 0))
