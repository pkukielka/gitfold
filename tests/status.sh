#!/usr/bin/env bash
# End-to-end test of what a workspace's status sees: deletions, half-way git
# operations, staged-only changes, ignore rules, modes, submodules and nested
# repositories, the index's cache tree.
# usage: tests/status.sh (or with the others: tests/run.sh); sections as in
# tests/lib.sh, each on a fresh repository (tests/fixture.sh).
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
CACHE_TREE=$(realpath "$(dirname "$0")/cache-tree.pl")
NAME=status
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

# A submodule entry (gitlink) at sm, committed.
add_submodule() { R update-index --add --cacheinfo "160000,$(R rev-parse HEAD),sm"; R commit -qm submodule; }

deletions_the_status_must_see() {
add_submodule
check "branch three" "$BIN" -C "$REPO" branch three
mount_fs
W3=$MNT/three
rm "$W3/a.txt"; mkdir "$W3/a.txt"          # a file replaced by an empty directory
rmdir "$W3/sm"                             # a deleted submodule
rm -r "$W3/d"; echo x >"$W3/d"             # a directory replaced by a file
unmount_fs
out=$("$BIN" -C "$REPO" list 2>/dev/null)
eq "counted (d changed; a.txt, sm, d/b.txt, d/c.txt deleted)" "1 4" \
    "$(echo "$out" | awk '$1 == "three" { print $2, $3 }')"
check "remove refused" fails "$BIN" -C "$REPO" remove three
check "remove --force" "$BIN" -C "$REPO" remove three --force
}

git_operation_stopped_half_way() {
R branch other main; R checkout -q other; echo theirs >"$REPO/a.txt"; R commit -qam theirs; R checkout -q main
check "branch four" "$BIN" -C "$REPO" branch four
mount_fs
echo mine >"$MNT/four/a.txt"; G "$MNT/four" commit -qam mine
fails G "$MNT/four" merge -q other >/dev/null 2>&1
echo resolved >"$MNT/four/a.txt"           # resolved, not committed: MERGE_HEAD stays
G "$MNT/four" add a.txt
unmount_fs
out=$("$BIN" -C "$REPO" remove four 2>&1)
eq "remove refused mid-merge" yes "$(grep -q 'in progress (MERGE_HEAD)' <<<"$out" && echo yes)"
check "remove --force" "$BIN" -C "$REPO" remove four --force
}

staged_only_changes_and_the_base_after_a() {
check "branch six" "$BIN" -C "$REPO" branch six
mount_fs
W6=$MNT/six
echo staged >"$W6/a.txt"; G "$W6" add a.txt; echo a >"$W6/a.txt"   # staged, then edited back
unmount_fs
out=$("$BIN" -C "$REPO" remove six 2>&1)
eq "staged change refused" yes "$(grep -q 'staged changes' <<<"$out" && echo yes)"
mount_fs
G "$W6" reset -q; echo c >>"$W6/a.txt"; G "$W6" commit -qam c   # then unmount at once
unmount_fs
eq "the base caught up at unmount" "$(R rev-parse six)" "$(cat "$COMMON/gitfold/six/base")"
c2=$(R commit-tree "six^{tree}" -p six -m 'moved while unmounted')
R update-ref refs/heads/six "$c2"                                  # base now behind the tip
rm -rf "$COMMON/gitfold/.tasks/six"
check "remove --check catches up offline" "$BIN" -C "$REPO" remove six --check
eq "and catching up sets up no task folder" "$(R rev-parse six) no" \
    "$(cat "$COMMON/gitfold/six/base") $(test -e "$COMMON/gitfold/.tasks/six" && echo made || echo no)"
check "remove catches up offline and removes" "$BIN" -C "$REPO" remove six
}

