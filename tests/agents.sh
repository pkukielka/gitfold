#!/usr/bin/env bash
# End-to-end test of the commands for running agents: overlaps, the task
# folder, inbox, send, watch.
# usage: tests/agents.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=agents
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# The paths workspace $1 recorded as read, space-separated.
reads_of() { tr '\0' ' ' <"$COMMON/gitfold/$1/reads" 2>/dev/null | sed 's/ $//'; }

overlaps_reads_and_writes_changed_elsewh() {
R branch st1; R branch st2; R branch st3
check "track-reads needs a branch" fails "$BIN" -C "$REPO" track-reads nope on
mount_fs
eq "track-reads on before it opens" "st1 isn't open: applied when it opens" "$("$BIN" -C "$REPO" track-reads st1 on 2>&1)"
cat "$MNT/st1/a.txt" "$MNT/st1/d/b.txt" >/dev/null
echo w >>"$MNT/st1/d/c.txt"; echo x >"$MNT/st1/x.txt"; echo nd >"$MNT/st1/nd"
echo n >"$MNT/st1/n.txt"; G "$MNT/st1" add n.txt; G "$MNT/st1" commit -qm n -- n.txt
for _ in $(seq 50); do [ "$(cat "$COMMON/gitfold/st1/base")" = "$(R rev-parse st1)" ] && break; sleep 0.1; done
eq "reads recorded once each, in order (git's read of n.txt too)" "a.txt d/b.txt n.txt" "$(reads_of st1)"
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
R branch other st2; echo o >>"$MNT/other/a.txt"
out=$("$BIN" -C "$REPO" overlaps st1 --against-workspaces 2>&1)
eq "--against-workspaces: every other one" "W	st2	nd
W	st2	x.txt" "$(grep st2 <<<"$out")"
eq "another workspace changed a file st1 read; not main" "yes no" "$(grep -q 'R	other	a.txt' <<<"$out" && echo yes) $(grep -q main <<<"$out" && echo yes || echo no)"
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
printf '[branch "st3"]\n\tgitfoldTrackReads\n' >>"$COMMON/config"
"$BIN" -C "$REPO" branch -w --from st3 st3f >/dev/null 2>&1
eq "a valueless gitfoldTrackReads (true, for git) is copied as true" true "$(R config --get branch.st3f.gitfoldTrackReads)"
eq "track-reads off, applied at once" "stopped recording reads in st1" "$("$BIN" -C "$REPO" track-reads st1 off 2>&1)"
cat "$MNT/st1/d/c.txt" >/dev/null
eq "nothing more recorded; what was recorded stays" "a.txt d/b.txt n.txt" "$(reads_of st1)"
unmount_fs
eq "unmounted, the same" "R	main	a.txt
W	main	d/c.txt
W	main	n.txt" "$("$BIN" -C "$REPO" overlaps st1 --against main 2>/dev/null)"
eq "an unknown branch: exit 3" 3 "$("$BIN" -C "$REPO" overlaps nope >/dev/null 2>&1; echo $?)"
mount_fs --track-reads --prewarm content
cat "$MNT/st2/a.txt" "$MNT/st1/a.txt" >/dev/null
eq "the mount's --track-reads: st2 records" "R	main	a.txt" "$("$BIN" -C "$REPO" overlaps st2 --reads 2>&1)"
eq "a branch's off wins" "a.txt d/b.txt n.txt" "$(reads_of st1)"
for _ in $(seq 50); do grep -q 'st2: prewarm content walks as meta' "$LOG" && break; sleep 0.1; done
check "a tracked workspace's content prewarm walks as meta" grep -q 'st2: prewarm content walks as meta' "$LOG"
unmount_fs
}

the_task_folder() {
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
echo secret >"$TASKS/tk/private"; chmod 000 "$TASKS/tk/private"
"$BIN" -C "$REPO" branch -w --from tk tk4 >/dev/null 2>&1
eq "a fork copies the folder but a file it can't read, which it leaves as it is" "status no 0" \
    "$(ls "$TASKS/tk4" | grep -x status) $(test -e "$TASKS/tk4/private" && echo copied || echo no) $(stat -c %a "$TASKS/tk/private")"
chmod 600 "$TASKS/tk/private"; rm "$TASKS/tk/private"
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
}

