//! Messages and watching, on the task folders (`state::task`):
//! `gitfold watch`, `gitfold inbox` and `gitfold send`.
//!
//! An inbox is a directory of a task folder (`task::inbox_name`) holding one
//! file per message, named `<nanoseconds>-<from id>` so that names sort by
//! arrival. A message is header lines (`from:`, `time:`, optionally
//! `subject:`), an empty line, then the body. Senders write a dot-named file
//! and link it into place; readers take a message by renaming it to
//! `.taken-<pid>-<name>`, so each goes to one reader, and delete it once it is
//! printed (one a dead reader left is put back). Dot names are never messages.
//! Everything is plain files: anything can send or read, and watchers use
//! inotify on the real directory, mounted or not.

mod message;
mod watch;

pub use watch::{WatchArgs, watch};

use std::fs;
use std::io::{ErrorKind, IsTerminal, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};

use crate::git;
use crate::state::task::{TaskDir, inbox_name};
use crate::state::workspace_id;

use crate::inotify::{self, Inotify};
use message::{Message, json_str, messages, rfc3339, take};

/// How often a rewake waiter checks that the session that started it is
/// still there.
const PARENT_CHECK: Duration = Duration::from_secs(5);

/// The task folder directory of `branch`, which must exist as a branch.
pub(super) fn task_dir(common_dir: &Path, branch: &str) -> Result<PathBuf> {
    git::require_branch(branch)?;
    Ok(TaskDir::of(common_dir, branch).dir)
}

/// `now + secs`, or `None` (no deadline) if that overflows or no time is given.
pub(super) fn deadline(secs: Option<u64>) -> Option<Instant> {
    secs.and_then(|s| Instant::now().checked_add(Duration::from_secs(s)))
}

pub(super) fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(Instant::now()))
}

/// How `gitfold inbox` hands messages to a Claude Code hook (`--claude-hook`).
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Hook {
    /// `UserPromptSubmit`: the messages on stdout, which becomes context.
    UserPromptSubmit,
    /// `PostToolUse`: the messages as `additionalContext`, in the hook's JSON;
    /// nothing for a subagent's tool calls (its context isn't the session's).
    PostToolUse,
    /// An `asyncRewake` hook (on `Stop`): waits for a message, then puts it on
    /// stderr and exits 2, which wakes an idle session. One such waiter per
    /// inbox; others exit at once. It ends with the session.
    Rewake,
}

/// What `gitfold inbox` was asked to do.
pub struct Options {
    pub wait: bool,
    pub timeout: Option<u64>,
    pub peek: bool,
    pub json: bool,
    pub hook: Option<Hook>,
}

/// `gitfold inbox`: takes the messages in the inbox of `branch` and prints
/// them (each as written, an empty line between; or one JSON object per line).
/// With `wait`, if there are none, waits for one (up to `timeout`, if given).
/// With `peek`, only lists them, taking nothing: `from` and `subject` (or the
/// body's first line), one per line; nothing when empty. With `hook`, hands
/// them to a Claude Code hook instead. Returns the exit code.
pub fn inbox(repo: &Path, branch: &str, args: Options) -> Result<i32> {
    let common_dir = git::init(repo)?;
    let dir = task_dir(&common_dir, branch)?.join(inbox_name(&common_dir));
    if args.peek {
        for path in messages(&dir)? {
            let Ok(bytes) = fs::read(&path) else {
                continue; // taken meanwhile
            };
            let m = Message::parse(&path, &String::from_utf8_lossy(&bytes));
            match args.json {
                true => println!("{}", m.json()),
                false => {
                    let about = match m.header("subject") {
                        "" => m.body.lines().next().unwrap_or(""),
                        subject => subject,
                    };
                    println!("{}\t{about}", m.header("from"));
                }
            }
        }
        return Ok(0);
    }
    if args.hook == Some(Hook::PostToolUse) && in_subagent()? {
        return Ok(0);
    }
    let rewake = args.hook == Some(Hook::Rewake);
    let wait = args.wait || rewake;
    // One rewake waiter per inbox: each `Stop` starts one.
    let _lock = match rewake {
        true => match lock(&dir)? {
            Some(lock) => Some(lock),
            None => return Ok(0),
        },
        false => None,
    };
    let parent = unsafe { libc::getppid() };
    let deadline = deadline(args.timeout);
    // Watching before looking: a message arriving in between isn't missed.
    let mut watch = None;
    let taken = loop {
        if wait && watch.is_none() {
            fs::create_dir_all(&dir)?;
            watch = Some(Inotify::watch(
                &dir,
                libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE,
            )?);
        }
        let mut taken = Vec::new();
        for path in messages(&dir)? {
            if let Some(found) = take(&path)? {
                taken.push(found);
            }
        }
        let Some(w) = &watch else { break taken };
        if !taken.is_empty() {
            break taken;
        }
        // In chunks for a rewake waiter, to notice its session is gone.
        let limit = match (remaining(deadline), rewake) {
            (Some(left), true) => Some(left.min(PARENT_CHECK)),
            (None, true) => Some(PARENT_CHECK),
            (left, false) => left,
        };
        let events = w.read(limit)?;
        if events.iter().any(|(mask, _)| mask & inotify::LOST != 0) {
            watch = None; // made again, then everything listed
        }
        let timed_out = deadline.is_some_and(|d| Instant::now() >= d);
        let orphaned = rewake && unsafe { libc::getppid() } != parent;
        if events.is_empty() && (timed_out || orphaned) {
            break taken;
        }
    };
    let text: String = match args.json {
        true => taken.iter().map(|(m, _)| m.json() + "\n").collect(),
        false => taken
            .iter()
            .map(|(m, _)| m.text())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let intro = "New messages in your gitfold inbox (each is taken: it won't be shown again):\n\n";
    let code = match args.hook {
        None => {
            print!("{text}");
            0
        }
        Some(_) if text.is_empty() => 0,
        Some(Hook::UserPromptSubmit) => {
            print!("{intro}{text}");
            0
        }
        Some(Hook::PostToolUse) => {
            println!(
                "{{\"hookSpecificOutput\":{{\"hookEventName\":\"PostToolUse\",\"additionalContext\":{}}}}}",
                json_str(&format!("{intro}{text}"))
            );
            0
        }
        Some(Hook::Rewake) => {
            eprint!("{intro}{text}");
            2
        }
    };
    use std::io::Write;
    std::io::stdout().flush()?;
    // Printed: now gone for good.
    for (_, file) in taken {
        let _ = fs::remove_file(file);
    }
    Ok(code)
}

/// Whether the hook input on stdin is a subagent's (it names an `agent_id`).
fn in_subagent() -> Result<bool> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(false);
    }
    let mut input = String::new();
    stdin.read_to_string(&mut input)?;
    Ok(input.contains("\"agent_id\""))
}

