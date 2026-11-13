#!/usr/bin/env bash
# End-to-end test of mount and unmount (background, odd paths, a stopped or failing daemon, -C), version, many packs, prewarm.
# usage: tests/mount.sh
# Each section runs on a fresh repository below /tmp/gf-mount.XXXXXX (see
# tests/lib.sh); SECTION=<text> runs only the sections whose name contains it.
set -uo pipefail

BIN=$(realpath "$(dirname "$0")/../target/release/gitfold")
NAME=mount
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/fixture.sh"

background_mount_and_unmount() {
check "branch bg" "$BIN" -C "$REPO" branch bg
out=$("$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" 2>&1)
eq "mount returns once mounted" yes "$(mountpoint -q "$MNT" && echo yes)"
eq "and says how to unmount" yes "$(grep -q "gitfold unmount $MNT" <<<"$out" && echo yes)"
check "usable at once" test -f "$MNT/bg/a.txt"
echo bg >>"$MNT/bg/a.txt"; G "$MNT/bg" commit -qam bg      # then unmount at once
check "unmount" "$BIN" unmount "$MNT"
check "unmounted" sh -c "! mountpoint -q '$MNT'"
check "remove right after commit + unmount" "$BIN" -C "$REPO" remove bg
check "branch bg3" "$BIN" -C "$REPO" branch bg3
cd "$MNT"   # the directory under the mount: not a user of it
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
( cd "$MNT/bg3" && exec sleep 5 ) & HOLDER=$!   # like the `git maintenance` a commit starts
for _ in $(seq 100); do [ "$(cat /proc/$HOLDER/comm 2>/dev/null)" = sleep ] && break; sleep 0.02; done
out=$("$BIN" unmount "$MNT" 2>&1)
cd /
eq "unmount names a process using the mount" yes "$(grep -q "sleep 5" <<<"$out" && echo yes)"
eq "and waits until gitfold exits" "unmounted $MNT" "$(tail -1 <<<"$out")"
check "remove right after a busy unmount" "$BIN" -C "$REPO" remove bg3
wait
check "branch bg4" "$BIN" -C "$REPO" branch bg4
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
cp "$REAL_BIN" "$MNT/bg4/gitfold-copy"
out=$(timeout 20 "$MNT/bg4/gitfold-copy" unmount "$MNT" 2>&1)
eq "unmount run from inside the mount doesn't wait for itself" yes "$(grep -q 'in use by this command' <<<"$out" && echo yes)"
for _ in $(seq 100); do flock -n "$COMMON" true 2>/dev/null && break; sleep 0.1; done
check "and gitfold exits after it" flock -n "$COMMON" true
check "remove bg4" "$BIN" -C "$REPO" remove bg4 --force
check "branch bg2" "$BIN" -C "$REPO" branch bg2
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
( cd "$MNT/bg2"; "$BIN" unmount "$MNT" >"$T/busy.out" 2>&1; sleep 0.5 )
eq "unmount with a shell inside detaches" yes "$(grep -q 'detached it' "$T/busy.out" && echo yes)"
for _ in $(seq 100); do flock -n "$COMMON" true 2>/dev/null && break; sleep 0.1; done
check "the daemon exits once the shell leaves" flock -n "$COMMON" true
check "a failed background mount reports the error" sh -c "! '$BIN' mount '$T/nonexistent' '$MNT' 2>&1 | grep -q mounted"
mount_fs; FG=$PID
kill -HUP $FG; wait $FG
check "SIGHUP unmounts a foreground daemon cleanly" sh -c "! mountpoint -q '$MNT'"
check "remove bg2" "$BIN" -C "$REPO" remove bg2 --force
}

