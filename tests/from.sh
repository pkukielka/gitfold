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
nothing_of() { ! R rev-parse --verify -q "refs/heads/$1" >/dev/null && [ ! -e "$MNT/$1" ] && ! "$BIN" -C "$REPO" list | grep -q "^$1 "; }
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
check "-w with several: not implemented yet" says 1 "not implemented yet" branch c -w --from api --from ui
check "  nothing left behind" nothing_of c
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
R config branch.lp.gitfoldParent refs/heads/api; R config branch.lp.gitfoldFollow false
R update-ref refs/gitfold-base/lp api
check "a deleted branch's relation left: forgotten" says 0 "created branch lp" branch lp --from api --from ui
eq "  no parent, following or base ref" "" "$(R config --get-regexp '^branch\.lp\.gitfold(parent|follow)'; R rev-parse -q --verify refs/gitfold-base/lp)"
R config branch.ls.gitfoldPrewarm none; R config branch.ls.gitfoldTrackReads true
check "a deleted branch's workspace settings left: forgotten, but for those given" says 0 "created branch ls" branch ls --from api --from ui --track-reads off
eq "  prewarm forgotten, read tracking as given" " false" "$(R config --get branch.ls.gitfoldPrewarm) $(R config --get branch.ls.gitfoldTrackReads)"
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

remount_flags() {
setup
unmount_fs
mount_fs --track-reads --prewarm none --log "$T/last.log"
eq "gitfold mount records its flags" "--prewarm=none --prewarm-walks=2 --track-reads --log=$T/last.log" "$(mount_options)"
unmount_fs
check "mounted again by branch --from" says 0 "created branch c" branch c --from api --from ui
eq "  with the flags it last had (recorded again by that mount)" "--prewarm=none --prewarm-walks=2 --track-reads --log=$T/last.log" "$(mount_options)"
check "  logging where it last did" grep -q "mounting" "$T/last.log"
"$BIN" unmount "$MNT" >/dev/null
mount_fs
eq "an explicit gitfold mount: its own flags, recorded" "--prewarm=meta --prewarm-walks=2" "$(mount_options)"
unmount_fs
}

section "from: clean merges, as git makes them" clean_merges
section "from: refused: nothing left" refused
section "from: conflicts: git's merge in progress" conflicts
section "from: names, as where the user stands" names
section "from: unmounted" unmounted
section "from: mounted again with the last flags" remount_flags
finish
