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

echo "== $FAILS failure(s)"
[ -n "${KEEP:-}" ] || rm -rf "$T"
exit $((FAILS > 0))
