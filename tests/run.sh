#!/usr/bin/env bash
# Runs end-to-end suites at once and sums up: tests/run.sh [SUITE...], by
# default manage, status, mount, agents, rebase and promisor (workspace.sh
# needs a repository: run it on its own). SECTION, PARALLEL and KEEP apply to
# each suite (see tests/lib.sh); with SECTION, only the suites made of
# sections run, and it fails only if no suite has a matching one.
set -uo pipefail
cd "$(dirname "$0")" || exit 1

if [ $# -gt 0 ]; then
    suites=("$@")
elif [ -n "${SECTION:-}" ]; then
    suites=(manage status mount agents)
else
    suites=(manage status mount agents rebase promisor)
fi
out=$(mktemp -d /tmp/gf-run.XXXXXX)
trap 'rm -rf "$out"' EXIT
for s in "${suites[@]}"; do
    SECTION_OPTIONAL=1 bash "$s.sh" >"$out/$s" 2>&1 </dev/null &
done
wait
failed=()
for s in "${suites[@]}"; do
    echo "=== tests/$s.sh"
    cat "$out/$s"
    grep -qx '== 0 failure(s)' "$out/$s" || failed+=("$s")
done
if [ -n "${SECTION:-}" ] && ! grep -qvE '^== [0-9]+ failure\(s\)$' <(grep -h '^== ' "$out"/*); then
    echo "SECTION=$SECTION matches no section"
    exit 1
fi
[ ${#failed[@]} = 0 ] && echo "=== all passed: ${suites[*]}" && exit 0
echo "=== failed: ${failed[*]}"
exit 1
