#!/usr/bin/env bash
# End-to-end test of the workspace management commands: branch, list, changes,
# remove, prune.
# usage: tests/cli.sh
# Creates /tmp/gf-cli-test/{repo,mnt}; removes them at the end unless KEEP=1.
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
CACHE_TREE=$(realpath "$(dirname "$0")/cache-tree.pl")
T=/tmp/gf-cli-test
REPO=$T/repo
MNT=$T/mnt
LOG=$T/gitfold.log
. "$(dirname "$0")/lib.sh"

G() { git -C "$1" -c user.name=t -c user.email=t@t "${@:2}"; }
R() { git -C "$REPO" -c user.name=t -c user.email=t@t "$@"; }
fails() { ! "$@"; }
COMMON=$REPO/.git
export HOME=$T/home XDG_CONFIG_HOME=$T/xdg   # no global git config or ignore file from the test machine

rm -rf "$T"; mkdir -p "$MNT" "$REPO"
R init -q -b main
(cd "$REPO" && mkdir d && echo a >a.txt && echo b >d/b.txt && echo c >d/c.txt)
R add -A; R commit -qm init

echo "== branch"
check "branch from HEAD" "$BIN" -C "$REPO" branch agent/one
check "branch --from" "$BIN" -C "$REPO" branch two --from main
check "names starting with @ refused" fails "$BIN" -C "$REPO" branch @x
check "existing branch refused" fails "$BIN" -C "$REPO" branch two

echo "== list"
eq "nothing yet" "no workspaces" "$("$BIN" -C "$REPO" list 2>/dev/null)"
mount_fs
W1=$MNT/agent/one; W2=$MNT/two
echo changed >>"$W1/a.txt"; rm "$W1/d/b.txt"; echo new >"$W1/new.txt"
echo x >>"$W2/a.txt"; G "$W2" commit -qam x
for _ in $(seq 50); do [ "$(cat "$COMMON/gitfold/two/base")" = "$(R rev-parse two)" ] && break; sleep 0.1; done
ls "$MNT/@HEAD" >/dev/null                     # a snapshot registration
echo "== the repository from the current directory"
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

echo "== changes: edge cases"
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
"$BIN" -C "$REPO" remove --force ch >/dev/null; R branch -q -D ch fresh

echo "== branch --with-changes (a fork)"
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
for b in fk fk2 fk3 fk4 fk5 sl/x sl/y sl/z; do "$BIN" -C "$REPO" remove --force "$b" >/dev/null; done
R branch -q -D fk fk2 fk3 fk4 fk5 nv nv2 sl/x sl/y sl/z
R branch cw
mount_fs
echo here >"$MNT/cw/here.txt"
eq "in a workspace, -w alone copies that one" "created branch cw2 at cw, with its uncommitted changes; open it as <mountpoint>/cw2" "$(cd "$MNT/cw" && "$BIN" branch -w cw2 2>&1)"
eq "with its changes" "A	here.txt" "$("$BIN" -C "$REPO" changes cw2 2>&1)"
check "-w with --from a revision, not a branch, refused" fails "$BIN" -C "$REPO" branch -w --from HEAD cw3
unmount_fs
for b in cw cw2; do "$BIN" -C "$REPO" remove --force "$b" >/dev/null; done
R branch -q -D cw cw2

