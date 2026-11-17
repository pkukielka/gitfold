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
# Whether `gf ARGS` exits with $1 and doesn't say $2.
lacks() { local code=$1 text=$2; shift 2; local r; r=$(gf "$@"); [[ $r == "$code "* && $r != *"$text"* ]] || { echo "  got: $r"; false; }; }

the_contract() {
R branch api
check "branch --from one revision: as before" says 0 "created branch two" branch two --from main
check "the name may come after --from" says 0 "created branch three" branch --from main three
check "several --from: not implemented yet" says 1 "not implemented yet" branch c --from main --from api
check "several --from: said plainly" says 1 "gitfold branch with several --from: not implemented yet" branch c --from main --from api
check "no Error: prefix on not implemented" fails sh -c "'$BIN' -C '$REPO' branch c --from main --from api 2>&1 | grep -q '^Error:'"
check "--parent: created on P, following it" says 0 "created branch c on api (following it)" branch c --parent api
check "--parent an unknown branch: exit 3" says 3 "no branch nosuch" branch x --parent nosuch
check "--parent --no-follow: created, not following" says 0 "created branch e on api (not following it)" branch e --parent api --no-follow
check "--no-follow needs --parent" says 2 "--parent" branch c --no-follow
check "--from and --parent: refused" says 2 "cannot be used with" branch c --from main --parent api
check "-w and --parent: refused" says 2 "cannot be used with" branch c -w --parent api
check "--on: a hint" says 2 "to build on a branch: --parent P" branch c --on api
check "--merge: a hint" says 2 "--from api --from main" branch c --merge api main
check "bare --on: the hint" says 2 "--parent P" branch c --on
check "bare --merge: the hint" says 2 "--from A --from B" branch c --merge
check "parent: shown" says 0 "api has no parent" parent api
check "--none: done" says 0 "api has no parent" parent api --none
check "--set: not implemented yet" says 1 "gitfold parent: not implemented yet" parent c --set main
check "--update: not implemented yet" says 1 "gitfold parent: not implemented yet" parent c --update
check "--update --yes: not implemented yet" says 1 "not implemented yet" parent api --update --yes
check "--update --no-follow: not implemented yet, following unchanged" says 1 "not implemented yet" parent c --update --no-follow
check "  (still following)" says 0 "c is built on api, following it" parent c
check "--set and --none: refused" says 2 "cannot be used with" parent api --set main --none
check "--follow and --no-follow: refused" says 2 "cannot be used with" parent api --follow --no-follow
check "--none and --follow: refused" says 2 "cannot be used with" parent api --none --follow
check "--yes needs --update" says 2 "--update" parent api --yes
check "--update and --set: refused" says 2 "cannot be used with" parent api --update --set main
check "outside a workspace, the branch must be named" says 1 "not in a gitfold workspace" parent
eq "changes -- PATH: nobody changes it (exit 0)" "0 " "$(gf changes -- a.txt)"
check "changes BRANCH -- PATH: refused" says 2 "drop BRANCH" changes api -- a.txt
eq "changes -z -- PATH: nobody changes it (exit 0)" "0 " "$(gf changes -z -- a.txt)"
check "changes --stat -- PATH: refused" says 2 "cannot be used with" changes --stat -- a.txt
eq "changes without PATH: as before (no workspace yet: nothing, exit 0)" "0 " "$(gf changes api)"
check "changes -- PATH outside a repository: no repository" sh -c "cd / && ! '$BIN' changes -- a && '$BIN' changes -- a 2>&1 | grep -q 'no git repository'"
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
check "S7: building on the root: allowed (--set not implemented yet)" says 1 "not implemented yet" parent w --set main
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
check "F6: no root: following needs only a parent" says 2 "w has no parent to follow" parent w --follow
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
R remote add origin /nonexistent; remote_main; R update-ref refs/remotes/origin/feat main
check "P2: a remote-tracking branch" says 1 "not implemented yet" parent api --set origin/feat
check "P3: a full ref" says 1 "not implemented yet" parent api --set refs/remotes/origin/feat
check "P3: a full ref that doesn't exist" says 3 "no branch refs/heads/nope" parent api --set refs/heads/nope
R branch origin/feat main
check "P4: local and remote-tracking of one name: the local branch, as in git" says 1 "not implemented yet" parent api --set origin/feat
R branch -D -q origin/feat
check "P5: only origin/feat: both ways out (exit 3)" says 3 "no branch feat (only origin/feat exists). Either: " parent api --set feat
check "P5: naming both commands" says 3 "git branch feat origin/feat" parent api --set feat
check "P5 for branch --parent too" says 3 "only origin/feat exists" branch c --parent feat
check "P6: nothing of that name (exit 3)" says 3 "no branch nope" parent api --set nope
R tag v1 main
check "P6: a tag is no branch" says 3 "no branch v1: git takes it for refs/tags/v1" parent api --set v1
R branch v1 main
check "P6: a tag shadows a branch of its name, as in git" says 3 "no branch v1: git takes it for refs/tags/v1" parent api --set v1
check "P3: the full ref names the branch" says 1 "not implemented yet" parent api --set refs/heads/v1
R switch -q --orphan unborn
check "P5: never origin/HEAD for HEAD (unborn HEAD)" lacks 3 "only origin" parent api --set HEAD
R switch -q main
# Names stand for what git takes them for: symbolic refs followed.
check "origin/HEAD stands for origin/main" says 1 "not implemented yet" parent api --set origin/HEAD
check "origin stands for origin/main, as in git" says 1 "not implemented yet" parent api --set origin
R symbolic-ref refs/heads/trunk refs/heads/main
check "an alias stands for its branch" says 1 "not implemented yet" parent api --set trunk
check "S4: an alias of the root is the root" says 2 "main is the root of the tree" parent trunk --set api
# P5 looks at configured remotes only, by name.
R remote add team/a /nonexistent; R update-ref refs/remotes/team/a/x main
check "P5: a remote whose name has a slash" says 3 "no branch x (only team/a/x exists)" parent api --set x
R update-ref refs/remotes/origin/x main
check "P5: origin's preferred" says 3 "only origin/x exists" parent api --set x
R remote add origin/fork /nonexistent; R update-ref refs/remotes/origin/fork/q main
R remote add alpha /nonexistent; R update-ref refs/remotes/alpha/q main
check "P5: origin by its name, not a prefix" says 3 "only alpha/q exists" parent api --set q
R update-ref refs/remotes/gone/y main
check "P5: not for a remote no longer configured" lacks 3 "only" parent api --set y
R update-ref refs/remotes/origin/fix/test main
check "P5: not origin/fix/test for test" lacks 3 "only" parent api --set test
}

