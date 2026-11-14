//! Claude Code's worktrees as gitfold workspaces: its `WorktreeCreate` and
//! `WorktreeRemove` hooks (`gitfold branch --claude-hook worktree-create`,
//! `gitfold remove --claude-hook worktree-remove`), which replace its own
//! `git worktree` logic.
//!
//! The hooks run for every repository the session is in (a plugin's hooks
//! wherever it is installed), so for one not used with gitfold they do what
//! Claude Code would: a git worktree in `.claude/worktrees/<name>`. One used
//! with gitfold but not mounted is mounted again where `gitfold mount` last
//! mounted it, or else refused: a full checkout of it is what gitfold is there
//! to avoid.

use std::fs;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use super::hook::{field, field_at};
use crate::git;
use crate::mount_table;
use crate::settings;
use crate::state::state_root;

/// Claude Code's branch name for its worktree `name` (as bin/gitfold's
/// fallback names it too), kept so a worktree
/// named like a branch of the repository (`main`) is never that branch's
/// workspace.
fn branch_of(name: &str) -> String {
    format!("worktree-{name}")
}

/// `WorktreeCreate`: makes worktree `name` of the repository of the session's
/// directory and prints its path, the only thing on stdout, which Claude Code
/// reads. A workspace of branch `worktree-<name>` (created from the session's
/// HEAD if new) when the repository is mounted, or was.
pub fn worktree_create() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    let name = field(&input, "name").context("no \"name\" in the hook's input")?;
    let cwd = cwd(&input)?;
    let repo = git::discover(&cwd)?;
    let common_dir = git::init(&repo)?;
    let branch = branch_of(&name);
    let exists = git::branch_exists(&branch);
    let mount = match mount_table::mount_of(&common_dir)? {
        Some(mount) => Some(mount),
        None if state_root(&common_dir).exists() => Some(mount_again(&common_dir)?),
        None => None,
    };
    let path = match mount {
        Some(mount) => {
            fs::metadata(&mount).with_context(|| {
                format!(
                    "the gitfold mount at {} doesn't answer (its daemon gone?); `gitfold \
                     unmount` it",
                    mount.display()
                )
            })?;
            let path = mount.join(&branch);
            if !exists {
                super::branch::check_new(&common_dir, &branch)?;
                super::branch::create(&repo, &branch, None)?;
            }
            // Looking it up opens it.
            let meta = fs::metadata(&path)
                .with_context(|| format!("opening workspace {}", path.display()))?;
            ensure!(meta.is_dir(), "{} is not a workspace", path.display());
            path
        }
        // Kept as bin/gitfold's fallback does it, for when gitfold can't run.
        None => {
            // As Claude Code: below the top of the worktree the session is in.
            let top = git::work_tree().context("the session is in no worktree")?;
            let path = top.join(".claude/worktrees").join(&name);
            if !path.join(".git").exists() {
                let at = path.to_str().context("a worktree path that isn't UTF-8")?;
                let args: &[&str] = match exists {
                    true => &["worktree", "add", at, &branch],
                    false => &["worktree", "add", "-b", &branch, at],
                };
                git::cmd::output(&repo, args)?;
            }
            path
        }
    };
    println!("{}", path.display());
    Ok(())
}

/// Mounts the repository of `common_dir` again where it last was
/// (`gitfold.mountpoint`, recorded by `gitfold mount`), with the default
/// options: `gitfold mount` in the background, its messages on stderr.
fn mount_again(common_dir: &Path) -> Result<PathBuf> {
    let mountpoint = settings::Local::read(common_dir)
        .mountpoint()
        .with_context(|| {
            format!(
                "{} is used with gitfold but not mounted: mount it (`gitfold mount`) for a \
             workspace, rather than a full checkout",
                common_dir.display()
            )
        })?;
    // Gone with a reboot if it was on a tmpfs (/tmp, /run/user).
    fs::create_dir_all(&mountpoint)
        .with_context(|| format!("creating mount point {}", mountpoint.display()))?;
    let stderr = std::io::stderr().as_fd().try_clone_to_owned()?;
    let status = Command::new(std::env::current_exe()?)
        .arg("mount")
        .arg(common_dir)
        .arg(&mountpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stderr))
        .status()?;
    // Another hook may have mounted it meanwhile, its mount refused.
    match mount_table::mount_of(common_dir)? {
        Some(mount) => Ok(mount),
        None => bail!(
            "{} isn't mounted, and mounting it at {} failed ({status})",
            common_dir.display(),
            mountpoint.display()
        ),
    }
}