inbox_send_watch() {
R branch ib; R branch ib2
IB=$COMMON/gitfold/.tasks/ib
out=$("$BIN" -C "$REPO" send nope hi 2>&1); rc=$?
eq "send to no branch: exit 3" 3 "$rc"
eq "send, unmounted, to a branch never opened" "sent to ib" "$("$BIN" -C "$REPO" send ib --from ib2 -s "lexer API" I changed it 2>&1)"
echo "a longer
body" | "$BIN" -C "$REPO" send ib - >/dev/null
eq "--peek lists, taking nothing" "ib2	lexer API
user	a longer" "$("$BIN" -C "$REPO" inbox ib --peek 2>&1)"
out=$("$BIN" -C "$REPO" inbox ib 2>&1)
eq "inbox takes them, oldest first, as sent" "from: ib2
subject: lexer API

I changed it

from: user

a longer
body" "$(grep -v '^time: ' <<<"$out")"
check "with a time header" grep -qE '^time: [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$' <<<"$out"
eq "taken once: gone after" "" "$("$BIN" -C "$REPO" inbox ib 2>&1)"
eq "an empty inbox: --peek prints nothing" "" "$("$BIN" -C "$REPO" inbox ib --peek 2>&1)"
start=$(date +%s%N)
eq "--wait --timeout with nothing: nothing, after the wait" "" "$("$BIN" -C "$REPO" inbox ib --wait --timeout 1 2>&1)"
eq "  (about a second)" yes "$( (( ($(date +%s%N) - start) / 1000000 >= 900 )) && echo yes || echo no)"
"$BIN" -C "$REPO" inbox ib --wait --timeout 10 >"$T/waited" 2>&1 & WP=$!
sleep 0.3; start=$(date +%s%N)
"$BIN" -C "$REPO" send ib wake up >/dev/null; wait $WP
eq "--wait returns with the message as it arrives" "wake up" "$(tail -1 "$T/waited")"
eq "  (at once)" yes "$( (( ($(date +%s%N) - start) / 1000000 < 2000 )) && echo yes || echo no)"
"$BIN" -C "$REPO" inbox ib --wait --timeout 10 >"$T/waited" 2>&1 & WP=$!
sleep 0.3; rm -rf "$IB/inbox"; sleep 0.3
"$BIN" -C "$REPO" send ib after the inbox was deleted >/dev/null; wait $WP
eq "a waiter whose inbox directory is deleted watches the new one" "after the inbox was deleted" "$(tail -1 "$T/waited")"
printf 'from: x\r\nsubject: crlf\r\n\r\nbody\r\n' >"$IB/inbox/0001-x"; printf 'Hello\n\nworld\n' >"$IB/inbox/0002-y"
eq "a CRLF message; one without headers, all body" "x	crlf
	Hello" "$("$BIN" -C "$REPO" inbox ib --peek 2>&1)"
"$BIN" -C "$REPO" inbox ib >/dev/null
echo "from: z

left" >"$IB/inbox/.taken-999999-0003-z"
eq "a message a dead reader took is put back" "from: z

left" "$("$BIN" -C "$REPO" inbox ib 2>&1)"
"$BIN" -C "$REPO" send ib for ib only >/dev/null
"$BIN" -C "$REPO" branch -w --from ib ibf >/dev/null 2>&1
eq "a fork leaves the inbox behind" "no 1" "$(test -e "$COMMON/gitfold/.tasks/ibf/inbox" && echo yes || echo no) $("$BIN" -C "$REPO" inbox ib --peek | wc -l)"
eq "the post-tool-use hook skips a subagent's tool calls" "" "$(echo '{"agent_id":"a1","hook_event_name":"PostToolUse"}' | "$BIN" -C "$REPO" inbox ib --claude-hook post-tool-use 2>&1)"
eq "  (and leaves the message)" 1 "$("$BIN" -C "$REPO" inbox ib --peek | wc -l)"
"$BIN" -C "$REPO" inbox ib >/dev/null
for i in $(seq 40); do "$BIN" -C "$REPO" send ib "m$i" >/dev/null; done
"$BIN" -C "$REPO" inbox ib >"$T/r1" 2>&1 & "$BIN" -C "$REPO" inbox ib >"$T/r2" 2>&1 & wait
eq "two readers at once: each message to one of them" "40 40" "$(cat "$T/r1" "$T/r2" | grep -c '^m[0-9]') $(cat "$T/r1" "$T/r2" | grep '^m[0-9]' | sort -u | wc -l)"
"$BIN" -C "$REPO" send ib -s 'q"x' 'tab	here' >/dev/null
eq "--json" '{"file":"X","headers":{"from":"user","subject":"q\"x"},"body":"tab\there\n"}' "$("$BIN" -C "$REPO" inbox ib --json 2>&1 | sed -E 's/"file":"[^"]*"/"file":"X"/; s/,?"time":"[^"]*"//')"
"$BIN" -C "$REPO" watch ib:notes --timeout 10 >"$T/watched" 2>&1 & WP=$!
sleep 0.3; echo n >"$IB/notes/a.md"; echo m >"$IB/notes/b.md"; wait $WP
eq "watch: what changed in a burst, then exits" "C	a.md
C	b.md" "$(cat "$T/watched")"
"$BIN" -C "$REPO" watch ib:notes --timeout 10 >"$T/watched" 2>&1 & WP=$!
sleep 0.3; rm "$IB/notes/a.md"; wait $WP
eq "watch: a deletion" "D	a.md" "$(cat "$T/watched")"
echo h >"$IB/notes/handoff.md"
"$BIN" -C "$REPO" watch ib:notes --timeout 10 >"$T/watched" 2>&1 & WP=$!
sleep 0.3; echo new >"$IB/notes/.tmp"; mv "$IB/notes/.tmp" "$IB/notes/handoff.md"; rm "$IB/notes/b.md"; wait $WP
eq "watch: each name's last change (a file replaced: changed)" "D	b.md
C	handoff.md" "$(cat "$T/watched")"
check "watch refuses a file" fails "$BIN" -C "$REPO" watch ib:notes/handoff.md --timeout 1
eq "watch --timeout: nothing" "" "$("$BIN" -C "$REPO" watch ib:notes --timeout 1 2>&1)"
check "watch refuses .." fails "$BIN" -C "$REPO" watch ib:../x --timeout 1
eq "--claude-hook outside a workspace: silent, exit 0" "0" "$(cd / && "$BIN" inbox --claude-hook post-tool-use 2>&1; echo $?)"
eq "--claude-hook, nothing new: silent" "" "$("$BIN" -C "$REPO" inbox ib --claude-hook user-prompt-submit 2>&1; "$BIN" -C "$REPO" inbox ib --claude-hook post-tool-use 2>&1)"
"$BIN" -C "$REPO" send ib -s s1 one >/dev/null
eq "--claude-hook user-prompt-submit: the messages, introduced" "yes yes" "$("$BIN" -C "$REPO" inbox ib --claude-hook user-prompt-submit | grep -c -e '^New messages in your gitfold inbox' -e '^one$' | sed 's/^2$/yes yes/')"
"$BIN" -C "$REPO" send ib 'two "quoted"' >/dev/null
eq "--claude-hook post-tool-use: additionalContext JSON" yes "$("$BIN" -C "$REPO" inbox ib --claude-hook post-tool-use | grep -q '^{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"New messages.*two \\"quoted\\"\\n"}}$' && echo yes)"
"$BIN" -C "$REPO" inbox ib --claude-hook rewake >"$T/rw.out" 2>"$T/rw.err" & WP=$!
sleep 0.3
start=$(date +%s%N)
eq "a second rewake waiter exits at once (one per inbox)" "0" "$("$BIN" -C "$REPO" inbox ib --claude-hook rewake >/dev/null 2>&1; echo $?)"
eq "  (at once)" yes "$( (( ($(date +%s%N) - start) / 1000000 < 1000 )) && echo yes || echo no)"
"$BIN" -C "$REPO" send ib wake >/dev/null; wait $WP; rc=$?
eq "--claude-hook rewake: exit 2, the message on stderr" "2 yes" "$rc $(grep -qx wake "$T/rw.err" && echo yes)"
mount_fs
(cd "$MNT/ib2" && "$BIN" watch . --timeout 10 >"$T/watched" 2>&1) & WP=$!
sleep 0.3; echo s >"$MNT/ib2/.task/status"; wait $WP
eq "in a workspace: its own folder; writes through the link seen" "C	status" "$(cat "$T/watched")"
eq "send from a workspace: from it" "from: ib2" "$(cd "$MNT/ib2" && "$BIN" send ib hi >/dev/null && "$BIN" inbox ib | head -1)"
unmount_fs
R config gitfold.inbox mail
"$BIN" -C "$REPO" send ib configured >/dev/null
check "gitfold.inbox names it" test -n "$(ls "$IB/mail")"
}

