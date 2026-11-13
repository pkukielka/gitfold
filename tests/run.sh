#!/usr/bin/env bash
# Runs end-to-end suites at once and sums up: tests/run.sh [SUITE...], by
# default all of them but workspace.sh, which needs a repository (run it on
# its own). SECTION, PARALLEL and KEEP apply to each suite (see tests/lib.sh);
# with SECTION, only the suites made of sections run, and it fails only if no
# suite has a matching one. Each suite's output is printed, in order, once it
# is done; Ctrl-C stops them all.
set -uo pipefail
cd "$(dirname "$0")" || exit 1

all=(manage status mount agents rebase promisor)
named=$#
[ $# -gt 0 ] || set -- "${all[@]}"
suites=()
for s in "$@"; do
    s=$(basename "$s" .sh)
    if [[ " ${all[*]} " != *" $s "* ]]; then
        echo "usage: tests/run.sh [SUITE...], SUITE one of: ${all[*]}" >&2
        exit 2
    fi
    if [ -n "${SECTION:-}" ] && ! grep -q '^section ' "$s.sh"; then
        [ "$named" = 0 ] || echo "=== tests/$s.sh: skipped, it has no sections for SECTION"
        continue
    fi
    suites+=("$s")
done

out=$(mktemp -d /tmp/gf-run.XXXXXX)
# Each suite, stopped, unmounts and removes what it made (see tests/lib.sh).
trap 'kill $(jobs -p) 2>/dev/null; wait; rm -rf "$out"' EXIT
trap 'exit 130' INT TERM
declare -A pid
for s in "${suites[@]}"; do
    SECTIONS_RUN_FILE=$out/$s.sections bash "$s.sh" >"$out/$s" 2>&1 </dev/null &
    pid[$s]=$!
done
failed=()
for s in "${suites[@]}"; do
    wait "${pid[$s]}" || failed+=("$s")
    echo "=== tests/$s.sh"
    cat "$out/$s"
done
if [ -n "${SECTION:-}" ]; then
    n=0
    for f in "$out"/*.sections; do [ ! -f "$f" ] || n=$((n + $(cat "$f"))); done
    [ "$n" -gt 0 ] || { echo "SECTION=$SECTION matches no section"; exit 1; }
fi
[ ${#failed[@]} = 0 ] && echo "=== all passed: ${suites[*]}" && exit 0
echo "=== failed: ${failed[*]}"
exit 1