/// `WorktreeRemove`: removes the worktree at `worktree_path` (Claude Code asks
/// once the user chose to, or the worktree is clean), discarding its changes
/// as Claude Code does; its branch stays. A workspace in use (the session's
/// shell still in it) is waited for briefly.
pub fn worktree_remove() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    let path = PathBuf::from(
        field(&input, "worktree_path").context("no \"worktree_path\" in the hook's input")?,
    );
    // The hook starts in the worktree; from in there, it is in use.
    std::env::set_current_dir("/")?;
    if !path.exists() {
        return Ok(());
    }
    let repo = git::discover(&path)?;
    let common_dir = git::init(&repo)?;
    let mount = mount_table::mount_of(&common_dir)?;
    if mount.is_some_and(|m| path.starts_with(m)) {
        let args = super::remove::RemoveArgs {
            name: Some(super::branch_or_current(None, &repo)?),
            force: true,
            check: false,
            wait: 10,
            claude_hook: None,
        };
        return super::remove::remove(&repo, args);
    }
    let path = path.to_str().context("a worktree path that isn't UTF-8")?;
    git::cmd::output(&common_dir, &["worktree", "remove", "--force", path]).map(drop)
}

/// `PostToolUse` on Read and Bash (`gitfold track-reads --claude-hook
/// post-tool-use`): reports the files the tool read to the daemon of the
/// gitfold mount they are in, which records them in tracked workspaces: reads
/// it may not have seen (the kernel serves a file read before, by a search
/// say, from its cache; searches' reads aren't recorded). Read: its
/// `file_path`; Bash: the files simple viewers name (`viewed`). It runs after
/// every such tool call (async), so errors are only logged.
pub fn reads_hook() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    if let Err(e) = report_reads(&input) {
        log::debug!("gitfold track-reads --claude-hook: {e:#}");
    }
    Ok(())
}

fn report_reads(input: &str) -> Result<()> {
    let cwd = cwd(input)?;
    // The Read tool resolves `..` in its path lexically (it is Node's
    // `path.resolve`); a Bash command's files are as `cat` gets them, `..`
    // resolved by the filesystem below.
    let named = match field(input, "tool_name").as_deref() {
        Some("Read") => field_at(input, &["tool_input", "file_path"])
            .into_iter()
            .map(|p| normalized(&cwd.join(p)))
            .collect(),
        Some("Bash") => viewed(
            &field_at(input, &["tool_input", "command"]).unwrap_or_default(),
            &cwd,
        ),
        _ => return Ok(()),
    };
    if named.is_empty() {
        return Ok(());
    }
    let mounts = mount_table::mounts()?;
    // Per mount (its git common dir), the paths below it.
    let mut reads: std::collections::BTreeMap<&Path, Vec<String>> = Default::default();
    for path in named {
        // Its directory as the mount table has it (a symlink on the way
        // resolved), the file itself as named.
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            continue;
        };
        let Ok(path) = fs::canonicalize(dir).map(|d| d.join(name)) else {
            continue;
        };
        let below = mounts
            .iter()
            .filter_map(|(mount, common_dir)| Some((path.strip_prefix(mount).ok()?, common_dir)))
            .min_by_key(|(rel, _)| rel.as_os_str().len());
        let Some((rel, common_dir)) = below else {
            continue;
        };
        let Some(rel) = rel.to_str().filter(|_| path.is_file()) else {
            continue;
        };
        reads.entry(common_dir).or_default().push(rel.to_string());
    }
    for (common_dir, paths) in reads {
        let request = crate::daemon::control::Request::RecordReads { paths };
        // Not held up long behind a slow request (a removal); one mount's
        // failure doesn't lose the others' reads.
        let wait = Some(std::time::Duration::from_secs(5));
        if let Err(e) = crate::daemon::control::request(common_dir, &request, wait) {
            log::debug!("recording reads in {}: {e:#}", common_dir.display());
        }
    }
    Ok(())
}

/// The hook's working directory: `cwd` in its input, else this process's.
fn cwd(input: &str) -> Result<PathBuf> {
    match field(input, "cwd") {
        Some(cwd) => Ok(PathBuf::from(cwd)),
        None => Ok(std::env::current_dir()?),
    }
}

