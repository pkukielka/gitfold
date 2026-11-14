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
    let named = match field(input, "tool_name").as_deref() {
        Some("Read") => field_at(input, &["tool_input", "file_path"])
            .into_iter()
            .collect(),
        Some("Bash") => viewed(&field_at(input, &["tool_input", "command"]).unwrap_or_default()),
        _ => return Ok(()),
    };
    if named.is_empty() {
        return Ok(());
    }
    let cwd = cwd(input)?;
    let mounts = mount_table::mounts()?;
    // Per mount (its git common dir), the paths below it.
    let mut reads: Vec<(&Path, Vec<String>)> = Vec::new();
    for path in named.iter().map(|p| normalized(&cwd.join(p))) {
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
        match reads.iter_mut().find(|(c, _)| *c == common_dir.as_path()) {
            Some((_, paths)) => paths.push(rel.to_string()),
            None => reads.push((common_dir, vec![rel.to_string()])),
        }
    }
    for (common_dir, paths) in reads {
        let request = crate::daemon::control::Request::RecordReads { paths };
        crate::daemon::control::request(common_dir, &request, None)?;
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
/// arguments (or `<` a file), in each command of its pipelines and lists. A
/// guess, not a parse: quotes are handled, expansions aren't (what isn't a
/// file in a workspace is dropped later). Keeping this small is the point: a
/// file read by anything else is seen unless a search read it first.
fn viewed(command: &str) -> Vec<String> {
    // sed's options that give its script; then its first operand isn't one.
    const SED_SCRIPT: &[&str] = &["-e", "--expression", "-f", "--file"];
    let mut files = Vec::new();
    for words in commands(command) {
        let mut words = words.into_iter().peekable();
        // Assignments before the command (`LC_ALL=C cat x`).
        while words
            .peek()
            .is_some_and(|w| w.word.contains('=') && !w.word.starts_with('='))
        {
            words.next();
        }
        let Some(program) = words.next() else {
            continue;
        };
        let program = program.word.rsplit('/').next().unwrap_or_default();
        // Options that take the next word as their value; `None`: not a
        // viewer (only what `<` gives it is read).
        let valued: Option<&[&str]> = match program {
            "cat" | "less" | "more" | "nl" | "tac" | "bat" | "batcat" => Some(&[]),
            "head" | "tail" => Some(&["-n", "-c", "--lines", "--bytes"]),
            "sed" => Some(&["-e", "--expression", "-f", "--file", "-l"]),
            _ => None,
        };
        let mut operands = Vec::new();
        let mut script_given = false;
        let mut options_end = false;
        let mut words = words.peekable();
        while let Some(w) = words.next() {
            let arg = w.word;
            match w.redirect {
                Some('<') => files.push(arg),
                Some(_) => {}
                None if valued.is_none() => {}
                None if options_end || !arg.starts_with('-') || arg == "-" => operands.push(arg),
                None if arg == "--" => options_end = true,
                None => {
                    let name = arg.split('=').next().unwrap_or_default();
                    script_given |= SED_SCRIPT.contains(&name);
                    if valued.is_some_and(|v| v.contains(&arg.as_str())) {
                        words.next();
                    }
                }
            }
        }
        let skip = usize::from(program == "sed" && !script_given);
        files.extend(operands.into_iter().skip(skip).filter(|f| f != "-"));
    }
    files
}

/// A word of a shell command, and the redirection (`<`, `>`) it is the
/// target of, if any.
struct Word {
    word: String,
    redirect: Option<char>,
}

/// The simple commands of `command` (split at `|`, `;`, `&` and newlines),
/// each as its words, quotes removed.
fn commands(command: &str) -> Vec<Vec<Word>> {
    let mut commands = Vec::new();
    let mut current: Vec<Word> = Vec::new();
    let mut word: Option<String> = None;
    let mut redirect = None;
    let mut chars = command.chars().peekable();
    loop {
        let c = chars.next();
        // A word ends at whitespace, an operator or the end.
        if c.is_none_or(|c| c.is_whitespace() || "|;&\n<>".contains(c))
            && let Some(w) = word.take()
        {
            current.push(Word {
                word: w,
                redirect: redirect.take(),
            });
        }
        match c {
            None => break,
            Some('\'') => {
                let w = word.get_or_insert_default();
                w.extend(chars.by_ref().take_while(|c| *c != '\''));
            }
            Some('"') => {
                let w = word.get_or_insert_default();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => w.extend(chars.next()),
                        c => w.push(c),
                    }
                }
            }
            Some('\\') => word.get_or_insert_default().extend(chars.next()),
            Some('|' | ';' | '&' | '\n') => {
                redirect = None;
                commands.push(std::mem::take(&mut current));
            }
            Some(c @ ('<' | '>')) => {
                // `2>`: the digit was taken for a word; it is the redirection's.
                if current.last().is_some_and(|w| {
                    w.redirect.is_none() && w.word.chars().all(|c| c.is_ascii_digit())
                }) {
                    current.pop();
                }
                redirect = Some(c);
                while chars.peek().is_some_and(|c| matches!(c, '<' | '>' | '&')) {
                    chars.next();
                }
            }
            Some(c) if c.is_whitespace() => {}
            Some(c) => word.get_or_insert_default().push(c),
        }
    }
    commands.push(current);
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files simple viewers show, wherever they are in the command.
    #[test]
    fn viewed_files() {
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
    }

    #[test]
    fn normalized_paths() {
        assert_eq!(
            normalized(Path::new("/m/ws/./d/../a.txt")),
            Path::new("/m/ws/a.txt")
        );
    }
}
