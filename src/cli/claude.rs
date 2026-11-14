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

/// `PreToolUse` on Read and Bash (`gitfold track-reads --claude-hook
/// pre-tool-use`): reports the files the tool is about to read to the daemon
/// of the gitfold mount they are in, which records them in tracked
/// workspaces: reads it may not see (the kernel serves a file read before, by
/// a search say, from its cache; searches' reads aren't recorded). Read: its
/// `file_path`; Bash: the files simple viewers name (`named`). Before the tool
/// runs, for the directory a command starts in; so a call the user then
/// denies is recorded too. Async, on every such call: errors are only logged.
pub fn reads_hook() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    if let Err(e) = report_reads(&input) {
        log::debug!("gitfold track-reads --claude-hook: {e:#}");
    }
    Ok(())
}

fn report_reads(input: &str) -> Result<()> {
    let named = named(input, &cwd(input)?);
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

/// The files the tool in hook input `input` reads, from `cwd` (before the
/// tool runs: `PreToolUse`; after, a Bash command's own `cd` would count
/// twice). The Read tool resolves `..` in its path lexically (it is Node's
/// `path.resolve`); a Bash command's files are as `cat` gets them, `..`
/// resolved by the filesystem (`report_reads`).
fn named(input: &str, cwd: &Path) -> Vec<PathBuf> {
    match field(input, "tool_name").as_deref() {
        Some("Read") => field_at(input, &["tool_input", "file_path"])
            .into_iter()
            .map(|p| normalized(&cwd.join(p)))
            .collect(),
        Some("Bash") => viewed(
            &field_at(input, &["tool_input", "command"]).unwrap_or_default(),
            cwd,
        ),
        _ => Vec::new(),
    }
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
        physical: false,
    };
    // A syntax error stops bash there: what comes before it runs. Cut before
    // the error's line (and the lines continued into it by a `\`), again
    // while that still doesn't parse (the error was in an open `for`, say).
    let lines: Vec<&str> = command.split_inclusive('\n').collect();
    let mut end = lines.len();
    let nodes = loop {
        match rable::parse(&lines[..end].concat(), true) {
            Ok(nodes) => break nodes,
            Err(e) => {
                let mut cut = (e.line().saturating_sub(1)).min(end.saturating_sub(1));
                while cut > 0 && lines[cut - 1].trim_end_matches('\n').ends_with('\\') {
                    cut -= 1;
                }
                if cut == 0 {
                    break Vec::new();
                }
                end = cut;
            }
        }
    };
    for node in &nodes {
        walk(node, &mut at, &mut files);
    }
    files
}