/// `path` with `.` and `..` resolved lexically (as the shell's `cd` does).
fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The files the simple viewers in shell command `command` show: `cat`,
/// `head`, `tail`, `less`, `more`, `nl`, `tac`, `bat` and `sed` with file
/// arguments, and any command's `<` file. Parsed as bash parses it (`rable`):
/// in pipelines, lists, loops, conditionals, `case`s, subshells and command
/// substitutions, not in heredoc bodies or function definitions; a `cd` moves
/// where relative paths are, within its subshell. A word with an expansion is
/// left out (its value isn't known), and what isn't a file in a workspace is
/// dropped later. Small on purpose: a file read by anything else is seen,
/// unless a search read it first.
fn viewed(command: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut at = At {
        dir: Some(cwd.to_path_buf()),
        pushed: Vec::new(),
    };
    let lines: Vec<&str> = command.split_inclusive('\n').collect();
    // What rable can't parse whole: the longest runs of lines it can, in
    // order (a heredoc's body stays with its command), the rest skipped.
    let mut i = 0;
    while i < lines.len() {
        let parsed = (i + 1..=lines.len())
            .rev()
            .find_map(|j| Some((j, rable::parse(&lines[i..j].concat(), true).ok()?)));
        let Some((j, nodes)) = parsed else {
            i += 1;
            continue;
        };
        for node in &nodes {
            walk(node, &mut at, &mut files);
        }
        i = j;
    }
    files
}

/// Where relative paths are while walking a command: `dir`, absolute
/// (`None` once unknown: `cd ~`, `cd -`, `cd $X`), and the `pushd` stack.
#[derive(Clone)]
struct At {
    dir: Option<PathBuf>,
    pushed: Vec<Option<PathBuf>>,
}

impl At {
    /// Where file operand `file` is: absolute paths always, relative ones
    /// while `dir` is known.
    fn file(&self, file: &str) -> Option<PathBuf> {
        match Path::new(file).is_absolute() {
            true => Some(PathBuf::from(file)),
            false => Some(self.dir.as_ref()?.join(file)),
        }
    }
}

/// Command substitutions in `words` (`$(...)`, `<(...)`), each walked as the
/// subshell it is.
fn substitutions(words: &[rable::Node], at: &At, files: &mut Vec<PathBuf>) {
    use rable::NodeKind as K;
    for word in words {
        let K::Word { parts, .. } = &word.kind else {
            continue;
        };
        for part in parts {
            if let K::CommandSubstitution { command, .. } | K::ProcessSubstitution { command, .. } =
                &part.kind
            {
                walk(command, &mut at.clone(), files);
            }
        }
    }
}

/// `viewed` in `node`, from `at`.
fn walk(node: &rable::Node, at: &mut At, files: &mut Vec<PathBuf>) {
    use rable::NodeKind as K;
    // A subshell (and so its `cd`) is walked from a copy of `at`.
    let inner = |node: &rable::Node, at: &At, files: &mut Vec<PathBuf>| {
        walk(node, &mut at.clone(), files);
    };
    match &node.kind {
        K::Command {
            assignments,
            words,
            redirects,
        } => {
            substitutions(assignments, at, files);
            substitutions(words, at, files);
            redirections(redirects, at, files);
            simple_command(words, at, files);
        }
        // Each of a pipeline's commands runs in a subshell of its own.
        K::Pipeline { commands, .. } if commands.len() > 1 => {
            commands.iter().for_each(|c| inner(c, at, files));
        }
        K::Pipeline { commands, .. } => commands.iter().for_each(|c| walk(c, at, files)),
        // A command run in the background (`&`) is a subshell too.
        K::List { items } => {
            for item in items {
                match item.operator {
                    Some(rable::ListOperator::Background) => inner(&item.command, at, files),
                    _ => walk(&item.command, at, files),
                }
            }
        }
        K::If {
            condition,
            then_body,
            else_body,
            redirects,
        } => {
            redirections(redirects, at, files);
            walk(condition, at, files);
            walk(then_body, at, files);
            if let Some(e) = else_body {
                walk(e, at, files);
            }
        }
        K::While {
            condition,
            body,
            redirects,
        }
        | K::Until {
            condition,
            body,
            redirects,
        } => {
            redirections(redirects, at, files);
            walk(condition, at, files);
            walk(body, at, files);
        }
        K::For {
            words,
            body,
            redirects,
            ..
        }
        | K::Select {
            words,
            body,
            redirects,
            ..
        } => {
            substitutions(words.as_deref().unwrap_or_default(), at, files);
            redirections(redirects, at, files);
            walk(body, at, files);
        }
        K::ForArith {
            body, redirects, ..
        } => {
            redirections(redirects, at, files);
            walk(body, at, files);
        }
        K::Case {
            word,
            patterns,
            redirects,
        } => {
            substitutions(std::slice::from_ref(word), at, files);
            redirections(redirects, at, files);
            for body in patterns.iter().filter_map(|p| p.body.as_ref()) {
                walk(body, at, files);
            }
        }
        K::Subshell { body, redirects } => {
            redirections(redirects, at, files);
            inner(body, at, files);
        }
        K::BraceGroup { body, redirects } => {
            redirections(redirects, at, files);
            walk(body, at, files);
        }
        // Their terms are text (substitutions in them unseen): redirections.
        K::ConditionalExpr { redirects, .. } | K::ArithmeticCommand { redirects, .. } => {
            redirections(redirects, at, files);
        }
        K::Negation { pipeline } | K::Time { pipeline, .. } => walk(pipeline, at, files),
        K::Coproc { command, .. } => inner(command, at, files),
        _ => {}
    }
}

