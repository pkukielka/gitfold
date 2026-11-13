#!/usr/bin/env bash
# End-to-end test of the management commands: branch and its forks, list, changes, remove (exit codes, while mounted) and prune.
# usage: tests/manage.sh
# Each section runs on a fresh repository below /tmp/gf-manage.XXXXXX (see
# tests/lib.sh); SECTION=<text> runs only the sections whose name contains it.
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=manage
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

branch() {
check "branch from HEAD" "$BIN" -C "$REPO" branch agent/one
check "branch --from" "$BIN" -C "$REPO" branch two --from main
check "names starting with @ refused" fails "$BIN" -C "$REPO" branch @x
check "existing branch refused" fails "$BIN" -C "$REPO" branch two
}

list() {
eq "nothing yet" "no workspaces" "$("$BIN" -C "$REPO" list 2>/dev/null)"
mount_fs
W1=$MNT/agent/one; W2=$MNT/two
echo changed >>"$W1/a.txt"; rm "$W1/d/b.txt"; echo new >"$W1/new.txt"
echo x >>"$W2/a.txt"; G "$W2" commit -qam x
for _ in $(seq 50); do [ "$(cat "$COMMON/gitfold/two/base")" = "$(R rev-parse two)" ] && break; sleep 0.1; done
ls "$MNT/@HEAD" >/dev/null                     # a snapshot registration
}

the_repository_from_the_current_director() {
L=$("$BIN" -C "$REPO" list 2>&1)
eq "from its work tree" "$L" "$(cd "$REPO" && "$BIN" list 2>&1)"
eq "from a subdirectory" "$L" "$(cd "$REPO/d" && "$BIN" list 2>&1)"
eq "from inside a workspace" "$L" "$(cd "$W1/d" && "$BIN" list 2>&1)"
eq "-C after the command too" "$L" "$("$BIN" list -C "$REPO" 2>&1)"
check "outside any repository, an error naming the directory" sh -c "cd / && '$BIN' list 2>&1 | grep -q 'no git repository at or above /$'"
eq "from inside .git" "$L" "$(cd "$REPO/.git" && "$BIN" list 2>&1)"
EXPECT=$(printf 'M\ta.txt\nD\td/b.txt\nA\tnew.txt')
eq "changes while mounted: modified, deleted, added, by path" "$EXPECT" "$("$BIN" -C "$REPO" changes agent/one 2>&1)"
eq "--stat: lines added and removed, and git's summary" "M	1	0	a.txt
D	0	1	d/b.txt
A	1	0	new.txt
 3 files changed, 2 insertions(+), 1 deletion(-)" "$("$BIN" -C "$REPO" changes --stat agent/one 2>&1)"
eq "list: when each last changed (a file written; a commit)" "yes yes" "$(awk '$1 == "agent/one" || $1 == "two" { print ($6 ~ /^[0-9]+s$/ ? "yes" : $6) }' <<<"$L" | xargs)"
eq "list --stat: the lines in total" "+2 -1" "$("$BIN" -C "$REPO" list --stat 2>&1 | awk '$1 == "agent/one" { print $5, $6 }')"
eq "changes -z: NUL-terminated" "M	a.txt|D	d/b.txt|A	new.txt|" "$("$BIN" -C "$REPO" changes -z agent/one | tr '\0' '|')"
eq "a committed workspace has none" "" "$("$BIN" -C "$REPO" changes two 2>&1)"
check "an unknown workspace refused" fails "$BIN" -C "$REPO" changes nope
eq "in a workspace, it's that one's" "$EXPECT" "$(cd "$W1" && "$BIN" changes 2>&1)"
eq "from a subdirectory of it too" "$EXPECT" "$(cd "$W1/d" && "$BIN" changes 2>&1)"
check "outside a workspace, the branch must be named" sh -c "cd '$REPO' && '$BIN' changes 2>&1 | grep -q 'not in a gitfold workspace'"
mkdir -p "$T/mnt2"
check "second mount of the repository refused" fails timeout 5 "$BIN" mount "$REPO" "$T/mnt2"
check "second mount point not mounted" fails mountpoint -q "$T/mnt2"
out=$("$BIN" -C "$REPO" remove agent/one 2>&1)
eq "remove of a workspace with changes refused while mounted" yes "$(grep -q uncommitted <<<"$out" && echo yes)"
check "prune refused while mounted" fails "$BIN" -C "$REPO" prune
check "the git common directory is what is locked" fails flock -n "$COMMON" true
unmount_fs
out=$("$BIN" -C "$REPO" list 2>/dev/null)
eq "workspace with changes" "2 1" "$(echo "$out" | awk '$1 == "agent/one" { print $2, $3 }')"
eq "committed workspace is clean" "0 0" "$(echo "$out" | awk '$1 == "two" { print $2, $3 }')"
eq "changes unmounted: the same" "$EXPECT" "$("$BIN" -C "$REPO" changes agent/one 2>&1)"
}

