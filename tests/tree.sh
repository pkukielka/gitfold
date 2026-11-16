#!/usr/bin/env bash
# End-to-end test of the command-line contract of branches built on branches
# (gitfold parent, branch --parent, branch with several --from, changes --
# PATH, the hints for guessed commands): what parses, what is refused, and
# what says "not implemented yet" (exit 1) until it is built.
# usage: tests/tree.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=tree
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# gitfold run in the repository: its exit code, then its output on one line.
gf() { local out rc; out=$("$BIN" -C "$REPO" "$@" 2>&1); rc=$?; echo "$rc ${out//$'\n'/ }"; }
# Whether `gf ARGS` exits with $1 and says $2.
says() { local code=$1 text=$2; shift 2; local r; r=$(gf "$@"); [[ $r == "$code "* && $r == *"$text"* ]] || { echo "  got: $r"; false; }; }

the_contract() {
R branch api
check "branch --from one revision: as before" says 0 "created branch two" branch two --from main
check "the name may come after --from" says 0 "created branch three" branch --from main three
check "several --from: not implemented yet" says 1 "not implemented yet" branch c --from main --from api
check "--parent: not implemented yet, said plainly" says 1 "gitfold branch --parent: not implemented yet" branch c --parent api
check "no Error: prefix on not implemented" fails sh -c "'$BIN' -C '$REPO' branch c --parent api 2>&1 | grep -q '^Error:'"
check "--parent an unknown branch: exit 3" says 3 "no branch nosuch" branch c --parent nosuch
check "--parent --no-follow: not implemented yet" says 1 "not implemented yet" branch c --parent api --no-follow
check "--no-follow needs --parent" says 2 "--parent" branch c --no-follow
check "--from and --parent: refused" says 2 "cannot be used with" branch c --from main --parent api
check "-w and --parent: refused" says 2 "cannot be used with" branch c -w --parent api
check "--on: a hint" says 2 "to build on a branch: --parent P" branch c --on api
check "--merge: a hint" says 2 "--from api --from main" branch c --merge api main
check "bare --on: the hint" says 2 "--parent P" branch c --on
check "bare --merge: the hint" says 2 "--from A --from B" branch c --merge
check "parent: not implemented yet" says 1 "gitfold parent: not implemented yet" parent api
check "parent of an unknown branch: exit 3, nothing created" says 3 "no branch nosuch" parent nosuch
check "and nothing was created" fails R rev-parse -q --verify refs/heads/nosuch
check "--set an unknown branch: exit 3" says 3 "no branch nosuch" parent api --set nosuch
check "--set: not implemented yet" says 1 "not implemented yet" parent api --set main
check "--none: not implemented yet" says 1 "not implemented yet" parent api --none
check "--follow: not implemented yet" says 1 "not implemented yet" parent api --follow
check "--no-follow: not implemented yet" says 1 "not implemented yet" parent api --no-follow
check "--update --yes: not implemented yet" says 1 "not implemented yet" parent api --update --yes
check "--set and --none: refused" says 2 "cannot be used with" parent api --set main --none
check "--follow and --no-follow: refused" says 2 "cannot be used with" parent api --follow --no-follow
check "--none and --follow: refused" says 2 "cannot be used with" parent api --none --follow
check "--yes needs --update" says 2 "--update" parent api --yes
check "--update and --set: refused" says 2 "cannot be used with" parent api --update --set main
check "its own parent: refused" says 2 "can't be its own parent" parent api --set api
R branch ui; R config branch.ui.gitfoldParent api
check "a cycle: refused" says 2 "ui is built on api: that would be a cycle" parent api --set ui
check "not a cycle: not implemented yet" says 1 "not implemented yet" parent ui --set main
check "outside a workspace, the branch must be named" says 1 "not in a gitfold workspace" parent
check "changes -- PATH: not implemented yet" says 1 "not implemented yet" changes -- a.txt
check "changes BRANCH -- PATH: refused" says 2 "drop BRANCH" changes api -- a.txt
check "changes -z -- PATH: not implemented yet" says 1 "not implemented yet" changes -z -- a.txt
check "changes --stat -- PATH: refused" says 2 "cannot be used with" changes --stat -- a.txt
eq "changes without PATH: as before (no workspace yet: nothing, exit 0)" "0 " "$(gf changes api)"
check "changes -- PATH outside a repository: no repository" sh -c "cd / && ! '$BIN' changes -- a 2>&1 | grep -q 'not implemented'"
for g in sync pull restack rebase; do
    check "$g: a hint" says 2 "gitfold parent --update" "$g" --whatever x
    check "$g --help: the hint too" says 2 "gitfold parent --update" "$g" --help
done
}

# The main branch (git::main_branch), as overlaps' default target shows it:
# origin/HEAD, else main, else master, else an error naming --against.
the_main_branch() {
R branch w
R switch -q w; echo w >>"$REPO/a.txt"; R commit -qam w; R switch -q main
echo m >>"$REPO/a.txt"; R commit -qam m
check "main: chosen" says 6 "W	main	a.txt" overlaps w
R update-ref refs/remotes/origin/main main
R symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main
check "origin/HEAD: its branch chosen" says 6 "W	origin/main	a.txt" overlaps w
R branch origin/main main~1
echo m2 >>"$REPO/c.txt"; R add c.txt; R commit -qm m2; R update-ref refs/remotes/origin/main main
check "a stale local branch named like it: origin/main still compared" says 6 "W	origin/main	a.txt" overlaps w
check "and its newer change seen (the remote's, not the stale branch's)" sh -c "'$BIN' -C '$REPO' overlaps w --writes | grep -q 'W	origin/main	a.txt'"
R branch -D -q origin/main; R update-ref -d refs/remotes/origin/main
check "origin/HEAD pointing nowhere: main" says 6 "W	main	a.txt" overlaps w
R update-ref -d --no-deref refs/remotes/origin/HEAD
R update-ref refs/remotes/origin/main main; R branch -m main mn
check "no origin/HEAD, no main or master, origin/main: chosen" says 6 "W	origin/main	a.txt" overlaps w
R branch -m mn main; R update-ref -d refs/remotes/origin/main
R branch -m main master
check "no main, master: chosen" says 6 "W	master	a.txt" overlaps w
R branch -m master trunk
check "neither: an error naming --against" says 1 "no main branch: no origin/HEAD, and no branch main or master: name a target with --against" overlaps w
}

section "tree: the command-line contract" the_contract
section "tree: the main branch" the_main_branch
finish
