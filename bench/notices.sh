#!/usr/bin/env bash
# Times the conflict notices on a gitfold mount: from a commit (its ref update)
# to the message in the inboxes of those involved, the check of a commit that
# meets no one's work, a commit with many workspaces related (branches without
# a parent: siblings under the root), and the daemon's idle CPU with them open
# (against another build, if given).
# usage: notices.sh <repo-with-checkout-at-HEAD> [<other gitfold binary, for the idle row>]
# Creates branches bench-notices-* at HEAD, their commits (made without a
# checkout, through a temporary index) and workspaces, and deletes them at the
# end; the checkout and other branches are untouched. ms; "cold" = the first
# commit on a fresh mount, "warm" = the following ones (median). The check's
# own time is the daemon's debug line (`checked in`).
set -euo pipefail

REPO=$(realpath "$1")
OTHER_BIN=${2:+$(realpath "$2")}
BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
MNT=/tmp/gf-notices
LOG=/tmp/gf-notices.log
RUNS=${RUNS:-5}
TASKS=$REPO/.git/gitfold/.tasks
DEBUG_LOG='info,fuser=error,gitfold::fs::views::notices=debug'

P=bench-notices-p A=bench-notices-a B=bench-notices-b # a, b built on p
X=bench-notices-x                                     # no parent: lands in the root
SIBS=(bench-notices-s1 bench-notices-s2 bench-notices-s3 bench-notices-s4 bench-notices-s5 bench-notices-s6)
KIDS=(bench-notices-s1a bench-notices-s1b)            # built on s1

ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

g() { git -C "$REPO" "$@"; }
gf() { "$BIN" "$1" -C "$REPO" "${@:2}"; }

AT=0 # when commit_on updated the ref (ns)
commit_on() { # branch path line: commits the line appended to path, no checkout
    local idx=/tmp/gf-notices.index blob tree c
    GIT_INDEX_FILE=$idx g read-tree "$1"
    blob=$({ g cat-file blob "$1:$2"; echo "$3"; } | g hash-object -w --stdin)
    GIT_INDEX_FILE=$idx g update-index --cacheinfo "100644,$blob,$2"
    tree=$(GIT_INDEX_FILE=$idx g write-tree)
    c=$(g commit-tree "$tree" -p "$1" -m "$3")
    g update-ref "refs/heads/$1" "$c"
    AT=$(date +%s%N)
    rm -f "$idx"
}

edit() { echo "$3" >>"$MNT/$1/$2"; } # branch path line: an uncommitted edit

messages() { find "$TASKS/$1/inbox" -type f 2>/dev/null | wc -l; } # branch -> its inbox's count

checks() { grep -c "checked in" "$LOG" || true; }

told() { # ns-start branch count-before -> ms until its inbox has more (30 s at most)
    for _ in $(seq 3000); do
        [ "$(messages "$2")" -gt "$3" ] && { ms "$1"; return; }
        sleep 0.01
    done
    echo timeout
}

checked() { # ns-start checks-before -> "ms-until-logged check-ms" (30 s at most)
    for _ in $(seq 3000); do
        if [ "$(checks)" -gt "$2" ]; then
            local at; at=$(ms "$1")
            echo "$at $(grep "checked in" "$LOG" | tail -1 | sed 's/.*checked in //' | awk '
                /µs$/ { printf "%.1f", $1 / 1000; next }
                /ms$/ { printf "%.0f", $1 + 0; next }
                /s$/  { printf "%.0f", $1 * 1000 }')"
            return
        fi
        sleep 0.01
    done
    echo "timeout -"
}

