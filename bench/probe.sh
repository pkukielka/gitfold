#!/usr/bin/env bash
# Isolates content-read cost: dirs warmed by find, then cold/warm rg.
# usage: probe.sh <repo> <pattern> [gitfold flags...]
set -euo pipefail
REPO=$1; PAT=$2; shift 2
M=/tmp/gf-probe; BIN=$(dirname "$0")/../target/release/gitfold
mkdir -p $M
if mountpoint -q $M; then echo "$M already mounted" >&2; exit 1; fi
"$BIN" mount "$@" "$REPO" $M >/tmp/gf-probe.log 2>&1 &
trap 'cd /; fusermount3 -u $M || fusermount3 -uz $M; sleep 0.3; grep -o "ops:.*" /tmp/gf-probe.log || true' EXIT
for _ in $(seq 100); do mountpoint -q $M && break; sleep 0.05; done
mountpoint -q $M
cd $M/@HEAD
t() { local s=$(date +%s%N); "$@" >/dev/null; echo "$(( ($(date +%s%N)-s)/1000000 ))ms :: $*"; }
t find . -type f
t rg -l "$PAT" .
t rg -l "$PAT" .
t rg -j1 -l "$PAT" .