/// A simple command's words: a `cd`, `pushd` or `popd` moves `at`; a
/// viewer's file operands are recorded.
fn simple_command(words: &[rable::Node], at: &mut At, files: &mut Vec<PathBuf>) {
    let words: Vec<Option<String>> = words.iter().map(literal).collect();
    // `builtin cd`, `command cat`: the command after them.
    let start = words
        .iter()
        .position(|w| !matches!(w.as_deref(), Some("builtin" | "command")))
        .unwrap_or(words.len());
    let Some(Some(program)) = words.get(start) else {
        return;
    };
    let args = &words[start + 1..];
    // The first operand, past options (-L, -P, -n ...) and --.
    let operand = || {
        args.iter()
            .find(|a| !a.as_deref().is_some_and(|a| a.starts_with('-') && a != "-"))
    };
    // Where `cd` or `pushd` goes: absolute, or relative to a known `dir`;
    // logical, as the shell's (`a/..` is `.` whatever `a` is).
    let to = |to: &str| match to {
        to if to.starts_with('/') => Some(normalized(Path::new(to))),
        to if to.starts_with('~') || to == "-" => None,
        to => at.dir.as_ref().map(|d| normalized(&d.join(to))),
    };
    match program.as_str() {
        "cd" => {
            // Home, the previous directory, an expansion: unknown.
            at.dir = operand().cloned().flatten().and_then(|t| to(&t));
            return;
        }
        // `-n` (the stack only) and `+N`/`-N` (rotations): the stack is
        // unknown, and with `+N`/`-N` where it leaves us too.
        "pushd" | "popd"
            if args.iter().any(|a| {
                a.as_deref().is_none_or(|a| {
                    a == "-n"
                        || a.len() > 1
                            && a[1..].bytes().all(|b| b.is_ascii_digit())
                            && (a.starts_with('+') || a.starts_with('-'))
                })
            }) =>
        {
            at.pushed = vec![None];
            if !args.iter().any(|a| a.as_deref() == Some("-n")) {
                at.dir = None;
            }
            return;
        }
        "pushd" => {
            let dir = operand().cloned().flatten().and_then(|t| to(&t));
            at.pushed.push(at.dir.take());
            at.dir = dir;
            return;
        }
        "popd" => {
            at.dir = at.pushed.pop().flatten();
            return;
        }
        _ => {}
    }
    let program = program.rsplit('/').next().unwrap_or_default();
    // Options that take the next word as their value.
    let valued: &[&str] = match program {
        "cat" | "less" | "more" | "nl" | "tac" | "bat" | "batcat" => &[],
        "head" | "tail" => &["-n", "-c", "--lines", "--bytes"],
        "sed" => &["-e", "--expression", "-f", "--file", "-l"],
        _ => return,
    };
    // sed's options that give its script; then its first operand isn't one.
    const SED_SCRIPT: &[&str] = &["-e", "--expression", "-f", "--file"];
    let mut operands = Vec::new();
    let mut script_given = false;
    let mut options_end = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg else {
            operands.push(None);
            continue;
        };
        if options_end || !arg.starts_with('-') || arg == "-" {
            operands.push(Some(arg.as_str()));
        } else if arg == "--" {
            options_end = true;
        } else {
            script_given |= SED_SCRIPT.contains(&arg.split('=').next().unwrap_or_default());
            if valued.contains(&arg.as_str()) {
                args.next();
            }
        }
    }
    let skip = usize::from(program == "sed" && !script_given);
    let operands = operands.into_iter().skip(skip).flatten();
    files.extend(operands.filter(|f| *f != "-").filter_map(|f| at.file(f)));
}