echo "== overlaps: reads and writes changed elsewhere"
R branch st1; R branch st2; R branch st3
check "track-reads needs a branch" fails "$BIN" -C "$REPO" track-reads nope on
mount_fs
eq "track-reads on before it opens" "st1 isn't open: applied when it opens" "$("$BIN" -C "$REPO" track-reads st1 on 2>&1)"
cat "$MNT/st1/a.txt" "$MNT/st1/d/b.txt" >/dev/null
echo w >>"$MNT/st1/d/c.txt"; echo x >"$MNT/st1/x.txt"; echo nd >"$MNT/st1/nd"
echo n >"$MNT/st1/n.txt"; G "$MNT/st1" add n.txt; G "$MNT/st1" commit -qm n -- n.txt
for _ in $(seq 50); do [ "$(cat "$COMMON/gitfold/st1/base")" = "$(R rev-parse st1)" ] && break; sleep 0.1; done
eq "reads recorded once each, in order (git's read of n.txt too)" "a.txt d/b.txt n.txt" "$(tr '\0' ' ' <"$COMMON/gitfold/st1/reads" | sed 's/ $//')"
echo x >"$MNT/st2/x.txt"; mkdir "$MNT/st2/nd"; echo y >"$MNT/st2/nd/y"
R switch -q main; echo m >>"$REPO/a.txt"; echo m >>"$REPO/d/c.txt"; echo m >"$REPO/n.txt"; echo m >"$REPO/e.txt"
R add -A; R commit -qm main-moves
out=$("$BIN" -C "$REPO" overlaps st1 --against main 2>&1); rc=$?
eq "against main: a read file, a changed one, a committed one (exit 6)" "6
R	main	a.txt
W	main	d/c.txt
W	main	n.txt" "$rc
$out"
eq "the default target: main (no upstream)" "$out" "$("$BIN" -C "$REPO" overlaps st1 2>&1)"
eq "inside the workspace, it is that one" "$out" "$(cd "$MNT/st1/d" && "$BIN" overlaps 2>&1)"
R update-ref refs/remotes/origin/main main; R symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main
R update-ref refs/remotes/origin/st1 st1; R config branch.st1.remote origin; R config branch.st1.merge refs/heads/st1
eq "the default target: origin/HEAD, not the upstream (its own remote branch)" "W	origin/main	d/c.txt
W	origin/main	n.txt" "$("$BIN" -C "$REPO" overlaps st1 --writes 2>&1)"
R update-ref -d refs/remotes/origin/HEAD; R update-ref -d refs/remotes/origin/main; R update-ref -d refs/remotes/origin/st1
R config --unset branch.st1.remote; R config --unset branch.st1.merge
R tag st2 main
eq "--writes" "W	main	d/c.txt
W	main	n.txt" "$("$BIN" -C "$REPO" overlaps st1 --writes 2>&1)"
eq "--reads: also a file it wrote" "R	main	a.txt
R	main	n.txt" "$("$BIN" -C "$REPO" overlaps st1 --reads 2>&1)"
check "--reads with --writes is a usage error" fails "$BIN" -C "$REPO" overlaps st1 --reads --writes
eq "against another workspace: its uncommitted changes, a file over its directory (a tag st2 ignored)" "W	st2	nd
W	st2	x.txt" "$("$BIN" -C "$REPO" overlaps st1 --against st2 2>&1)"
R tag -d st2 >/dev/null
out=$("$BIN" -C "$REPO" overlaps st1 --against-workspaces 2>&1)
eq "--against-workspaces: every other one" "W	st2	nd
W	st2	x.txt" "$(grep st2 <<<"$out")"
eq "earlier tests' too (agent/one changed a.txt, which st1 read), not main" "yes no" "$(grep -q 'R	agent/one	a.txt' <<<"$out" && echo yes) $(grep -q main <<<"$out" && echo yes || echo no)"
eq "both: one line per target" "4" "$("$BIN" -C "$REPO" overlaps st1 --against main --against-workspaces --writes 2>&1 | wc -l)"
out=$("$BIN" -C "$REPO" overlaps st2 --against main 2>"$T/st.err"); rc=$?
eq "untracked: writes only, saying so; no overlaps (exit 0)" "0  yes" "$rc $out $(grep -q "reads aren't tracked in st2" "$T/st.err" && echo yes)"
out=$("$BIN" -C "$REPO" overlaps st2 --reads 2>&1); rc=$?
eq "untracked with --reads: exit 3" 3 "$rc"
out=$("$BIN" -C "$REPO" overlaps st1 --against st1 2>&1); rc=$?
eq "against itself refused" "1 yes" "$rc $(grep -q 'compared with itself' <<<"$out" && echo yes)"
eq "-z" "W	main	d/c.txt" "$("$BIN" -C "$REPO" overlaps st1 --writes -z 2>&1 | tr '\0' '\n' | head -1)"
eq "a fork inherits the reads" "R	main	a.txt
R	main	n.txt" "$("$BIN" -C "$REPO" branch -w --from st1 st1f >/dev/null 2>&1; "$BIN" -C "$REPO" overlaps st1f --reads 2>&1)"
eq "and the setting" true "$(R config --get branch.st1f.gitfoldTrackReads)"
eq "track-reads off, applied at once" "stopped recording reads in st1" "$("$BIN" -C "$REPO" track-reads st1 off 2>&1)"
cat "$MNT/st1/d/c.txt" >/dev/null
eq "nothing more recorded; what was recorded stays" "a.txt d/b.txt n.txt" "$(tr '\0' ' ' <"$COMMON/gitfold/st1/reads" | sed 's/ $//')"
unmount_fs
eq "unmounted, the same" "R	main	a.txt
W	main	d/c.txt
W	main	n.txt" "$("$BIN" -C "$REPO" overlaps st1 --against main 2>/dev/null)"
eq "an unknown branch: exit 3" 3 "$("$BIN" -C "$REPO" overlaps nope >/dev/null 2>&1; echo $?)"
mount_fs --track-reads --prewarm content
cat "$MNT/st2/a.txt" "$MNT/st1/a.txt" >/dev/null
eq "the mount's --track-reads: st2 records" "R	main	a.txt" "$("$BIN" -C "$REPO" overlaps st2 --reads 2>&1)"
eq "a branch's off wins" "a.txt d/b.txt n.txt" "$(tr '\0' ' ' <"$COMMON/gitfold/st1/reads" | sed 's/ $//')"
for _ in $(seq 50); do grep -q 'st2: prewarm content walks as meta' "$LOG" && break; sleep 0.1; done
check "a tracked workspace's content prewarm walks as meta" grep -q 'st2: prewarm content walks as meta' "$LOG"
unmount_fs
for b in st1 st1f st2; do "$BIN" -C "$REPO" remove --force "$b" >/dev/null; done
R branch -q -D st1 st1f st2 st3
R reset -q --hard HEAD~1

