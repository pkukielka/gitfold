#!/usr/bin/env bash
# End-to-end test of `gitfold branch NEW --from A --from B …`: the branch
# starts at the merge of the revisions, as `git merge` makes it on a branch at
# the first (compared with git's own merge in a scratch worktree). A
# conflicting merge says "not implemented yet" for now, leaving nothing.
# usage: tests/from.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

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
# commit's parents, tree and whole message (or "same" if it didn't move).
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

setup() {
    R config user.name t; R config user.email t@t   # for the merge commits
    work api main api.txt api
    work ui main ui.txt ui
    work x main x.txt x
}

clean_merges() {
setup
check "two sources: created at a merge" says 0 "created branch c at a merge of api and ui" branch c --from api --from ui
eq "two sources: as git merge makes it" "$(git_merge c api ui)" "$(made c)"
eq "two sources: the merged files" "api ui" "$(R show c:api.txt) $(R show c:ui.txt)"
check "three: an octopus" says 0 "at a merge of api, ui and x" branch c3 --from api --from ui --from x
eq "three: as git merge makes it" "$(git_merge c3 api ui x)" "$(made c3)"
eq "three: three parents" 3 "$(R log -1 --format=%P c3 | wc -w)"
check "the first contained in the others: dropped from the parents" says 0 "created branch s at a merge of api and ui" branch s --from main --from api --from ui
eq "  as git merge does" "$(git_merge s main api ui)" "$(made s)"
check "a source repeated counts once" says 0 "created branch r" branch r --from api --from ui --from api
eq "  as git merge does" "$(git_merge r api ui api)" "$(made r)"
check "a revision, a tag, an early part: git's message" says 0 "created branch m" branch m --from x --from "$(R rev-parse ui)" --from api~0
eq "  as git merge does" "$(git_merge m x "$(R rev-parse ui)" api~0)" "$(made m)"
R tag -a -m "the tag's message" v1 ui; R tag lw api
check "tags" says 0 "created branch t" branch t --from x --from v1 --from lw
eq "  as git merge does (an annotated tag's message too)" "$(git_merge t x v1 lw)" "$(made t)"
v1=$(R rev-parse v1)
check "an annotated tag by its id" says 0 "created branch ti" branch ti --from x --from "$v1"
eq "  as git merge does (a tag, with its message)" "$(git_merge ti x "$v1")" "$(made ti)"
work api2 api api.txt api2
check "an early part (api2~1)" says 0 "created branch e" branch e --from ui --from api2~1
eq "  as git merge does" "$(git_merge e ui api2~1)" "$(made e)"
eq "no relation afterwards" "" "$(R config --get-regexp '^branch\.(c|c3)\.' || true)"
}

octopus_steps() {
setup
work lines main m.txt "$(printf '1\n2\n3\n4\n5')"
work top lines m.txt "$(printf 'one\n2\n3\n4\n5')"
work bottom lines m.txt "$(printf '1\n2\n3\n4\nfive')"
check "an octopus merging a file's content (merge-one-file)" says 0 "at a merge of top, bottom and x" branch o --from top --from bottom --from x
eq "  as git merge does" "$(git_merge o top bottom x)" "$(made o)"
R branch ren main; R worktree add -q "$T/wt-ren" ren; G "$T/wt-ren" mv a.txt moved.txt; G "$T/wt-ren" commit -qm ren; R worktree remove "$T/wt-ren"
work edit main a.txt edited
check "two sources: renames followed (ort, git's default)" says 0 "at a merge of ren and edit" branch two --from ren --from edit
eq "  as git merge does" "$(git_merge two ren edit)" "$(made two)"
check "an octopus follows no renames: refused before the last, as git" says 1 "Should not be doing an octopus" branch o2 --from ren --from edit --from x
check "  git refuses it too" git_refuses ren edit x
check "  nothing left behind" fails R rev-parse --verify -q refs/heads/o2
eq "  no index of the steps left" "" "$(ls -A "$COMMON" | grep merge-index)"
check "  nothing of gitfold's made in a repository never mounted" test ! -e "$COMMON/gitfold"
R checkout -q -b bins main; printf 'a\0b' >"$REPO/bin.dat"; R add bin.dat; R commit -qm bin; R checkout -q main
work bin1 bins bin.dat "$(printf 'one\001')"; work bin2 bins bin.dat "$(printf 'two\001')"
check "binary files changed in both, before the last: refused, as git" says 1 "Should not be doing an octopus" branch b1 --from bin1 --from bin2 --from x
check "  git refuses it too" git_refuses bin1 bin2 x
check "binary files changed in both, last: a conflict (not implemented yet)" says 1 "not implemented yet" branch b2 --from x --from bin1 --from bin2
work df1 main q file; work df2 main q/f file
check "a file in one, a directory in another, last: a conflict (not implemented yet)" says 1 "not implemented yet" branch d1 --from x --from df1 --from df2
check "  git conflicts too" git_refuses x df1 df2
check "  nothing left behind" fails R rev-parse --verify -q refs/heads/d1
}

