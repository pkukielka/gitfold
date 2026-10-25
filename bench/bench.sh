#!/usr/bin/env bash
# Compares agent-style bulk operations on a native checkout vs a gitfold mount.
# usage: bench.sh <repo-with-checkout-at-HEAD> <grep-pattern>
# Mount numbers: "cold" = fresh mount (kernel FUSE caches empty), "warm" = repeat.
# Git pack files and the native checkout are in page cache in both cases.
set -euo pipefail
GF_FLAGS=${GF_FLAGS:-}

REPO=$(realpath "$1")
PATTERN=$2
BIN=$(dirname "$0")/../target/release/gitfold
MNT=/tmp/gf-bench
CLAUDE_BIN=${CLAUDE_CODE_EXECPATH:?run from a Claude Code Bash tool or set CLAUDE_CODE_EXECPATH}

# What Claude Code's `grep` shell function runs.
cgrep() {
    (exec -a ugrep "$CLAUDE_BIN" -G --ignore-files --hidden -I --exclude-dir=.git "$@")
}
# What Claude Code itself runs periodically.
cstatus() {
    git -c core.fsmonitor= status --porcelain=v2 -z --untracked-files=normal --ignored=matching \
        --no-renames --ignore-submodules=all
}

declare -A CMDS=(
    [find]='find . -type f | wc -l'
    [claude-grep]="cgrep -rl '$PATTERN' . | wc -l"
    [rg]="rg -l '$PATTERN' . | wc -l"
    [git-status]='cstatus | tr "\0" "\n" | wc -l'
)
ORDER=(find claude-grep rg git-status)

ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

run() { # dir cmd -> "ms result"
    local s out
    s=$(date +%s%N)
    out=$(cd "$1" && eval "$2" 2>/dev/null) || true
    echo "$(ms "$s") $out"
}

unmount() { fusermount3 -u "$MNT" 2>/dev/null || true; sleep 0.3; }

remount() {
    unmount
    mkdir -p "$MNT"
    "$BIN" mount $GF_FLAGS "$REPO" "$MNT" >/tmp/gf-bench.log 2>&1 &
    for _ in $(seq 100); do mountpoint -q "$MNT" && break; sleep 0.05; done
    local s; s=$(date +%s%N)
    stat "$MNT/@HEAD" >/dev/null # registers worktree + synthesizes index
    SNAP_MS=$(ms "$s")
    if [ -n "${GF_WAIT_PREWARM:-}" ]; then
        for _ in $(seq 1200); do grep -q "prewarm done" /tmp/gf-bench.log && break; sleep 0.1; done
        PREWARM=$(grep -o "prewarm done.*" /tmp/gf-bench.log || echo "prewarm: not finished")
    fi
}
trap unmount EXIT

echo "repo: $REPO ($(git -C "$REPO" ls-files | wc -l) files)"
remount
echo "snapshot creation (worktree + index): ${SNAP_MS} ms"
[ -n "${GF_WAIT_PREWARM:-}" ] && echo "$PREWARM"
printf '%-12s %10s %10s %10s %10s  %s\n' cmd native1 native2 mnt-cold mnt-warm "results(native/mnt)"
for c in "${ORDER[@]}"; do
    read -r n1 rn < <(run "$REPO" "${CMDS[$c]}")
    read -r n2 _ < <(run "$REPO" "${CMDS[$c]}")
    remount
    read -r m1 rm < <(run "$MNT/@HEAD" "${CMDS[$c]}")
    read -r m2 _ < <(run "$MNT/@HEAD" "${CMDS[$c]}")
    printf '%-12s %8sms %8sms %8sms %8sms  %s/%s\n' "$c" "$n1" "$n2" "$m1" "$m2" "$rn" "$rm"
    unmount; echo "             $(grep -o "ops:.*" /tmp/gf-bench.log)"
done
