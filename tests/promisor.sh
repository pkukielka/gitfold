#!/usr/bin/env bash
# End-to-end test of partial clones (`--filter=blob:none`): gitfold fetches
# what a tree load finds missing from the promisor remote (src/git/promisor.rs).
# All local: a bare file:// remote that allows filters and raw-SHA wants.
# usage: tests/promisor.sh
# Creates /tmp/gf-pc-test; removes it at the end unless KEEP=1.
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
T=/tmp/gf-pc-test
MNT=$T/mnt
LOG=$T/gitfold.log
. "$(dirname "$0")/lib.sh"
# uploadpack.packObjectsHook is read only from protected (global) config.
export HOME=$T/home XDG_CONFIG_HOME=$T/xdg

fails() { ! "$@"; }
S() { git -C "$T/seed" -c user.name=t -c user.email=t@t "$@"; }
C() { git -C "$REPO" -c user.name=t -c user.email=t@t "$@"; }
# A fresh blob:none clone of the remote at $1.
clone() { rm -rf "$1"; git clone -q --no-checkout --filter=blob:none "file://$T/remote.git" "$1"; }
# Objects of revision $1's tree missing from $REPO.
missing() { GIT_NO_LAZY_FETCH=1 C rev-list --objects --missing=print "$1^{tree}" | grep -c '^?'; }
# Fetches the daemon logged.
fetches() { grep -c "fetched .* missing objects" "$LOG"; }
# A mount of $REPO with a fresh log, replacing any (env, GITFOLD_*, passes
# through); waits for the previous daemon to exit, so the new one mounts.
remount() {
    cd /
    mountpoint -q "$MNT" && "$BIN" unmount "$MNT" >/dev/null 2>&1
    wait "${PID:-}" 2>/dev/null
    : >"$LOG"; mount_fs
}
hook() { # the server's pack-objects hook: $1 = a script body run before pack-objects
    printf '#!/bin/sh\n%s\nexec "$@"\n' "$1" >"$T/hook"; chmod +x "$T/hook"
    git config --global uploadpack.packObjectsHook "$T/hook"
}
no_hook() { git config --global --unset uploadpack.packObjectsHook; }

# A mount left by an interrupted run must go first: rm would delete through it.
mountpoint -q "$MNT" 2>/dev/null && "$BIN" unmount "$MNT" >/dev/null
rm -rf "$T"; mkdir -p "$MNT" "$HOME" "$XDG_CONFIG_HOME"
git init -q "$T/seed"
(cd "$T/seed" && mkdir -p a/b c && for i in $(seq 40); do
    echo "f$i" >a/f$i; echo "g$i" >a/b/g$i; echo "h$i" >c/h$i; done
    # One blob at 4 paths, 3 of them in one repeated subtree.
    for d in d1 d2 d3; do mkdir $d; echo same >$d/same; done; echo same >same)
S add -A; S commit -qm one
# Every blob unique to its commit, so each commit has its own to fetch.
for n in two three four five; do echo "f1 $n" >"$T/seed/a/f1"; echo "c $n" >"$T/seed/c/$n"; S add -A; S commit -qm "$n"; done
git clone -q --bare "$T/seed" "$T/remote.git"
git -C "$T/remote.git" config uploadpack.allowFilter true
git -C "$T/remote.git" config uploadpack.allowAnySHA1InWant true
REPO=$T/repo; clone "$REPO"
C branch -q ws1 HEAD~1; C branch -q ws2 HEAD~4

echo "== opens fetch once"
eq "a blob:none clone misses the blobs" 121 "$(missing HEAD~4)"
remount
eq "a snapshot opens" f1 "$(cat "$MNT/@HEAD~4/a/f1")"
eq "with one fetch" 1 "$(fetches)"
eq "nothing of it missing after" 0 "$(missing HEAD~4)"
eq "a workspace opens" "f1 four" "$(cat "$MNT/ws1/a/f1")"
eq "with one more fetch" 2 "$(fetches)"
eq "git status is clean" "" "$(git -C "$MNT/ws1" status --porcelain)"
eq "a workspace on a fetched commit fetches nothing" 2 "$(cat "$MNT/ws2/a/f1" >/dev/null; fetches)"
SHA=$(C rev-parse HEAD~2)
cat "$MNT/@HEAD~2/a/f1" >/dev/null & P1=$!; cat "$MNT/@$SHA/a/f1" >/dev/null & P2=$!; wait $P1 $P2
eq "two names of one commit opened at once: one fetch" 3 "$(fetches)"