unmount_paths_odd_repository_names_the_d() {
"$BIN" mount --log "$T/bg.log" "$REPO" "$MNT" >/dev/null 2>&1
eq "the daemon doesn't keep the caller's directory" "/" \
    "$(for p in $(pgrep -x gitfold); do grep -qa "$T/" /proc/$p/cmdline 2>/dev/null && readlink /proc/$p/cwd; done | sort -u)"
check "unmount with a trailing slash" "$BIN" unmount "$MNT/"
ln -s "$MNT" "$T/mntlink"
"$BIN" mount --log "$T/bg.log" "$REPO" "$T/mntlink" >/dev/null 2>&1
check "mounted through a symlink" mountpoint -q "$MNT"
check "unmount through the symlink" "$BIN" unmount "$T/mntlink"
git clone -q "$REPO" "$T/r,c"; git -C "$T/r,c" branch c1
"$BIN" mount --log "$T/bg.log" "$T/r,c" "$MNT" >/dev/null 2>&1
echo c >>"$MNT/c1/a.txt"; G "$MNT/c1" commit -qam c
check "unmount of a repository with a comma in its path" "$BIN" unmount "$MNT"
check "remove right after it" "$BIN" -C "$T/r,c" remove c1
( cd "$T" && "$BIN" mount --log "$T/bg.log" repo mnt >/dev/null 2>&1 )
check "a relative repository path works" test -f "$MNT/@HEAD/a.txt"
check "unmount" "$BIN" unmount "$MNT"
check "arguments after -- work" "$BIN" mount --log "$T/bg.log" -- "$REPO" "$MNT"
check "unmount" "$BIN" unmount "$MNT"
}

many_packs_added_while_mounted() {
# The object database's pack index table was sized at open and never grew.
mount_fs
test -f "$MNT/@main/a.txt"
prev=$(R rev-parse main)
for i in $(seq 100); do
    blob=$(echo "$i" | R hash-object -w --stdin)
    tree=$(printf '100644 blob %s\tn.txt\n' "$blob" | R mktree)
    prev=$(R commit-tree "$tree" -p "$prev" -m "pack $i")
    printf '%s\n' "$prev" "$tree" "$blob" | R pack-objects -q "$COMMON/objects/pack/pack" >/dev/null
    R prune-packed
done
R branch packs "$prev"
eq "a commit in the 100th new pack opens" 100 "$(cat "$MNT/@packs/n.txt" 2>&1)"
unmount_fs
}

prewarm_walks_at_once_per_workspace_opt() {
PW=$T/pwrepo
git init -q "$PW"; mkdir "$PW/d"
for i in $(seq 5000); do echo $i >"$PW/d/f$i"; done
G "$PW" add -A; G "$PW" commit -qm many
for b in pw1 pw2 pw3; do G "$PW" branch $b; done
check "branch --prewarm none" "$BIN" -C "$PW" branch pw4 --prewarm none
eq "kept in the branch's git config" none "$(G "$PW" config --get branch.pw4.gitfoldPrewarm)"
"$BIN" mount --prewarm-walks 1 --log "$T/pw.log" "$PW" "$MNT" >/dev/null 2>&1
for b in pw1 pw2 pw3 pw4; do cat "$MNT/$b/d/f1" >/dev/null & done; wait
for _ in $(seq 200); do [ "$(grep -c 'prewarm done' "$T/pw.log")" -ge 3 ] && break; sleep 0.05; done
sleep 0.3
# Starts and ends alternate: never two walks at once.
eq "one walk at a time" "SDSDSD" "$(grep -oE 'prewarm (of [^:]*: started|done for)' "$T/pw.log" | sed 's/.*started/S/; s/.*done for/D/' | tr -d '\n')"
eq "each workspace walked once" "pw1 pw2 pw3" "$(grep -o 'prewarm done for [^:]*' "$T/pw.log" | sed 's|.*/||' | sort | xargs)"
check "a workspace that opted out isn't walked" sh -c "! grep -q 'mnt/pw4' '$T/pw.log'"
check "unmount" "$BIN" unmount "$MNT"
# Slow walks, one at a time: an unmount must not start the queued ones.
"$BIN" mount --prewarm-walks 1 --prewarm content --prewarm-threads 1 --log "$T/pw2.log" "$PW" "$MNT" >/dev/null 2>&1
for b in pw1 pw2 pw3; do cat "$MNT/$b/d/f1" >/dev/null & done; wait
started=$(grep -c ': started' "$T/pw2.log")
eq "unmount with walks queued unmounts (doesn't detach)" "unmounted $MNT" "$("$BIN" unmount "$MNT" 2>&1 | tail -1)"
eq "no walk started once it began" "$started" "$(grep -c ': started' "$T/pw2.log")"
check "--prewarm-walks 0 refused" fails "$BIN" mount --foreground --prewarm-walks 0 "$PW" "$MNT"
# A walker killed from outside (an older `gitfold unmount`, a user) drops the
# queue, but later opens are still walked.
"$BIN" mount --prewarm content --prewarm-threads 1 --prewarm-walks 1 --log "$T/pw3.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw1/d/f1" "$MNT/pw2/d/f1" >/dev/null
WALKER=
for _ in $(seq 100); do
    WALKER=$(ps -eo pid=,args= | awk -v r="$MNT/pw1" '$2 == "gitfold" && $3 == "prewarm-walk" && $4 == r { print $1 }')
    [ -n "$WALKER" ] && break; sleep 0.02
done
check "pw1's walker found" test -n "$WALKER"
kill -TERM $WALKER 2>/dev/null
for _ in $(seq 100); do grep -q "stopped from outside" "$T/pw3.log" && break; sleep 0.05; done
check "its death taken for an outside stop" grep -q "stopped from outside" "$T/pw3.log"
cat "$MNT/pw3/d/f1" >/dev/null
for _ in $(seq 200); do grep -q "prewarm done for $MNT/pw3" "$T/pw3.log" && break; sleep 0.05; done
check "the queued walk dropped" sh -c "! grep -q 'prewarm of $MNT/pw2: started' '$T/pw3.log'"
check "later opens still walked" grep -q "prewarm done for $MNT/pw3" "$T/pw3.log"
check "unmount" "$BIN" unmount "$MNT"
G "$PW" config branch.pw1.gitfoldPrewarm content
"$BIN" mount --prewarm none --log "$T/pw4.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw1/d/f1" >/dev/null; sleep 0.5
check "--prewarm none for the mount wins over a branch's mode" sh -c "! grep -q 'prewarm of' '$T/pw4.log'"
check "unmount" "$BIN" unmount "$MNT"
}

