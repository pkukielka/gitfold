#!/usr/bin/env bash
# End-to-end test of `gitfold branch NEW --from A --from B …`: NEW at A, then
# `git merge B …` run in NEW's workspace, so it is what git's own merge makes
# (compared with git merge in a scratch worktree). A conflict leaves NEW with
# git's merge in progress; git refusing leaves nothing. Unmounted, the
# repository is mounted again where it last was.
# usage: tests/from.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail
export LC_ALL=C   # git's messages, as checked

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=from
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# gitfold run in the repository: its exit code, then its output on one line.
gf() { local out rc; out=$("$BIN" -C "$REPO" "$@" 2>&1); rc=$?; echo "$rc ${out//$'\n'/ }"; }
# Whether `gf ARGS` exits with $1 and says $2.
says() { local code=$1 text=$2; shift 2; local r; r=$(gf "$@"); [[ $r == "$code "* && $r == *"$text"* ]] || { echo "  got: $r"; false; }; }

# Branch $1 off $2 with a commit writing $4 into file $3.
work() { R branch "$1" "$2"; R worktree add -q "$T/wt-$1" "$1"; mkdir -p "$(dirname "$T/wt-$1/$3")"; echo "$4" >"$T/wt-$1/$3"; G "$T/wt-$1" add -A; G "$T/wt-$1" commit -qm "$1: $3"; R worktree remove "$T/wt-$1"; }
# What `git merge REV…` makes on a branch at $2 for branch name $1: the
# commit's parents, tree and whole message.
git_merge() {
    local name=$1 at=$2; shift 2
    R worktree add -q --detach "$T/git-$name" "$at"
    G "$T/git-$name" merge -q --no-edit --into-name "$name" "$@" >/dev/null 2>&1
    G "$T/git-$name" log -1 --format='%P %T%n%B'
    R worktree remove --force "$T/git-$name"
}
made() { R log -1 --format='%P %T%n%B' "$1"; }
# Whether `git merge REV…` on a branch at $1 fails.
git_refuses() {
    local at=$1; shift
    R worktree add -q --detach "$T/git-refuses" "$at"
    ! G "$T/git-refuses" merge -q --no-edit "$@" >/dev/null 2>&1
    local refused=$?
    R worktree remove --force "$T/git-refuses"
    return $refused
}
# Whether branch $1 left nothing: no branch, no workspace.
nothing_of() { ! R rev-parse --verify -q "refs/heads/$1" >/dev/null && [ ! -e "$MNT/$1" ] && ! "$BIN" -C "$REPO" list | grep -Eq "^[ └─]*$1 "; }
# git in workspace $1 of the mount.
W() { git -C "$MNT/$1" -c user.name=t -c user.email=t@t "${@:2}"; }

setup() {
    R config user.name t; R config user.email t@t   # for the merge commits
    work api main api.txt api
    work ui main ui.txt ui
    work x main x.txt x
    mount_fs
}

clean_merges() {
setup
check "two sources: created, git saying how" says 0 "created branch c; open it as $MNT/c" branch c --from api --from ui --prewarm none
eq "  with its setting" none "$(R config --get branch.c.gitfoldPrewarm)"
eq "  as git merge makes it" "$(git_merge c api ui)" "$(made c)"
eq "  its workspace shows it, clean" "api ui " "$(cat "$MNT/c/api.txt" "$MNT/c/ui.txt" | tr '\n' ' ')$(W c status --porcelain)"
check "three: an octopus" says 0 "Merge made by the 'octopus' strategy." branch c3 --from api --from ui --from x
eq "  as git merge makes it" "$(git_merge c3 api ui x)" "$(made c3)"
check "the first contained in the others" says 0 "created branch s" branch s --from main --from api --from ui
eq "  as git merge makes it" "$(git_merge s main api ui)" "$(made s)"
R tag -a -m "the tag's message" v1 ui
check "a tag" says 0 "created branch t" branch t --from x --from v1
eq "  as git merge makes it (the tag's message too)" "$(git_merge t x v1)" "$(made t)"
check "the first contains the rest: up to date, at it" says 0 "Already up to date." branch u --from api --from main
eq "  at api" "$(R rev-parse api)" "$(R rev-parse u)"
check "another contains the first: a fast-forward" says 0 "Fast-forward" branch f --from main --from api
eq "  at api" "$(R rev-parse api)" "$(R rev-parse f)"
R config merge.ff false
check "merge.ff=false: a merge, as git" says 0 "created branch n" branch n --from main --from api
eq "  as git merge makes it" "$(git_merge n main api)" "$(made n)"
R config --unset merge.ff
eq "no relation afterwards" "" "$(R config --get-regexp '^branch\.(c|c3)\.(remote|merge|gitfoldparent)' || true)"
}