remove() {
check "uncommitted changes refused" fails "$BIN" -C "$REPO" remove agent/one
out=$("$BIN" -C "$REPO" remove agent/one 2>&1); eq "refusal lists the paths" yes "$(grep -q new.txt <<<"$out" && echo yes)"
check "clean workspace removed" "$BIN" -C "$REPO" remove two
check "its state is gone" test ! -e "$COMMON/gitfold/two" -a ! -e "$COMMON/worktrees/gitfold-ws-two"
check "its base ref is gone" fails git -C "$REPO" rev-parse -q --verify refs/gitfold/two
check "its commits stay" test "$(R log -1 --format=%s two)" = x
check "branch deletable now" R branch -D two
check "--force discards changes" "$BIN" -C "$REPO" remove agent/one --force
check "state gone" test ! -e "$COMMON/gitfold/agent%2Fone"
check "unknown workspace refused" fails "$BIN" -C "$REPO" remove nope
eq "list empty again" "no workspaces" "$("$BIN" -C "$REPO" list 2>/dev/null)"
}

prune() {
check "snapshot registration exists" sh -c "ls '$COMMON/worktrees' | grep -q '^gitfold-[0-9a-f]\{12\}$'"
check "prune" "$BIN" -C "$REPO" prune
check "snapshot registrations gone" sh -c "! ls '$COMMON/worktrees' 2>/dev/null | grep -q '^gitfold-'"
check "git worktree list is clean" test "$(git -C "$REPO" worktree list | wc -l)" = 1
mount_fs
check "snapshot reopens after prune" test -f "$MNT/@HEAD/a.txt"
check "workspace reopens after remove" test -f "$MNT/agent/one/d/b.txt"
unmount_fs
}

# Steps that share a repository, workspaces and a mount run as one section.
# Branches made, listed, used from anywhere, then removed and pruned:
story() { branch; list; the_repository_from_the_current_director; remove; prune; }

changes_edge_cases() {
R branch ch; R branch fresh
mount_fs
WC=$MNT/ch
rm -r "$WC/d"; echo x >"$WC/d"                  # a file where the base has a directory
ln -s a.txt "$WC/link"; chmod +x "$WC/a.txt"
echo '*.log' >"$WC/.gitignore"; echo l >"$WC/x.log"
echo t >"$WC/t	b"
EXPECT=$(printf 'A\t.gitignore\nM\ta.txt\nA\td\nD\td/b.txt\nD\td/c.txt\nA\tlink\nA\t"t\\tb"')
eq "file over a directory, symlink, exec bit, ignored file left out, quoted tab" "$EXPECT" "$("$BIN" -C "$REPO" changes ch 2>&1)"
eq "-z leaves the path as is" "yes" "$("$BIN" -C "$REPO" changes -z ch | tr '\0' '\n' | grep -qx "A	t	b" && echo yes)"
printf 'a\0b' >"$WC/bin"
eq "--stat: an exec bit only, a binary file, a file over a directory, a symlink" "M	0	0	a.txt
A	-	-	bin
A	1	0	d
D	0	1	d/b.txt
A	1	0	link" "$("$BIN" -C "$REPO" changes --stat ch 2>&1 | grep -E '	(a\.txt|bin|d|d/b\.txt|link)$')"
rm "$WC/bin"
eq "a branch never opened has no changes" "" "$("$BIN" -C "$REPO" changes fresh 2>&1)"
check "an invalid name refused" fails "$BIN" -C "$REPO" changes ..
unmount_fs
R update-ref refs/heads/ch "$(R commit-tree "ch^{tree}" -p ch -m moved)"
out=$("$BIN" -C "$REPO" changes ch 2>&1 >/dev/null)
eq "a branch moved since: a note on stderr" yes "$(grep -q 'base behind the branch tip' <<<"$out" && echo yes)"
find "$COMMON/gitfold/ch" -exec touch -h -d '3 days ago' {} +; touch -d '3 days ago' "$COMMON/logs/refs/heads/ch"
eq "list: nothing changed for 3 days" 3d "$("$BIN" -C "$REPO" list 2>&1 | awk '$1 == "ch" { print $6 }')"
R update-ref -m commit refs/heads/ch "$(R commit-tree "ch^{tree}" -p ch -m again)"
eq "a commit alone (the branch's reflog) counts" yes "$("$BIN" -C "$REPO" list 2>&1 | awk '$1 == "ch" { print ($6 ~ /^[0-9]+s$/ ? "yes" : $6) }')"
}

