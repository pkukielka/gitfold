# Helpers shared by the end-to-end tests. The sourcing script sets BIN (the
# gitfold binary) and NAME (a short name for its scratch directory) first.
#
# - Each run gets its own directory, `/tmp/gf-<NAME>.XXXXXX` (T), so runs and
#   suites can run at once; it is removed at the end unless KEEP=1, and
#   whatever is still mounted below it is unmounted, however the run ends.
# - stdin is /dev/null: a command reading it ends instead of waiting on the
#   terminal or the runner (pipes in a test still feed what they pipe).
# - BIN becomes a wrapper giving every gitfold command GF_TIMEOUT seconds
#   (default 120) but `mount --foreground`, so one stuck command fails its
#   check instead of holding up the run.
# - `section NAME FUNCTION` runs a section in a subshell of its own (see
#   there); SECTION=<text> runs only the sections whose name contains it.

exec </dev/null
ROOT=$(mktemp -d "/tmp/gf-$NAME.XXXXXX")
T=$ROOT
FAILS=0
REAL_BIN=$BIN
BIN=$ROOT/gitfold
cat >"$BIN" <<EOF
#!/bin/sh
for a in "\$@"; do [ "\$a" = --foreground ] && exec "$REAL_BIN" "\$@"; done
exec timeout ${GF_TIMEOUT:-120} "$REAL_BIN" "\$@"
EOF
chmod +x "$BIN"

pass() { echo "ok   $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS + 1)); echo "$*" >>"$ROOT/.fails"; }
check() { # description, command...
    local desc=$1; shift
    if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}
eq() { # description, expected, actual
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (expected '$2', got '$3')"; fi
}

mount_fs() {
    "$BIN" mount --foreground "$@" "$REPO" "$MNT" >>"$LOG" 2>&1 &
    PID=$!
    for _ in $(seq 100); do mountpoint -q "$MNT" && return; sleep 0.05; done
    echo "mount failed"; cat "$LOG"; exit 1
}
unmount_fs() { # retries: a background prewarm walk keeps the mount busy briefly
    cd /
    for _ in $(seq 20); do fusermount3 -u "$MNT" 2>/dev/null && break; sleep 0.5; done
    wait "$PID" 2>/dev/null
}

# Unmounts (lazily) every FUSE mount below directory $1.
unmount_below() {
    cd /
    awk -v d="$1/" 'index($2, d) == 1 { print $2 }' /proc/mounts | sort -r |
        while read -r m; do fusermount3 -u -z "$m" 2>/dev/null; done
}

# Runs function $2 as section $1, unless SECTION is set and $1 doesn't contain
# it: in a subshell, with T, REPO, MNT and LOG of its own and the suite's
# `fixture` run first, and whatever it mounted unmounted after, so sections
# share no state. Failures are counted through a file, across subshells.
SECTION_N=0
section() {
    SECTION_N=$((SECTION_N + 1))
    if [ -n "${SECTION:-}" ] && [[ "$1" != *"$SECTION"* ]]; then
        return
    fi
    echo "== $1"
    (
        T=$ROOT/$SECTION_N
        REPO=$T/repo MNT=$T/mnt LOG=$T/gitfold.log
        mkdir -p "$T" "$MNT" "$REPO"
        trap 'unmount_below "$T"' EXIT
        fixture
        "$2"
    )
}

# Ends the run: the summary, the exit code; removes T unless KEEP=1.
finish() {
    local n=0
    [ -f "$ROOT/.fails" ] && n=$(wc -l <"$ROOT/.fails")
    echo "== $n failure(s)"
    [ -n "${KEEP:-}" ] && echo "kept $ROOT"
    exit $((n > 0))
}
trap 'unmount_below "$ROOT"; [ -n "${KEEP:-}" ] || rm -rf "$ROOT"' EXIT