echo "== the sync thread fetches for a branch moved outside"
(cd "$T/seed" && echo six >a/f1 && echo six >c/six); S add -A; S commit -qm six
S push -q "$T/remote.git" HEAD:master
C fetch -q origin
C update-ref refs/heads/ws1 origin/master
TIP=$(C rev-parse ws1)
for _ in $(seq 100); do [ "$(cat "$REPO/.git/gitfold/ws1/base")" = "$TIP" ] && break; sleep 0.1; done
eq "the workspace's base moves to the new tip" "$TIP" "$(cat "$REPO/.git/gitfold/ws1/base")"
eq "its new blobs fetched" 0 "$(missing ws1)"

echo "== a slow fetch holds up only its own tree"
hook 'mkdir "$HOME/slow" 2>/dev/null && sleep 6'
(cat "$MNT/@HEAD~3/c/two" >/dev/null) & SLOW=$!
sleep 0.5
s=$(date +%s%N)
cat "$MNT/@HEAD/c/five" >/dev/null
eq "an open of another commit doesn't wait" yes "$([ $(( ($(date +%s%N) - s) / 1000000 )) -lt 3000 ] && echo yes)"
wait $SLOW; no_hook

echo "== stalls"
export GITFOLD_FETCH_STALL_SECS=2
REPO=$T/stall; clone "$REPO"; remount
hook 'sleep 8.71'
s=$(date +%s%N)
check "a silent fetch fails the open" fails cat "$MNT/@HEAD/a/f1"
eq "within the stall limit" yes "$([ $(( ($(date +%s%N) - s) / 1000000 )) -lt 7000 ] && echo yes)"
check "logged as made no progress" grep -q "made no progress for 2 s" "$LOG"
check "nothing of it left running" sh -c "! ps -eo args= | awk '\$1 == \"sleep\" && \$2 == \"8.71\"' | grep -q ."
REPO=$T/tick; clone "$REPO"
hook 'for i in 1 2 3 4; do echo tick >&2; sleep 1; done'
remount
eq "a fetch that shows progress outlives the limit" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
no_hook; unset GITFOLD_FETCH_STALL_SECS

echo "== a fetch leaves nothing behind"
REPO=$T/leftover; clone "$REPO"; remount
# Started by the server, in the fetch's process group, holding its stderr.
hook 'sleep 7.29 >/dev/null 2>"/proc/$PPID/fd/2" </dev/null &'
eq "a snapshot opens" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
check "what the fetch started is stopped once it ends" sh -c "! ps -eo args= | awk '\$1 == \"sleep\" && \$2 == \"7.29\"' | grep -q ."
hook 'sleep 7.31 >/dev/null 2>&1 </dev/null &'
eq "a second commit opens" "c four" "$(cat "$MNT/@HEAD~1/c/four" 2>&1)"
left=$(ps -eo pid=,args= | awk '$2 == "sleep" && $3 == "7.31" {print $1}')
check "one not holding stderr is left to run (a credential cache daemon)" test -n "$left"
[ -n "$left" ] && kill $left
no_hook

echo "== an unreachable remote, and the backoff"
REPO=$T/unreach; clone "$REPO"
C branch -q u1 HEAD~1; C branch -q s1 HEAD~4
remount
eq "a workspace opens while the remote is there" "f1 two" "$(cat "$MNT/s1/a/f1" 2>&1)"
C remote set-url origin "file://$T/nonexistent.git"   # read by each fetch
errors() { grep ERROR "$LOG" | grep -c "$1"; }
C update-ref refs/heads/s1 HEAD~2   # never fetched
sleep 5
eq "the sync thread's failing fetch: one error in 5 s" 1 "$(errors "workspace s1")"
s=$(date +%s%N)
check "an open fails" fails timeout 60 cat "$MNT/u1/a/f1"
eq "fast" yes "$([ $(( ($(date +%s%N) - s) / 1000000 )) -lt 10000 ] && echo yes)"
check "and leaves no workspace state" test ! -e "$REPO/.git/gitfold/u1"
check "the error names the remote and command" grep -q "fetching .* missing objects from origin: .git fetch origin" "$LOG"
SHA3=$(C rev-parse HEAD~3)
cat "$MNT/@HEAD~3/a/f1" >/dev/null 2>&1 & P1=$!; cat "$MNT/@$SHA3/a/f1" >/dev/null 2>&1 & P2=$!; wait $P1 $P2
eq "two opens of a commit behind a failing fetch: one fetch error" 1 "$(grep ERROR "$LOG" | grep -c "HEAD~3\|$SHA3")"
before=$(grep -c ERROR "$LOG")
for _ in 1 2 3 4; do cat "$MNT/u1/a/f1" 2>/dev/null; done
eq "repeated opens in the backoff log no more errors" "$before" "$(grep -c ERROR "$LOG")"
# Brought by hand, from the real remote: the backoff ends at once.
GIT_NO_LAZY_FETCH=1 C rev-list --objects --missing=print "u1^{tree}" | sed -n 's/^?//p' |
    C -c fetch.negotiationAlgorithm=noop fetch -q "file://$T/remote.git" --no-tags \
        --no-write-fetch-head --filter=blob:none --stdin