median() { printf '%s\n' "$@" | sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'; }

row() { printf '%-52s %8s %8s  %s\n' "$@"; }

unmount() { # retried while busy (a prewarm walking it)
    for _ in $(seq 100); do
        mountpoint -q "$MNT" || break
        "$BIN" unmount "$MNT" >/dev/null 2>&1 || sleep 0.2
    done
    for _ in $(seq 100); do kill -0 "${DAEMON:-0}" 2>/dev/null || break; sleep 0.05; done
}

remount() { # [binary]
    unmount
    mkdir -p "$MNT"
    RUST_LOG=$DEBUG_LOG "${1:-$BIN}" mount --foreground "$REPO" "$MNT" >"$LOG" 2>&1 &
    DAEMON=$!
    for _ in $(seq 100); do mountpoint -q "$MNT" && break; sleep 0.05; done
}

ALL=("$P" "$A" "$B" "$X" "${SIBS[@]}" "${KIDS[@]}")
open_all() { for b in "${ALL[@]}"; do stat "$MNT/$b/README.md" >/dev/null; done; }

cpu_ticks() { awk '{ print $14 + $15 + $16 + $17 }' "/proc/$DAEMON/stat"; } # utime stime cutime cstime

OLD_MOUNTPOINT=$(g config --local gitfold.mountpoint || true)
OLD_OPTIONS=$(g config --local gitfold.mountoptions || true)
VIEWS_NEW=$([ -e "$REPO/.git/gitfold/.views.git" ] || echo yes)
cleanup() {
    set +e
    mountpoint -q "$MNT" || remount
    for b in $(g for-each-ref --format='%(refname:short)' 'refs/heads/bench-notices-*'); do
        gf remove "$b" --force >/dev/null 2>&1
    done
    unmount
    for b in $(g for-each-ref --format='%(refname:short)' 'refs/heads/bench-notices-*'); do
        g branch -qD "$b"
    done
    [ -n "$OLD_MOUNTPOINT" ] && g config --local gitfold.mountpoint "$OLD_MOUNTPOINT"
    [ -n "$OLD_OPTIONS" ] && g config --local gitfold.mountoptions "$OLD_OPTIONS"
    [ -n "$VIEWS_NEW" ] && rm -rf "$REPO/.git/gitfold/.views.git"
    rm -rf "$TASKS"/bench-notices-* # task folders outlive workspaces
}
trap cleanup EXIT

if [ -n "$(g for-each-ref 'refs/heads/bench-notices-*')" ]; then
    echo "branches bench-notices-* exist already; delete them first" >&2; exit 1
fi
echo "repo: $REPO ($(g ls-files | wc -l) files)"
# The paths committed: Markdown files spread over the tree, one per commit.
mapfile -t PATHS < <(g ls-files '*.md' | awk 'NR % 97 == 0' | head -60)
next_path() { NEXT=${PATHS[0]}; PATHS=("${PATHS[@]:1}"); }
remount

# p with children a and b; x and s1..s6 without a parent (siblings under the
# root), s1 with children s1a, s1b. Each with one commit and one uncommitted
# edit, on files of its own (`own`); all with workspaces.
gf branch "$P" --from HEAD >/dev/null
for b in "$A" "$B"; do gf branch "$b" --parent "$P" >/dev/null; done
for b in "$X" "${SIBS[@]}"; do gf branch "$b" --from HEAD >/dev/null; done
for b in "${KIDS[@]}"; do gf branch "$b" --parent "${SIBS[0]}" >/dev/null; done
for b in "${ALL[@]}"; do
    next_path; commit_on "$b" "$NEXT" "$b: own commit"
    next_path; edit "$b" "$NEXT" "$b: own uncommitted"
done
echo "workspaces: $(gf list | tail -n +2 | wc -l) (bench-notices-*: ${#ALL[@]})"

echo
echo "1. a commits a line on a file b changed uncommitted (a conflict): until each inbox has it"
remount
open_all
sleep 3 # first syncs done
lat_b=() lat_a=() lat_p=() chk=()
for i in $(seq "$RUNS"); do
    next_path; f=$NEXT
    edit "$B" "$f" "b: uncommitted $i"
    sleep 2
    nb=$(messages "$B") na=$(messages "$A") np=$(messages "$P") nc=$(checks)
    commit_on "$A" "$f" "a: committed $i"
    lb=$(told "$AT" "$B" "$nb"); la=$(told "$AT" "$A" "$na"); lp=$(told "$AT" "$P" "$np")
    read -r _ c < <(checked "$AT" "$nc")
    echo "  run $i: b $lb, a $la, p $lp; the check $c ($f)"
    [ "$i" = 1 ] && cold=("$lb" "$la" "$lp" "$c") && continue
    lat_b+=("$lb") lat_a+=("$la") lat_p+=("$lp") chk+=("$c")
done
row "" cold warm "(median of $((RUNS - 1)))"
row "b's inbox (uncommitted side)" "${cold[0]}" "$(median "${lat_b[@]}")"
row "a's inbox (committer)" "${cold[1]}" "$(median "${lat_a[@]}")"
row "p's inbox (where they land)" "${cold[2]}" "$(median "${lat_p[@]}")"
row "  the check (daemon's time)" "${cold[3]}" "$(median "${chk[@]}")"
echo "  b's message:"; sed 's/^/    /' "$(find "$TASKS/$B/inbox" -type f | sort | tail -1)"

echo
echo "2. a commits a file no one else changed: until checked (no messages)"
lat=() chk=()
for i in $(seq "$RUNS"); do
    sleep 2
    nc=$(checks) n=$(( $(messages "$A") + $(messages "$B") + $(messages "$P") ))
    next_path; commit_on "$A" "$NEXT" "a: alone $i"
    read -r l c < <(checked "$AT" "$nc")
    lat+=("$l") chk+=("$c")
    sleep 1
    [ $(( $(messages "$A") + $(messages "$B") + $(messages "$P") )) = "$n" ] || echo "  run $i: a message!"
done
row "" median "" "runs"
row "until checked (incl. the sync's poll, 1 s)" "$(median "${lat[@]}")" "" "${lat[*]}"
row "  the check (daemon's time)" "$(median "${chk[@]}")" "" "${chk[*]}"

related=$(( ${#ALL[@]} - 1 + $(gf list | tail -n +2 | grep -cv bench-notices || true) ))
echo
echo "3. x (no parent) commits; $related workspaces related (siblings under the root, what's built on them)"
lat=() chk=()
for i in $(seq "$RUNS"); do
    sleep 2
    nc=$(checks)
    next_path; commit_on "$X" "$NEXT" "x: alone $i"
    read -r l c < <(checked "$AT" "$nc")
    lat+=("$l") chk+=("$c")
done
row "" median "" "runs"
row "no overlap: until checked" "$(median "${lat[@]}")" "" "${lat[*]}"
row "  the check (daemon's time)" "$(median "${chk[@]}")" "" "${chk[*]}"
S=${SIBS[2]}
lat=() chk=()
for i in $(seq "$RUNS"); do
    next_path; f=$NEXT
    edit "$S" "$f" "s3: uncommitted $i"
    sleep 2
    ns=$(messages "$S") nc=$(checks)
    commit_on "$X" "$f" "x: committed $i"
    ls=$(told "$AT" "$S" "$ns")
    read -r _ c < <(checked "$AT" "$nc")
    lat+=("$ls") chk+=("$c")
done
row "s3 edits it uncommitted: until s3's inbox has it" "$(median "${lat[@]}")" "" "${lat[*]}"
row "  the check (daemon's time)" "$(median "${chk[@]}")" "" "${chk[*]}"

echo
echo "4. Idle: the daemon's CPU (user+system, its git children included) in 60 s, ${#ALL[@]} workspaces open"
idle() { # [binary] -> ms of CPU in 60 s
    remount "$@"
    open_all
    sleep 10
    local t0; t0=$(cpu_ticks)
    sleep 60
    echo $(( ($(cpu_ticks) - t0) * 1000 / $(getconf CLK_TCK) ))
}
row "this build" "$(idle)" "" "ms"
[ -n "$OTHER_BIN" ] && row "$("$OTHER_BIN" --version)" "$(idle "$OTHER_BIN")" "" "ms"
row "this build, again" "$(idle)" "" "ms"
grep -E "WARN|ERROR" "$LOG" | grep -v "checked in" | head -5 || true