/// An exclusive lock on `dir`'s `.waiting` file, if no one else holds it.
fn lock(dir: &Path) -> Result<Option<fs::File>> {
    fs::create_dir_all(dir)?;
    let file = fs::File::create(dir.join(".waiting"))?;
    // SAFETY: a plain syscall on an open descriptor.
    let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
    Ok((!held).then_some(file))
}

/// `gitfold send`: leaves a message from `from` in the inbox of branch `to`:
/// `from:` and `time:` headers, `subject:` if given, then `body`.
pub fn send(repo: &Path, to: &str, from: &str, subject: Option<&str>, body: &str) -> Result<()> {
    let common_dir = git::init(repo)?;
    let dir = task_dir(&common_dir, to)?.join(inbox_name(&common_dir));
    fs::create_dir_all(&dir)?;
    let one_line = |s: &str| s.replace(['\r', '\n'], " ");
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    let mut text = format!(
        "from: {}\ntime: {}\n",
        one_line(from),
        rfc3339(now.as_secs())
    );
    if let Some(subject) = subject {
        text += &format!("subject: {}\n", one_line(subject));
    }
    text.push('\n');
    text += body;
    if !body.ends_with('\n') {
        text.push('\n');
    }
    let name = format!("{:020}-{}", now.as_nanos(), workspace_id(from));
    let tmp = dir.join(format!(".new-{}-{name}", std::process::id()));
    {
        use std::io::Write;
        let mut file = fs::File::create_new(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    // Linked, not renamed, into place: a message of the same name (the same
    // sender in the same nanosecond) is never replaced.
    let mut attempt = 0;
    let delivered = loop {
        let target = match attempt {
            0 => dir.join(&name),
            n => dir.join(format!("{name}-{n}")),
        };
        match fs::hard_link(&tmp, &target) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
            other => break other,
        }
    };
    let _ = fs::remove_file(&tmp);
    delivered.with_context(|| format!("delivering to {to}"))?;
    println!("sent to {to}");
    Ok(())
}

/// The arguments of `gitfold inbox` (see `main`).
#[derive(clap::Args)]
pub struct InboxArgs {
    /// The workspace's branch (default: the workspace you are in).
    pub branch: Option<String>,
    /// If there are none, wait for one.
    #[arg(long)]
    pub wait: bool,
    /// With --wait: give up after this many seconds.
    #[arg(long, value_name = "SECS", requires = "wait")]
    pub timeout: Option<u64>,
    /// Only list them, taking nothing: sender and subject (or first line),
    /// one per line; nothing if there are none.
    #[arg(long, conflicts_with = "wait")]
    pub peek: bool,
    /// One JSON object per message: {"file", "headers", "body"}.
    #[arg(long)]
    pub json: bool,
    /// Hand them to a Claude Code hook: `user-prompt-submit` (as context),
    /// `post-tool-use` (as `additionalContext`), or `rewake` (an async
    /// `Stop` hook with `asyncRewake`: waits for one, then wakes the idle
    /// session). See skills/gitfold/SKILL.md.
    #[arg(long, value_enum, value_name = "EVENT", conflicts_with = "peek")]
    pub claude_hook: Option<crate::cli::inbox::Hook>,
}

/// The arguments of `gitfold send` (see `main`).
#[derive(clap::Args)]
pub struct SendArgs {
    /// The recipient's branch.
    pub to: String,
    /// The message (`-`: read it from stdin).
    #[arg(required = true)]
    pub text: Vec<String>,
    /// Who it is from (default: the workspace you are in, else `user`).
    #[arg(long)]
    pub from: Option<String>,
    #[arg(short, long)]
    pub subject: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_saturate() {
        assert!(deadline(None).is_none());
        assert!(deadline(Some(u64::MAX)).is_none(), "overflow: no deadline");
        assert!(deadline(Some(5)).is_some());
    }
}
