# Sourced by the hooks gitfold installs as a worktree's own core.hooksPath
# (gitfold's src/git/worktree_config.rs). Git never runs this file itself:
# it isn't a hook name.

# Runs the user's own hook $1 with the remaining arguments and this stdin, if
# there is one: the hook git would run without gitfold's per-worktree
# core.hooksPath, from core.hooksPath in any other scope, else from the
# repository's hooks directory. A relative core.hooksPath works as for git:
# hooks run in the same directory as this one.
gitfold_user_hook() {
	gitfold_name=$1
	shift
	gitfold_dir=$(git config --show-scope --type=path --get-all core.hooksPath |
		grep -v '^worktree	' | tail -n 1 | cut -f 2-)
	if [ -z "$gitfold_dir" ]; then
		gitfold_dir=$(git rev-parse --git-common-dir) || exit 1
		gitfold_dir=$gitfold_dir/hooks
	fi
	gitfold_hook=$gitfold_dir/$gitfold_name
	[ -f "$gitfold_hook" ] && [ -x "$gitfold_hook" ] || exit 0
	# Never this directory again (a loop).
	[ "$(cd "$gitfold_dir" && pwd -P)" != "$(cd "${0%/*}" && pwd -P)" ] || exit 0
	exec "$gitfold_hook" "$@"
}

# The guard (gitfold's src/git/guard.rs): checks the updates on stdin, as
# hook $1 gets them (ref-update, pre-push), for commits that would bring
# another layer's uncommitted lines; fails, saying why. GITFOLD_BIN is set
# on the line above this file's text when installed.
gitfold_guard() {
	"$GITFOLD_BIN" guard "$1" "${0%/*}/.."
}