parent_set() {
R branch api; R branch ui; R config branch.ui.gitfoldParent refs/heads/api
check "S1: B doesn't exist (exit 3)" says 3 "gitfold parent: no branch nope" parent nope --set api
check "S1: and nothing was created" fails R rev-parse -q --verify refs/heads/nope
check "S1: without --set too (exit 3)" says 3 "gitfold parent: no branch nope" parent nope
check "S2: P doesn't resolve (exit 3)" says 3 "gitfold parent: no branch nope" parent api --set nope
check "S4: B is the root (exit 2)" says 2 "main is the root of the tree" parent main --set api
check "S5: P is B (exit 2)" says 2 "a branch can't be its own parent" parent api --set api
check "S6: a cycle (exit 2)" says 2 "ui is built on api: that would be a cycle" parent api --set ui
check "S7: accepted (not implemented yet)" says 1 "gitfold parent: not implemented yet" parent ui --set main
check "S7: the root as a parent, accepted" says 1 "not implemented yet" parent api --set main
check "F6: --follow without a parent (exit 2)" says 2 "api has no parent to follow" parent api --follow
check "F6: --no-follow without a parent (exit 2)" says 2 "api has no parent to follow" parent api --no-follow
check "F6: with a parent: --no-follow accepted" says 0 "ui doesn't follow api" parent ui --no-follow
check "F6: with a parent: --follow accepted" says 0 "ui follows api" parent ui --follow
check "F6 on the root: the root has no parent (S4)" says 2 "main is the root of the tree" parent main --follow
}

branch_parent() {
R branch api
check "N1: NEW exists: refused as branch refuses it (exit 1)" says 1 "a branch named 'api' already exists" branch api --parent main
check "N2: P doesn't resolve (exit 3)" says 3 "gitfold branch: no branch nope" branch c --parent nope
remote_main; R branch -m main mn
check "N4: NEW is the root's name (exit 2)" says 2 "main is the root of the tree" branch main --parent api
R branch -m mn main
check "N5: accepted" says 0 "created branch c on api" branch c --parent api
eq "N5: P stored as a full ref" "refs/heads/api" "$(R config branch.c.gitfoldParent)"
eq "N5: at P's tip" "$(R rev-parse api)" "$(R rev-parse c)"
check "N5: on a remote-tracking parent too" says 0 "created branch d on origin/main" branch d --parent origin/main
eq "N5: stored as the remote-tracking ref" "refs/remotes/origin/main" "$(R config branch.d.gitfoldParent)"
check "N5: shown short" says 0 "d is built on origin/main, following it" parent d
}