echo "== the task folder"
R branch tk; R branch tk3; R branch old
R switch -q -c tracked main; echo t >"$REPO/.task"; R add -f .task; R commit -qm tracked-task; R switch -q main
mount_fs
TK=$MNT/tk; TASKS=$COMMON/gitfold/.tasks
eq "every workspace links to it" "$TASKS/tk" "$(readlink "$TK/.task")"
echo "writing the parser" >"$TK/.task/status"; echo spec >"$TK/.task/spec.md"; mkdir "$TK/.task/notes"
eq "listed in the workspace root, with .git" ".git .task" "$(ls -A "$TK" | grep '^\.' | xargs)"
eq "stored apart from the changes" spec "$(cat "$TASKS/tk/spec.md")"
echo ext >"$TASKS/tk/ext.md"
eq "a file written outside the mount is there at once" ext "$(cat "$TK/.task/ext.md" 2>&1)"
eq "git doesn't see it" "" "$(git -C "$TK" status --porcelain 2>&1)"
eq "nor changes" "" "$("$BIN" -C "$REPO" changes tk 2>&1)"
eq "excluded once, for every worktree" 1 "$(grep -cx '/.task' "$COMMON/info/exclude")"
eq "list shows its status" "writing the parser" "$("$BIN" -C "$REPO" list 2>&1 | awk '$1 == "tk" { $1=$2=$3=$4=$5=$6=""; sub(/^ +/, ""); print }')"
git -C "$TK" clean -qfdx >/dev/null 2>&1
eq "git clean -fdx leaves it" "spec yes" "$(cat "$TK/.task/spec.md" 2>&1) $(test -L "$TK/.task" && echo yes)"
check "the link can't be removed" fails rm "$TK/.task"
check "nor renamed" fails mv "$TK/.task" "$TK/elsewhere"
echo d >"$TK/draft.md"; mv "$TK/draft.md" "$TK/.task/draft.md"
eq "files move in (copied across: another file system for the mount)" "d 0" "$(cat "$TK/.task/draft.md") $(ls "$TK" | grep -c draft.md)"
eq "a fork copies it" "writing the parser" "$("$BIN" -C "$REPO" branch -w --from tk tk2 >/dev/null 2>&1; cat "$MNT/tk2/.task/status" 2>&1)"
mkdir -p "$TASKS/fk-old"; echo old >"$TASKS/fk-old/status"
out=$("$BIN" -C "$REPO" branch -w --from tk fk-old 2>&1)
eq "a fork onto an earlier branch's folder keeps it, saying so" "yes old" "$(grep -q 'kept the task folder an earlier branch fk-old left' <<<"$out" && echo yes) $(cat "$TASKS/fk-old/status")"
eq "the tracked .task of a base wins: no link there" t "$(cat "$MNT/tracked/.task" 2>&1)"
check "and the daemon says why" grep -q 'tracked: no task folder: its base tracks .task' "$LOG"
echo mine >"$MNT/old/notes2"
unmount_fs
"$BIN" -C "$REPO" remove --force tk >/dev/null
check "remove keeps it" test -f "$TASKS/tk/spec.md"
R config gitfold.taskDir .notes
mount_fs
eq "back when the branch opens again, under the configured name" "writing the parser" "$(cat "$MNT/tk/.notes/status" 2>&1)"
check "the old name is gone" test ! -e "$MNT/tk/.task"
unmount_fs
R config gitfold.taskDir notes2
mount_fs
eq "a workspace with a change of that name keeps its file, no link" "mine no" "$(cat "$MNT/old/notes2" 2>&1) $(test -L "$MNT/old/notes2" && echo yes || echo no)"
unmount_fs
R config gitfold.taskDir a/b; R config gitfold.tasksDir relative/dir
mount_fs
eq "a taskDir that isn't one name, a relative tasksDir: the defaults" "$TASKS/tk3" "$(readlink "$MNT/tk3/.task")"
unmount_fs
R config gitfold.taskDir .task; R config gitfold.tasksDir "$T/tasks"
mount_fs
eq "gitfold.tasksDir: elsewhere" "$T/tasks/tk3" "$(readlink "$MNT/tk3/.task")"
unmount_fs
R config --unset gitfold.taskDir; R config --unset gitfold.tasksDir
for b in tk tk2 tk3 fk-old old tracked; do "$BIN" -C "$REPO" remove --force "$b" >/dev/null 2>&1; done
R branch -q -D tk tk2 tk3 fk-old old tracked
rm -rf "$TASKS" "$T/tasks"

echo "== remove"
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

echo "== prune"
check "snapshot registration exists" sh -c "ls '$COMMON/worktrees' | grep -q '^gitfold-[0-9a-f]\{12\}$'"
check "prune" "$BIN" -C "$REPO" prune
check "snapshot registrations gone" sh -c "! ls '$COMMON/worktrees' 2>/dev/null | grep -q '^gitfold-'"
check "git worktree list is clean" test "$(git -C "$REPO" worktree list | wc -l)" = 1
mount_fs
check "snapshot reopens after prune" test -f "$MNT/@HEAD/a.txt"
check "workspace reopens after remove" test -f "$MNT/agent/one/d/b.txt"
unmount_fs

echo "== deletions the status must see"
sm=$(R rev-parse HEAD)
R update-index --add --cacheinfo "160000,$sm,sm"; R commit -qm submodule
check "branch three" "$BIN" -C "$REPO" branch three
mount_fs
W3=$MNT/three
rm "$W3/a.txt"; mkdir "$W3/a.txt"          # a file replaced by an empty directory
rmdir "$W3/sm"                             # a deleted submodule
rm -r "$W3/d"; echo x >"$W3/d"             # a directory replaced by a file
unmount_fs
out=$("$BIN" -C "$REPO" list 2>/dev/null)
eq "counted (d changed; a.txt, sm, d/b.txt, d/c.txt deleted)" "1 4" \
    "$(echo "$out" | awk '$1 == "three" { print $2, $3 }')"
check "remove refused" fails "$BIN" -C "$REPO" remove three
check "remove --force" "$BIN" -C "$REPO" remove three --force

echo "== git operation stopped half-way"
R branch other main; R checkout -q other; echo theirs >"$REPO/a.txt"; R commit -qam theirs; R checkout -q main
check "branch four" "$BIN" -C "$REPO" branch four
mount_fs
echo mine >"$MNT/four/a.txt"; G "$MNT/four" commit -qam mine
fails G "$MNT/four" merge -q other >/dev/null 2>&1
echo resolved >"$MNT/four/a.txt"           # resolved, not committed: MERGE_HEAD stays
G "$MNT/four" add a.txt
unmount_fs
out=$("$BIN" -C "$REPO" remove four 2>&1)
eq "remove refused mid-merge" yes "$(grep -q 'in progress (MERGE_HEAD)' <<<"$out" && echo yes)"
check "remove --force" "$BIN" -C "$REPO" remove four --force

echo "== leftover state of a deleted branch"
check "branch five" "$BIN" -C "$REPO" branch five
mount_fs; ls "$MNT/five" >/dev/null; unmount_fs
R update-ref -d refs/heads/five            # deleted behind gitfold's back
check "recreating it refused" fails "$BIN" -C "$REPO" branch five
check "remove the leftover" "$BIN" -C "$REPO" remove five --force
check "recreating it works then" "$BIN" -C "$REPO" branch five