claude_code_worktree_hooks() {
create() { printf '{"session_id":"s","cwd":"%s","hook_event_name":"WorktreeCreate","name":"%s"}' "$1" "$2" | "$BIN" branch --claude-hook worktree-create; }
# As Claude Code runs it: the hook itself is the process in the worktree (exec,
# no timeout wrapper, which would stay in it).
remove() { exec "$REAL_BIN" remove --claude-hook worktree-remove <<<"{\"session_id\":\"s\",\"hook_event_name\":\"WorktreeRemove\",\"worktree_path\":\"$1\"}"; }
check "--claude-hook takes no branch" fails "$BIN" -C "$REPO" branch x --claude-hook worktree-create
out=$(cd / && create "$REPO/d" w1 2>"$T/err")
eq "not used with gitfold: a git worktree, as Claude Code makes, its path alone on stdout" "$REPO/.claude/worktrees/w1 worktree-w1" "$out $(G "$out" rev-parse --abbrev-ref HEAD 2>&1)"
eq "  again: the same one" "$out" "$(create "$REPO" w1 2>/dev/null)"
check "  not left as a gitfold repository" test ! -e "$COMMON/gitfold"
(cd "$out/d" && remove "$out" >/dev/null 2>&1)
eq "  removed from inside it, its branch kept" "no yes" "$([ -e "$out" ] && echo yes || echo no) $(R rev-parse -q --verify worktree-w1 >/dev/null && echo yes)"
R branch st
mount_fs
out=$(create "$REPO" w2 2>"$T/err"); rc=$?
eq "mounted: a workspace of worktree-w2, its path alone on stdout" "0 $MNT/worktree-w2 worktree-w2" "$rc $out $(G "$out" rev-parse --abbrev-ref HEAD 2>&1)"
eq "  again: the same one" "$out" "$(create "$REPO" w2 2>/dev/null)"
eq "named like a branch: still its own (worktree-main)" "$MNT/worktree-main" "$(create "$REPO" main 2>/dev/null)"
echo s >"$MNT/st/s.txt"; G "$MNT/st" add s.txt; G "$MNT/st" commit -qm s
eq "from a session in a workspace: from its HEAD" "$(R rev-parse st)" "$(G "$(create "$MNT/st/d" w3 2>/dev/null)" rev-parse HEAD)"
echo x >"$MNT/worktree-w2/x.txt"
(cd "$MNT/worktree-w2/d" && remove "$MNT/worktree-w2" >/dev/null 2>"$T/err"); rc=$?
eq "removed from inside it, changes discarded, its branch kept" "0 no yes" "$rc $(R worktree list | grep -q worktree-w2 && echo yes || echo no) $(R rev-parse -q --verify worktree-w2 >/dev/null && echo yes)"
eq "  a worktree already gone: done" "0" "$( (remove "$MNT/worktree-w2" >/dev/null 2>&1); echo $?)"
unmount_fs
eq "gitfold mount recorded where" "$MNT" "$(R config gitfold.mountpoint)"
rmdir "$MNT"
out=$(create "$REPO" w4 2>"$T/err"); rc=$?
eq "not mounted: mounted again there (made again, as a tmpfs after a reboot), then a workspace" "0 $MNT/worktree-w4 yes" "$rc $out $(mountpoint -q "$MNT" && echo yes)"
"$BIN" unmount "$MNT" >/dev/null
mount_fs; kill -9 "$PID"; wait "$PID" 2>/dev/null
out=$(create "$REPO" w6 2>"$T/err"); rc=$?
eq "a mount whose daemon is gone: refused, saying so, no branch made" "1 yes no" "$rc $(grep -q "doesn't answer" "$T/err" && echo yes) $(R rev-parse -q --verify worktree-w6 >/dev/null && echo yes || echo no)"
fusermount3 -u -z "$MNT"
R config --unset gitfold.mountpoint
out=$(create "$REPO" w5 2>"$T/err"); rc=$?
eq "  where no mount was recorded: refused, nothing made" "1  yes no" "$rc $out $(grep -q 'not mounted' "$T/err" && echo yes) $(R rev-parse -q --verify worktree-w5 >/dev/null && echo yes || echo no)"
}