refused() {
setup
work clash main api.txt clash; work clash2 main ui.txt clash2
R checkout -q --orphan lone; R rm -rqf .; echo z >"$REPO/z"; R add z; R commit -qm lone; R checkout -q main
check "a conflict before the last: git's octopus refusal" says 1 "Should not be doing an octopus" branch c --from api --from clash --from x
check "  git refuses it too" git_refuses api clash x
check "  nothing left behind" nothing_of c
check "unrelated histories: git's refusal" says 1 "refusing to merge unrelated histories" branch c --from api --from lone
check "  nothing left behind" nothing_of c
R config merge.ff only
check "merge.ff=only: git's refusal" says 1 "Not possible to fast-forward" branch c --from api --from ui
check "  nothing left behind" nothing_of c
R config --unset merge.ff
check "an unknown revision: git's refusal" says 1 "not something we can merge" branch c --from api --from nosuch
check "  nothing left behind" nothing_of c
check "an unknown first revision: refused as git merge refuses it" says 1 "nosuch - not something we can merge" branch c --from nosuch --from api
check "  nothing left behind" nothing_of c
check "an existing branch" says 1 "a branch named 'ui' already exists" branch ui --from api --from x
check "refused with --prewarm: the setting goes too" says 1 "refusing to merge unrelated histories" branch c --from api --from lone --prewarm none
eq "  no setting left" "" "$(R config --get branch.c.gitfoldPrewarm)"
}

conflicts() {
setup
work clash main api.txt clash; work clash2 main ui.txt clash2
check "a conflict: exit 1, git's merge in progress" says 1 "created branch c with its merge in progress (see above): finish it in $MNT/c with \`git merge --continue\`, or \`git merge --abort\`" branch c --from api --from clash
eq "  git status says so" "AA api.txt" "$(W c status --porcelain)"
check "  the markers in the file" grep -q '^<<<<<<< HEAD' "$MNT/c/api.txt"
echo resolved >"$MNT/c/api.txt"; W c add api.txt
check "  git merge --continue" W c merge --continue
eq "  a merge of both" "$(R rev-parse api clash | tr '\n' ' ')" "$(R log -1 --format=%P c | tr ' ' '\n' | tr '\n' ' ')"
check "aborted" says 1 "merge in progress" branch c2 --from api --from clash
check "  git merge --abort" W c2 merge --abort
eq "  back at the first, clean" "$(R rev-parse api)" "$(R rev-parse c2)$(W c2 status --porcelain)"
check "a conflict at the last of several: in progress" says 1 "merge in progress" branch c3 --from x --from ui --from clash2
eq "  merging both others" "$(R rev-parse ui clash2 | tr '\n' ' ')" "$(cat "$(W c3 rev-parse --absolute-git-dir)/MERGE_HEAD" | tr '\n' ' ')"
check "unmounted, remounted: still in progress" says 1 "merge in progress" branch c4 --from api --from clash
unmount_fs; mount_fs
eq "  git status says so" "AA api.txt" "$(W c4 status --porcelain)"
check "  git merge --abort" W c4 merge --abort
printf '#!/bin/sh\nexit 1\n' >"$COMMON/hooks/pre-merge-commit"; chmod +x "$COMMON/hooks/pre-merge-commit"
check "stopped by a hook: in progress, no conflict claimed" says 1 "merge in progress (see above)" branch h --from api --from ui
eq "  nothing conflicts" "" "$(W h diff --name-only --diff-filter=U)"
rm "$COMMON/hooks/pre-merge-commit"
}

