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
check "--none: not implemented yet" says 1 "not implemented yet" parent api --none
check "--update --yes: not implemented yet" says 1 "not implemented yet" parent api --update --yes
check "--set and --none: refused" says 2 "cannot be used with" parent api --set main --none
check "--follow and --no-follow: refused" says 2 "cannot be used with" parent api --follow --no-follow
check "--none and --follow: refused" says 2 "cannot be used with" parent api --none --follow
check "--yes needs --update" says 2 "--update" parent api --yes
check "--update and --set: refused" says 2 "cannot be used with" parent api --update --set main
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

# A clone's remote: origin/main (at main, or $1) with origin/HEAD at it.
remote_main() { R update-ref refs/remotes/origin/main "${1:-main}"; R symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main; }

# Two questions about "the main branch" that must stay separate (see the
# note in src/git/mod.rs): which commit overlaps compares with by default
# (local main and origin/main differ), and which branch is the tree's root
# (local main and origin/main are the same). Each check below would fail if
# the two were merged into one rule. The root's checks are the rules
# table's rows R1 and R3-R5 (layers-design/16-root-rules.md); R2 follows.
two_questions_about_main() {
R branch w
R switch -q w; echo w >>"$REPO/a.txt"; R commit -qam w; R switch -q main
echo m >>"$REPO/a.txt"; R commit -qam m
check "compare: local main" says 6 "W	main	a.txt" overlaps w
check "R4: root: main" says 2 "main is the root of the tree" parent main --set w
check "building on the root: allowed (no parent stored)" says 1 "not implemented yet" parent w --set main
# A clone: origin/main at main, origin/HEAD pointing at it; local main
# gets an unpushed commit of its own.
remote_main
echo u >"$REPO/u.txt"; R add u.txt; R commit -qm unpushed
check "compare: origin/main, not local main" says 6 "W	origin/main	a.txt" overlaps w
check "compare main with origin/main: allowed (they differ)" says 0 "" overlaps main
check "R1: origin/HEAD -> origin/main: root is local main" says 2 "main is the root of the tree" parent main --set w
# A stale local branch named origin/main (at the first commit).
R branch origin/main main~2
check "compare: still the remote's origin/main" says 6 "remotes/origin/main	a.txt" overlaps w
R branch -D -q origin/main
# origin/HEAD pointing nowhere: compare falls back to local main.
R update-ref -d refs/remotes/origin/main
check "dangling origin/HEAD: compare with local main" says 6 "W	main	a.txt" overlaps w
check "R3: dangling origin/HEAD: root is local main" says 2 "main is the root of the tree" parent main --set w
R update-ref -d --no-deref refs/remotes/origin/HEAD
# Only master: overlaps has no default (as on main); the tree's root is master.
R branch -m main master
check "only master: compare needs --against" says 1 "no origin/HEAD and no branch main: name a target with --against" overlaps w
check "R4: only master: root is master" says 2 "master is the root of the tree" parent master --set w
# No main branch at all: no root, so no parents.
R branch -m master trunk
check "R5, S3: no root: setting a parent refused (exit 1)" says 1 "the tree of branches has no root" parent w --set trunk
check "N3: no root: a new branch with a parent refused (exit 1)" says 1 "the tree of branches has no root" branch c --parent trunk
}

# The rest of the rules table of the root, parents and names (blog notes,
# layers-design/16-root-rules.md): one check per row, named after it.

the_root() {
R branch api; R branch develop
R update-ref refs/remotes/origin/develop develop; R symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/develop
check "R2: origin/HEAD -> origin/develop: develop is the root" says 2 "develop is the root of the tree" parent develop --set api
check "R2: and local main is an ordinary branch" says 1 "not implemented yet" parent main --set api
}

