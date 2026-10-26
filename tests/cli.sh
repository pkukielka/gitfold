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
export HOME=$T/home XDG_CONFIG_HOME=$T/xdg   # no global git config or ignore file from the test machine

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
mkdir -p "$T/mnt2"
check "second mount of the repository refused" fails timeout 5 "$BIN" mount "$REPO" "$T/mnt2"
check "second mount point not mounted" fails mountpoint -q "$T/mnt2"
check "remove refused while mounted" fails "$BIN" remove "$REPO" two
check "prune refused while mounted" fails "$BIN" prune "$REPO"
check "the git common directory is what is locked" fails flock -n "$COMMON" true
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

echo "== staged-only changes, and the base after a commit"
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
eq "the base caught up at unmount" "$(R rev-parse six)" "$(cat "$COMMON/gitfold/six/base")"
c2=$(R commit-tree "six^{tree}" -p six -m 'moved while unmounted')
R update-ref refs/heads/six "$c2"                                  # base now behind the tip
check "remove catches up offline and removes" "$BIN" remove "$REPO" six

echo "== ignored files"
printf 'target/\n*.log\n!keep.log\n' >"$REPO/.gitignore"; echo t >"$REPO/tracked.log"
R add .gitignore; R add -f tracked.log; R commit -qm ignores
check "branch seven" "$BIN" branch "$REPO" seven
mount_fs
W7=$MNT/seven
mkdir -p "$W7/target/deep" "$W7/d/gen"
echo o >"$W7/target/deep/a.o"              # in an ignored directory
echo l >"$W7/x.log"; echo l >"$W7/d/y.log" # ignored by pattern, at any depth
echo k >"$W7/keep.log"                     # re-included by a negation
echo g >"$W7/d/gen/out.txt"; echo 'gen/' >"$W7/d/.gitignore"   # a new .gitignore
unmount_fs
out=$("$BIN" list "$REPO" 2>/dev/null)
eq "keep.log and d/.gitignore changed; 4 ignored" "2 0 4" \
    "$(echo "$out" | awk '$1 == "seven" { print $2, $3, $4 }')"
check "remove still refused (real changes)" fails "$BIN" remove "$REPO" seven
mount_fs
rm "$W7/keep.log" "$W7/d/.gitignore"; rm -r "$W7/d/gen"
echo changed >>"$W7/tracked.log"           # tracked: a change even though *.log
unmount_fs
eq "tracked file matching a pattern is a change" "1 0 3" \
    "$("$BIN" list "$REPO" 2>/dev/null | awk '$1 == "seven" { print $2, $3, $4 }')"
mount_fs
G "$W7" checkout -q -- tracked.log; ls "$W7" >/dev/null
unmount_fs
out=$("$BIN" remove "$REPO" seven 2>&1)
eq "only ignored files: removed without --force" yes \
    "$(grep -q 'removed workspace seven (and 3 ignored files' <<<"$out" && echo yes)"
check "branch eight" "$BIN" branch "$REPO" eight
mount_fs
echo l >"$MNT/eight/x.log"; rm "$MNT/eight/.gitignore"   # deleting .gitignore un-ignores
unmount_fs
eq "without .gitignore: x.log changed, .gitignore deleted" "1 1 0" \
    "$("$BIN" list "$REPO" 2>/dev/null | awk '$1 == "eight" { print $2, $3, $4 }')"
check "remove eight" "$BIN" remove "$REPO" eight --force

echo "== modes"
check "branch nine" "$BIN" branch "$REPO" nine
mount_fs; chmod 664 "$MNT/nine/a.txt"; unmount_fs
eq "group-write bit alone: no change (as for git)" "0" \
    "$("$BIN" list "$REPO" 2>/dev/null | awk '$1 == "nine" { print $2 }')"
mount_fs; chmod +x "$MNT/nine/a.txt"; unmount_fs
eq "executable bit: a change" "1" \
    "$("$BIN" list "$REPO" 2>/dev/null | awk '$1 == "nine" { print $2 }')"
check "remove nine" "$BIN" remove "$REPO" nine --force

echo "== .gitignore files git doesn't read"
mkdir -p "$REPO/s"; ln -s '*.tmp' "$REPO/s/.gitignore"; R add s 2>/dev/null; R commit -qm 'symlinked gitignore'
check "branch ten" "$BIN" branch "$REPO" ten
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
    "$("$BIN" list "$REPO" 2>/dev/null | awk '$1 == "ten" { print $2, $4 }')"
mkdir -p "$T/cwd/git"; echo '*' >"$T/cwd/git/ignore"
eq "empty XDG_CONFIG_HOME is unset, not the current directory" "8 0" \
    "$(cd "$T/cwd" && XDG_CONFIG_HOME= "$BIN" list "$REPO" 2>/dev/null | awk '$1 == "ten" { print $2, $4 }')"
check "remove ten" "$BIN" remove "$REPO" ten --force