ignored_files() {
printf 'target/\n*.log\n!keep.log\n' >"$REPO/.gitignore"; echo t >"$REPO/tracked.log"
R add .gitignore; R add -f tracked.log; R commit -qm ignores
check "branch seven" "$BIN" -C "$REPO" branch seven
mount_fs
W7=$MNT/seven
mkdir -p "$W7/target/deep" "$W7/d/gen"
echo o >"$W7/target/deep/a.o"              # in an ignored directory
echo l >"$W7/x.log"; echo l >"$W7/d/y.log" # ignored by pattern, at any depth
echo k >"$W7/keep.log"                     # re-included by a negation
echo g >"$W7/d/gen/out.txt"; echo 'gen/' >"$W7/d/.gitignore"   # a new .gitignore
unmount_fs
out=$("$BIN" -C "$REPO" list 2>/dev/null)
eq "keep.log and d/.gitignore changed; 4 ignored" "2 0 4" \
    "$(echo "$out" | awk '$1 == "seven" { print $2, $3, $4 }')"
check "remove still refused (real changes)" fails "$BIN" -C "$REPO" remove seven
mount_fs
rm "$W7/keep.log" "$W7/d/.gitignore"; rm -r "$W7/d/gen"
echo changed >>"$W7/tracked.log"           # tracked: a change even though *.log
unmount_fs
eq "tracked file matching a pattern is a change" "1 0 3" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "seven" { print $2, $3, $4 }')"
mount_fs
G "$W7" checkout -q -- tracked.log; ls "$W7" >/dev/null
unmount_fs
out=$("$BIN" -C "$REPO" remove seven 2>&1)
eq "only ignored files: removed without --force" yes \
    "$(grep -q 'removed workspace seven (and 3 ignored files' <<<"$out" && echo yes)"
check "branch eight" "$BIN" -C "$REPO" branch eight
mount_fs
echo l >"$MNT/eight/x.log"; rm "$MNT/eight/.gitignore"   # deleting .gitignore un-ignores
unmount_fs
eq "without .gitignore: x.log changed, .gitignore deleted" "1 1 0" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "eight" { print $2, $3, $4 }')"
check "remove eight" "$BIN" -C "$REPO" remove eight --force
}

modes() {
check "branch nine" "$BIN" -C "$REPO" branch nine
mount_fs; chmod 664 "$MNT/nine/a.txt"; unmount_fs
eq "group-write bit alone: no change (as for git)" "0" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "nine" { print $2 }')"
mount_fs; chmod +x "$MNT/nine/a.txt"; unmount_fs
eq "executable bit: a change" "1" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "nine" { print $2 }')"
check "remove nine" "$BIN" -C "$REPO" remove nine --force
}

gitignore_files_git_doesn_t_read() {
mkdir -p "$REPO/s"; ln -s '*.tmp' "$REPO/s/.gitignore"; R add s 2>/dev/null; R commit -qm 'symlinked gitignore'
check "branch ten" "$BIN" -C "$REPO" branch ten
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
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "ten" { print $2, $4 }')"
mkdir -p "$T/cwd/git"; echo '*' >"$T/cwd/git/ignore"
eq "empty XDG_CONFIG_HOME is unset, not the current directory" "8 0" \
    "$(cd "$T/cwd" && XDG_CONFIG_HOME= "$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "ten" { print $2, $4 }')"
check "remove ten" "$BIN" -C "$REPO" remove ten --force
}

