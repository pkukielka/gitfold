#!/usr/bin/env bash
# Times the tree of branches on a gitfold mount: the @committed and @live views
# of a branch with children (bulk reads, refresh, rest), trial merges (`branch
# --from a --from b`), `parent --update`, `changes -- PATH` and the landing check.
# usage: tree.sh <repo-with-checkout-at-HEAD> <grep-pattern>
# Creates branches bench-tree-* at HEAD, their commits (made without a checkout,
# through a temporary index) and workspaces, and a native git worktree for
# comparison, and deletes them at the end; the checkout and other branches
# are untouched. ms; "cold" = fresh mount (kernel FUSE caches empty), "warm" =
# repeat; git pack files in page cache. Cold runs start once the target's
# metadata prewarm is done, as bench.sh does.
set -euo pipefail

REPO=$(realpath "$1")
PATTERN=$2
BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
MNT=/tmp/gf-tree
LOG=/tmp/gf-tree.log
NATIVE=/tmp/gf-tree-native # a git worktree, for the native rebase
FILES=/tmp/gf-tree.files   # the files the `reads` row cats
CLAUDE_BIN=${CLAUDE_CODE_EXECPATH:?run from a Claude Code Bash tool or set CLAUDE_CODE_EXECPATH}

. "$(dirname "$0")/claude.sh"

declare -A CMDS=(
    [find]='find . -type f | wc -l'
    [claude-grep]="cgrep -rl '$PATTERN' . | wc -l"
    [rg]="rg -l '$PATTERN' . | wc -l"
    [reads]="xargs -a $FILES cat | wc -c"
)
ORDER=(find claude-grep rg reads)

N=bench-tree-n # the view's branch; children a, b (conflicting with a), c
A=bench-tree-a B=bench-tree-b C=bench-tree-c

ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

run() { # dir cmd -> "ms result"
    local s out
    s=$(date +%s%N)
    out=$(cd "$1" && eval "$2" 2>/dev/null) || true
    echo "$(ms "$s") $out"
}

t() { # cmd... -> ms (its output to the log; a failure said)
    local s rc=0
    s=$(date +%s%N)
    "$@" >>"$LOG.out" 2>&1 || rc=$?
    local took; took=$(ms "$s")
    [ "$rc" = 0 ] || echo "  ($* exited $rc)" >&2
    echo "$took"
}

row() { printf '%-44s %8s %8s  %s\n' "$@"; }

g() { git -C "$REPO" "$@"; }
gf() { "$BIN" "$1" -C "$REPO" "${@:2}"; }

commit_on() { # branch path line: commits the line appended to path, no checkout
    local idx=/tmp/gf-tree.index blob tree c
    GIT_INDEX_FILE=$idx g read-tree "$1"
    blob=$({ g cat-file blob "$1:$2"; echo "$3"; } | g hash-object -w --stdin)
    GIT_INDEX_FILE=$idx g update-index --cacheinfo "100644,$blob,$2"
    tree=$(GIT_INDEX_FILE=$idx g write-tree)
    c=$(g commit-tree "$tree" -p "$1" -m "$3")
    g update-ref "refs/heads/$1" "$c"
    rm -f "$idx"
}

edit() { echo "$3" >>"$MNT/$1/$2"; } # branch path line: an uncommitted edit

visible() { # ms-start file marker -> ms until file has the marker (30 s at most)
    for _ in $(seq 600); do
        grep -qF "$3" "$2" 2>/dev/null && { ms "$1"; return; }
        sleep 0.05
    done
    echo "timeout"
}

unmount() { # retried while busy (a prewarm walking it)
    for _ in $(seq 100); do
        mountpoint -q "$MNT" || break
        fusermount3 -u "$MNT" 2>/dev/null || sleep 0.2
    done
    for _ in $(seq 100); do kill -0 "${DAEMON:-0}" 2>/dev/null || break; sleep 0.05; done
}

remount() { # [env assignments for the daemon]
    unmount
    mkdir -p "$MNT"
    env "$@" "$BIN" mount --foreground "$REPO" "$MNT" >"$LOG" 2>&1 &
    DAEMON=$!
    for _ in $(seq 100); do mountpoint -q "$MNT" && break; sleep 0.05; done
}