eq "fetched by hand, the workspace opens within the backoff" "f1 five" "$(cat "$MNT/u1/a/f1" 2>&1)"
eq "without gitfold fetching" 1 "$(fetches)"

echo "== an unmount doesn't wait for a hanging remote"
REPO=$T/hang; clone "$REPO"; C branch -q h1 HEAD~2
remount
cat "$MNT/h1/a/f1" >/dev/null
hook 'sleep 8.37'
C update-ref refs/heads/h1 HEAD   # never fetched: the sync thread fetches, and hangs
sleep 1.5
s=$(date +%s%N)
cd /; "$BIN" unmount "$MNT" >/dev/null 2>&1; wait "$PID" 2>/dev/null
eq "the unmount doesn't fetch" yes "$([ $(( ($(date +%s%N) - s) / 1000000 )) -lt 5000 ] && echo yes)"
check "and says why the base stayed" grep -q "base left behind the branch tip at unmount" "$LOG"
no_hook
for _ in $(seq 100); do ps -eo args= | awk '$1 == "sleep" && $2 == "8.37"' | grep -q . || break; sleep 0.1; done

echo "== tree:0 clones"
REPO=$T/tree0; rm -rf "$REPO"
git clone -q --no-checkout --filter=tree:0 "file://$T/remote.git" "$REPO"
remount
eq "a commit whose trees are missing opens" "c five" "$(cat "$MNT/@HEAD~1/c/five" 2>&1)"
eq "with 2 fetches: the trees, then the blobs" 2 "$(fetches)"

echo "== a resumed workspace whose stored base is missing"
REPO=$T/res1; clone "$REPO"; C branch -q rw HEAD~3
remount; cat "$MNT/rw/a/f1" >/dev/null
REPO=$T/res2; clone "$REPO"; C branch -q rw HEAD~1
mkdir -p "$REPO/.git/gitfold"; cp -r "$T/res1/.git/gitfold/rw" "$REPO/.git/gitfold/"
remount
check "it opens" test -f "$MNT/rw/a/f1"
check "resumed" grep -q "workspace rw: resumed" "$LOG"
TIP=$(C rev-parse rw)
for _ in $(seq 100); do [ "$(cat "$REPO/.git/gitfold/rw/base")" = "$TIP" ] && break; sleep 0.1; done
eq "then follows the tip" "$TIP" "$(cat "$REPO/.git/gitfold/rw/base")"
eq "base and tip fetched" 0 "$(( $(missing HEAD~3) + $(missing HEAD~1) ))"
# Its tip moved while unmounted and the remote is gone: the base is enough.
REPO=$T/res1; C update-ref refs/heads/rw HEAD; C remote set-url origin "file://$T/nonexistent.git"
remount
check "with only its tip unfetchable, it opens" test -f "$MNT/rw/a/f1"