merge_ff() {
setup
R config merge.ff false
check "merge.ff=false: a merge where a fast-forward would do" says 0 "at a merge of main and api" branch n --from main --from api
eq "  as git merge does" "$(git_merge n main api)" "$(made n)"
check "  already up to date stays so" says 0 "already up to date" branch n2 --from api --from main
R config merge.ff only
check "merge.ff=only: a fast-forward" says 0 "a fast-forward" branch f --from main --from api
check "merge.ff=only: no merge" says 1 "Not possible to fast-forward" branch m --from api --from ui
check "  git refuses it too" git_refuses api ui
check "  nothing left behind" fails R rev-parse --verify -q refs/heads/m
}

no_merge_needed() {
setup
check "the first contains the rest: up to date, at it" says 0 "created branch u at api (already up to date: it contains main)" branch u --from api --from main
eq "  at api" "$(R rev-parse api)" "$(R rev-parse u)"
check "another contains the first: a fast-forward" says 0 "created branch f at api (a fast-forward: it contains main)" branch f --from main --from api
eq "  at api" "$(R rev-parse api)" "$(R rev-parse f)"
check "contained ones dropped: a fast-forward to the one left" says 0 "at api" branch f2 --from main --from api --from main~0
eq "  at api" "$(R rev-parse api)" "$(R rev-parse f2)"
}

refused() {
setup
work clash main api.txt clash
work clash2 main ui.txt clash
R checkout -q --orphan lone; R rm -rqf .; echo z >"$REPO/z"; R add z; R commit -qm lone; R checkout -q main
check "a conflict: not implemented yet" says 1 "not implemented yet" branch c --from api --from clash
check "  nothing left behind" fails R rev-parse --verify -q refs/heads/c
check "a conflict at the last of several: not implemented yet" says 1 "not implemented yet" branch c --from x --from ui --from clash2
check "  nothing left behind" fails R rev-parse --verify -q refs/heads/c
check "a conflict before the last: refused as the octopus refuses it" says 1 "Should not be doing an octopus" branch c --from api --from clash --from x
check "  nothing left behind" fails R rev-parse --verify -q refs/heads/c
check "unrelated histories: refused, as git does" says 1 "refusing to merge unrelated histories" branch c --from api --from lone
check "an unknown revision" says 1 "nosuch - not something we can merge" branch c --from api --from nosuch
check "an existing branch" says 1 "a branch named 'ui' already exists" branch ui --from api --from x
check "-w with several: not implemented yet" says 1 "not implemented yet" branch c -w --from api --from ui
check "  nothing left behind" fails R rev-parse --verify -q refs/heads/c
}

mounted() {
setup
mount_fs
check "mounted: created at a merge" says 0 "created branch c at a merge of api and ui" branch c --from api --from ui
eq "mounted: its workspace shows the merge" "api ui" "$(cat "$MNT/c/api.txt" "$MNT/c/ui.txt" | tr '\n' ' ' | sed 's/ $//')"
eq "mounted: git status is clean" "" "$(git -C "$MNT/c" status --porcelain)"
eq "mounted: HEAD is the merge" "$(R rev-parse c)" "$(git -C "$MNT/c" rev-parse HEAD)"
unmount_fs
}

section "from: clean merges, as git makes them" clean_merges
section "from: octopus steps, as git takes them" octopus_steps
section "from: merge.ff" merge_ff
section "from: no merge needed" no_merge_needed
section "from: refused, and conflicts" refused
section "from: mounted" mounted
finish