prewarmed() { # ms-start target -> ms until its prewarm walk is done (60 s at most)
    for _ in $(seq 1200); do
        grep -qF "prewarm done for $MNT/$2:" "$LOG" && { ms "$1"; return; }
        sleep 0.05
    done
    echo "timeout"
}

opened() { # target -> ms of the first access (a view: made, merged)
    local s; s=$(date +%s%N)
    case $1 in
        @HEAD) stat "$MNT/@HEAD" >/dev/null
               ms "$s"
               for _ in $(seq 1200); do grep -q "prewarm done" "$LOG" && break; sleep 0.1; done ;;
        *) cat "$MNT/$1/.gitfold-conflicts" >/dev/null; ms "$s" ;;
    esac
}

live_complete() { # ms-start -> ms until @live/N shows each member's uncommitted edit
    visible "$1" "$MNT/@live/$N/net/README.md" "a: uncommitted" >/dev/null
    visible "$1" "$MNT/@live/$N/ui/README.md" "b: uncommitted" >/dev/null
    visible "$1" "$MNT/@live/$N/url/README.md" "c: uncommitted"
}

OLD_MOUNTPOINT=$(g config --local gitfold.mountpoint || true)
OLD_OPTIONS=$(g config --local gitfold.mountoptions || true)
VIEWS_NEW=$([ -e "$REPO/.git/gitfold/.views.git" ] || echo yes)
cleanup() {
    set +e
    mountpoint -q "$MNT" || remount
    gf remove @HEAD >/dev/null 2>&1
    for b in $(g for-each-ref --format='%(refname:short)' 'refs/heads/bench-tree-*'); do
        gf remove "$b" --force >/dev/null 2>&1
    done
    unmount
    [ -d "$NATIVE" ] && g worktree remove --force "$NATIVE"
    for b in $(g for-each-ref --format='%(refname:short)' 'refs/heads/bench-tree-*'); do
        g branch -qD "$b"
    done
    [ -n "$OLD_MOUNTPOINT" ] && g config --local gitfold.mountpoint "$OLD_MOUNTPOINT"
    [ -n "$OLD_OPTIONS" ] && g config --local gitfold.mountoptions "$OLD_OPTIONS"
    [ -n "$VIEWS_NEW" ] && rm -rf "$REPO/.git/gitfold/.views.git"
    rm -rf "$REPO"/.git/gitfold/.tasks/bench-tree-* # task folders outlive workspaces
    rm -f "$FILES"
}
trap cleanup EXIT

if [ -n "$(g for-each-ref 'refs/heads/bench-tree-*')" ]; then
    echo "branches bench-tree-* exist already; delete them first" >&2; exit 1
fi
: >"$LOG.out"
echo "repo: $REPO ($(g ls-files | wc -l) files)"
g ls-files | awk 'NR % 500 == 0' >"$FILES"
echo "reads: cat of $(wc -l <"$FILES") files (every 500th)"
remount

# The family: N with children a, b, c, each a few commits on files of its
# own, a and b also on AUTHORS (a conflict); a, b and c with workspaces and
# uncommitted edits (files of their own).
gf branch "$N" --from HEAD >/dev/null
commit_on "$N" CODE_OF_CONDUCT.md "n: 1"
for x in $A $B $C; do gf branch "$x" --parent "$N" >/dev/null; done
commit_on "$A" base/README.md "a: 1"; commit_on "$A" docs/README.md "a: 2"
commit_on "$A" AUTHORS "a: 3"
commit_on "$B" chrome/README.md "b: 1"; commit_on "$B" README.md "b: 2"
commit_on "$B" AUTHORS "b: 3"
commit_on "$C" .mailmap "c: 1"; commit_on "$C" WATCHLISTS "c: 2"
edit "$A" net/README.md "a: uncommitted"
edit "$B" ui/README.md "b: uncommitted"
edit "$C" url/README.md "c: uncommitted"
echo "@committed/$N conflicts: $(tr '\t\n' ' ;' <"$MNT/@committed/$N/.gitfold-conflicts")"
echo "@live/$N conflicts: $(tr '\t\n' ' ;' <"$MNT/@live/$N/.gitfold-conflicts")"

