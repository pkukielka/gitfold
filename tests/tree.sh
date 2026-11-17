#!/usr/bin/env bash
# End-to-end test of the command-line contract of branches built on branches
# (gitfold parent, branch --parent, branch with several --from, changes --
# PATH, the hints for guessed commands): what parses, what is refused, and
# what the commands do.
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
check "several --from, never mounted: refused, nothing made" says 1 "mount it first" branch c --from main --from api
check "-w with several --from, never mounted: refused, nothing made" says 1 "mount it first" branch c -w --from main --from api
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
check "--update" says 0 "c is up to date with api" parent c --update
check "--update without a parent (exit 2)" says 2 "api has no parent to follow" parent api --update --yes
check "--update --no-follow: following off, then updated" says 0 "c is up to date with api" parent c --update --no-follow
check "  (not following)" says 0 "c is built on api, not following it" parent c
check "--set" says 0 "c is now built on main" parent c --set main
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
check "S7: building on the root: allowed (a conflict here: nothing moved, exit 4)" says 4 "w's commits conflict with main's on a.txt" parent w --set main
"$BIN" -C "$REPO" parent w --none >/dev/null
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
check "R2: and local main is an ordinary branch (checked out here: not moved, exit 5)" says 5 "main is checked out in" parent main --set api
}

parent_names() {
R branch api
check "P1: a local branch" says 0 "api is now built on main" parent api --set main
R remote add origin /nonexistent; remote_main; R update-ref refs/remotes/origin/feat main
check "P2: a remote-tracking branch" says 0 "api is now built on origin/feat" parent api --set origin/feat
check "P3: a full ref" says 0 "api is built on origin/feat already" parent api --set refs/remotes/origin/feat
check "P3: a full ref that doesn't exist" says 3 "no branch refs/heads/nope" parent api --set refs/heads/nope
R branch origin/feat main
check "P4: local and remote-tracking of one name: the local branch, as in git" says 0 "api is now built on origin/feat" parent api --set origin/feat
eq "  stored as the local branch" "refs/heads/origin/feat" "$(R config branch.api.gitfoldParent)"
R branch -D -q origin/feat
check "P5: only origin/feat: both ways out (exit 3)" says 3 "no branch feat (only origin/feat exists). Either: " parent api --set feat
check "P5: naming both commands" says 3 "git branch feat origin/feat" parent api --set feat
check "P5 for branch --parent too" says 3 "only origin/feat exists" branch c --parent feat
check "P6: nothing of that name (exit 3)" says 3 "no branch nope" parent api --set nope
R tag v1 main
check "P6: a tag is no branch" says 3 "no branch v1: git takes it for refs/tags/v1" parent api --set v1
R branch v1 main
check "P6: a tag shadows a branch of its name, as in git" says 3 "no branch v1: git takes it for refs/tags/v1" parent api --set v1
check "P3: the full ref names the branch" says 0 "api is now built on v1" parent api --set refs/heads/v1
R switch -q --orphan unborn
check "P5: never origin/HEAD for HEAD (unborn HEAD)" lacks 3 "only origin" parent api --set HEAD
R switch -q main
# Names stand for what git takes them for: symbolic refs followed.
check "origin/HEAD stands for origin/main" says 0 "api is now built on origin/main" parent api --set origin/HEAD
check "origin stands for origin/main, as in git" says 0 "api is built on origin/main already" parent api --set origin
R symbolic-ref refs/heads/trunk refs/heads/main
check "an alias stands for its branch" says 0 "api is now built on main" parent api --set trunk
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
R branch api; R branch ui; R branch web; R config branch.ui.gitfoldParent refs/heads/api
check "S1: B doesn't exist (exit 3)" says 3 "gitfold parent: no branch nope" parent nope --set api
check "S1: and nothing was created" fails R rev-parse -q --verify refs/heads/nope
check "S1: without --set too (exit 3)" says 3 "gitfold parent: no branch nope" parent nope
check "S2: P doesn't resolve (exit 3)" says 3 "gitfold parent: no branch nope" parent api --set nope
check "S4: B is the root (exit 2)" says 2 "main is the root of the tree" parent main --set api
check "S5: P is B (exit 2)" says 2 "a branch can't be its own parent" parent api --set api
check "S6: a cycle (exit 2)" says 2 "ui is built on api: that would be a cycle" parent api --set ui
check "S7: accepted" says 0 "ui is now built on main" parent ui --set main
check "S7: the root as a parent, accepted" says 0 "web is now built on main" parent web --set main
check "F6: --follow without a parent (exit 2)" says 2 "api has no parent to follow" parent api --follow
check "F6: --no-follow without a parent (exit 2)" says 2 "api has no parent to follow" parent api --no-follow
check "F6: with a parent: --no-follow accepted" says 0 "ui doesn't follow main" parent ui --no-follow
check "F6: with a parent: --follow accepted" says 0 "ui follows main" parent ui --follow
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
check "T1: a parent stored on the root is ignored (no cycle through it)" says 0 "api is now built on main" parent api --set main
R config branch.api.gitfoldParent refs/heads/ui
check "T4: a hand-made cycle: walks stop, joining it refused" says 2 "would be a cycle" parent api --set ui
R config --unset branch.api.gitfoldParent
printf '[branch "x"\n' >>"$REPO/.git/config"
check "T5: an unreadable config fails (exit 1)" says 1 "configuration could not be loaded" parent api --set main
}