names() {
setup
ls "$MNT/ui" >/dev/null
# gitfold as run in a workspace: its HEAD, its names.
in_ui() { (cd "$MNT/ui" && "$BIN" "$@" 2>&1); }
check "HEAD where the user stands, not in the new workspace" in_ui branch h --from api --from HEAD
eq "  as git merge makes it (named as git names it)" "$(git_merge h api ui)" "$(made h)"
check "a revision named relative to a branch" says 0 "created branch t" branch t --from x --from ui~0
eq "  as git merge makes it" "$(git_merge t x ui~0)" "$(made t)"
W ui commit -q --allow-empty -m more
check "HEAD~1 where the user stands: its commit" in_ui branch p --from api --from HEAD~1
eq "  merged" "$(R rev-parse api ui~1 | tr '\n' ' ')" "$(R log -1 --format=%P p | tr ' ' '\n' | tr '\n' ' ')"
R config merge.ff true
check "configuration given in the environment applies (git -c)" sh -c "GIT_CONFIG_PARAMETERS=\"'merge.ff'='false'\" '$BIN' -C '$REPO' branch n --from main --from api"
eq "  a merge, not a fast-forward" 2 "$(R log -1 --format=%P n | wc -w)"
check "a repository's variables from a caller: not for the new workspace" sh -c "GIT_INDEX_FILE='$T/nowhere/index' '$BIN' -C '$REPO' branch g --from api --from ui"
eq "  as git merge makes it" "$(git_merge g api ui)" "$(made g)"
R checkout -q x; R checkout -q main
check "- : the previous branch where the user stands, as git merge -" says 0 "created branch prev" branch prev --from api --from -
eq "  named as git merge - names it" "Merge branch 'x' into prev" "$(R log -1 --format=%s prev --)"
R config branch.x.gitfoldParent refs/heads/api
for how in "--from api" "-w --from api" "--from api --from ui"; do
    check "an existing branch ($how): refused, its settings untouched" says 1 "exist" branch x $how
    eq "  its parent kept" refs/heads/api "$(R config --get branch.x.gitfoldParent)"
done
}

unmounted() {
setup
unmount_fs
check "unmounted, an unknown revision: refused before mounting" says 1 "nosuch - not something we can merge" branch c --from api --from nosuch
check "  nothing mounted" fails mountpoint -q "$MNT"
check "unmounted: mounted again where it was, then merged" says 0 "created branch c; open it as $MNT/c" branch c --from api --from ui
check "  the mount stays" mountpoint -q "$MNT"
eq "  as git merge makes it" "$(git_merge c api ui)" "$(made c)"
"$BIN" unmount "$MNT" >/dev/null
}

# The flags gitfold mount recorded, on one line.
mount_options() { R config --get gitfold.mountOptions | paste -sd" "; }
# Waits (up to 5 s) until the mount recorded flags $1: it does so right after mounting.
recorded() { for _ in $(seq 100); do [ "$(mount_options)" = "$1" ] && return; sleep 0.05; done; false; }

remount_flags() {
setup
unmount_fs
mount_fs --track-reads --prewarm none --log "$T/last.log"
check "gitfold mount records its flags" recorded "--prewarm=none --prewarm-walks=2 --track-reads --log=$T/last.log"
unmount_fs
for i in 1 2 3; do
    out=$("$BIN" -C "$REPO" branch "c$i" --from api --from ui 2>&1)
    check "mounted again by branch --from ($i), no config locked meanwhile" eq_ok "$out" "created branch c$i"
    "$BIN" unmount "$MNT" >/dev/null
done
check "  with the flags it last had: logging where it last did" grep -q "mounting" "$T/last.log"
"$BIN" mount --prewarm content "$REPO" "$MNT" >/dev/null
eq "gitfold mount in the background: its flags recorded before it returns" "--prewarm=content --prewarm-walks=2" "$(mount_options)"
"$BIN" unmount "$MNT" >/dev/null
# The daemon it starts: it would record them once mounted, before it answers
# on its control socket (`gitfold version`).
GITFOLD_RECORDED_BY_MOUNT=1 mount_fs --prewarm none
check "  the daemon answering on its control socket" "$BIN" version "$MNT"
eq "  not by the daemon it starts, at a moment after" "--prewarm=content --prewarm-walks=2" "$(mount_options)"
unmount_fs
mount_fs
check "an explicit gitfold mount: its own flags, recorded" recorded "--prewarm=meta --prewarm-walks=2"
unmount_fs
}
# Whether output $1 says $2 and nothing about a locked file.
eq_ok() { [[ $1 == *"$2"* && $1 != *"lock"* ]] || { echo "  got: $1"; false; }; }