echo "== which remote: git's precedence"
REPO=$T/remotes; clone "$REPO"
C config --unset remote.origin.promisor; C config --unset remote.origin.partialclonefilter
C remote set-url origin "file://$T/nonexistent.git"
C remote add aaa "file://$T/nonexistent.git"
C remote add zzz "file://$T/remote.git"; C config remote.zzz.promisor true
C remote add bbb "file://$T/nonexistent.git"; C config remote.bbb.promisor true
remount
eq "the first promisor remote in config order" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
check "zzz, not aaa or bbb" grep -q "missing objects from zzz" "$LOG"
REPO=$T/filteronly; clone "$REPO"; C config --unset remote.origin.promisor
remount
eq "a remote with only a partialclonefilter" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
REPO=$T/extension; clone "$REPO"
C config --unset remote.origin.promisor; C config --unset remote.origin.partialclonefilter
C config extensions.partialClone origin
remount
eq "extensions.partialClone alone" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
REPO=$T/bare; clone "$REPO"
C config --unset remote.origin.promisor; C config --unset remote.origin.partialclonefilter
C remote set-url origin "file://$T/nonexistent.git"
printf '[remote "q"]\n\turl = file://%s/remote.git\n\tpromisor\n' "$T" >>"$REPO/.git/config"
remount
eq "\`promisor\` without a value is true" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
check "q" grep -q "missing objects from q" "$LOG"
REPO=$T/tail; clone "$REPO"
C remote set-url origin "file://$T/nonexistent.git"
C remote add other "file://$T/remote.git"; C config remote.other.promisor true
C config core.repositoryformatversion 1; C config extensions.partialClone origin
remount
eq "the extensions.partialClone remote is tried last" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
check "other, not origin" grep -q "missing objects from other" "$LOG"
echo "== refusals, the next remote, free space"
# A remote without the objects: protocol v2 says "not our ref".
git init -q --bare "$T/empty.git"
git -C "$T/empty.git" config uploadpack.allowFilter true
git -C "$T/empty.git" config uploadpack.allowAnySHA1InWant true
REPO=$T/next; clone "$REPO"
C remote set-url origin "file://$T/empty.git"
C remote add good "file://$T/remote.git"; C config remote.good.promisor true
remount
eq "a first remote without the objects: the next one has them" "c five" "$(cat "$MNT/@HEAD/c/five" 2>&1)"
check "fetched from it" grep -q "missing objects from good" "$LOG"
check "the first one's failure logged as a warning" grep -q "WARN.*not our ref.*another promisor remote had them" "$LOG"
REPO=$T/refused; clone "$REPO"
C remote set-url origin "file://$T/empty.git"
C remote add also "file://$T/empty.git"; C config remote.also.promisor true
RUST_LOG=info,gitfold=debug remount
check "refused by every remote: the open fails" fails cat "$MNT/@HEAD/c/five"
check "the error says what the server needs" grep -q "ERROR.*still missing after every promisor remote.*uploadpack.allowAnySHA1InWant" "$LOG"
check "refused again at once" fails cat "$MNT/@HEAD/c/five"
check "for 10 min" grep -q "retried within 600 s" "$LOG"
REPO=$T/mixed; clone "$REPO"
C remote set-url origin "file://$T/empty.git"
C remote add gone "file://$T/nonexistent.git"; C config remote.gone.promisor true
RUST_LOG=info,gitfold=debug remount
check "refused by one remote, unreachable on the other: the open fails" fails cat "$MNT/@HEAD/c/five"
check "refused again at once" fails cat "$MNT/@HEAD/c/five"
check "for the usual 30 s (the other may come back)" grep -q "retried within 30 s" "$LOG"
REPO=$T/full; clone "$REPO"
GITFOLD_FETCH_MIN_FREE_MB=999999999 remount
check "too little free space: the open fails" fails cat "$MNT/@HEAD/c/five"
check "without fetching, saying why" grep -q "not fetching: .* MB free in .*, a fetch needs at least 999999999 MB" "$LOG"
check "no fetch started" sh -c "! grep -q 'fetching .* missing objects from' '$LOG'"

REPO=$T/nopromisor; clone "$REPO"
C config --unset remote.origin.promisor; C config --unset remote.origin.partialclonefilter
remount
check "no promisor remote: the open fails" fails cat "$MNT/@HEAD/c/five"
check "without fetching" sh -c "! grep -q 'fetch' '$LOG'"
eq "the missing objects reported once" 1 "$(grep -c "objects are missing" "$LOG")"
DISTINCT=$(C ls-tree -r HEAD | awk '$2 == "blob" { print $3 }' | sort -u | wc -l)
eq "each missing object once (a repeated subtree walked once)" "$DISTINCT" "$(grep -o "[0-9]* objects are missing" "$LOG" | cut -d" " -f1)"

cd /; "$BIN" unmount "$MNT" >/dev/null; wait "$PID" 2>/dev/null

echo "== $FAILS failure(s)"
[ -n "${KEEP:-}" ] || rm -rf "$T"
exit $((FAILS > 0))
