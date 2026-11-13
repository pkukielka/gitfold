# The repository the command suites (manage, status, mount, agents) start each
# section from, and their git helpers; sourced after tests/lib.sh.

G() { git -C "$1" -c user.name=t -c user.email=t@t "${@:2}"; }
# A submodule entry (gitlink) at sm, committed.
add_submodule() { R update-index --add --cacheinfo "160000,$(R rev-parse HEAD),sm"; R commit -qm submodule; }

# Every section starts from this: a repository with a.txt, d/b.txt, d/c.txt.
fixture() {
    COMMON=$REPO/.git
    export HOME=$T/home XDG_CONFIG_HOME=$T/xdg   # no global git config or ignore file from the test machine
    R init -q -b main
    (cd "$REPO" && mkdir d && echo a >a.txt && echo b >d/b.txt && echo c >d/c.txt)
    R add -A; R commit -qm init
}
