#!/usr/bin/env bash
# Write-heavy workloads in a workspace vs a plain directory on the same disk.
# usage: bench/writes.sh <source-repo>   (e.g. ~/work/spark)
# Uses /tmp/gf-wb (a scratch clone, its mount, and a native directory); removes
# it at the end unless KEEP=1. Prints milliseconds for two runs of each.
set -euo pipefail

SRC=$(realpath "$1")
BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
T=/tmp/gf-wb
REPO=$T/repo
MNT=$T/mnt
NATIVE=$T/native
LOG=$T/log
. "$(dirname "$0")/../tests/lib.sh"
rm -rf "$T"; mkdir -p "$MNT" "$NATIVE"
git clone -q "$SRC" "$REPO"
git -C "$REPO" branch w
mount_fs
W=$MNT/w
ls "$W" >/dev/null
for _ in $(seq 120); do grep -q "prewarm done" "$LOG" && break; sleep 0.5; done
tar -C "$REPO" --exclude=.git -cf "$T/tree.tar" .
files=$(tar -tf "$T/tree.tar" | grep -vc '/$')
mb=$(( $(stat -c %s "$T/tree.tar") / 1024 / 1024 ))

ms() {
    local s; s=$(date +%s%N)
    "$@" >/dev/null 2>&1 || { echo FAILED; return; }
    echo $(( ($(date +%s%N) - s) / 1000000 ))
}
# workload name, then a function taking the target directory, run twice per target
run() {
    local name=$1 fn=$2 out=""
    for target in "$NATIVE" "$W"; do
        for i in 1 2; do out+="$(ms "$fn" "$target" "$i") "; done
        out+="| "
    done
    printf '%-34s %s\n' "$name" "$out"
}
untar() { mkdir "$1/x$2" && tar -C "$1/x$2" -xf "$T/tree.tar"; }
remove() { rm -r "$1/x$2"; }
bigfile() { dd if=/dev/zero of="$1/big$2" bs=1M count=1024 status=none; }
copydir() { cp -r "$REPO/sql" "$1/sql$2"; }
# Appends to files of the checkout itself: copy-ups in the workspace.
append() {
    local dir=$1
    [ "$dir" = "$NATIVE" ] && dir=$REPO
    (cd "$dir" && git ls-files 'core/*.scala' | head -1000 | while read -r f; do echo "// $2" >>"$f"; done)
}

echo "tree: $files files, $mb MB; ms: native run1 run2 | workspace run1 run2"
run "untar the checkout ($files files)" untar
run "rm -r of it" remove
run "write a 1 GiB file" bigfile
run "cp -r sql/ ($(git -C "$REPO" ls-files sql | wc -l) files)" copydir
run "append to 1000 files" append

unmount_fs
[ -n "${KEEP:-}" ] || rm -rf "$T"