# Moves of branches without a workspace (spec sections 2 and 3): git rebase
# does them, in a worktree of its own.
# A commit on branch $1 (not checked out after) appending $3 to file $2.
on() { R switch -q "$1"; echo "$3" >>"$REPO/$2"; R add "$2"; R commit -qm "$1: $3"; R switch -q main; }
own() { R rev-list --count "$1..$2"; }
# git rebase commits: it needs a committer, as anywhere.
identity() { R config user.name t; R config user.email t@t; }

moves() {
identity; on main f.txt base
R branch api
check "branch --parent records where its own commits start" says 0 "created branch ui on api" branch ui --parent api
eq "  at P's tip" "$(R rev-parse api)" "$(R rev-parse refs/gitfold-base/ui)"
on ui u.txt one; on ui u.txt two; on api a.txt new
check "--update: moved onto the parent's new commits" says 0 "moved 2 commits of ui onto api" parent ui --update
check "  ui sits on api" R merge-base --is-ancestor api ui
eq "  with its own commits only" "2" "$(own api ui)"
eq "  its base recorded" "$(R rev-parse api)" "$(R rev-parse refs/gitfold-base/ui)"
check "  no move left behind" fails R config branch.ui.gitfoldMoving
check "  nor a worktree" sh -c "! git -C '$REPO' worktree list | grep -q gitfold-move"
check "up to date" says 0 "ui is up to date with api" parent ui --update
# api takes one of ui's changes as it is: git rebase drops it from ui.
R switch -q api; R cherry-pick -x ui~1 >/dev/null; R switch -q main
check "a commit already in the parent: dropped, as git rebase does" says 0 "(1 commit already in api: dropped)" parent ui --update
eq "  one own commit left" "1" "$(own api ui)"
# api rewritten (its last commit amended): only ui's own commit moves.
R switch -q api; R commit -q --amend -m "api: amended"; R switch -q main
check "a parent rewritten: only its own commits move" says 0 "moved 1 commit of ui onto api" parent ui --update
eq "  one own commit" "1" "$(own api ui)"
# A conflict: nothing moves, recorded until api changes again.
on api u.txt theirs; on ui u.txt mine
before=$(R rev-parse ui)
check "a conflict: exit 4, the paths named, the rebase to run by hand" says 4 "ui's commits conflict with api's on u.txt. Rebase by hand (git rebase api ui)" parent ui --update
eq "  nothing moved" "$before" "$(R rev-parse ui)"
check "  no rebase left anywhere" sh -c "! git -C '$REPO' worktree list | grep -q gitfold-move"
check "  shown by parent" says 0 "following paused: ui's commits conflict with api's on u.txt" parent ui
R switch -q api; R revert --no-edit HEAD >/dev/null; R switch -q main
check "  forgotten once the parent changed" lacks 0 "conflict" parent ui
check "  and the move goes through" says 0 "moved 2 commits of ui onto api" parent ui --update
check "  the record cleared" fails R config branch.ui.gitfoldConflict
# api takes one of ui's commits, then changes the same line again: git
# rebase skips the commit by patch id (replayed, it would conflict).
on ui g.txt mine; on ui h.txt other
R switch -q api; R cherry-pick -x ui~1 >/dev/null; echo again >>"$REPO/g.txt"; R commit -qam "api: again"; R switch -q main
check "a commit the parent has, changed since: skipped by patch id" says 0 "(1 commit already in api: dropped)" parent ui --update
# ui takes api's new commits by hand (a merge, fast-forward here, of api),
# then api moves on: api's commits aren't ui's own.
on api a.txt more; R switch -q ui; R rebase -q api; R switch -q main
own_before=$(own api ui)
on api a.txt evenmore
check "commits taken by hand: only its own move" says 0 "moved $own_before commits of ui onto api" parent ui --update
# gitfold run with git's environment set (from a hook): its rebase still
# acts on the branch, not on the checkout the environment points at.
on api a.txt hooked; head=$(R rev-parse HEAD)
check "git's environment set: moved all the same" sh -c "cd / && GIT_DIR='$REPO/.git' GIT_WORK_TREE='$REPO' GIT_INDEX_FILE='$REPO/.git/index' '$BIN' -C '$REPO' parent ui --update"
eq "  the checkout untouched" "$head refs/heads/main" "$(R rev-parse HEAD) $(R symbolic-ref HEAD)"
check "  and clean" sh -c "[ -z \"\$(git -C '$REPO' status --porcelain)\" ]"
check "moves don't make the repository look used with mounts" test ! -e "$REPO/.git/gitfold"
# A merge in the parent's history isn't one of the branch's own commits.
R branch side main; on side s.txt s
R switch -q api; R merge -q --no-ff --no-edit side; R switch -q main
R branch plain api; on plain p.txt p; on api a.txt after
check "a merge in the parent's history: moved all the same" says 0 "moves 1 commit of plain onto api" parent plain --set api
# Conflicting paths are said and recorded as git quotes them.
"$BIN" -C "$REPO" branch q --parent api >/dev/null
on q 'n"q.txt' mine; on api 'n"q.txt' theirs
check "a conflict on a path git quotes: quoted" says 4 'conflict with api'"'"'s on "n\"q.txt"' parent q --update
eq "  recorded so" '"n\"q.txt"' "$(R config branch.q.gitfoldConflict | sed -n 2p)"
check "  shown as a pause while following" says 0 "following paused" parent q
R config branch.q.gitfoldFollow false
check "  not while not following" lacks 0 "paused" parent q
}

