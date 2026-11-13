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

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{ErrorKind, IsTerminal, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

use crate::git;
use crate::state::task::{TaskDir, inbox_name};
use crate::state::workspace_id;

/// How long a watch keeps collecting events after the first, so one burst
/// (a file written then renamed, several messages) comes out together.
const SETTLE: Duration = Duration::from_millis(100);

/// How often a rewake waiter checks that the session that started it is
/// still there.
const PARENT_CHECK: Duration = Duration::from_secs(5);

/// Events that mean the watch itself is gone or incomplete: the directory was
/// deleted or moved, or events were dropped.
const WATCH_LOST: u32 =
    libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_Q_OVERFLOW;

/// The task folder directory of `branch`, which must exist as a branch.
fn task_dir(common_dir: &Path, branch: &str) -> Result<PathBuf> {
    git::require_branch(branch)?;
    Ok(TaskDir::of(common_dir, branch).dir)
}

/// Whether `name` in an inbox is a message (not one being written or taken).
fn is_message(name: &[u8]) -> bool {
    !name.is_empty() && !name.starts_with(b".")
}

/// `now + secs`, or `None` (no deadline) if that overflows or no time is given.
fn deadline(secs: Option<u64>) -> Option<Instant> {
    secs.and_then(|s| Instant::now().checked_add(Duration::from_secs(s)))
}

fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(Instant::now()))
}