# Piece 1 of following: the relation stored, shown, forgotten, and
# following turned on and off (F1-F6, T1-T3).
relation() {
echo api >"$REPO/api.txt"; R add api.txt; R commit -qm api; R branch api; R reset -q --hard HEAD~1
check "N5: --no-follow" says 0 "created branch ui on api (not following it)" branch ui --parent api --no-follow --prewarm none --track-reads off
eq "  following stored off" "false" "$(R config branch.ui.gitfoldFollow)"
eq "  the other settings applied too" "none false" "$(R config branch.ui.gitfoldPrewarm) $(R config branch.ui.gitfoldTrackReads)"
check "shown: not following" says 0 "ui is built on api, not following it" parent ui
check "--follow" says 0 "ui follows api" parent ui --follow
check "  stored as the default (unset)" fails R config branch.ui.gitfoldFollow
check "F1: following never set: follows" says 0 "ui is built on api, following it" parent ui
check "--no-follow" says 0 "ui doesn't follow api" parent ui --no-follow
check "F5: --none" says 0 "ui no longer has a parent (it was api)" parent ui --none
check "  parent forgotten" fails R config branch.ui.gitfoldParent
check "  following forgotten" fails R config branch.ui.gitfoldFollow
eq "  history kept as it is" "$(R rev-parse api)" "$(R rev-parse ui)"
check "F2: shown without a parent" says 0 "ui has no parent" parent ui
check "--none again: nothing to forget" says 0 "ui has no parent" parent ui --none
check "the root is shown as the root" says 0 "main is the root of the tree; it has no parent" parent main
R config branch.main.gitfoldParent refs/heads/api
check "T1: a parent stored on the root: not shown" says 0 "main is the root of the tree" parent main
check "T1: --none on the root forgets it" says 0 "main is the root of the tree; it has no parent" parent main --none
check "  gone from the config" fails R config branch.main.gitfoldParent
R config branch.ui.gitfoldParent api
check "a short stored value is a local branch" says 0 "ui is built on api, following it" parent ui
R branch -D -q api
check "T3: parent gone" says 0 "ui is built on api (parent gone), following it" parent ui
check "T3: --no-follow still possible" says 0 "ui doesn't follow api" parent ui --no-follow
check "--no-follow again: the same" says 0 "ui doesn't follow api" parent ui --no-follow
R config branch.ui.gitfoldFollow maybe
check "--follow replaces a malformed value" says 0 "ui follows api" parent ui --follow
check "  (unset: on)" fails R config branch.ui.gitfoldFollow
R config branch.ui.gitfoldFollow maybe
check "--no-follow replaces it too" says 0 "ui doesn't follow api" parent ui --no-follow
eq "  (stored off)" "false" "$(R config branch.ui.gitfoldFollow)"
# A remote-tracking parent isn't the upstream (git branch would make it one).
R remote add origin /nonexistent; R update-ref refs/remotes/origin/main main
check "a remote-tracking parent" says 0 "created branch rt on origin/main" branch rt --parent origin/main
check "  sets no upstream: no branch.rt.remote" fails R config branch.rt.remote
check "  nor branch.rt.merge" fails R config branch.rt.merge
R config branch.autoSetupMerge always
check "a local parent with branch.autoSetupMerge=always" says 0 "created branch lt on main" branch lt --parent main
check "  sets no upstream either" fails R config branch.lt.remote
# Settings a deleted branch left (T2) don't carry over to a new one.
R update-ref -d refs/heads/ui
check "N5: following on, whatever a deleted branch of its name left" says 0 "created branch ui on main (following it)" branch ui --parent main
check "  and shown so" says 0 "ui is built on main, following it" parent ui
check "an invalid name is refused first, as git branch does" says 1 "is not a valid branch name" branch 'a..b' --parent nosuch
# The parent can't be stored: the branch goes again, so it can be retried.
touch "$REPO/.git/config.lock"
check "the parent not stored: refused" says 1 "" branch half --parent main
rm "$REPO/.git/config.lock"
check "  and the branch gone again" fails R rev-parse -q --verify refs/heads/half
check "  so it can be retried" says 0 "created branch half on main" branch half --parent main
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
section "tree: the relation (F, T)" relation
finish