pushed() {
identity; on main f.txt base
R branch api; "$BIN" -C "$REPO" branch ui --parent api >/dev/null
on ui u.txt one; on ui u.txt two
R remote add origin /nonexistent; R update-ref refs/remotes/origin/ui ui~1
on api a.txt new
before=$(R rev-parse ui)
check "pushed commits: not moved" says 1 "1 commit is pushed. \`gitfold parent ui --update --yes\` moves them; then force-push with: git push --force-with-lease=ui:$(R rev-parse ui~1) origin ui" parent ui --update
eq "  nothing moved" "$before" "$(R rev-parse ui)"
check "  shown by parent" says 0 "following paused: 1 commit is pushed" parent ui
check "--yes moves them, and says how to push" says 0 "force-push with: git push --force-with-lease=ui:$(R rev-parse ui~1) origin ui" parent ui --update --yes
check "  moved" R merge-base --is-ancestor api ui
# Only its same-name branch counts: not its upstream (here another branch).
R update-ref -d refs/remotes/origin/ui
R update-ref refs/remotes/origin/other ui; R config branch.ui.remote origin; R config branch.ui.merge refs/heads/other
on api a.txt newer
check "never compared with its upstream" says 0 "moved 2 commits of ui onto api" parent ui --update
# --set with pushed commits: built on the new parent, following paused.
R update-ref refs/remotes/origin/ui ui
check "--set with pushed commits: the parent set, the move paused" says 0 "ui is now built on main; following paused: 2 commits are pushed" parent ui --set main
eq "  stored" "refs/heads/main" "$(R config branch.ui.gitfoldParent)"
check "  then --update --yes moves only its own commits" says 0 "moved 2 commits of ui onto main" parent ui --update --yes
eq "  api's commits gone from it" "2" "$(own main ui)"
# A branch built on its own remote branch: its commits there aren't
# "pushed" ones to protect (they are its parent's).
R update-ref refs/remotes/origin/feat main; "$BIN" -C "$REPO" branch feat --parent origin/feat >/dev/null
# Pushed, then a collaborator's commit on top of it, fetched; then one more
# of its own.
on feat x.txt mine; R update-ref refs/remotes/origin/feat feat
R switch -q --detach feat; echo theirs >"$REPO/y.txt"; R add y.txt; R commit -qm theirs
R update-ref refs/remotes/origin/feat HEAD; R switch -q main
on feat z.txt mine2
check "built on its own remote branch: not paused" says 0 "moved 1 commit of feat onto origin/feat" parent feat --update
}

