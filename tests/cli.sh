#!/usr/bin/env bash
# End-to-end test of the workspace management commands: branch, list, remove,
# prune.
# usage: tests/cli.sh
# Creates /tmp/gf-cli-test/{repo,mnt}; removes them at the end unless KEEP=1.
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
T=/tmp/gf-cli-test
REPO=$T/repo
MNT=$T/mnt
LOG=$T/gitfold.log
. "$(dirname "$0")/lib.sh"

G() { git -C "$1" -c user.name=t -c user.email=t@t "${@:2}"; }
R() { git -C "$REPO" -c user.name=t -c user.email=t@t "$@"; }
fails() { ! "$@"; }
COMMON=$REPO/.git

rm -rf "$T"; mkdir -p "$MNT" "$REPO"
R init -q -b main
(cd "$REPO" && mkdir d && echo a >a.txt && echo b >d/b.txt && echo c >d/c.txt)
R add -A; R commit -qm init

echo "== branch"
check "branch from HEAD" "$BIN" branch "$REPO" agent/one
check "branch --from" "$BIN" branch "$REPO" two --from main
check "names starting with @ refused" fails "$BIN" branch "$REPO" @x
check "existing branch refused" fails "$BIN" branch "$REPO" two

echo "== list"
eq "nothing yet" "no workspaces" "$("$BIN" list "$REPO" 2>/dev/null)"
mount_fs
W1=$MNT/agent/one; W2=$MNT/two
echo changed >>"$W1/a.txt"; rm "$W1/d/b.txt"; echo new >"$W1/new.txt"
echo x >>"$W2/a.txt"; G "$W2" commit -qam x
for _ in $(seq 50); do [ "$(cat "$COMMON/gitfold/two/base")" = "$(R rev-parse two)" ] && break; sleep 0.1; done
ls "$MNT/@HEAD" >/dev/null                     # a snapshot registration
check "remove refused while mounted" fails "$BIN" remove "$REPO" two
check "prune refused while mounted" fails "$BIN" prune "$REPO"
unmount_fs
out=$("$BIN" list "$REPO" 2>/dev/null)
eq "workspace with changes" "2 1" "$(echo "$out" | awk '$1 == "agent/one" { print $2, $3 }')"
eq "committed workspace is clean" "0 0" "$(echo "$out" | awk '$1 == "two" { print $2, $3 }')"

echo "== remove"
check "uncommitted changes refused" fails "$BIN" remove "$REPO" agent/one
out=$("$BIN" remove "$REPO" agent/one 2>&1); eq "refusal lists the paths" yes "$(grep -q new.txt <<<"$out" && echo yes)"
check "clean workspace removed" "$BIN" remove "$REPO" two
check "its state is gone" test ! -e "$COMMON/gitfold/two" -a ! -e "$COMMON/worktrees/gitfold-ws-two"
check "its base ref is gone" fails git -C "$REPO" rev-parse -q --verify refs/gitfold/two
check "its commits stay" test "$(R log -1 --format=%s two)" = x
check "branch deletable now" R branch -D two
check "--force discards changes" "$BIN" remove "$REPO" agent/one --force
check "state gone" test ! -e "$COMMON/gitfold/agent%2Fone"
check "unknown workspace refused" fails "$BIN" remove "$REPO" nope
eq "list empty again" "no workspaces" "$("$BIN" list "$REPO" 2>/dev/null)"

echo "== prune"
check "snapshot registration exists" sh -c "ls '$COMMON/worktrees' | grep -q '^gitfold-[0-9a-f]\{12\}$'"
check "prune" "$BIN" prune "$REPO"
check "snapshot registrations gone" sh -c "! ls '$COMMON/worktrees' 2>/dev/null | grep -q '^gitfold-'"
check "git worktree list is clean" test "$(git -C "$REPO" worktree list | wc -l)" = 1
mount_fs
check "snapshot reopens after prune" test -f "$MNT/@HEAD/a.txt"
check "workspace reopens after remove" test -f "$MNT/agent/one/d/b.txt"
unmount_fs

echo "== deletions the status must see"
sm=$(R rev-parse HEAD)
R update-index --add --cacheinfo "160000,$sm,sm"; R commit -qm submodule
check "branch three" "$BIN" branch "$REPO" three
mount_fs
W3=$MNT/three
rm "$W3/a.txt"; mkdir "$W3/a.txt"          # a file replaced by an empty directory
rmdir "$W3/sm"                             # a deleted submodule
rm -r "$W3/d"; echo x >"$W3/d"             # a directory replaced by a file
unmount_fs
out=$("$BIN" list "$REPO" 2>/dev/null)
eq "counted (d changed; a.txt, sm, d/b.txt, d/c.txt deleted)" "1 4" \
    "$(echo "$out" | awk '$1 == "three" { print $2, $3 }')"
check "remove refused" fails "$BIN" remove "$REPO" three
check "remove --force" "$BIN" remove "$REPO" three --force

echo "== git operation stopped half-way"
R branch other main; R checkout -q other; echo theirs >"$REPO/a.txt"; R commit -qam theirs; R checkout -q main
check "branch four" "$BIN" branch "$REPO" four
mount_fs
echo mine >"$MNT/four/a.txt"; G "$MNT/four" commit -qam mine
fails G "$MNT/four" merge -q other >/dev/null 2>&1
echo resolved >"$MNT/four/a.txt"           # resolved, not committed: MERGE_HEAD stays
G "$MNT/four" add a.txt
unmount_fs
out=$("$BIN" remove "$REPO" four 2>&1)
eq "remove refused mid-merge" yes "$(grep -q 'in progress (MERGE_HEAD)' <<<"$out" && echo yes)"
check "remove --force" "$BIN" remove "$REPO" four --force

echo "== leftover state of a deleted branch"
check "branch five" "$BIN" branch "$REPO" five
mount_fs; ls "$MNT/five" >/dev/null; unmount_fs
R update-ref -d refs/heads/five            # deleted behind gitfold's back
check "recreating it refused" fails "$BIN" branch "$REPO" five
check "remove the leftover" "$BIN" remove "$REPO" five --force
check "recreating it works then" "$BIN" branch "$REPO" five

echo "== staged-only changes and a base behind the tip"
check "branch six" "$BIN" branch "$REPO" six
mount_fs
W6=$MNT/six
echo staged >"$W6/a.txt"; G "$W6" add a.txt; echo a >"$W6/a.txt"   # staged, then edited back
unmount_fs
out=$("$BIN" remove "$REPO" six 2>&1)
eq "staged change refused" yes "$(grep -q 'staged changes' <<<"$out" && echo yes)"
mount_fs
G "$W6" reset -q; echo c >>"$W6/a.txt"; G "$W6" commit -qam c   # then unmount at once
unmount_fs
out=$("$BIN" remove "$REPO" six 2>&1)
eq "base behind the tip refused" yes "$(grep -q 'behind the branch tip' <<<"$out" && echo yes)"
mount_fs; ls "$W6" >/dev/null; sleep 2.5; unmount_fs   # the base catches up
check "removable once the base caught up" "$BIN" remove "$REPO" six

echo "== $FAILS failure(s)"
[ -n "${KEEP:-}" ] || rm -rf "$T"
exit $((FAILS > 0))