echo
echo "1, 2. Bulk reads. first: the first access (@HEAD: its opening; a view: made, merged; @live waits"
echo "      5 s at most); complete: until @live shows every member's uncommitted edit; prewarmed: until"
echo "      the view's metadata prewarm is done (from the first access)"
printf '%-12s %-24s %8s %9s %9s %9s %9s  %s\n' cmd target first complete prewarmed cold warm results
for target in @HEAD "@committed/$N" "@live/$N"; do
    for c in "${ORDER[@]}"; do
        remount
        s=$(date +%s%N); o=$(opened "$target"); full=- pw=-
        [ "$target" = "@live/$N" ] && full=$(live_complete "$s")ms
        [ "$target" = @HEAD ] || pw=$(prewarmed "$s" "$target")ms
        read -r m1 r < <(run "$MNT/$target" "${CMDS[$c]}")
        read -r m2 _ < <(run "$MNT/$target" "${CMDS[$c]}")
        printf '%-12s %-24s %6sms %9s %9s %7sms %7sms  %s\n' "$c" "$target" "$o" "$full" "$pw" "$m1" "$m2" "$r"
    done
    echo "             daemon $(grep RssAnon /proc/$DAEMON/status | tr -s ' ')"
    unmount; echo "             $(grep -o "ops:.*" "$LOG")"
done

echo
echo "2. Refresh: a change saved, until visible in the view (polled every 50 ms)"
remount
s=$(date +%s%N); opened "@live/$N" >/dev/null; live_complete "$s" >/dev/null
opened "@committed/$N" >/dev/null
for i in 1 2 3; do
    sleep 3 # the workspace quiet after the last round
    s=$(date +%s%N); edit "$A" net/README.md "a: live $i"
    row "@live: edit in a's workspace (round $i)" "$(visible "$s" "$MNT/@live/$N/net/README.md" "a: live $i")" "" "incl. the 2 s quiet period"
done
for i in 1 2; do
    edit "$C" url/README.md "c: committed $i"
    git -C "$MNT/$C" commit -qam "c: committed $i"
    s=$(date +%s%N)
    row "@committed: commit in c's workspace ($i)" "$(visible "$s" "$MNT/@committed/$N/url/README.md" "c: committed $i")" "" "from git commit's end"
done
edit "$C" url/README.md "c: uncommitted again"

echo
echo "3. A resting view's first access (GITFOLD_VIEW_REST_SECS=5, rested 10 s)"
remount GITFOLD_VIEW_REST_SECS=5
for v in committed live; do
    opened "@$v/$N" >/dev/null
    for i in 1 2; do
        sleep 10
        row "@$v, nothing changed ($i)" "$(t cat "$MNT/@$v/$N/.gitfold-conflicts")"
    done
done
for i in 1 2; do
    opened "@live/$N" >/dev/null; sleep 10
    edit "$A" net/README.md "a: rested $i"; sleep 3
    s=$(date +%s%N)
    got=$(grep -cF "a: rested $i" "$MNT/@live/$N/net/README.md" || true)
    row "@live, an edit while resting ($i)" "$(ms "$s")" "" "seen at once: $got"
done
for i in 1 2; do
    opened "@committed/$N" >/dev/null; sleep 10
    commit_on "$B" chrome/README.md "b: rested $i"; sleep 3
    s=$(date +%s%N)
    got=$(grep -cF "b: rested $i" "$MNT/@committed/$N/chrome/README.md" || true)
    row "@committed, a commit while resting ($i)" "$(ms "$s")" "" "seen at once: $got"
done

echo
echo "4. Trial merges (a and c: no conflict), total; then removed"
remount
s=$(date +%s%N); g worktree add -q --detach "$NATIVE" HEAD
echo "native worktree (for comparison): $(ms "$s") ms"
row "" first repeat
for w in "" -w; do
    ts=()
    for i in 1 2; do
        ts+=("$(t gf branch bench-tree-t $w --from "$A" --from "$C")")
        [ -n "$w" ] && files=$(gf changes bench-tree-t 2>/dev/null | wc -l)
        gf remove bench-tree-t --force >/dev/null; g branch -qD bench-tree-t
    done
    row "branch t ${w:+-w }--from a --from c" "${ts[@]}" "${w:+uncommitted carried: $files}"
done
native_merge() { git -C "$NATIVE" switch -q -c bench-tree-t "$A" && git -C "$NATIVE" merge -q --no-edit "$C"; }
ts=()
for i in 1 2; do
    ts+=("$(t native_merge)")
    git -C "$NATIVE" switch -q --detach HEAD~; g branch -qD bench-tree-t
