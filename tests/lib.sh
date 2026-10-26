# Helpers shared by the end-to-end tests; expects T (scratch dir), REPO, MNT,
# LOG and BIN to be set by the sourcing script.

FAILS=0
pass() { echo "ok   $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS + 1)); }
check() { # description, command...
    local desc=$1; shift
    if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}
eq() { # description, expected, actual
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (expected '$2', got '$3')"; fi
}

mount_fs() {
    "$BIN" mount "$REPO" "$MNT" >>"$LOG" 2>&1 &
    PID=$!
    for _ in $(seq 100); do mountpoint -q "$MNT" && return; sleep 0.05; done
    echo "mount failed"; cat "$LOG"; exit 1
}
unmount_fs() { # retries: a background prewarm walk keeps the mount busy briefly
    cd /
    for _ in $(seq 20); do fusermount3 -u "$MNT" 2>/dev/null && break; sleep 0.5; done
    wait "$PID" 2>/dev/null
}
