#!/usr/bin/env bash
# Read tracking with every open seen (--kernel-open) vs the default
# (FUSE_NO_OPEN): per repo, mode and command, a fresh mount of a workspace
# (branch gf-measure at HEAD, prewarm none), one cold run, three warm runs
# (median), and the opens per run from the daemon's counters. Run from a
# Claude Code Bash tool (it runs Claude Code's grep).
# usage: bench/opens.sh <repo>...
set -uo pipefail
BIN=$(dirname "$0")/../target/release/gitfold
MNT=/tmp/gf-measure/mnt
CLAUDE_BIN=${CLAUDE_CODE_EXECPATH:?}
PATTERN=gitfoldmeasureneedle
cgrep() { (exec -a ugrep "$CLAUDE_BIN" -G --ignore-files --hidden -I --exclude-dir=.git "$@"); }
cstatus() { git -c core.fsmonitor= status --porcelain=v2 -z --untracked-files=normal --ignored=matching --no-renames --ignore-submodules=all; }
declare -A CMDS=(
    [find]='find . -type f | wc -l'
    [rg]="rg -l $PATTERN . | wc -l"
    [claude-grep]="cgrep -rl $PATTERN . | wc -l"
    [git-status]='cstatus | tr "\0" "\n" | wc -l'
    [tar]='tar --exclude=.git -cf - . | wc -c'
)
ORDER=(find rg claude-grep git-status tar)
ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }
run() { local s; s=$(date +%s%N); (cd "$1" && eval "$2" >/dev/null 2>&1); ms "$s"; }
median() { printf '%s\n' "$@" | sort -n | sed -n 2p; }
printf '%-9s %-12s %-6s %9s %9s %9s\n' repo cmd opens cold_ms warm_ms opens/run
for repo in "$@"; do
    name=$(basename "$repo")
    git -C "$repo" branch -f gf-measure HEAD >/dev/null 2>&1
    for cmd in "${ORDER[@]}"; do
        for mode in no-open open; do
            flags="--prewarm none"; [ $mode = open ] && flags="$flags --kernel-open"
            mkdir -p $MNT; log=/tmp/gf-measure/$name-$cmd-$mode.log
            $BIN mount --foreground $flags "$repo" $MNT >"$log" 2>&1 & pid=$!
            for _ in $(seq 200); do mountpoint -q $MNT && break; sleep 0.05; done
            stat $MNT/gf-measure >/dev/null
            cold=$(run $MNT/gf-measure "${CMDS[$cmd]}")
            w=(); for i in 1 2 3; do w+=("$(run $MNT/gf-measure "${CMDS[$cmd]}")"); done
            fusermount3 -u $MNT; wait $pid
            opens=$(grep -o 'open[ =:]*[0-9]*' "$log" | head -1 | grep -o '[0-9]*$')
            printf '%-9s %-12s %-6s %9s %9s %9s\n' "$name" "$cmd" "$mode" "$cold" "$(median "${w[@]}")" "$(( ${opens:-0} / 4 ))"
        done
    done
    (cd / && "$BIN" -C "$repo" remove --force gf-measure >/dev/null && git -C "$repo" branch -q -D gf-measure)
done