/// The messages in `dir`, oldest first, after putting back those a reader
/// took and died before printing.
fn messages(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if is_message(name.as_bytes()) {
            found.push(entry.path());
        } else if let Some(original) = abandoned(&name.to_string_lossy()) {
            let back = dir.join(original);
            // Another reader may put it back first, or take it right after.
            if fs::rename(entry.path(), &back).is_ok() {
                found.push(back);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The message name in `.taken-<pid>-<name>` if process `<pid>` is gone.
fn abandoned(name: &str) -> Option<&str> {
    let (pid, original) = name.strip_prefix(".taken-")?.split_once('-')?;
    let pid: libc::pid_t = pid.parse().ok()?;
    // SAFETY: signal 0 only checks that the process exists.
    let gone = unsafe { libc::kill(pid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    gone.then_some(original)
}

/// A message's headers and body.
struct Message {
    file: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Message {
    /// Parses `text`: headers if its first paragraph is all `key: value`
    /// lines, else all body (a file written by hand). CRLF counts as LF.
    fn parse(file: &Path, text: &str) -> Message {
        let text = text.replace("\r\n", "\n");
        fn header(l: &str) -> Option<(&str, &str)> {
            let ok = |k: &str| {
                !k.is_empty()
                    && k.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            };
            l.split_once(':').filter(|(k, _)| ok(k))
        }
        let (headers, body) = match text.split_once("\n\n") {
            Some((head, body)) if head.lines().all(|l| header(l).is_some()) => (
                head.lines()
                    .filter_map(header)
                    .map(|(k, v)| (k.to_string(), v.trim().to_string()))
                    .collect(),
                body.to_string(),
            ),
            _ => (Vec::new(), text.clone()),
        };
        Message {
            file: file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            headers,
            body,
        }
    }

    fn header(&self, key: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str())
    }

    /// As sent: headers, an empty line, the body (ending with a newline).
    fn text(&self) -> String {
        let mut out: String = self
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}\n"))
            .collect();
        if !out.is_empty() {
            out.push('\n');
        }
        out += &self.body;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    fn json(&self) -> String {
        let headers: Vec<String> = self
            .headers
            .iter()
            .map(|(k, v)| format!("{}:{}", json_str(k), json_str(v)))
            .collect();
        format!(
            "{{\"file\":{},\"headers\":{{{}}},\"body\":{}}}",
            json_str(&self.file),
            headers.join(","),
            json_str(&self.body)
        )
    }
}

/// `s` as a JSON string.
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Takes the message at `path`: renames it to a dot name (so no other reader
/// gets it) and reads it; the caller deletes the returned file once the
/// message is printed. `None` if another reader took it first.
fn take(path: &Path) -> Result<Option<(Message, PathBuf)>> {
    let name = path.file_name().context("message")?;
    let taken = path.with_file_name(format!(
        ".taken-{}-{}",
        std::process::id(),
        name.to_string_lossy()
    ));
    match fs::rename(path, &taken) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("taking {}", path.display())),
    }
    match fs::read(&taken) {
        Ok(text) => Ok(Some((
            Message::parse(path, &String::from_utf8_lossy(&text)),
            taken,
        ))),
        Err(e) => {
            // Left for another reader.
            let _ = fs::rename(&taken, path);
            Err(e).with_context(|| format!("reading {}", path.display()))
        }
    }
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
pub struct InboxArgs {
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
pub fn inbox(repo: &Path, branch: &str, args: InboxArgs) -> Result<i32> {
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
        if events.iter().any(|(mask, _)| mask & WATCH_LOST != 0) {
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

/// `gitfold watch`: waits until something in `target` (`[<BRANCH>:]<DIR>`, a
/// directory of a task folder; `branch`'s when no branch is given) is written,
/// created, moved in, deleted or moved out, then prints what (`C` or `D`, a tab
/// and the name, each name's last change; or JSON), collecting a burst of them,
/// and exits; `D` and `.` if the directory itself goes. Prints nothing if
/// `timeout` passes first.
pub fn watch(
    repo: &Path,
    branch: Option<&str>,
    target: &str,
    timeout: Option<u64>,
    json: bool,
) -> Result<()> {
    let common_dir = git::init(repo)?;
    let (branch, dir) = match target.split_once(':') {
        Some((b, d)) => (b.to_string(), d),
        None => match branch {
            Some(b) => (b.to_string(), target),
            None => bail!("not in a gitfold workspace: name the task as <BRANCH>:<DIR>"),
        },
    };
    let dir = dir.trim_matches('/');
    if dir.split('/').any(|c| c == "..") {
        bail!("{dir}: a directory of the task folder, without `..`");
    }
    let root = task_dir(&common_dir, &branch)?;
    let path = if dir.is_empty() || dir == "." {
        root
    } else {
        root.join(dir)
    };
    match fs::metadata(&path) {
        Ok(meta) if !meta.is_dir() => {
            bail!("{dir} is a file: watch the directory it is in")
        }
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => fs::create_dir_all(&path)?,
        Err(e) => return Err(e.into()),
    }
    let changed = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE;
    let deleted = libc::IN_DELETE | libc::IN_MOVED_FROM;
    let watch = Inotify::watch(&path, changed | deleted)?;
    let deadline = deadline(timeout);
    // Each name's last change: a file replaced in one burst ends as changed.
    let mut events: BTreeMap<Vec<u8>, char> = BTreeMap::new();
    let mut settle: Option<Instant> = None;
    'reading: loop {
        let limit = match settle {
            Some(until) => Some(until.saturating_duration_since(Instant::now())),
            None => remaining(deadline),
        };
        let batch = watch.read(limit)?;
        if batch.is_empty() {
            break; // timed out, or settled
        }
        for (mask, name) in batch {
            if mask & libc::IN_Q_OVERFLOW != 0 {
                bail!("too many changes at once in {dir}: some weren't seen; look at it");
            }
            if mask & WATCH_LOST != 0 {
                events.insert(b".".to_vec(), 'D');
                break 'reading;
            }
            // Dot names: files being written or taken, not there yet.
            if is_message(&name) {
                events.insert(name, if mask & deleted != 0 { 'D' } else { 'C' });
            }
        }
        if !events.is_empty() && settle.is_none() {
            settle = Some(Instant::now() + SETTLE);
        }
    }
    for (name, kind) in &events {
        let name = String::from_utf8_lossy(name);
        match json {
            true => println!(
                "{{\"event\":{},\"path\":{}}}",
                json_str(if *kind == 'D' { "deleted" } else { "changed" }),
                json_str(&name)
            ),
            false => println!("{kind}\t{name}"),
        }
    }
    Ok(())
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

/// `secs` since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(secs: u64) -> String {
    let (days, rest) = (secs / 86400, secs % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// One inotify watch on a directory (also told when the directory itself is
/// deleted or moved).
struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    fn watch(dir: &Path, mask: u32) -> Result<Inotify> {
        // SAFETY: plain syscalls; the descriptor is owned from here on.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("inotify");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let path = CString::new(dir.as_os_str().as_bytes())?;
        let mask = mask | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("watching {}", dir.display()));
        }
        Ok(Inotify { fd })
    }

    /// The events (mask, name) that come within `limit` (forever if `None`),
    /// as one read returns them; empty if none came in time.
    fn read(&self, limit: Option<Duration>) -> Result<Vec<(u32, Vec<u8>)>> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = limit.map_or(-1, |d| d.as_millis().min(i32::MAX as u128) as i32);
        loop {
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(e).context("waiting for inotify");
            }
            if n == 0 {
                return Ok(Vec::new());
            }
            break;
        }
        let mut buf = vec![0u8; 64 * 1024];
        // SAFETY: reading into a buffer of the given length.
        let len = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if len < 0 {
            return Err(std::io::Error::last_os_error()).context("reading inotify");
        }
        let len = len as usize;
        let mut events = Vec::new();
        let mut at = 0;
        let header = std::mem::size_of::<libc::inotify_event>();
        while at + header <= len {
            // SAFETY: the kernel writes whole events; read unaligned.
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
            let name = &buf[at + header..at + header + event.len as usize];
            let name = name.split(|b| *b == 0).next().unwrap_or_default().to_vec();
            events.push((event.mask, name));
            at += header + event.len as usize;
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Message {
        Message::parse(Path::new("/inbox/1-x"), text)
    }

    #[test]
    fn headers_then_body() {
        let m = parse("from: a/b\nsubject: x: y\n\nline 1\n\nline 2: not a header\n");
        assert_eq!(m.header("from"), "a/b");
        assert_eq!(m.header("subject"), "x: y");
        assert_eq!(m.body, "line 1\n\nline 2: not a header\n");
        assert_eq!(m.file, "1-x");
    }

    #[test]
    fn crlf_is_lf() {
        let m = parse("from: x\r\nsubject: s\r\n\r\nbody\r\n");
        assert_eq!((m.header("from"), m.header("subject")), ("x", "s"));
        assert_eq!(m.body, "body\n");
    }

    #[test]
    fn without_headers_all_body() {
        for text in [
            "Hello\n\nworld\n",
            "just one line",
            "a: b\nno colon here\n\nbody",
        ] {
            let m = parse(text);
            assert!(m.headers.is_empty(), "{text:?}");
            assert_eq!(m.body, text);
        }
    }

    #[test]
    fn text_round_trips() {
        let sent = "from: a\ntime: 2026-01-01T00:00:00Z\n\nhi\n";
        assert_eq!(parse(sent).text(), sent);
        assert_eq!(parse("no headers").text(), "no headers\n");
    }

    #[test]
    fn json_escapes() {
        assert_eq!(json_str("a\"b\\c\nd\te\u{1}é"), r#""a\"b\\c\nd\te\u0001é""#);
        let m = parse("from: q\"\n\nx");
        assert_eq!(
            m.json(),
            r#"{"file":"1-x","headers":{"from":"q\""},"body":"x"}"#
        );
    }

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(946_684_799), "1999-12-31T23:59:59Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
        assert_eq!(rfc3339(4_102_444_800), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn messages_and_abandoned_takes() {
        assert!(is_message(b"0001-a"));
        assert!(!is_message(b".new-1-x") && !is_message(b".taken-1-x") && !is_message(b""));
        // Our own pid is alive: not abandoned; a malformed name never is.
        let mine = format!(".taken-{}-0001-a", std::process::id());
        assert_eq!(abandoned(&mine), None);
        assert_eq!(abandoned(".taken-notapid-0001-a"), None);
        assert_eq!(abandoned(".new-123-0001-a"), None);
        // A pid above the kernel's maximum never exists.
        assert_eq!(abandoned(".taken-2147483646-0001-a"), Some("0001-a"));
    }

    #[test]
    fn deadlines_saturate() {
        assert!(deadline(None).is_none());
        assert!(deadline(Some(u64::MAX)).is_none(), "overflow: no deadline");
        assert!(deadline(Some(5)).is_some());
    }
}