branch_with_changes_a_fork() {
R branch fk; R branch nv; R branch sl/x
mount_fs
WF=$MNT/fk
echo changed >>"$WF/a.txt"; rm "$WF/d/b.txt"; echo new >"$WF/new.txt"; echo staged >"$WF/staged.txt"
G "$WF" add staged.txt
eq "fork while mounted" "created branch fk2 at fk, with its uncommitted changes; open it as <mountpoint>/fk2" "$("$BIN" -C "$REPO" branch -w --from fk fk2 2>&1)"
eq "the fork has the same changes" "$("$BIN" -C "$REPO" changes fk)" "$("$BIN" -C "$REPO" changes fk2 2>&1)"
eq "and the same git status, staged file included" "$(git -C "$WF" status --porcelain)" "$(git -C "$MNT/fk2" status --porcelain 2>&1)"
eq "the same content" "$(cat "$WF/a.txt")" "$(cat "$MNT/fk2/a.txt")"
eq "the branch starts at the source's tip" "$(R rev-parse fk)" "$(R rev-parse fk2)"
echo more >>"$MNT/fk2/a.txt"
eq "then each changes on its own" "a
changed" "$(cat "$WF/a.txt")"
check "a fork onto an existing branch refused" fails "$BIN" -C "$REPO" branch -w --from fk fk2
out=$("$BIN" -C "$REPO" branch -w --from fk fk 2>&1)
eq "a fork into itself refused" yes "$(grep -q "created from itself" <<<"$out" && echo yes)"
check "a fork of an unknown branch refused" fails "$BIN" -C "$REPO" branch -w --from nope fk9
check "a fork of a snapshot refused" fails "$BIN" -C "$REPO" branch -w --from @HEAD fk9
eq "a branch never opened: just the branch" "created branch nv2 at nv (it had no workspace yet: no changes to copy); open it as <mountpoint>/nv2" "$("$BIN" -C "$REPO" branch -w --from nv nv2 2>&1)"
check "nv2 exists" git -C "$REPO" rev-parse -q --verify refs/heads/nv2
echo s >>"$MNT/sl/x/a.txt"; chmod +x "$MNT/sl/x/d/c.txt"
eq "names with slashes" "created branch sl/y at sl/x, with its uncommitted changes; open it as <mountpoint>/sl/y" "$("$BIN" -C "$REPO" branch -w --from sl/x sl/y 2>&1)"
eq "mode and mtime kept" "$(stat -c '%a %Y' "$MNT/sl/x/d/c.txt")" "$(stat -c '%a %Y' "$MNT/sl/y/d/c.txt" 2>&1)"
eq "its git status" "$(git -C "$MNT/sl/x" status --porcelain)" "$(git -C "$MNT/sl/y" status --porcelain 2>&1)"
touch "$COMMON/worktrees/gitfold-ws-fk/MERGE_HEAD"
out=$("$BIN" -C "$REPO" branch -w --from fk fkb 2>&1); rc=$?
eq "a source with a merge in progress refused (exit 4)" "4 yes" "$rc $(grep -q 'in progress' <<<"$out" && echo yes)"
rm "$COMMON/worktrees/gitfold-ws-fk/MERGE_HEAD"
mkdir "$COMMON/worktrees/gitfold-ws-lo"
check "a registration left for the new name refused" fails "$BIN" -C "$REPO" branch -w --from fk lo
check "and left alone" test -d "$COMMON/worktrees/gitfold-ws-lo"
rmdir "$COMMON/worktrees/gitfold-ws-lo"
mkdir "$COMMON/gitfold/lo2"
check "a state directory left for the new name refused" fails "$BIN" -C "$REPO" branch -w --from fk lo2
rmdir "$COMMON/gitfold/lo2"
R update-ref refs/gitfold/lo3 HEAD
out=$("$BIN" -C "$REPO" branch -w --from fk lo3 2>&1)
eq "a pin left alone: refused, saying how to delete it" yes "$(grep -q 'git update-ref -d refs/gitfold/lo3' <<<"$out" && echo yes)"
R update-ref -d refs/gitfold/lo3
check "no copied index left in a fork's state" test ! -e "$COMMON/gitfold/fk2/.index"
out=$("$BIN" -C "$REPO" branch -w --from nope fk9 2>&1); rc=$?
eq "an unknown source: exit code 3" 3 "$rc"
chmod 000 "$WF/new.txt"
eq "a file its owner can't read is copied" "created branch fk4 at fk, with its uncommitted changes; open it as <mountpoint>/fk4" "$("$BIN" -C "$REPO" branch -w --from fk fk4 2>&1)"
eq "with its mode, in both" "0 0" "$(stat -c %a "$WF/new.txt") $(stat -c %a "$MNT/fk4/new.txt" 2>&1)"
chmod 644 "$WF/new.txt" "$MNT/fk4/new.txt"
eq "and its content" new "$(cat "$MNT/fk4/new.txt")"
G "$WF" update-index --split-index
eq "a split index: forked with its shared index" "created branch fk5 at fk, with its uncommitted changes; open it as <mountpoint>/fk5" "$("$BIN" -C "$REPO" branch -w --from fk fk5 2>&1)"
eq "git status works there" "$(git -C "$WF" status --porcelain)" "$(git -C "$MNT/fk5" status --porcelain 2>&1)"
G "$WF" update-index --no-split-index
check "a fork named HEAD refused" fails "$BIN" -C "$REPO" branch -w --from fk HEAD
check "and no branch HEAD made" fails git -C "$REPO" rev-parse -q --verify refs/heads/HEAD
out=$("$BIN" -C "$REPO" branch -w fk9 2>&1)
eq "outside a workspace, -w asks for --from" yes "$(grep -q 'name the source with --from' <<<"$out" && echo yes)"
unmount_fs
G "$REPO" config branch.fk.gitfoldPrewarm none
eq "fork unmounted" "created branch fk3 at fk, with its uncommitted changes; open it as <mountpoint>/fk3" "$("$BIN" -C "$REPO" branch -w --from fk fk3 2>&1)"
eq "the prewarm mode comes along" none "$(G "$REPO" config --get branch.fk3.gitfoldPrewarm)"
eq "the same changes" "$("$BIN" -C "$REPO" changes fk)" "$("$BIN" -C "$REPO" changes fk3 2>&1)"
eq "unmounted, names with slashes" "created branch sl/z at sl/x, with its uncommitted changes; open it as <mountpoint>/sl/z" "$("$BIN" -C "$REPO" branch -w --from sl/x sl/z 2>&1)"
mount_fs
eq "and once mounted, the same git status" "$(git -C "$WF" status --porcelain)" "$(git -C "$MNT/fk3" status --porcelain 2>&1)"
eq "sl/z too (its registration found the mount from sl/x's)" "$(git -C "$MNT/sl/x" status --porcelain)" "$(git -C "$MNT/sl/z" status --porcelain 2>&1)"
unmount_fs
R branch cw
mount_fs
echo here >"$MNT/cw/here.txt"
eq "in a workspace, -w alone copies that one" "created branch cw2 at cw, with its uncommitted changes; open it as <mountpoint>/cw2" "$(cd "$MNT/cw" && "$BIN" branch -w cw2 2>&1)"
eq "with its changes" "A	here.txt" "$("$BIN" -C "$REPO" changes cw2 2>&1)"
check "-w with --from a revision, not a branch, refused" fails "$BIN" -C "$REPO" branch -w --from HEAD cw3
unmount_fs
}