with_changes() {
setup
echo '*.o' >>"$COMMON/info/exclude"
echo "api changed" >"$MNT/api/api.txt"; echo new >"$MNT/api/new.txt"; echo built >"$MNT/api/x.o"
echo "ui changed" >"$MNT/ui/ui.txt"; (cd "$MNT/ui" && git add ui.txt)
status() { git -C "$MNT/$1" status --porcelain | sort | tr '\n' ' '; }
before_api=$(status api); before_ui=$(status ui)
check "-w: the merge, then each source's uncommitted changes" says 0 "created branch w" branch w -w --from api --from ui --from x
eq "  unstaged, untracked as untracked, ignored not carried" " M api.txt  M ui.txt ?? new.txt " "$(status w)"
eq "  the changes" "api changed ui changed" "$(cat "$MNT/w/api.txt" "$MNT/w/ui.txt" | tr '\n' ' ' | sed 's/ $//')"
check "  ignored files not carried" test ! -e "$MNT/w/x.o"
eq "  the sources as they were" "$before_api|$before_ui" "$(status api)|$(status ui)"
eq "  HEAD the merge, as without -w" "$(git_merge w api ui x)" "$(made w)"
echo "a by api" >"$MNT/api/a.txt"; echo "a by ui" >"$MNT/ui/a.txt"
check "-w: changes that conflict: exit 1 as git cherry-pick, the paths named" says 1 "created branch w2; the uncommitted changes conflict (see above): ui's on a.txt" branch w2 -w --from api --from ui
check "  the markers" grep -q '^<<<<<<< HEAD' "$MNT/w2/a.txt"
eq "  unstaged" "" "$(git -C "$MNT/w2" diff --cached --name-only)"
echo "x changed" >"$MNT/x/x.txt"
check "-w: a conflict before the last source's changes" says 1 "ui's on a.txt" branch w6 -w --from api --from ui --from x
eq "  the markers, and the last one's changes after them" "1 x changed" "$(grep -c '^<<<<<<< ' "$MNT/w6/a.txt") $(cat "$MNT/w6/x.txt")"
check "-w, a source given twice: its changes once" says 0 "created branch w8" branch w8 -w --from api --from x --from api
work clash main api.txt clash
check "-w, the branches conflicting: refused, nothing left" says 1 "not created" branch w3 -w --from api --from clash
check "  nothing left behind" nothing_of w3
work c1 main q.txt one; work c2 main q.txt two
check "-w, the branches conflicting, no changes at all: refused too" says 1 "the merge didn't complete" branch w7 -w --from c1 --from c2
check "  nothing left behind" nothing_of w7
check "-w, a revision that is no branch: no changes from it" says 0 "created branch w4" branch w4 -w --from api --from "$(R rev-parse ui)"
eq "  the others' changes" "api changed" "$(cat "$MNT/w4/api.txt")"
R tag api api
check "a first source git finds ambiguous: refused as git branch refuses it" says 1 "fatal: ambiguous object name: 'api'" branch w9 -w --from api --from x
check "  nothing left behind" nothing_of w9
r=$(gf branch w11 -w --from x --from api)
check "a later source git finds ambiguous: created" eq_ok "$r" "created branch w11"
eq "  git's warning said once" 1 "$(grep -o "warning: refname 'api' is ambiguous." <<<"$r" | wc -l)"
check "  no changes from it (it names no branch)" test ! -e "$MNT/w11/new.txt"
R tag -d api >/dev/null
for i in $(seq -w 12); do for b in api ui x; do echo "p by $b" >"$MNT/$b/p$i.txt"; done; done
check "-w, many paths conflicting: the first 10 named, of all" says 1 "ui's on a.txt, p01.txt, p02.txt, p03.txt, p04.txt, p05.txt, p06.txt, p07.txt, p08.txt, p09.txt and x's … and 15 more; in" branch w10 -w --from api --from ui --from x
for b in api ui x; do W "$b" clean -fq -- 'p??.txt'; done
gf branch mp --from api --from clash >/dev/null
check "-w, a source with a git operation in progress: refused (exit 4)" says 4 "in progress" branch w5 -w --from mp --from ui
check "  nothing left behind" nothing_of w5
W mp merge --quit
check "-w, a source whose index has unmerged paths: refused (exit 4), as git stash" says 4 "unmerged paths" branch w5 -w --from mp --from ui
eq "  no copy of an index left" "" "$(ls -A "$COMMON" | grep gitfold-snapshot)"
unmount_fs
check "  unmounted: refused before mounting" says 4 "unmerged paths" branch w5 -w --from mp --from ui
check "  nothing mounted" fails mountpoint -q "$MNT"
}

section "from: clean merges, as git makes them" clean_merges
section "from: --with-changes" with_changes
section "from: refused: nothing left" refused
section "from: conflicts: git's merge in progress" conflicts
section "from: names, as where the user stands" names
section "from: unmounted" unmounted
section "from: mounted again with the last flags" remount_flags
finish
