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

# Fails, saying why, if a push of the updates "<local ref> <local oid>
# <remote ref> <remote oid>" (one per line in $1, as pre-push gets them)
# would publish a layers commit: one with a gitfold-layers header (it holds other
# workspaces' uncommitted work). Commits a remote already has don't count.
gitfold_check_layers() {
	printf '%s\n' "$1" | while read -r gitfold_lref gitfold_local gitfold_rref gitfold_remote; do
		# Deletions push no commits.
		case $gitfold_local in *[!0]*) ;; *) continue ;; esac
		gitfold_known=
		case $gitfold_remote in *[!0]*)
			if git cat-file -e "$gitfold_remote^{commit}" 2>/dev/null; then
				gitfold_known=^$gitfold_remote
			fi
		esac
		# The first commit with the header, among those pushed; on a failed
		# walk, a line the headers can't have (they don't hold spaces before
		# their name), so refused.
		gitfold_layers=$({
			git rev-list --pretty=raw "$gitfold_local" $gitfold_known --not --remotes ||
				echo ' failed'
		} | awk '
			/^ failed$/ { print "?"; exit }
			/^commit / { commit = $2; header = 1; next }
			/^$/ { header = 0; next }
			header && /^gitfold-layers( |$)/ { print commit; exit }
		')
		case $gitfold_layers in
		'') ;;
		'?')
			echo "gitfold: refusing to push $gitfold_rref: couldn't check it for layers commits" >&2
			exit 1
			;;
		*)
			echo "gitfold: refusing to push $gitfold_rref: it contains the layers commit $gitfold_layers (other branches' work, shown as layers). Push your branch instead (git push origin <branch>): your commits go onto it without the others' work." >&2
			exit 1
			;;
		esac
	done
}