/// Where relative paths are while walking a command: `dir`, absolute
/// (`None` once unknown: `cd ~`, `cd -`, `cd $X`), and the `pushd` stack.
#[derive(Clone)]
struct At {
    dir: Option<PathBuf>,
    pushed: Vec<Option<PathBuf>>,
    /// `dir` holds a `..` the filesystem resolves (after `cd -P`): later
    /// `cd`s leave theirs to it too, as the shell's PWD is then physical.
    physical: bool,
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
    // `-P` (the last of `-L`/`-P`, alone or combined: `-LP`): `..` resolved
    // physically, by the filesystem later; else logically, as the shell does
    // (`a/..` is `.` whatever `a` is).
    let options = args
        .iter()
        .flatten()
        .take_while(|a| a.starts_with('-') && *a != "-" && *a != "--");
    let physical = at.physical
        || options
            .flat_map(|a| a.chars().filter(|c| matches!(c, 'L' | 'P')))
            .last()
            == Some('P');
    // Where `cd` or `pushd` goes: `None` for home, the previous directory,
    // an expansion, or anywhere relative to an unknown `dir`.
    let to = |to: &str| {
        let to = (!to.starts_with('~') && to != "-").then(|| at.file(to))??;
        Some(if physical { to } else { normalized(&to) })
    };
    // `-n` (the stack only); a rotation (`+N`, `-N`), or for popd any
    // argument we can't read (it takes no directory): an entry.
    let no_cd = args.iter().any(|a| a.as_deref() == Some("-n"));
    let rotation = args.iter().any(|a| match a.as_deref() {
        Some(a) => a
            .strip_prefix(['+', '-'])
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        None => program == "popd",
    });
    match program.as_str() {
        "cd" => {
            at.dir = operand().cloned().flatten().and_then(|t| to(&t));
            at.physical = physical;
            return;
        }
        // The stack unknown, and with a rotation where it leaves us too.
        "pushd" | "popd" if no_cd || rotation => {
            at.pushed = vec![None];
            if !no_cd {
                at.dir = None;
            }
            return;
        }
        "pushd" => {
            let dir = operand().cloned().flatten().and_then(|t| to(&t));
            at.pushed.push(at.dir.take());
            at.dir = dir;
            at.physical = physical;
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
    let rable::NodeKind::Word { value, parts, .. } = &node.kind else {
        return None;
    };
    // Its parts: literals, or expansions (variables, substitutions).
    if !parts
        .iter()
        .all(|p| matches!(p.kind, rable::NodeKind::WordLiteral { .. }))
    {
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
            ["a"],
            "bash stops at the error"
        );
        assert_eq!(viewed("cd d\ncat x\nnot ( valid\ncat y"), ["d/x"]);
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
        assert_eq!(viewed("pushd a; popd +1; cat z"), Vec::<String>::new());
        assert_eq!(
            viewed("pushd sub; popd \"$i\"; cat z"),
            Vec::<String>::new()
        );
        assert_eq!(
            viewed("cat a\nfor f in x; do\ncat b\n"),
            ["a"],
            "an open for"
        );
        assert_eq!(viewed("cat a\necho 'open\ncat b"), ["a"], "an open quote");
        assert_eq!(
            viewed("cat a \\\n  b; not ( valid"),
            Vec::<String>::new(),
            "a continued line"
        );
        assert_eq!(
            super::viewed("cd -P link && cd .. && cat x", Path::new("/w")),
            [Path::new("/w/link/../x")],
            "after -P, cd .. is physical too"
        );
        assert_eq!(
            super::viewed("cd -P -L link/.. && cat x", Path::new("/w")),
            [Path::new("/w/x")],
            "the last of -L/-P"
        );
        assert_eq!(
            super::viewed("cd -LP link/.. && cat x", Path::new("/w")),
            [Path::new("/w/link/../x")]
        );
        assert_eq!(viewed("pushd ünï && cat x"), ["ünï/x"], "no panic on UTF-8");
        assert_eq!(
            viewed("pushd \"$B\" && popd && cat r"),
            ["r"],
            "an expansion: popd still known"
        );
        assert_eq!(
            viewed("cat x<(cat y)"),
            ["y"],
            "a substitution inside a word"
        );
        assert_eq!(viewed("cd +build && cat log"), ["+build/log"]);
        assert_eq!(
            super::viewed("cd -P /m/ws/link/.. && cat x", Path::new("/w")),
            [Path::new("/m/ws/link/../x")],
            "-P: physical"
        );
        assert_eq!(viewed("[[ -f x ]] < r; (( 1 )) < s"), ["r", "s"]);
    }

    /// Read: its path, `..` resolved lexically; Bash: from the directory
    /// before the command.
    #[test]
    fn named_files() {
        let read = r#"{"tool_name":"Read","tool_input":{"file_path":"/m/ws/link/../a.rs"}}"#;
        assert_eq!(named(read, Path::new("/w")), [Path::new("/m/ws/a.rs")]);
        let bash = r#"{"tool_name":"Bash","tool_input":{"command":"cd sub && cat x"}}"#;
        assert_eq!(named(bash, Path::new("/w")), [Path::new("/w/sub/x")]);
        assert!(named(r#"{"tool_name":"Grep"}"#, Path::new("/w")).is_empty());
    }

    #[test]
    fn normalized_paths() {
        assert_eq!(
            normalized(Path::new("/m/ws/./d/../a.txt")),
            Path::new("/m/ws/a.txt")
        );
    }
}