/// The files of `redirects` that are read: `<` and `<>` (not heredocs or
/// here-strings, which are text); and command substitutions in their targets
/// (`< <(cat f)`).
fn redirections(redirects: &[rable::Node], at: &At, files: &mut Vec<PathBuf>) {
    for redirect in redirects {
        let rable::NodeKind::Redirect { op, target, .. } = &redirect.kind else {
            continue;
        };
        substitutions(std::slice::from_ref(target), at, files);
        if matches!(op.as_str(), "<" | "<>")
            && let Some(file) = literal(target).and_then(|t| at.file(&t))
        {
            files.push(file);
        }
    }
}

/// The text word `node` stands for, quotes removed; `None` if it has an
/// expansion (a variable, a command substitution: its value isn't known).
fn literal(node: &rable::Node) -> Option<String> {
    let rable::NodeKind::Word { value, .. } = &node.kind else {
        return None;
    };
    // A process substitution: a pipe, not a file name.
    if value.starts_with("<(") || value.starts_with(">(") {
        return None;
    }
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => out.extend(chars.by_ref().take_while(|c| *c != '\'')),
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '$' | '`' => return None,
                    '\\' => out.extend(chars.next()),
                    c => out.push(c),
                }
            },
            '$' | '`' => return None,
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files simple viewers show, wherever they are in the command.
    #[test]
    fn viewed_files() {
        // From /w, shown relative to it.
        let viewed = |c: &str| -> Vec<String> {
            let files = super::viewed(c, Path::new("/w"));
            let shown = |p: PathBuf| p.strip_prefix("/w").unwrap_or(&p).display().to_string();
            files.into_iter().map(shown).collect()
        };
        assert_eq!(viewed("cat a.txt"), ["a.txt"]);
        assert_eq!(
            viewed("cat -n a.txt 'b c.txt' | head -n 20"),
            ["a.txt", "b c.txt"]
        );
        assert_eq!(
            viewed("head -n 50 src/x.rs && tail -c 10 y"),
            ["src/x.rs", "y"]
        );
        assert_eq!(viewed("sed -n '1,80p' src/main.rs"), ["src/main.rs"]);
        assert_eq!(viewed("sed -e s/a/b/ -n f1 f2"), ["f1", "f2"]);
        assert_eq!(viewed("LC_ALL=C /usr/bin/cat x; ls y"), ["x"]);
        assert_eq!(viewed("wc -l < input.txt 2>/dev/null"), ["input.txt"]);
        assert_eq!(viewed("cat a > out.txt"), ["a"]);
        assert_eq!(viewed("cat -- -dash"), ["-dash"]);
        assert_eq!(viewed("echo cat x | grep y"), Vec::<String>::new());
        assert_eq!(viewed("rg foo src; cargo test"), Vec::<String>::new());
        assert_eq!(viewed("cat \"a \\\"q\\\".txt\""), ["a \"q\".txt"]);
        assert_eq!(viewed("cat -"), Vec::<String>::new());
        assert_eq!(
            viewed("cd src/fs && cat view.rs ../x"),
            ["src/fs/view.rs", "src/fs/../x"]
        );
        assert_eq!(viewed("cd a/../b && cat x"), ["b/x"], "cd is logical");
        assert_eq!(
            viewed("head -n 3 < notes.txt"),
            ["notes.txt"],
            "3 isn't a descriptor"
        );
        assert_eq!(viewed("cat 2>/dev/null f"), ["f"]);
        assert_eq!(
            viewed("cat > run.sh <<'EOF'\nhead -n 5 data.csv\ndon't sed x y\nEOF\ncat z"),
            ["z"],
            "a heredoc's body isn't commands"
        );
        assert_eq!(viewed("cat <<-END\n\tcat q\n\tEND\nnl r"), ["r"]);
        assert_eq!(viewed("cat <<< 'not a file'"), Vec::<String>::new());
        assert_eq!(viewed("head -n $((n+5)) src/x.rs"), ["src/x.rs"]);
        assert_eq!(viewed("m=$((1<<4))\ncat f.txt"), ["f.txt"]);
        assert_eq!(
            viewed("(cd d && cat x) && cat y; { head -n 2 z; }"),
            ["d/x", "y", "z"],
            "a subshell's cd stays in it"
        );
        assert_eq!(
            viewed("for f in a; do cat m.rs; done; if true; then nl n; fi"),
            ["m.rs", "n"]
        );
        assert_eq!(viewed("case $x in *) cat c;; esac"), ["c"]);
        assert_eq!(
            viewed("cd src && case $x in *) cat main.rs;; esac"),
            ["src/main.rs"]
        );
        assert_eq!(
            viewed("cd && cat .env; cd - ; cat q"),
            Vec::<String>::new(),
            "home: unknown"
        );
        assert_eq!(viewed("cd -P sub && cat x"), ["sub/x"]);
        assert_eq!(viewed("cat $F \"$G\" lit"), ["lit"], "expansions left out");
        assert_eq!(viewed("echo $(cat inner) | head -n1 < in"), ["inner", "in"]);
        assert_eq!(viewed("f() { cat defined; }"), Vec::<String>::new());
        assert_eq!(viewed("! cat w"), ["w"]);
        assert_eq!(viewed("not ( valid"), Vec::<String>::new());
        assert_eq!(
            viewed("cat a\nnot ( valid\ncat b"),
            ["a", "b"],
            "the lines it can parse"
        );
        assert_eq!(
            viewed("cd d\ncat x\nnot ( valid\ncat > f <<'E'\ncat body\nE\ncat y"),
            ["d/x", "d/y"],
            "around a bad line: the cd kept, a heredoc's body still text"
        );
        assert_eq!(viewed("while read l; do :; done < list.txt"), ["list.txt"]);
        assert_eq!(
            viewed("if true; then :; fi < f; case x in *) :;; esac < g"),
            ["f", "g"]
        );
        assert_eq!(
            viewed("x=$(cat README.md); for f in $(cat files); do :; done"),
            ["README.md", "files"]
        );
        assert_eq!(viewed("while read l; do :; done < <(cat x)"), ["x"]);
        assert_eq!(
            viewed("cd sub & cat x"),
            ["x"],
            "a background cd stays in it"
        );
        assert_eq!(
            viewed("pushd src/fs && cat view.rs && popd && cat y"),
            ["src/fs/view.rs", "y"]
        );
        assert_eq!(viewed("builtin cd d; command cat e"), ["d/e"]);
        assert_eq!(
            viewed("cd ~ && cd /m/ws/src && cat main.rs"),
            ["/m/ws/src/main.rs"]
        );
        assert_eq!(
            viewed("cat docs/../README"),
            ["docs/../README"],
            "left to the filesystem"
        );
        assert_eq!(viewed("cd a/../b && cat x"), ["b/x"], "cd is logical");
        let from = |c: &str, cwd: &str| -> Vec<String> {
            let files = super::viewed(c, Path::new(cwd));
            files.into_iter().map(|p| p.display().to_string()).collect()
        };
        assert_eq!(
            from("cd .. && cat Cargo.toml", "/m/crates/foo"),
            ["/m/crates/Cargo.toml"]
        );
        assert_eq!(
            from("cd ../bar && cat x", "/m/crates/foo"),
            ["/m/crates/bar/x"]
        );
        assert_eq!(
            from("cd /m/ws/link/.. && cat x", "/w"),
            ["/m/ws/x"],
            "logical"
        );
        assert_eq!(
            viewed("cd ~ && cat /m/ws/a.rs < /m/ws/b"),
            ["/m/ws/b", "/m/ws/a.rs"],
            "absolute: known"
        );
        assert_eq!(
            viewed("pushd -n /other && cat f"),
            ["f"],
            "-n: the stack only"
        );
        assert_eq!(
            viewed("pushd a; pushd b; popd -n; cat x; popd; cat y"),
            ["a/b/x"],
            "then unknown"
        );
        assert_eq!(viewed("pushd +1; cat z"), Vec::<String>::new());
        assert_eq!(viewed("cd +build && cat log"), ["+build/log"]);
        assert_eq!(viewed("[[ -f x ]] < r; (( 1 )) < s"), ["r", "s"]);
    }

    #[test]
    fn normalized_paths() {
        assert_eq!(
            normalized(Path::new("/m/ws/./d/../a.txt")),
            Path::new("/m/ws/a.txt")
        );
    }
}