gitfold_prewarm_a_mode_applied_to_an_ope() {
"$BIN" mount --log "$T/pw5.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw4/d/f1" >/dev/null; sleep 0.5
check "pw4 (none) opened unwalked" sh -c "! grep -q 'prewarm of $MNT/pw4' '$T/pw5.log'"
eq "prewarm meta: walks it at once" "walking $MNT/pw4 (meta)" "$("$BIN" -C "$PW" prewarm pw4 meta 2>&1)"
for _ in $(seq 200); do grep -q "prewarm done for $MNT/pw4" "$T/pw5.log" && break; sleep 0.05; done
check "walked" grep -q "prewarm done for $MNT/pw4" "$T/pw5.log"
eq "and kept in git config" meta "$(G "$PW" config --get branch.pw4.gitfoldPrewarm)"
eq "a workspace not open: walked when it opens" "pw3 isn't open: walked when it opens" "$("$BIN" -C "$PW" prewarm pw3 meta 2>&1)"
check "an unknown branch refused" fails "$BIN" -C "$PW" prewarm nope meta
check "unmount" "$BIN" unmount "$MNT"
eq "unmounted: only set" "set; pw4 isn't walked" "$("$BIN" -C "$PW" prewarm pw4 none 2>&1)"
eq "set" none "$(G "$PW" config --get branch.pw4.gitfoldPrewarm)"
}

an_interrupted_unmount_prewarm_resumes_s() {
mkdir -p "$T/fakebin"; printf '#!/bin/sh\nexec sleep 30\n' >"$T/fakebin/fusermount3"; chmod +x "$T/fakebin/fusermount3"
GITFOLD_PREWARM_RESUME_SECS=2 "$BIN" mount --prewarm content --prewarm-threads 1 --prewarm-walks 1 \
    --log "$T/pw6.log" "$PW" "$MNT" >/dev/null 2>&1
cat "$MNT/pw1/d/f1" "$MNT/pw2/d/f1" >/dev/null
for _ in $(seq 100); do grep -q "prewarm of $MNT/pw1: started" "$T/pw6.log" && break; sleep 0.02; done
setsid env PATH="$T/fakebin:$PATH" "$BIN" unmount "$MNT" >/dev/null 2>&1 & UM=$!
for _ in $(seq 100); do grep -q "prewarm stopped for the unmount" "$T/pw6.log" && break; sleep 0.05; done
check "pw1's walk was running when it stopped" sh -c "! grep -q 'prewarm done for $MNT/pw1' '$T/pw6.log'"
kill -KILL -- -$UM; wait $UM 2>/dev/null
check "the unmount killed before unmounting" mountpoint -q "$MNT"
for _ in $(seq 200); do grep -q "prewarm done for $MNT/pw2" "$T/pw6.log" && break; sleep 0.05; done
check "prewarm resumed by itself" grep -q "prewarm resumed: still mounted 2 s after an unmount stopped it" "$T/pw6.log"
check "the stopped walk restarted" grep -q "prewarm done for $MNT/pw1" "$T/pw6.log"
check "the queued one too" grep -q "prewarm done for $MNT/pw2" "$T/pw6.log"
check "unmount" "$BIN" unmount "$MNT"
for b in pw1 pw2 pw3 pw4; do "$BIN" -C "$PW" remove "$b" >/dev/null; done
G "$PW" branch -q -D pw4
check "the opt-out goes with the branch" fails G "$PW" config --get branch.pw4.gitfoldPrewarm
}