the_plugins_launcher() {
L=$(realpath "$(dirname "$0")/../bin/gitfold")
export XDG_DATA_HOME=$T/data
nogf=$(echo "$PATH" | tr ':' '\n' | while read -r d; do [ -x "$d/gitfold" ] || printf '%s:' "$d"; done)
want=$("$REAL_BIN" --version)
mkdir "$T/pbin"; ln -s "$REAL_BIN" "$T/pbin/gitfold"
eq "a gitfold on PATH" "$want" "$(PATH=$T/pbin:$nogf "$L" --version 2>&1)"
eq "GITFOLD_BIN first" "$want" "$(GITFOLD_BIN=$REAL_BIN PATH=$nogf "$L" --version 2>&1)"
mkdir -p "$T/l2/bin"; cp "$L" "$T/l2/bin/"; cp "$(dirname "$L")/../Cargo.toml" "$T/l2/"
eq "another launcher on PATH: skipped by both, not run in turn" "$want" "$(PATH=$T/l2/bin:$(dirname "$L"):$T/pbin:$nogf timeout 10 "$L" --version 2>&1)"
v=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$(dirname "$L")/../Cargo.toml" | head -n 1)
case "$(uname -m)" in aarch64 | arm64) a=aarch64 ;; *) a=x86_64 ;; esac
mkdir -p "$T/rel/v$v"; cp "$REAL_BIN" "$T/rel/v$v/gitfold-$a-linux"
(cd "$T/rel/v$v" && sha256sum "gitfold-$a-linux" >SHA256SUMS)
export GITFOLD_RELEASE_URL=file://$T/rel
eq "none on PATH (itself skipped): the release, downloaded" "$want yes" "$(PATH=$(dirname "$L"):$nogf "$L" --version 2>&1) $([ -x "$T/data/gitfold/bin/gitfold-$v-$a" ] && echo yes)"
rm -rf "$T/data"; echo "0000  gitfold-$a-linux" >"$T/rel/v$v/SHA256SUMS"
out=$(PATH=$nogf "$L" --version 2>&1); rc=$?
eq "a checksum that doesn't match: no binary, saying why" "1 yes yes" "$rc $(grep -q "doesn't match" <<<"$out" && echo yes) $([ -e "$T/data/gitfold/bin/gitfold-$v-$a" ] && echo no || echo yes)"
(cd "$T/rel/v$v" && sha256sum "gitfold-$a-linux" >SHA256SUMS)
eq "  not tried again within a day" "1" "$(PATH=$nogf "$L" --version >/dev/null 2>&1; echo $?)"
out=$(echo '{}' | PATH=$nogf "$L" inbox --claude-hook post-tool-use 2>&1); rc=$?
eq "  without gitfold, the inbox hooks: nothing" "0 " "$rc $out"
G "$T" init -q plain; G "$T/plain" commit -q --allow-empty -m init
out=$(printf '{"hook_event_name":"WorktreeCreate","cwd":"%s","name":"p1"}' "$T/plain" | PATH=$nogf "$L" branch --claude-hook worktree-create 2>/dev/null)
eq "  the worktree hooks: a git worktree, as Claude Code makes" "$T/plain/.claude/worktrees/p1 worktree-p1" "$out $(G "$out" rev-parse --abbrev-ref HEAD 2>&1)"
(cd "$out" && exec env PATH="$nogf" "$L" remove --claude-hook worktree-remove <<<"{\"worktree_path\":\"$out\"}" >/dev/null 2>&1)
eq "    and removed" "no" "$([ -e "$out" ] && echo yes || echo no)"
eq "    an unreadable path: an error, not done" "1" "$( (echo '{}' | PATH=$nogf "$L" remove --claude-hook worktree-remove) >/dev/null 2>&1; echo $?)"
mkdir -p "$COMMON/gitfold"
out=$(printf '{"hook_event_name":"WorktreeCreate","cwd":"%s","name":"p2"}' "$REPO" | PATH=$nogf "$L" branch --claude-hook worktree-create 2>&1); rc=$?
eq "    in a repository used with gitfold: refused" "1 yes" "$rc $(grep -q "used with gitfold" <<<"$out" && echo yes)"
}

