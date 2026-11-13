//! The message format: one file per message in an inbox (see the module doc
//! of `cli::inbox`), how readers list, take and parse them.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Whether `name` in an inbox is a message (not one being written or taken).
pub(super) fn is_message(name: &[u8]) -> bool {
    !name.is_empty() && !name.starts_with(b".")
}

/// The messages in `dir`, oldest first, after putting back those a reader
/// took and died before printing.
pub(super) fn messages(dir: &Path) -> Result<Vec<PathBuf>> {
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
pub(super) fn abandoned(name: &str) -> Option<&str> {
    let (pid, original) = name.strip_prefix(".taken-")?.split_once('-')?;
    let pid: libc::pid_t = pid.parse().ok()?;
    // SAFETY: signal 0 only checks that the process exists.
    let gone = unsafe { libc::kill(pid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    gone.then_some(original)
}

/// A message's headers and body.
pub(super) struct Message {
    pub(super) file: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: String,
}

impl Message {
    /// Parses `text`: headers if its first paragraph is all `key: value`
    /// lines, else all body (a file written by hand). CRLF counts as LF.
    pub(super) fn parse(file: &Path, text: &str) -> Message {
        let text = text.replace("\r\n", "\n");
        pub(super) fn header(l: &str) -> Option<(&str, &str)> {
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

    pub(super) fn header(&self, key: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str())
    }

    /// As sent: headers, an empty line, the body (ending with a newline).
    pub(super) fn text(&self) -> String {
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

    pub(super) fn json(&self) -> String {
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
pub(super) fn json_str(s: &str) -> String {
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
pub(super) fn take(path: &Path) -> Result<Option<(Message, PathBuf)>> {
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

/// `secs` since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub(super) fn rfc3339(secs: u64) -> String {
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
}