# A 5000-file repository walked, given modes, then unmounted half-way:
prewarm_story() { prewarm_walks_at_once_per_workspace_opt; gitfold_prewarm_a_mode_applied_to_an_ope; an_interrupted_unmount_prewarm_resumes_s; }

unmount_of_a_stopped_daemon() {
mount_fs
kill -STOP $PID
timeout 10 "$BIN" unmount "$MNT" >/dev/null 2>&1
check "unmounts without the daemon's answer" sh -c "! mountpoint -q '$MNT'"
kill -CONT $PID; wait $PID 2>/dev/null
}

version() {
eq "--version names the build" yes "$("$BIN" --version | grep -qE '^gitfold [0-9]+\.[0-9]+\.[0-9]+' && echo yes)"
mount_fs
out=$("$BIN" version "$MNT" 2>&1)
eq "the daemon runs the same build" "$(sed -n "s/^gitfold //p" <<<"$out")" "$(sed -n "s/^daemon //p" <<<"$out")"
check "version of a mount exits 0" "$BIN" version "$MNT"
check "version of a non-mount fails" fails "$BIN" version "$T"
check "the daemon logged its version" grep -q "gitfold .*: mounting" "$LOG"
kill -STOP $PID
rc=$("$BIN" version "$MNT" >/dev/null 2>&1; echo $?)
kill -CONT $PID
eq "a daemon that doesn't answer: exit code 5 (not a mismatch)" 5 "$rc"
unmount_fs
}

an_unmount_that_fails_lets_prewarm_resum() {
R branch rs1; R branch rs2
mkdir -p "$T/fakebin"
printf '#!/bin/sh\necho "fusermount3: simulated failure" >&2\nexit 1\n' >"$T/fakebin/fusermount3"
chmod +x "$T/fakebin/fusermount3"
"$BIN" mount --log "$T/resume.log" "$REPO" "$MNT" >/dev/null 2>&1
cat "$MNT/rs1/a.txt" >/dev/null
check "the unmount fails" fails env PATH="$T/fakebin:$PATH" "$BIN" unmount "$MNT"
check "and the mount stays" mountpoint -q "$MNT"
cat "$MNT/rs2/a.txt" >/dev/null
for _ in $(seq 100); do grep -q "prewarm done for $MNT/rs2" "$T/resume.log" && break; sleep 0.05; done
check "prewarm resumed" grep -q "prewarm resumed" "$T/resume.log"
check "a workspace opened afterwards is walked" grep -q "prewarm done for $MNT/rs2" "$T/resume.log"
check "unmount" "$BIN" unmount "$MNT"
}

c_for_mount_relative_paths_from_there() {
(cd "$T" && "$BIN" -C repo mount . ../mnt >/dev/null 2>&1)
check "mounted in the background through -C" mountpoint -q "$MNT"
check "and serves the repository" test -d "$MNT/@HEAD"
check "unmount" "$BIN" unmount "$MNT"
}

section "background mount and unmount" background_mount_and_unmount
section "unmount: paths, odd repository names, the daemon's directory" unmount_paths_odd_repository_names_the_d
section "many packs added while mounted" many_packs_added_while_mounted
section "prewarm: walks, per-workspace modes, an interrupted unmount" prewarm_story
section "unmount of a stopped daemon" unmount_of_a_stopped_daemon
section "version" version
section "an unmount that fails lets prewarm resume" an_unmount_that_fails_lets_prewarm_resum
section "-C for mount: relative paths from there" c_for_mount_relative_paths_from_there
finish
