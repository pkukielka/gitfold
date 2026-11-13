# The repository each section of the command suites starts from, and `G`.

# git in directory $1, as the test user.
G() { git -C "$1" -c user.name=t -c user.email=t@t "${@:2}"; }

# Every section starts from this: a repository with a.txt, d/b.txt, d/c.txt.
fixture() {
    COMMON=$REPO/.git
    export HOME=$T/home XDG_CONFIG_HOME=$T/xdg   # no global git config or ignore file from the test machine
    R init -q -b main
    (cd "$REPO" && mkdir d && echo a >a.txt && echo b >d/b.txt && echo c >d/c.txt)
    R add -A; R commit -qm init
}