echo "== staged-only changes, and the base after a commit"
check "branch six" "$BIN" -C "$REPO" branch six
mount_fs
W6=$MNT/six
echo staged >"$W6/a.txt"; G "$W6" add a.txt; echo a >"$W6/a.txt"   # staged, then edited back
unmount_fs
out=$("$BIN" -C "$REPO" remove six 2>&1)
eq "staged change refused" yes "$(grep -q 'staged changes' <<<"$out" && echo yes)"
mount_fs
G "$W6" reset -q; echo c >>"$W6/a.txt"; G "$W6" commit -qam c   # then unmount at once
unmount_fs
eq "the base caught up at unmount" "$(R rev-parse six)" "$(cat "$COMMON/gitfold/six/base")"
c2=$(R commit-tree "six^{tree}" -p six -m 'moved while unmounted')
R update-ref refs/heads/six "$c2"                                  # base now behind the tip
check "remove catches up offline and removes" "$BIN" -C "$REPO" remove six

echo "== ignored files"
printf 'target/\n*.log\n!keep.log\n' >"$REPO/.gitignore"; echo t >"$REPO/tracked.log"
R add .gitignore; R add -f tracked.log; R commit -qm ignores
check "branch seven" "$BIN" -C "$REPO" branch seven
mount_fs
W7=$MNT/seven
mkdir -p "$W7/target/deep" "$W7/d/gen"
echo o >"$W7/target/deep/a.o"              # in an ignored directory
echo l >"$W7/x.log"; echo l >"$W7/d/y.log" # ignored by pattern, at any depth
echo k >"$W7/keep.log"                     # re-included by a negation
echo g >"$W7/d/gen/out.txt"; echo 'gen/' >"$W7/d/.gitignore"   # a new .gitignore
unmount_fs
out=$("$BIN" -C "$REPO" list 2>/dev/null)
eq "keep.log and d/.gitignore changed; 4 ignored" "2 0 4" \
    "$(echo "$out" | awk '$1 == "seven" { print $2, $3, $4 }')"
check "remove still refused (real changes)" fails "$BIN" -C "$REPO" remove seven
mount_fs
rm "$W7/keep.log" "$W7/d/.gitignore"; rm -r "$W7/d/gen"
echo changed >>"$W7/tracked.log"           # tracked: a change even though *.log
unmount_fs
eq "tracked file matching a pattern is a change" "1 0 3" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "seven" { print $2, $3, $4 }')"
mount_fs
G "$W7" checkout -q -- tracked.log; ls "$W7" >/dev/null
unmount_fs
out=$("$BIN" -C "$REPO" remove seven 2>&1)
eq "only ignored files: removed without --force" yes \
    "$(grep -q 'removed workspace seven (and 3 ignored files' <<<"$out" && echo yes)"
check "branch eight" "$BIN" -C "$REPO" branch eight
mount_fs
echo l >"$MNT/eight/x.log"; rm "$MNT/eight/.gitignore"   # deleting .gitignore un-ignores
unmount_fs
eq "without .gitignore: x.log changed, .gitignore deleted" "1 1 0" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "eight" { print $2, $3, $4 }')"
check "remove eight" "$BIN" -C "$REPO" remove eight --force

echo "== modes"
check "branch nine" "$BIN" -C "$REPO" branch nine
mount_fs; chmod 664 "$MNT/nine/a.txt"; unmount_fs
eq "group-write bit alone: no change (as for git)" "0" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "nine" { print $2 }')"
mount_fs; chmod +x "$MNT/nine/a.txt"; unmount_fs
eq "executable bit: a change" "1" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "nine" { print $2 }')"
check "remove nine" "$BIN" -C "$REPO" remove nine --force

echo "== .gitignore files git doesn't read"
mkdir -p "$REPO/s"; ln -s '*.tmp' "$REPO/s/.gitignore"; R add s 2>/dev/null; R commit -qm 'symlinked gitignore'
check "branch ten" "$BIN" -C "$REPO" branch ten
mount_fs
W10=$MNT/ten
echo t >"$W10/s/x.tmp"                    # base .gitignore is a symlink: not read
mkdir "$W10/u1" "$W10/u2" "$W10/u3"
echo '*.log' >"$T/patterns"; ln -s "$T/patterns" "$W10/u1/.gitignore"   # symlink to a file
ln -s /nonexistent "$W10/u2/.gitignore"   # dangling symlink
mkdir "$W10/u3/.gitignore"                # a directory
for d in u1 u2 u3; do echo l >"$W10/$d/a.log"; done
rm "$W10/.gitignore"; ln -s "$T/patterns" "$W10/.gitignore"   # over the base's: base not used
echo l >"$W10/b.log"
unmount_fs
# changed: s/x.tmp, u1-u3/a.log, u1-u2/.gitignore (u3's is a directory), .gitignore, b.log
eq "nothing ignored through them" "8 0" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "ten" { print $2, $4 }')"
mkdir -p "$T/cwd/git"; echo '*' >"$T/cwd/git/ignore"
eq "empty XDG_CONFIG_HOME is unset, not the current directory" "8 0" \
    "$(cd "$T/cwd" && XDG_CONFIG_HOME= "$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "ten" { print $2, $4 }')"
check "remove ten" "$BIN" -C "$REPO" remove ten --force