global_ignore_file_as_git_reads_it() {
add_submodule
mkdir -p "$XDG_CONFIG_HOME/git"; echo '*.xdg' >"$XDG_CONFIG_HOME/git/ignore"
check "branch eleven" "$BIN" -C "$REPO" branch eleven
mount_fs
echo x >"$MNT/eleven/w.xdg"
mkdir -p "$MNT/eleven/sm2"; ls "$MNT/eleven/sm" >/dev/null
echo '*' >"$MNT/eleven/sm/.gitignore"; echo c >"$MNT/eleven/sm/work.c"   # inside a submodule
unmount_fs
cnt() { "$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "eleven" { print $2, $4 }'; }
eq "XDG ignore file applies; submodule files are changes" "2 1" "$(cnt)"
R config core.excludesFile ''
eq "core.excludesFile set but empty: no global file" "3 0" "$(cnt)"
R config --unset core.excludesFile
R config extensions.worktreeConfig true
git --git-dir "$COMMON/worktrees/gitfold-ws-eleven" config --worktree core.excludesFile /dev/null
eq "worktree config is honoured" "3 0" "$(cnt)"
R config --unset extensions.worktreeConfig
check "remove eleven" "$BIN" -C "$REPO" remove eleven --force
}

nested_repositories_and_a_relative_exclu() {
echo 'target/' >"$REPO/.gitignore"; R add .gitignore; R commit -qm ignores
check "branch twelve" "$BIN" -C "$REPO" branch twelve
mount_fs
W12=$MNT/twelve
mkdir -p "$W12/nested/.git" "$W12/target/vendored/.git"
echo '*' >"$W12/nested/.gitignore"; echo p >"$W12/nested/precious.txt"   # its own ignores
touch "$W12/nested/.git/HEAD" "$W12/target/vendored/.git/HEAD"         # target/ is ignored
unmount_fs
eq "a nested repository is one untracked entry, or one ignored" "1 1" \
    "$("$BIN" -C "$REPO" list 2>/dev/null | awk '$1 == "twelve" { print $2, $4 }')"
check "remove refused for the nested repository" fails "$BIN" -C "$REPO" remove twelve
R config core.excludesFile relign
eq "relative core.excludesFile: status refused" yes "$("$BIN" -C "$REPO" list 2>/dev/null | grep "^twelve" | grep -q "relative path" && echo yes)"
R config --unset core.excludesFile
check "remove twelve" "$BIN" -C "$REPO" remove twelve --force
}

the_index_s_cache_tree_tree_extension() {
# A submodule, a symlink, an executable and an empty directory; git's own
# `read-tree` writes the cache tree to compare with.
(cd "$REPO" && mkdir -p t/u/v && echo v >t/u/v/f && ln -s a.txt t/link && echo x >t/x && chmod +x t/x)
R add -A; R commit -qm tree; R update-index --add --cacheinfo "160000,$(R rev-parse HEAD),t/sub"; R commit -qm sub
EMPTY=$(R hash-object -w -t tree /dev/null)
R branch cachetree "$(R commit-tree "$( (R ls-tree HEAD; printf '040000 tree %s\tempty\n' "$EMPTY") | R mktree)" -p HEAD -m empty)"
mount_fs
for v in @cachetree cachetree; do
    check "$v opens" test -e "$MNT/$v/a.txt"
    GIT_INDEX_VERSION=4 GIT_INDEX_FILE=$T/git.idx R read-tree "$(G "$MNT/$v" rev-parse HEAD^{tree})"
    eq "$v: cache tree as git writes it" "$(perl "$CACHE_TREE" "$T/git.idx")" \
        "$(perl "$CACHE_TREE" "$(G "$MNT/$v" rev-parse --path-format=absolute --git-path index)")"
done
eq "it has every directory (the root, d, t, t/u, t/u/v, empty)" 6 "$(perl "$CACHE_TREE" "$T/git.idx" | wc -l)"
(cd "$MNT/cachetree" && echo w >t/u/w && echo d >>d/b.txt && G . add -A && G . commit -qm w)
R worktree add -q "$T/plain-ct" cachetree~1
(cd "$T/plain-ct" && echo w >t/u/w && echo d >>d/b.txt && G . add -A && G . commit -qm w)
eq "a commit builds the trees git would" "$(G "$T/plain-ct" rev-parse HEAD^{tree})" "$(G "$MNT/cachetree" rev-parse HEAD^{tree})"
# A tree object stored out of order (git never writes one): its node is left
# for git to work out, since the index entries come sorted.
BLOB=$(R rev-parse HEAD:a.txt)
UNSORTED=$(perl -e 'print map { "100644 $_\0" . pack("H*", $ARGV[0]) } qw(b a)' "$BLOB" | R hash-object -t tree --literally -w --stdin)
R branch unsorted "$(R commit-tree "$( (R ls-tree HEAD; printf '040000 tree %s\tuns\n' "$UNSORTED") | R mktree)" -m unsorted)"
check "a snapshot with an unsorted tree opens" test -f "$MNT/@unsorted/uns/a"
eq "its cache tree node is left to git" "/uns -1 0 " \
    "$(perl "$CACHE_TREE" "$(G "$MNT/@unsorted" rev-parse --path-format=absolute --git-path index)" | grep "^/uns ")"
eq "git status is clean there" "" "$(G "$MNT/@unsorted" status --porcelain)"
unmount_fs
check "remove cachetree" "$BIN" -C "$REPO" remove cachetree --force
}

section "deletions the status must see" deletions_the_status_must_see
section "git operation stopped half-way" git_operation_stopped_half_way
section "staged-only changes, and the base after a commit" staged_only_changes_and_the_base_after_a
section "ignored files" ignored_files
section "modes" modes
section ".gitignore files git doesn't read" gitignore_files_git_doesn_t_read
section "global ignore file as git reads it" global_ignore_file_as_git_reads_it
section "nested repositories and a relative excludes file" nested_repositories_and_a_relative_exclu
section "the index's cache tree (TREE extension)" the_index_s_cache_tree_tree_extension
finish