not_moved() {
identity; on main f.txt base
R branch api; "$BIN" -C "$REPO" branch ui --parent api >/dev/null
on ui u.txt one; on api a.txt new
# A merge among its own commits.
R branch side main; on side s.txt s
R switch -q ui; R merge -q --no-ff --no-edit side; R switch -q main
check "a merge among its own commits: not moved" says 1 "ui's own commits include a merge" parent ui --update
R branch elsewhere main; on elsewhere e.txt e
check "--set with such a merge: refused" says 1 "ui's own commits include a merge" parent ui --set elsewhere
eq "  its parent unchanged" "refs/heads/api" "$(R config branch.ui.gitfoldParent)"
check "--set where it sits already, merge and all: up to date" says 0 "ui is now built on main" parent ui --set main
R config branch.ui.gitfoldParent refs/heads/api
R switch -q ui; R reset -q --hard HEAD~1; R switch -q main
# Checked out in a git worktree: never moved by gitfold.
R worktree add -q "$T/wt" ui
check "checked out in a git worktree: refused (exit 5)" says 5 "ui is checked out in $T/wt" parent ui --update
R worktree remove "$T/wt"
# Checked out in a git worktree meanwhile (by a hook, while it moves): git
# rebase there, not --update (which refuses it).
cat >"$REPO/.git/hooks/post-rewrite" <<EOF
#!/bin/sh
env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE git -C '$REPO' worktree add -q '$T/wt' ui
EOF
chmod +x "$REPO/.git/hooks/post-rewrite"
before=$(R rev-parse ui)
check "checked out in a git worktree meanwhile: git rebase there (exit 5)" says 5 "ui was checked out in $T/wt meanwhile: gitfold doesn't move it there; take the new commits with git rebase in it (git rebase api ui)" parent ui --update
eq "  nothing moved" "$before" "$(R rev-parse ui)"
rm "$REPO/.git/hooks/post-rewrite"; R worktree remove "$T/wt"
# A workspace's branch in a repository never mounted: mount it first.
mkdir -p "$REPO/.git/gitfold/ui"
check "a workspace's branch, never mounted: mount it first" says 1 "mount it first (\`gitfold mount\`)" parent ui --update
r=$(gf parent ui --set elsewhere)
check "--set, never mounted: written, then --update finishes" test "${r%% *} $([[ $r == *"ui is now built on elsewhere "*"isn't mounted: mount it first (\`gitfold mount\`), then: \`gitfold parent ui --update\`"* ]] && echo yes)" = "1 yes"
eq "  its parent written" "refs/heads/elsewhere" "$(R config branch.ui.gitfoldParent)"
R config branch.ui.gitfoldParent refs/heads/api
rmdir "$REPO/.git/gitfold/ui"
# A move a crash interrupted after git moved the branch: its base recorded.
R update-ref refs/gitfold-base/ui main~1
R config branch.ui.gitfoldMoving "$(R rev-parse main~1) $(R rev-parse api~1)"
check "an interrupted move: finished by the next move" says 0 "moved 1 commit of ui onto api" parent ui --update
eq "  only its own commit moved (its base was recorded first)" "1" "$(own api ui)"
check "  the record gone" fails R config branch.ui.gitfoldMoving
# A move recorded that never happened (the branch is where it was, though
# on the target's history): forgotten, nothing written.
R config branch.ui.gitfoldMoving "$(R rev-parse ui) $(R rev-parse main~1)"
check "a recorded move that didn't happen: forgotten" says 0 "ui is up to date with api" parent ui --update
eq "  its base unchanged" "$(R rev-parse api)" "$(R rev-parse refs/gitfold-base/ui)"
check "  the record gone" fails R config branch.ui.gitfoldMoving
# A record that can't be read: reported, and left for the user.
R config branch.ui.gitfoldMoving "not a record"
check "a record that can't be read: reported (exit 1)" says 1 "branch.ui.gitfoldMoving holds \"not a record\"" parent ui --update
check "  and left" R config branch.ui.gitfoldMoving
R config --unset branch.ui.gitfoldMoving
# --set that conflicts: nothing moves; built on P all the same, following
# paused by the conflict recorded (back is --set the old parent).
R branch other main; on other u.txt other-side
before=$(R rev-parse ui)
check "--set with a conflict: nothing moves (exit 4), built on P, paused" says 4 "ui is now built on other; following paused" parent ui --set other
eq "  built on other" "refs/heads/other" "$(R config branch.ui.gitfoldParent)"
eq "  nothing moved" "$before" "$(R rev-parse ui)"
check "  shown paused" says 0 "following paused: ui's commits conflict with other's" parent ui
check "  back by --set the old parent" says 0 "ui is now built on api" parent ui --set api
# The tree changes one --set at a time.
mkdir -p "$REPO/.git/gitfold-moves"
check "another --set changing the tree: refused (exit 5)" sh -c "flock '$REPO/.git/gitfold-moves/.tree.lock' sh -c \"GITFOLD_TEST_TREE_WAIT=0 '$BIN' -C '$REPO' parent ui --set main; [ \\\$? = 5 ]\""
# A move's worktree a dead process left: cleaned up by the next move.
on api a.txt newer
R -c worktree.useRelativePaths=true worktree add -q --no-checkout "$REPO/.git/gitfold-moves/gitfold-move-ui" ui
check "a dead move's worktree: cleaned up" says 0 "moved 1 commit of ui onto api" parent ui --update
check "  gone" sh -c "! git -C '$REPO' worktree list | grep -q gitfold-move"
# --set: its own commits onto another parent.
check "--set: says what moves" says 0 "moves 1 commit of ui onto main" parent ui --set main
eq "  only its own commits" "1" "$(own main ui)"
check "  api's commits gone from it" fails R merge-base --is-ancestor api ui
check "--set --no-follow: following off" says 0 "ui is now built on api" parent ui --set api --no-follow
eq "  stored" "false" "$(R config branch.ui.gitfoldFollow)"
check "--set the same parent --follow: only following changes" says 0 "ui is built on api already" parent ui --set api --follow
check "  on" fails R config branch.ui.gitfoldFollow
check "--none: the base forgotten" says 0 "ui no longer has a parent" parent ui --none
check "  the ref gone" fails R rev-parse -q --verify refs/gitfold-base/ui
}