leftover_state_of_a_deleted_branch() {
check "branch five" "$BIN" -C "$REPO" branch five
mount_fs; ls "$MNT/five" >/dev/null; unmount_fs
R update-ref -d refs/heads/five            # deleted behind gitfold's back
check "recreating it refused" fails "$BIN" -C "$REPO" branch five
check "remove the leftover" "$BIN" -C "$REPO" remove five --force
check "recreating it works then" "$BIN" -C "$REPO" branch five
}

the_lock_leaves_nothing_behind() {
git init -q "$T/plain"; git -C "$T/plain" -c user.name=t -c user.email=t@t commit -q --allow-empty -m e
check "prune of a repository without gitfold state" "$BIN" -C "$T/plain" prune
check "creates nothing in it" test ! -e "$T/plain/.git/gitfold"

mkdir -p "$T/plain/.git/gitfold"; touch "$T/plain/.git/gitfold/.lock"
flock "$T/plain/.git/gitfold/.lock" sleep 3 & OLD=$!   # a daemon of an earlier build
sleep 0.3
check "an earlier build's lock file is honoured" fails "$BIN" -C "$T/plain" prune
wait $OLD
check "free again" "$BIN" -C "$T/plain" prune
}

remove_while_mounted() {
R branch live1; R branch live2; R branch nsx/live3; R branch live4; R branch live5
# Many files, for removals racing lookups below.
git init -q "$T/many"; mkdir "$T/many/d"
for i in $(seq 3000); do echo $i >"$T/many/d/f$i"; done
G "$T/many" add -A; G "$T/many" commit -qm many; R fetch -q "$T/many" HEAD:many
MAIN=$(R rev-parse main)
SNAPREG=$COMMON/worktrees/gitfold-$(R rev-parse --short=12 main)
mount_fs
cat "$MNT/live5/a.txt" >/dev/null   # stored, not open in the next mount
unmount_fs
mount_fs
check "remove a stored workspace that isn't open" "$BIN" -C "$REPO" remove live5
check "its state is gone too" test ! -e "$COMMON/gitfold/live5"
cat "$MNT/live1/a.txt" "$MNT/live2/a.txt" "$MNT/nsx/live3/a.txt" "$MNT/live4/a.txt" "$MNT/@main/a.txt" "$MNT/@$MAIN/a.txt" >/dev/null
check "remove an idle workspace while mounted" "$BIN" -C "$REPO" remove live1
check "it leaves the listing" sh -c "! ls '$MNT' | grep -qx live1"
check "its state is gone" test ! -e "$COMMON/gitfold/live1"
check "its registration and base ref too" sh -c "test ! -e '$COMMON/worktrees/gitfold-ws-live1' && ! git -C '$REPO' rev-parse -q --verify refs/gitfold/live1"
echo x >>"$MNT/live2/a.txt"
check "uncommitted changes refused" fails "$BIN" -C "$REPO" remove live2
check "the refused workspace stays usable" sh -c "echo y >>'$MNT/live2/a.txt' && grep -q y '$MNT/live2/a.txt'"
check "and in the listing" sh -c "ls '$MNT' | grep -qx live2"
out=$(cd "$MNT/live2" && "$BIN" -C "$REPO" remove live2 --force 2>&1)
eq "removing the workspace it runs in refused" yes "$(grep -q 'in use' <<<"$out" && echo yes)"
check "--force discards changes" "$BIN" -C "$REPO" remove live2 --force
( cd "$MNT/nsx/live3" && exec sleep 30 ) & HOLDER=$!
for _ in $(seq 100); do [ "$(cat /proc/$HOLDER/comm 2>/dev/null)" = sleep ] && break; sleep 0.02; done
out=$("$BIN" -C "$REPO" remove nsx/live3 2>&1)
eq "a workspace in use refused, naming the process" yes "$(grep -q 'sleep 30' <<<"$out" && echo yes)"
kill $HOLDER; wait $HOLDER 2>/dev/null
check "removed once the process left" "$BIN" -C "$REPO" remove nsx/live3
check "it leaves its namespace" sh -c "! ls '$MNT/nsx' | grep -qx live3"
out=$("$BIN" -C "$REPO" remove @main 2>&1)
eq "a snapshot sharing its commit keeps the registration" yes "$(grep -q stays <<<"$out" && test -d "$SNAPREG" && echo yes)"
check "the snapshot leaves the listing" sh -c "! ls '$MNT' | grep -qx @main"
check "the other name still works" test -f "$MNT/@$MAIN/a.txt"
check "removing the last name removes the registration" "$BIN" -C "$REPO" remove "@$MAIN"
check "registration gone" test ! -e "$SNAPREG"
check "a missing workspace refused" fails "$BIN" -C "$REPO" remove nonexistent
for name in .. . ""; do
    check "'$name' refused while mounted" fails "$BIN" -C "$REPO" remove "$name"
done
check "and nothing deleted" sh -c "test -d '$COMMON/objects' && test -d '$COMMON/gitfold/live4'"
( exec 9<"$MNT/live4/a.txt"; exec sleep 30 ) & HOLDER=$!
for _ in $(seq 100); do [ "$(cat /proc/$HOLDER/comm 2>/dev/null)" = sleep ] && break; sleep 0.02; done
out=$("$BIN" -C "$REPO" remove live4 2>&1)
eq "a file open in a workspace refuses its removal" yes "$(grep -q 'sleep 30' <<<"$out" && echo yes)"
kill $HOLDER; wait $HOLDER 2>/dev/null
check "removed once it was closed" "$BIN" -C "$REPO" remove live4
out=$("$BIN" -C "$REPO" remove @main 2>&1)
eq "a snapshot not open, without a registration" yes "$(grep -q 'no registration' <<<"$out" && echo yes)"
echo x >>"$MNT/many/d/f1"
( failed=0; for i in $(seq 2 3000); do stat "$MNT/many/d/f$i" >/dev/null 2>&1 || failed=$((failed+1)); done
  echo $failed >"$T/failed" ) & WALK=$!
for _ in 1 2 3 4 5; do "$BIN" -C "$REPO" remove many >/dev/null 2>&1; done
wait $WALK
eq "refused removals don't fail requests racing them" 0 "$(cat "$T/failed")"
missing=0; for i in $(seq 2 3000); do test -e "$MNT/many/d/f$i" || missing=$((missing+1)); done
eq "nor leave files missing" 0 "$missing"
check "a removed workspace opens again, fresh" sh -c "test -f '$MNT/live1/a.txt' && test -z \"\$(git -C '$MNT/live1' status --porcelain)\""
check "a removed snapshot opens again" test -f "$MNT/@main/a.txt"
kill -9 $PID; wait $PID 2>/dev/null
"$BIN" unmount "$MNT" >/dev/null 2>&1
mount_fs
cat "$MNT/many/d/f1" >/dev/null
check "after a crash, the new daemon takes requests (a stale socket replaced)" "$BIN" -C "$REPO" remove many --force
check "it removed it from the mount" sh -c "! ls '$MNT' | grep -qx many"
unmount_fs
check "a snapshot registration removed unmounted" sh -c "'$BIN' -C '$REPO' remove @main && test ! -e '$SNAPREG'"
check "'..' refused unmounted" fails "$BIN" -C "$REPO" remove ..
check "remove live1" "$BIN" -C "$REPO" remove live1
}