parent_names() {
R branch api
check "P1: a local branch" says 1 "not implemented yet" parent api --set main
remote_main; R update-ref refs/remotes/origin/feat main
check "P2: a remote-tracking branch" says 1 "not implemented yet" parent api --set origin/feat
check "P3: a full ref" says 1 "not implemented yet" parent api --set refs/remotes/origin/feat
check "P3: a full ref that doesn't exist" says 3 "no branch refs/heads/nope" parent api --set refs/heads/nope
R branch origin/feat main
check "P4: both local and remote-tracking: ambiguous (exit 2)" says 2 "origin/feat is both a local branch and a remote-tracking branch" parent api --set origin/feat
check "P4: the full ref resolves it" says 1 "not implemented yet" parent api --set refs/remotes/origin/feat
R branch -D -q origin/feat
check "P5: only origin/feat: both ways out (exit 3)" says 3 "no branch feat (only origin/feat exists). Either: " parent api --set feat
check "P5: naming both commands" says 3 "git branch feat origin/feat" parent api --set feat
check "P6: nothing of that name (exit 3)" says 3 "no branch nope" parent api --set nope
check "P5 for branch --parent too" says 3 "only origin/feat exists" branch c --parent feat
}

parent_set() {
R branch api; R branch ui; R config branch.ui.gitfoldParent refs/heads/api
check "S1: B doesn't exist (exit 3)" says 3 "gitfold parent: no branch nope" parent nope --set api
check "S1: and nothing was created" fails R rev-parse -q --verify refs/heads/nope
check "S2: P doesn't resolve (exit 3)" says 3 "gitfold parent: no branch nope" parent api --set nope
check "S4: B is the root (exit 2)" says 2 "main is the root of the tree" parent main --set api
check "S5: P is B (exit 2)" says 2 "a branch can't be its own parent" parent api --set api
check "S6: a cycle (exit 2)" says 2 "ui is built on api: that would be a cycle" parent api --set ui
check "S7: accepted (not implemented yet)" says 1 "gitfold parent: not implemented yet" parent ui --set main
check "S7: the root as a parent, accepted" says 1 "not implemented yet" parent api --set main
check "F6: --follow without a parent (exit 2)" says 2 "api has no parent to follow" parent api --follow
check "F6: --no-follow without a parent (exit 2)" says 2 "api has no parent to follow" parent api --no-follow
check "F6: with a parent: accepted (not implemented yet)" says 1 "not implemented yet" parent ui --no-follow
}

branch_parent() {
R branch api
check "N1: NEW exists: refused as branch refuses it (exit 1)" says 1 "a branch named 'api' already exists" branch api --parent main
check "N2: P doesn't resolve (exit 3)" says 3 "gitfold branch: no branch nope" branch c --parent nope
remote_main; R branch -m main mn
check "N4: NEW is the root's name (exit 2)" says 2 "main is the root of the tree" branch main --parent api
R branch -m mn main
check "N5: accepted (not implemented yet)" says 1 "gitfold branch --parent: not implemented yet" branch c --parent api
check "N5: on a remote-tracking parent too" says 1 "not implemented yet" branch c --parent origin/main
}

stored_tree() {
R branch api; R branch ui
R config branch.main.gitfoldParent refs/heads/ui
R config branch.ui.gitfoldParent refs/heads/api
check "T1: a parent stored on the root is ignored (no cycle through it)" says 1 "not implemented yet" parent api --set main
R config branch.api.gitfoldParent refs/heads/ui
check "T4: a hand-made cycle: walks stop, joining it refused" says 2 "would be a cycle" parent api --set ui
R config --unset branch.api.gitfoldParent
printf '[branch "x"\n' >>"$REPO/.git/config"
check "T5: an unreadable config fails (exit 1)" says 1 "configuration could not be loaded" parent api --set main
}

section "tree: the command-line contract" the_contract
section "tree: two questions about main" two_questions_about_main
section "tree: rules R (the root)" the_root
section "tree: rules P (parent names)" parent_names
section "tree: rules S, F (parent --set)" parent_set
section "tree: rules N (branch --parent)" branch_parent
section "tree: rules T (the stored tree)" stored_tree
finish