echo "== global ignore file as git reads it"
mkdir -p "$XDG_CONFIG_HOME/git"; echo '*.xdg' >"$XDG_CONFIG_HOME/git/ignore"
check "branch eleven" "$BIN" branch "$REPO" eleven
mount_fs
echo x >"$MNT/eleven/w.xdg"
mkdir -p "$MNT/eleven/sm2"; ls "$MNT/eleven/sm" >/dev/null
echo '*' >"$MNT/eleven/sm/.gitignore"; echo c >"$MNT/eleven/sm/work.c"   # inside a submodule
unmount_fs
cnt() { "$BIN" list "$REPO" 2>/dev/null | awk '$1 == "eleven" { print $2, $4 }'; }
eq "XDG ignore file applies; submodule files are changes" "2 1" "$(cnt)"
R config core.excludesFile ''
eq "core.excludesFile set but empty: no global file" "3 0" "$(cnt)"
R config --unset core.excludesFile
R config extensions.worktreeConfig true
git --git-dir "$COMMON/worktrees/gitfold-ws-eleven" config --worktree core.excludesFile /dev/null
eq "worktree config is honoured" "3 0" "$(cnt)"
R config --unset extensions.worktreeConfig
check "remove eleven" "$BIN" remove "$REPO" eleven --force
rm "$XDG_CONFIG_HOME/git/ignore"

echo "== nested repositories and a relative excludes file"
check "branch twelve" "$BIN" branch "$REPO" twelve
mount_fs
W12=$MNT/twelve
mkdir -p "$W12/nested/.git" "$W12/target/vendored/.git"
echo '*' >"$W12/nested/.gitignore"; echo p >"$W12/nested/precious.txt"   # its own ignores
touch "$W12/nested/.git/HEAD" "$W12/target/vendored/.git/HEAD"         # target/ is ignored
unmount_fs
eq "a nested repository is one untracked entry, or one ignored" "1 1" \
    "$("$BIN" list "$REPO" 2>/dev/null | awk '$1 == "twelve" { print $2, $4 }')"
check "remove refused for the nested repository" fails "$BIN" remove "$REPO" twelve
R config core.excludesFile relign
eq "relative core.excludesFile: status refused" yes "$("$BIN" list "$REPO" 2>/dev/null | grep "^twelve" | grep -q "relative path" && echo yes)"
R config --unset core.excludesFile
check "remove twelve" "$BIN" remove "$REPO" twelve --force

echo "== the lock leaves nothing behind"
git init -q "$T/plain"; git -C "$T/plain" -c user.name=t -c user.email=t@t commit -q --allow-empty -m e
check "prune of a repository without gitfold state" "$BIN" prune "$T/plain"
check "creates nothing in it" test ! -e "$T/plain/.git/gitfold"

mkdir -p "$T/plain/.git/gitfold"; touch "$T/plain/.git/gitfold/.lock"
flock "$T/plain/.git/gitfold/.lock" sleep 3 & OLD=$!   # a daemon of an earlier build
sleep 0.3
check "an earlier build's lock file is honoured" fails "$BIN" prune "$T/plain"
wait $OLD
check "free again" "$BIN" prune "$T/plain"

echo "== background mount and unmount"
check "branch bg" "$BIN" branch "$REPO" bg
out=$("$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" 2>&1)
eq "mount returns once mounted" yes "$(mountpoint -q "$MNT" && echo yes)"
eq "and says how to unmount" yes "$(grep -q "gitfold unmount $MNT" <<<"$out" && echo yes)"
check "usable at once" test -f "$MNT/bg/a.txt"
echo bg >>"$MNT/bg/a.txt"; G "$MNT/bg" commit -qam bg      # then unmount at once
check "unmount" "$BIN" unmount "$MNT"
check "unmounted" sh -c "! mountpoint -q '$MNT'"
check "remove right after commit + unmount" "$BIN" remove "$REPO" bg
check "branch bg3" "$BIN" branch "$REPO" bg3
cd "$MNT"   # the directory under the mount: not a user of it
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
( cd "$MNT/bg3" && exec sleep 5 ) & HOLDER=$!   # like the `git maintenance` a commit starts
for _ in $(seq 100); do [ "$(cat /proc/$HOLDER/comm 2>/dev/null)" = sleep ] && break; sleep 0.02; done
out=$("$BIN" unmount "$MNT" 2>&1)
cd /
eq "unmount names a process using the mount" yes "$(grep -q "sleep 5" <<<"$out" && echo yes)"
eq "and waits until gitfold exits" "unmounted $MNT" "$(tail -1 <<<"$out")"
check "remove right after a busy unmount" "$BIN" remove "$REPO" bg3
wait
check "branch bg4" "$BIN" branch "$REPO" bg4
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
cp "$BIN" "$MNT/bg4/gitfold-copy"
out=$(timeout 20 "$MNT/bg4/gitfold-copy" unmount "$MNT" 2>&1)
eq "unmount run from inside the mount doesn't wait for itself" yes "$(grep -q 'in use by this command' <<<"$out" && echo yes)"
for _ in $(seq 100); do flock -n "$COMMON" true 2>/dev/null && break; sleep 0.1; done
check "and gitfold exits after it" flock -n "$COMMON" true
check "remove bg4" "$BIN" remove "$REPO" bg4 --force
check "branch bg2" "$BIN" branch "$REPO" bg2
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
check "remove bg2" "$BIN" remove "$REPO" bg2 --force

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
check "remove right after it" "$BIN" remove "$T/r,c" c1
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
check "remove later" "$BIN" remove "$REPO" later --force
check "remove ns/b" "$BIN" remove "$REPO" ns/b --force

echo "== $FAILS failure(s)"
[ -n "${KEEP:-}" ] || rm -rf "$T"
exit $((FAILS > 0))