done
row "  native: git switch -c t a; git merge c" "${ts[@]}" "(in a full worktree)"
ts=(); for i in 1 2; do ts+=("$(t g merge-tree --write-tree "$A" "$C")"); done
row "  native: git merge-tree --write-tree a c" "${ts[@]}" "(in memory, no checkout)"

echo
echo "5. parent --update after a new commit on the parent (P5; u1, u2 not following)"
P5=bench-tree-p5 U1=bench-tree-u1 U2=bench-tree-u2 NAT=bench-tree-nat
gf branch "$P5" --from HEAD >/dev/null
gf branch "$U1" --parent "$P5" --no-follow >/dev/null
gf branch "$U2" --parent "$P5" --no-follow >/dev/null
commit_on "$U1" OWNERS "u1: 1"; commit_on "$U1" BUILD.gn "# u1: 2"
commit_on "$U2" ATL_OWNERS "u2: 1"; commit_on "$U2" .gn "# u2: 2"
git -C "$NATIVE" switch -q -c "$NAT" "$U2"
stat "$MNT/$U2" >/dev/null
declare -A T
for i in 1 2; do
    commit_on "$P5" CPPLINT.cfg "# p5: $i"
    old=$(g merge-base "$P5~1" "$U1")
    T[replay]+=" $(t g replay --onto "$P5" "$old..$U1")"
    T[u1]+=" $(t gf parent "$U1" --update)"
    T[u2]+=" $(t gf parent "$U2" --update)"
    T[nat]+=" $(t git -C "$NATIVE" rebase -q "$P5")"
    commit_on "$P5" CPPLINT.cfg "# p5: $i dirty"
    edit "$U2" .clangd "# u2: uncommitted $i"; echo "# uncommitted $i" >>"$NATIVE/.clangd"
    T[u2dirty]+=" $(t gf parent "$U2" --update)"
    T[natdirty]+=" $(t git -C "$NATIVE" rebase -q --autostash "$P5")"
    git -C "$MNT/$U2" checkout -q -- .clangd; git -C "$NATIVE" checkout -q -- .clangd
done
row "" first repeat
row "u1, no workspace" ${T[u1]}
row "  native: git replay (in memory, no refs)" ${T[replay]}
row "u2, workspace, clean" ${T[u2]}
row "u2, workspace, uncommitted work" ${T[u2dirty]}
row "  native: git rebase in a full worktree" ${T[nat]}
row "  native: git rebase --autostash, dirty" ${T[natdirty]}

echo
echo "6. The landing check: gitfold parent c7 (built on p7, built on g7)"
G7=bench-tree-g7 P7=bench-tree-p7 C7=bench-tree-c7
gf branch "$G7" --from HEAD >/dev/null
gf branch "$P7" --parent "$G7" --no-follow >/dev/null
commit_on "$P7" SECURITY_OWNERS "p7: 1"; commit_on "$P7" CRYPTO_OWNERS "p7: 2"
gf branch "$C7" --parent "$P7" --no-follow >/dev/null
commit_on "$C7" BRANCH_OWNERS "c7: 1"
unrecorded() { g config --local --unset "branch.$C7.gitfoldLanding" || true; }
unrecorded
row "" first repeat "unrecorded again"
row "not landed" "$(t gf parent "$C7")" "$(t gf parent "$C7")" "$(unrecorded; t gf parent "$C7")"
# p7 squash-merged into g7, then g7 moves on.
g update-ref "refs/heads/$G7" "$(g commit-tree "$P7^{tree}" -p "$G7" -m "p7 (squash)")"
commit_on "$G7" DIR_METADATA "# g7: after"
unrecorded
row "squash-merged into its parent" "$(t gf parent "$C7")" "$(t gf parent "$C7")" "$(unrecorded; t gf parent "$C7")"
gf parent "$C7" | sed 's/^/  says: /'

echo
echo "7. gitfold changes -- PATH, $(g for-each-ref 'refs/heads/bench-tree-*' | wc -l) branches, $(gf list | tail -n +2 | wc -l) workspaces"
row "" first repeat
for p in AUTHORS docs; do
    row "changes -- $p" "$(t gf changes -- "$p")" "$(t gf changes -- "$p")" "$(gf changes -- "$p" | wc -l) lines"
done