# A workspace's branch moves in its workspace, through the mount, by git
# there: its uncommitted work carried along (git's autostash).
# git in workspace $1 of the mount.
W() { git -C "$MNT/$1" -c user.name=t -c user.email=t@t "${@:2}"; }
# Line $2 of file $1 in workspace ui replaced by $3, uncommitted.
edit_line() { sed -i "$2s/.*/$3/" "$MNT/ui/$1"; }

workspaces() {
identity
seq 1 10 >"$REPO/g.txt"; R add g.txt; R commit -qm "main: g"
R branch api; "$BIN" -C "$REPO" branch ui --parent api >/dev/null
mount_fs
echo one >"$MNT/ui/u.txt"; W ui add u.txt; W ui commit -qm "ui: one"
edit_line g.txt 2 mine; echo dirty >"$MNT/ui/d.txt"
R switch -q api; sed -i '9s/.*/theirs/' "$REPO/g.txt"; R commit -qam "api: g"; echo new >"$REPO/a.txt"; R add a.txt; R commit -qm "api: a"; R switch -q main
check "a workspace: moved through the mount" says 0 "moved 1 commit of ui onto api" parent ui --update
check "  ui sits on api" R merge-base --is-ancestor api ui
eq "  its files moved" "new" "$(cat "$MNT/ui/a.txt")"
eq "  its uncommitted work carried along, merged" "mine theirs" "$(sed -n '2p;9p' "$MNT/ui/g.txt" | tr '\n' ' ' | sed 's/ $//')"
eq "  git sees only that work" " M g.txt|?? d.txt" "$(W ui status --porcelain | tr '\n' '|' | sed 's/|$//')"
eq "  its base recorded" "$(R rev-parse api)" "$(R rev-parse refs/gitfold-base/ui)"
# Uncommitted work that conflicts with the move: nothing moves.
edit_line g.txt 5 mine-too
R switch -q api; sed -i '5s/.*/theirs-too/' "$REPO/g.txt"; R commit -qam "api: g again"; R switch -q main
before=$(R rev-parse ui)
check "uncommitted work in conflict: nothing moves (exit 4)" says 4 "ui's uncommitted work is in the way of api's commits on g.txt" parent ui --update
eq "  the branch where it was" "$before" "$(R rev-parse ui)"
eq "  the work as it was" "mine-too" "$(sed -n 5p "$MNT/ui/g.txt")"
check "  no rebase left in it" sh -c "! git -C '$MNT/ui' status | grep -q rebase"
edit_line g.txt 5 5
# git busy in the workspace: not moved.
W ui switch -q --detach
check "git busy in the workspace: refused (exit 4)" says 4 "in its workspace" parent ui --update
W ui switch -q ui
check "  then moved" says 0 "moved 1 commit of ui onto api" parent ui --update
# A conflict left unresolved in the workspace (no git operation under way,
# as a stash popped onto a change leaves it): neither committed nor stashed
# until resolved. Refused before anything is said or moved.
blob=$(W ui rev-parse HEAD:g.txt)
printf '0 %s 0\tg.txt\n100644 %s 1\tg.txt\n100644 %s 2\tg.txt\n100644 %s 3\tg.txt\n' \
    "$(printf '0%.0s' {1..40})" "$blob" "$blob" "$blob" | W ui update-index --index-info
on api a.txt unresolved
before=$(R rev-parse ui)
check "a conflict left unresolved: resolve it (exit 4)" says 4 "ui has a conflict left unresolved in its workspace on g.txt: resolve it, then: \`gitfold parent ui --update\`" parent ui --update
eq "  the branch where it was" "$before" "$(R rev-parse ui)"
r=$(gf parent ui --set main)
eq "--set with it: refused before it says what moves" "4 yes no" "${r%% *} $([[ $r == *"resolve it"* ]] && echo yes || echo no) $([[ $r == *"moves "* ]] && echo yes || echo no)"
R config branch.ui.gitfoldParent refs/heads/api
W ui reset -q -- g.txt
check "  moved once resolved" says 0 "moved 1 commit of ui onto api" parent ui --update
# An untracked file where the parent adds one: git refuses, nothing moves.
echo mine >"$MNT/ui/n.txt"; on api n.txt theirs
before=$(R rev-parse ui)
check "an untracked file in the way: git refuses, nothing moves (exit 4)" says 4 "nothing moved: git refused in ui's workspace" parent ui --update
check "  git's message said" says 4 "n.txt" parent ui --update
eq "  the branch where it was" "$before" "$(R rev-parse ui)"
eq "  the file as it was" "mine" "$(cat "$MNT/ui/n.txt")"
check "  no rebase left in it" sh -c "! git -C '$MNT/ui' status | grep -q rebase"
check "  not recorded as a conflict" fails R config branch.ui.gitfoldConflict
check "  no move recorded" fails R config branch.ui.gitfoldMoving
rm "$MNT/ui/n.txt"
check "  moved once it is out of the way" says 0 "moved 1 commit of ui onto api" parent ui --update
# The same where the parent renames a file onto an untracked one.
echo mine >"$MNT/ui/ren.txt"; R switch -q api; R mv a.txt ren.txt; R commit -qm "api: rename"; R switch -q main
check "an untracked file where the parent renames one to: git refuses (exit 4)" says 4 "nothing moved: git refused in ui's workspace" parent ui --update
rm "$MNT/ui/ren.txt"
check "  moved once it is out of the way" says 0 "moved 1 commit of ui onto api" parent ui --update
# Unmounted: mounted again where it last was, then moved.
unmount_fs
on api a.txt more
check "unmounted: mounted again, then moved" says 0 "moved 1 commit of ui onto api" parent ui --update
check "  mounted" mountpoint -q "$MNT"
eq "  its files moved" "more" "$(cat "$MNT/ui/a.txt" | tr '\n' ' ' | sed 's/ $//')"
"$REAL_BIN" unmount "$MNT" >/dev/null 2>&1
}

section "tree: the command-line contract" the_contract
section "tree: two questions about main" two_questions_about_main
section "tree: rules R (the root)" the_root
section "tree: rules P (parent names)" parent_names
section "tree: rules S, F (parent --set)" parent_set
section "tree: rules N (branch --parent)" branch_parent
section "tree: rules T (the stored tree)" stored_tree
section "tree: the relation (F, T)" relation
section "tree: moves (--update, --set)" moves
section "tree: pushed commits" pushed
section "tree: what isn't moved, and crashes" not_moved
section "tree: workspaces moved through the mount" workspaces
finish
