#!/usr/bin/env bash
# Linux kernel build in a directory: configure, full build, no-op build,
# incremental build (kernel/fork.c touched) and `git status`, in ms. Run it
# on a native worktree and on a gitfold workspace of the same commit, on the
# same disk, and compare. On NixOS, inside `nix develop nixpkgs#linux`.
# usage: bench/kbuild.sh <kernel-dir> [defconfig|allmodconfig]
# (allmodconfig with WERROR and RUST off). Starts with `make mrproper`; the
# build log goes to <kernel-dir>.log when that is writable, else /tmp.
set -euo pipefail

DIR=$(realpath "$1") CONFIG=${2:-defconfig}
LOG=$DIR.log
touch "$LOG" 2>/dev/null || LOG=/tmp/kbuild-$(basename "$DIR").log
: >"$LOG"
# `nix develop` exports the kernel package's outputs, one of them `modules`:
# make would pass it on as the kernel's own list of modules, past the 128 KiB
# a single environment string may have.
unset modules

# Runs a step, printing its time; a failed step stops the run.
ms() {
    local s; s=$(date +%s%N)
    "$@" >>"$LOG" 2>&1 || { echo "failed: $* (see $LOG)" >&2; exit 1; }
    echo $(( ($(date +%s%N) - s) / 1000000 ))
}
configure() {
    make "$CONFIG"
    if [ "$CONFIG" = allmodconfig ]; then
        scripts/config --disable WERROR --disable RUST
        make olddefconfig
    fi
}
cd "$DIR"
ms make mrproper >/dev/null
cfg=$(ms configure)
full=$(ms make -j"$(nproc)")
noop=$(ms make -j"$(nproc)")
touch kernel/fork.c
incr=$(ms make -j"$(nproc)")
st=$(ms git status --porcelain)
echo "$CONFIG: configure $cfg | full $full | no-op $noop | incremental $incr | git status $st"
