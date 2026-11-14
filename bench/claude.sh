# Claude Code's commands, as the benchmarks run them; sourced. Needs CLAUDE_BIN
# (CLAUDE_CODE_EXECPATH, set in Claude Code's Bash tool).

# What Claude Code's `grep` shell function runs.
cgrep() {
    (exec -a ugrep "$CLAUDE_BIN" -G --ignore-files --hidden -I --exclude-dir=.git "$@")
}
# What Claude Code runs around Bash commands with its bash edit diff on (the
# scan it always runs has no --ignored; see README, "With coding agents").
cstatus() {
    git -c core.fsmonitor= status --porcelain=v2 -z --untracked-files=normal --ignored=matching \
        --no-renames --ignore-submodules=all
}