echo "== global ignore file as git reads it"
mkdir -p "$XDG_CONFIG_HOME/git"; echo '*.xdg' >"$XDG_CONFIG_HOME/git/ignore"
check "branch eleven" "$BIN" -C "$REPO" branch eleven
mount_fs
echo x >"$MNT/eleven/w.xdg"
mkdir -p "$MNT/eleven/sm2"; ls "$MNT/eleven/sm" >/dev/null
echo '*' >"$MNT/eleven/sm/.gitignore"; echo c >"$MNT/eleven/sm/work.c"   # inside a submodule
unmount_fs
cnt() { "$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "eleven" { print $2, $4 }'; }
eq "XDG ignore file applies; submodule files are changes" "2 1" "$(cnt)"
R config core.excludesFile ''
eq "core.excludesFile set but empty: no global file" "3 0" "$(cnt)"
R config --unset core.excludesFile
R config extensions.worktreeConfig true
git --git-dir "$COMMON/worktrees/gitfold-ws-eleven" config --worktree core.excludesFile /dev/null
eq "worktree config is honoured" "3 0" "$(cnt)"
R config --unset extensions.worktreeConfig
check "remove eleven" "$BIN" -C "$REPO" remove eleven --force
rm "$XDG_CONFIG_HOME/git/ignore"

echo "== nested repositories and a relative excludes file"
check "branch twelve" "$BIN" -C "$REPO" branch twelve
mount_fs
W12=$MNT/twelve
mkdir -p "$W12/nested/.git" "$W12/target/vendored/.git"
echo '*' >"$W12/nested/.gitignore"; echo p >"$W12/nested/precious.txt"   # its own ignores
touch "$W12/nested/.git/HEAD" "$W12/target/vendored/.git/HEAD"         # target/ is ignored
unmount_fs
eq "a nested repository is one untracked entry, or one ignored" "1 1" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "twelve" { print $2, $4 }')"
check "remove refused for the nested repository" fails "$BIN" -C "$REPO" remove twelve
R config core.excludesFile relign
eq "relative core.excludesFile: status refused" yes "$("$BIN" -C "$REPO" list 2>/dev/null | grep "^twelve" | grep -q "relative path" && echo yes)"
R config --unset core.excludesFile
check "remove twelve" "$BIN" -C "$REPO" remove twelve --force

echo "== the lock leaves nothing behind"
git init -q "$T/plain"; git -C "$T/plain" -c user.name=t -c user.email=t@t commit -q --allow-empty -m e
check "prune of a repository without gitfold state" "$BIN" -C "$T/plain" prune
check "creates nothing in it" test ! -e "$T/plain/.git/gitfold"

mkdir -p "$T/plain/.git/gitfold"; touch "$T/plain/.git/gitfold/.lock"
flock "$T/plain/.git/gitfold/.lock" sleep 3 & OLD=$!   # a daemon of an earlier build
sleep 0.3
check "an earlier build's lock file is honoured" fails "$BIN" -C "$T/plain" prune
wait $OLD
check "free again" "$BIN" -C "$T/plain" prune

echo "== background mount and unmount"
check "branch bg" "$BIN" -C "$REPO" branch bg
out=$("$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" 2>&1)
eq "mount returns once mounted" yes "$(mountpoint -q "$MNT" && echo yes)"
eq "and says how to unmount" yes "$(grep -q "gitfold unmount $MNT" <<<"$out" && echo yes)"
check "usable at once" test -f "$MNT/bg/a.txt"
echo bg >>"$MNT/bg/a.txt"; G "$MNT/bg" commit -qam bg      # then unmount at once
check "unmount" "$BIN" unmount "$MNT"
check "unmounted" sh -c "! mountpoint -q '$MNT'"
check "remove right after commit + unmount" "$BIN" -C "$REPO" remove bg
check "branch bg3" "$BIN" -C "$REPO" branch bg3
cd "$MNT"   # the directory under the mount: not a user of it
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
( cd "$MNT/bg3" && exec sleep 5 ) & HOLDER=$!   # like the `git maintenance` a commit starts
for _ in $(seq 100); do [ "$(cat /proc/$HOLDER/comm 2>/dev/null)" = sleep ] && break; sleep 0.02; done
out=$("$BIN" unmount "$MNT" 2>&1)
cd /
eq "unmount names a process using the mount" yes "$(grep -q "sleep 5" <<<"$out" && echo yes)"
eq "and waits until gitfold exits" "unmounted $MNT" "$(tail -1 <<<"$out")"
check "remove right after a busy unmount" "$BIN" -C "$REPO" remove bg3
wait
check "branch bg4" "$BIN" -C "$REPO" branch bg4
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
cp "$BIN" "$MNT/bg4/gitfold-copy"
out=$(timeout 20 "$MNT/bg4/gitfold-copy" unmount "$MNT" 2>&1)
eq "unmount run from inside the mount doesn't wait for itself" yes "$(grep -q 'in use by this command' <<<"$out" && echo yes)"
for _ in $(seq 100); do flock -n "$COMMON" true 2>/dev/null && break; sleep 0.1; done
check "and gitfold exits after it" flock -n "$COMMON" true
check "remove bg4" "$BIN" -C "$REPO" remove bg4 --force
check "branch bg2" "$BIN" -C "$REPO" branch bg2
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
( cd "$MNT/bg2"; "$BIN" unmount "$MNT" >"$T/busy.out" 2>&1; sleep 0.5 )
eq "unmount with a shell inside detaches" yes "$(grep -q 'detached it' "$T/busy.out" && echo yes)"
for _ in $(seq 100); do flock -n "$COMMON" true 2>/dev/null && break; sleep 0.1; done
check "the daemon exits once the shell leaves" flock -n "$COMMON" true
check "a failed background mount reports the error" sh -c "! '$BIN' mount '$T/nonexistent' '$MNT' 2>&1 | grep -q mounted"
"$BIN" mount --foreground "$REPO" "$MNT" >>"$LOG" 2>&1 & FG=$!
for _ in $(seq 100); do mountpoint -q "$MNT" && break; sleep 0.05; done
kill -HUP $FG; wait $FG
check "SIGHUP unmounts a foreground daemon cleanly" sh -c "! mountpoint -q '$MNT'"
check "remove bg2" "$BIN" -C "$REPO" remove bg2 --force

echo "== unmount: paths, odd repository names, the daemon's directory"
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
eq "the daemon doesn't keep the caller's directory" "/" \
    "$(for p in $(pgrep -x gitfold); do grep -qa "$T" /proc/$p/cmdline 2>/dev/null && readlink /proc/$p/cwd; done | sort -u)"
check "unmount with a trailing slash" "$BIN" unmount "$MNT/"
ln -s "$MNT" "$T/mntlink"
"$BIN" mount --log "$T/bg.log" "$REPO" "$T/mntlink" >/dev/null 2>&1
check "mounted through a symlink" mountpoint -q "$MNT"
check "unmount through the symlink" "$BIN" unmount "$T/mntlink"
git clone -q "$REPO" "$T/r,c"; git -C "$T/r,c" branch c1
"$BIN" mount --log "$T/bg.log" "$T/r,c" "$MNT" >/dev/null 2>&1
echo c >>"$MNT/c1/a.txt"; G "$MNT/c1" commit -qam c
check "unmount of a repository with a comma in its path" "$BIN" unmount "$MNT"
check "remove right after it" "$BIN" -C "$T/r,c" remove c1
( cd "$T" && "$BIN" mount --log "$T/bg.log" repo mnt >/dev/null 2>&1 )
check "a relative repository path works" test -f "$MNT/@HEAD/a.txt"
check "unmount" "$BIN" unmount "$MNT"
check "arguments after -- work" "$BIN" mount --log "$T/bg.log" -- "$REPO" "$MNT"
check "unmount" "$BIN" unmount "$MNT"

echo "== names that appear later, and negative entries"
mount_fs
R branch ns/a
ls "$MNT" >/dev/null; ls "$MNT/ns" >/dev/null
check "missing branch absent" test ! -e "$MNT/later"
check "missing rev absent" test ! -e "$MNT/@later-tag"
check "missing namespaced branch absent" test ! -e "$MNT/ns/b"
R branch later; R tag later-tag; R branch ns/b
check "branch created afterwards opens" test -f "$MNT/later/a.txt"
check "tag created afterwards opens" test -f "$MNT/@later-tag/a.txt"
check "root listing shows them" sh -c "ls '$MNT' | grep -qx later && ls '$MNT' | grep -qx @later-tag"
check "namespace listing shows a new branch" sh -c "test -d '$MNT/ns/b' && ls '$MNT/ns' | grep -qx b"
check "dot names stay absent in the root" test ! -e "$MNT/.git"
check "missing file in a workspace" test ! -e "$MNT/later/new.txt"
echo n >"$MNT/later/new.txt"
check "created after its negative lookup" test -f "$MNT/later/new.txt"
check "missing file in a snapshot stays missing" test ! -e "$MNT/@later-tag/.ignore"
unmount_fs
check "remove later" "$BIN" -C "$REPO" remove later --force
check "remove ns/b" "$BIN" -C "$REPO" remove ns/b --force

echo "== the index's cache tree (TREE extension)"
# A submodule, a symlink, an executable and an empty directory; git's own
# `read-tree` writes the cache tree to compare with.
(cd "$REPO" && mkdir -p t/u/v && echo v >t/u/v/f && ln -s a.txt t/link && echo x >t/x && chmod +x t/x)
R add -A; R commit -qm tree; R update-index --add --cacheinfo "160000,$(R rev-parse HEAD),t/sub"; R commit -qm sub
EMPTY=$(R hash-object -w -t tree /dev/null)
R branch cachetree "$(R commit-tree "$( (R ls-tree HEAD; printf '040000 tree %s\tempty\n' "$EMPTY") | R mktree)" -p HEAD -m empty)"
mount_fs
for v in @cachetree cachetree; do
    check "$v opens" test -e "$MNT/$v/a.txt"
    GIT_INDEX_VERSION=4 GIT_INDEX_FILE=$T/git.idx R read-tree "$(G "$MNT/$v" rev-parse HEAD^{tree})"
    eq "$v: cache tree as git writes it" "$(perl "$CACHE_TREE" "$T/git.idx")" \
        "$(perl "$CACHE_TREE" "$(G "$MNT/$v" rev-parse --path-format=absolute --git-path index)")"
done
eq "it has every directory" 7 "$(perl "$CACHE_TREE" "$T/git.idx" | wc -l)"
(cd "$MNT/cachetree" && echo w >t/u/w && echo d >>d/b.txt && G . add -A && G . commit -qm w)
R worktree add -q "$T/plain-ct" cachetree~1
(cd "$T/plain-ct" && echo w >t/u/w && echo d >>d/b.txt && G . add -A && G . commit -qm w)
eq "a commit builds the trees git would" "$(G "$T/plain-ct" rev-parse HEAD^{tree})" "$(G "$MNT/cachetree" rev-parse HEAD^{tree})"
# A tree object stored out of order (git never writes one): its node is left
# for git to work out, since the index entries come sorted.
BLOB=$(R rev-parse HEAD:a.txt)
UNSORTED=$(perl -e 'print map { "100644 $_\0" . pack("H*", $ARGV[0]) } qw(b a)' "$BLOB" | R hash-object -t tree --literally -w --stdin)
R branch unsorted "$(R commit-tree "$( (R ls-tree HEAD; printf '040000 tree %s\tuns\n' "$UNSORTED") | R mktree)" -m unsorted)"
check "a snapshot with an unsorted tree opens" test -f "$MNT/@unsorted/uns/a"
eq "its cache tree node is left to git" "/uns -1 0 " \
    "$(perl "$CACHE_TREE" "$(G "$MNT/@unsorted" rev-parse --path-format=absolute --git-path index)" | grep "^/uns ")"
eq "git status is clean there" "" "$(G "$MNT/@unsorted" status --porcelain)"
unmount_fs
R worktree remove --force "$T/plain-ct"
check "remove cachetree" "$BIN" -C "$REPO" remove cachetree --force

echo "== remove while mounted"
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
R branch -q -D live1 live2 nsx/live3 live4 live5 many

echo "== many packs added while mounted"
# The object database's pack index table was sized at open and never grew.
mount_fs
test -f "$MNT/@main/a.txt"
prev=$(R rev-parse main)
for i in $(seq 100); do
    blob=$(echo "$i" | R hash-object -w --stdin)
    tree=$(printf '100644 blob %s\tn.txt\n' "$blob" | R mktree)
    prev=$(R commit-tree "$tree" -p "$prev" -m "pack $i")
    printf '%s\n' "$prev" "$tree" "$blob" | R pack-objects -q "$COMMON/objects/pack/pack" >/dev/null
    R prune-packed
done
R branch packs "$prev"
eq "a commit in the 100th new pack opens" 100 "$(cat "$MNT/@packs/n.txt" 2>&1)"
unmount_fs
R branch -q -D packs
R repack -qad

echo "== prewarm: walks at once, per-workspace opt-out"
PW=$T/pwrepo
git init -q "$PW"; mkdir "$PW/d"
for i in $(seq 5000); do echo $i >"$PW/d/f$i"; done
G "$PW" add -A; G "$PW" commit -qm many
for b in pw1 pw2 pw3; do G "$PW" branch $b; done
check "branch --prewarm none" "$BIN" -C "$PW" branch pw4 --prewarm none
eq "kept in the branch's git config" none "$(G "$PW" config --get branch.pw4.gitfoldPrewarm)"
"$BIN" mount --prewarm-walks 1 --log "$T/pw.log" "$PW" "$MNT" >/dev/null 2>&1
for b in pw1 pw2 pw3 pw4; do cat "$MNT/$b/d/f1" >/dev/null & done; wait
for _ in $(seq 200); do [ "$(grep -c 'prewarm done' "$T/pw.log")" -ge 3 ] && break; sleep 0.05; done
sleep 0.3
# Starts and ends alternate: never two walks at once.
eq "one walk at a time" "SDSDSD" "$(grep -oE 'prewarm (of [^:]*: started|done for)' "$T/pw.log" | sed 's/.*started/S/; s/.*done for/D/' | tr -d '\n')"
eq "each workspace walked once" "pw1 pw2 pw3" "$(grep -o 'prewarm done for [^:]*' "$T/pw.log" | sed 's|.*/||' | sort | xargs)"
check "a workspace that opted out isn't walked" sh -c "! grep -q 'mnt/pw4' '$T/pw.log'"
check "unmount" "$BIN" unmount "$MNT"
# Slow walks, one at a time: an unmount must not start the queued ones.
"$BIN" mount --prewarm-walks 1 --prewarm content --prewarm-threads 1 --log "$T/pw2.log" "$PW" "$MNT" >/dev/null 2>&1
for b in pw1 pw2 pw3; do cat "$MNT/$b/d/f1" >/dev/null & done; wait
started=$(grep -c ': started' "$T/pw2.log")
eq "unmount with walks queued unmounts (doesn't detach)" "unmounted $MNT" "$("$BIN" unmount "$MNT" 2>&1 | tail -1)"
eq "no walk started once it began" "$started" "$(grep -c ': started' "$T/pw2.log")"
check "--prewarm-walks 0 refused" fails "$BIN" mount --foreground --prewarm-walks 0 "$PW" "$MNT"
# A walker killed from outside (an older `gitfold unmount`, a user) drops the
# queue, but later opens are still walked.
"$BIN" mount --prewarm content --prewarm-threads 1 --prewarm-walks 1 --log "$T/pw3.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw1/d/f1" "$MNT/pw2/d/f1" >/dev/null
WALKER=
for _ in $(seq 100); do
    WALKER=$(ps -eo pid=,args= | awk -v r="$MNT/pw1" '$2 == "gitfold" && $3 == "prewarm-walk" && $4 == r { print $1 }')
    [ -n "$WALKER" ] && break; sleep 0.02
done
check "pw1's walker found" test -n "$WALKER"
kill -TERM $WALKER 2>/dev/null
for _ in $(seq 100); do grep -q "stopped from outside" "$T/pw3.log" && break; sleep 0.05; done
check "its death taken for an outside stop" grep -q "stopped from outside" "$T/pw3.log"
cat "$MNT/pw3/d/f1" >/dev/null
for _ in $(seq 200); do grep -q "prewarm done for $MNT/pw3" "$T/pw3.log" && break; sleep 0.05; done
check "the queued walk dropped" sh -c "! grep -q 'prewarm of $MNT/pw2: started' '$T/pw3.log'"
check "later opens still walked" grep -q "prewarm done for $MNT/pw3" "$T/pw3.log"
check "unmount" "$BIN" unmount "$MNT"
G "$PW" config branch.pw1.gitfoldPrewarm content
"$BIN" mount --prewarm none --log "$T/pw4.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw1/d/f1" >/dev/null; sleep 0.5
check "--prewarm none for the mount wins over a branch's mode" sh -c "! grep -q 'prewarm of' '$T/pw4.log'"
check "unmount" "$BIN" unmount "$MNT"
echo "== gitfold prewarm: a mode applied to an open workspace"
"$BIN" mount --log "$T/pw5.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw4/d/f1" >/dev/null; sleep 0.5
check "pw4 (none) opened unwalked" sh -c "! grep -q 'prewarm of $MNT/pw4' '$T/pw5.log'"
eq "prewarm meta: walks it at once" "walking $MNT/pw4 (meta)" "$("$BIN" -C "$PW" prewarm pw4 meta 2>&1)"
for _ in $(seq 200); do grep -q "prewarm done for $MNT/pw4" "$T/pw5.log" && break; sleep 0.05; done
check "walked" grep -q "prewarm done for $MNT/pw4" "$T/pw5.log"
eq "and kept in git config" meta "$(G "$PW" config --get branch.pw4.gitfoldPrewarm)"
eq "a workspace not open: walked when it opens" "pw3 isn't open: walked when it opens" "$("$BIN" -C "$PW" prewarm pw3 meta 2>&1)"
check "an unknown branch refused" fails "$BIN" -C "$PW" prewarm nope meta
check "unmount" "$BIN" unmount "$MNT"
eq "unmounted: only set" "set; pw4 isn't walked" "$("$BIN" -C "$PW" prewarm pw4 none 2>&1)"
eq "set" none "$(G "$PW" config --get branch.pw4.gitfoldPrewarm)"
G "$PW" config --unset branch.pw3.gitfoldPrewarm

echo "== an interrupted unmount: prewarm resumes, stopped walks too"
mkdir -p "$T/fakebin"; printf '#!/bin/sh\nexec sleep 30\n' >"$T/fakebin/fusermount3"; chmod +x "$T/fakebin/fusermount3"
GITFOLD_PREWARM_RESUME_SECS=2 "$BIN" mount --prewarm content --prewarm-threads 1 --prewarm-walks 1 \
    --log "$T/pw6.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw1/d/f1" "$MNT/pw2/d/f1" >/dev/null
for _ in $(seq 100); do grep -q "prewarm of $MNT/pw1: started" "$T/pw6.log" && break; sleep 0.02; done
setsid env PATH="$T/fakebin:$PATH" "$BIN" unmount "$MNT" >/dev/null 2>&1 & UM=$!
for _ in $(seq 100); do grep -q "prewarm stopped for the unmount" "$T/pw6.log" && break; sleep 0.05; done
check "pw1's walk was running when it stopped" sh -c "! grep -q 'prewarm done for $MNT/pw1' '$T/pw6.log'"
kill -KILL -- -$UM; wait $UM 2>/dev/null
check "the unmount killed before unmounting" mountpoint -q "$MNT"
for _ in $(seq 200); do grep -q "prewarm done for $MNT/pw2" "$T/pw6.log" && break; sleep 0.05; done
check "prewarm resumed by itself" grep -q "prewarm resumed: still mounted 2 s after an unmount stopped it" "$T/pw6.log"
check "the stopped walk restarted" grep -q "prewarm done for $MNT/pw1" "$T/pw6.log"
check "the queued one too" grep -q "prewarm done for $MNT/pw2" "$T/pw6.log"
check "unmount" "$BIN" unmount "$MNT"
for b in pw1 pw2 pw3 pw4; do "$BIN" -C "$PW" remove "$b" >/dev/null; done
G "$PW" branch -q -D pw4
check "the opt-out goes with the branch" fails G "$PW" config --get branch.pw4.gitfoldPrewarm

echo "== unmount of a stopped daemon"
mount_fs
kill -STOP $PID
timeout 10 "$BIN" unmount "$MNT" >/dev/null 2>&1
check "unmounts without the daemon's answer" sh -c "! mountpoint -q '$MNT'"
kill -CONT $PID; wait $PID 2>/dev/null

echo "== remove: exit codes, --check, --wait"
R branch ec1; R branch ec2
code() { "$@" >/dev/null 2>&1; echo $?; }
eq "not found: exit code 3" 3 "$(code "$BIN" -C "$REPO" remove nope)"
eq "--check of a missing workspace: 3" 3 "$(code "$BIN" -C "$REPO" remove nope --check)"
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
R branch -q -D ec1 ec2

echo "== version"
eq "--version names the build" yes "$("$BIN" --version | grep -qE '^gitfold [0-9]+\.[0-9]+\.[0-9]+' && echo yes)"
mount_fs
out=$("$BIN" version "$MNT" 2>&1)
eq "the daemon runs the same build" "$(sed -n "s/^gitfold //p" <<<"$out")" "$(sed -n "s/^daemon //p" <<<"$out")"
check "version of a mount exits 0" "$BIN" version "$MNT"
check "version of a non-mount fails" fails "$BIN" version "$T"
check "the daemon logged its version" grep -q "gitfold .*: mounting" "$LOG"
kill -STOP $PID
rc=$("$BIN" version "$MNT" >/dev/null 2>&1; echo $?)
kill -CONT $PID
eq "a daemon that doesn't answer: exit code 5 (not a mismatch)" 5 "$rc"
unmount_fs

echo "== an unmount that fails lets prewarm resume"
R branch rs1; R branch rs2
mkdir -p "$T/fakebin"
printf '#!/bin/sh\necho "fusermount3: simulated failure" >&2\nexit 1\n' >"$T/fakebin/fusermount3"
chmod +x "$T/fakebin/fusermount3"
"$BIN" mount --log "$T/resume.log" "$REPO" "$MNT" >/dev/null 2>&1
cat "$MNT/rs1/a.txt" >/dev/null
check "the unmount fails" fails env PATH="$T/fakebin:$PATH" "$BIN" unmount "$MNT"
check "and the mount stays" mountpoint -q "$MNT"
cat "$MNT/rs2/a.txt" >/dev/null
for _ in $(seq 100); do grep -q "prewarm done for $MNT/rs2" "$T/resume.log" && break; sleep 0.05; done
check "prewarm resumed" grep -q "prewarm resumed" "$T/resume.log"
check "a workspace opened afterwards is walked" grep -q "prewarm done for $MNT/rs2" "$T/resume.log"
check "unmount" "$BIN" unmount "$MNT"
for b in rs1 rs2; do "$BIN" -C "$REPO" remove "$b" >/dev/null; done
R branch -q -D rs1 rs2

echo "== -C for mount: relative paths from there"
(cd "$T" && "$BIN" -C repo mount . ../mnt >/dev/null 2>&1)
check "mounted in the background through -C" mountpoint -q "$MNT"
check "and serves the repository" test -d "$MNT/@HEAD"
check "unmount" "$BIN" unmount "$MNT"

echo "== $FAILS failure(s)"
[ -n "${KEEP:-}" ] || rm -rf "$T"
exit $((FAILS > 0))