remove_exit_codes_check_wait() {
R branch ec1; R branch ec2
code() { "$@" >/dev/null 2>&1; echo $?; }
eq "not found: exit code 3" 3 "$(code "$BIN" -C "$REPO" remove nope)"
eq "--check of a missing workspace: 3" 3 "$(code "$BIN" -C "$REPO" remove nope --check)"
for cmd in "changes nope" "changes bad..name" "prewarm nope meta" "track-reads nope on" "send nope hi"; do
    eq "$cmd: exit code 3 as everywhere" 3 "$(code "$BIN" -C "$REPO" $cmd)"
done
flock "$COMMON" sleep 2 & HOLD=$!
sleep 0.3
eq "the lock held by another command: in use, exit 5" 5 "$(code "$BIN" -C "$REPO" remove nope)"
eq "--wait retries until it is free" 3 "$(code "$BIN" -C "$REPO" remove nope --wait 10)"
wait $HOLD
mount_fs
cat "$MNT/ec1/a.txt" >/dev/null
unmount_fs
eq "--check unmounted: 0" 0 "$(code "$BIN" -C "$REPO" remove ec1 --check)"
check "and nothing removed" test -d "$COMMON/gitfold/ec1"
mount_fs
cat "$MNT/ec1/a.txt" "$MNT/ec2/a.txt" "$MNT/@main/a.txt" >/dev/null
echo x >>"$MNT/ec2/a.txt"
eq "mounted, not found: 3" 3 "$(code "$BIN" -C "$REPO" remove nope)"
eq "uncommitted changes: 4" 4 "$(code "$BIN" -C "$REPO" remove ec2)"
eq "--check of a workspace with changes: 4" 4 "$(code "$BIN" -C "$REPO" remove ec2 --check)"
( cd "$MNT/ec2" && exec sleep 60 ) & HOLDER=$!
for _ in $(seq 100); do [ "$(cat /proc/$HOLDER/comm 2>/dev/null)" = sleep ] && break; sleep 0.02; done
out=$("$BIN" -C "$REPO" remove ec2 --check 2>&1); rc=$?
eq "--check of changes in a workspace in use: 4" 4 "$rc"
eq "and it reports both" yes "$(grep -q uncommitted <<<"$out" && grep -q "in use" <<<"$out" && echo yes)"
kill $HOLDER; wait $HOLDER 2>/dev/null
eq "--check of a removable workspace: 0" 0 "$(code "$BIN" -C "$REPO" remove ec1 --check)"
check "--check removes nothing" sh -c "test -d '$COMMON/gitfold/ec1' && ls '$MNT' | grep -qx ec1"
eq "--check of a snapshot: 0" 0 "$(code "$BIN" -C "$REPO" remove @main --check)"
check "and leaves it open" sh -c "ls '$MNT' | grep -qx @main"
( cd "$MNT/ec1" && exec sleep 60 ) & HOLDER=$!
for _ in $(seq 100); do [ "$(cat /proc/$HOLDER/comm 2>/dev/null)" = sleep ] && break; sleep 0.02; done
eq "in use: 5" 5 "$(code "$BIN" -C "$REPO" remove ec1)"
eq "--check of a workspace in use: 5" 5 "$(code "$BIN" -C "$REPO" remove ec1 --check)"
check "the refusal's kind isn't shown" sh -c "! '$BIN' -C '$REPO' remove ec1 2>&1 | grep -q refusal:"
eq "--wait gives up: 5" 5 "$(code "$BIN" -C "$REPO" remove ec1 --wait 1)"
( "$BIN" -C "$REPO" remove ec1 --wait 30 >/dev/null 2>&1; echo $? >"$T/waited" ) & WAITER=$!
sleep 1
kill $HOLDER; wait $HOLDER 2>/dev/null
wait $WAITER
eq "--wait retries until it is free" 0 "$(cat "$T/waited")"
check "and it is gone" test ! -e "$COMMON/gitfold/ec1"
unmount_fs
eq "unmounted, changes: 4" 4 "$(code "$BIN" -C "$REPO" remove ec2)"
check "remove ec2 --force" "$BIN" -C "$REPO" remove ec2 --force
}

section "branch, list, the current directory, remove, prune" story
section "changes: edge cases" changes_edge_cases
section "branch --with-changes (a fork)" branch_with_changes_a_fork
section "leftover state of a deleted branch" leftover_state_of_a_deleted_branch
section "the lock leaves nothing behind" the_lock_leaves_nothing_behind
section "remove while mounted" remove_while_mounted
section "remove: exit codes, --check, --wait" remove_exit_codes_check_wait
finish