reads_searches_left_out_cached_ones_hook() {
R branch rd
mount_fs
"$BIN" -C "$REPO" track-reads rd on >/dev/null
W=$MNT/rd
cat "$W/a.txt" >/dev/null
eq "a program's reads are recorded" "a.txt" "$(reads_of rd)"
grep -rq needle "$W" 2>/dev/null; (cd "$W" && exec -a ugrep awk 'NR < 0' d/c.txt)
if command -v rg >/dev/null; then rg -q needle "$W"; fi
eq "  a search's aren't (grep, rg; a program run as ugrep)" "a.txt" "$(reads_of rd)"
cat "$W/d/b.txt" >/dev/null
eq "  nor, the kernel serving it from its cache, a read after one" "a.txt" "$(reads_of rd)"
hook() { printf '%s' "$1" | "$BIN" track-reads --claude-hook pre-tool-use; }
hook "{\"tool_name\":\"Read\",\"cwd\":\"$REPO\",\"tool_input\":{\"file_path\":\"$W/d/b.txt\"}}"
eq "Claude Code's Read, of a file the kernel had cached (grep read it): recorded by the hook" "a.txt d/b.txt" "$(reads_of rd)"
hook "{\"tool_name\":\"Bash\",\"cwd\":\"$W/d\",\"tool_input\":{\"command\":\"sed -n 1,2p c.txt | head -n 1; cat ../nope\"}}"
eq "  its Bash: the files simple viewers show (relative to cwd), not missing ones" "a.txt d/b.txt d/c.txt" "$(reads_of rd)"
eq "  silent, exit 0, wherever it runs" "0 " "$(hook '{"tool_name":"Read","tool_input":{"file_path":"/etc/hostname"}}' 2>&1; echo "$?" "$(hook 'not json' 2>&1)")"
ln -s "$MNT" "$T/link"; echo e >"$W/d/e.txt"; grep -rq needle "$W/d"
hook "{\"tool_name\":\"Bash\",\"cwd\":\"$T/link/rd\",\"tool_input\":{\"command\":\"cd d && head -n 1 < e.txt\"}}"
eq "  through a symlink to the mount, after a cd in the command" "a.txt d/b.txt d/c.txt d/e.txt" "$(reads_of rd)"
"$BIN" -C "$REPO" track-reads rd off >/dev/null
hook "{\"tool_name\":\"Read\",\"cwd\":\"$REPO\",\"tool_input\":{\"file_path\":\"$W/d/c.txt\"}}"
eq "  untracked: nothing" "a.txt d/b.txt d/c.txt d/e.txt" "$(reads_of rd)"
unmount_fs
R branch ko
mount_fs --kernel-open
"$BIN" -C "$REPO" track-reads ko on >/dev/null
grep -rq needle "$MNT/ko"; cat "$MNT/ko/a.txt" >/dev/null
eq "with every open seen (--kernel-open): a read after a search too, not the search" "a.txt" "$(reads_of ko)"
(exec 3<>"$MNT/ko/d/b.txt"; cat <&3 >/dev/null)
eq "  a file opened read-write: its read" "a.txt d/b.txt" "$(reads_of ko)"
unmount_fs
}

section "overlaps: reads and writes changed elsewhere" overlaps_reads_and_writes_changed_elsewh
section "reads: searches left out, cached ones from the hook" reads_searches_left_out_cached_ones_hook
section "the plugin's launcher" the_plugins_launcher
section "Claude Code worktree hooks" claude_code_worktree_hooks
section "the task folder" the_task_folder
section "inbox, send, watch" inbox_send_watch
finish
