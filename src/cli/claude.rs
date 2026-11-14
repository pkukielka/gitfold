//! Claude Code's worktrees as gitfold workspaces: its `WorktreeCreate` and
//! `WorktreeRemove` hooks (`gitfold branch --claude-hook worktree-create`,
//! `gitfold remove --claude-hook worktree-remove`), which replace its own
//! `git worktree` logic.
//!
//! The hooks run for every repository the session is in (a plugin's hooks
//! wherever it is installed), so for one not used with gitfold they do what
//! Claude Code would: a git worktree in `.claude/worktrees/<name>`. One used
//! with gitfold but not mounted fails instead: a full checkout of it is what
//! gitfold is there to avoid.

use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::git;
use crate::mount_table;

/// Claude Code's branch name for its worktree `name`, kept so a worktree
/// named like a branch of the repository (`main`) is never that branch's
/// workspace.
fn branch_of(name: &str) -> String {
    format!("worktree-{name}")
}

/// `WorktreeCreate`: makes worktree `name` of the repository of the session's
/// directory and prints its path, the only thing on stdout, which Claude Code
/// reads. A workspace of branch `worktree-<name>` (created from the session's
/// HEAD if new) when the repository is mounted.
pub fn worktree_create() -> Result<()> {
    let input = stdin()?;
    let name = field(&input, "name").context("no \"name\" in the hook's input")?;
    let cwd = match field(&input, "cwd") {
        Some(cwd) => PathBuf::from(cwd),
        None => std::env::current_dir()?,
    };
    let repo = git::discover(&cwd)?;
    let common_dir = git::init(&repo)?;
    let branch = branch_of(&name);
    let path = match mount_table::mount_of(&common_dir)? {
        Some(mount) => {
            if !git::branch_exists(&branch) {
                quiet(git::cmd::command(&repo).args(["branch", "--", &branch]))?;
            }
            let path = mount.join(&branch);
            // Looking it up opens it.
            let meta = fs::metadata(&path)
                .with_context(|| format!("opening workspace {}", path.display()))?;
            ensure!(meta.is_dir(), "{} is not a workspace", path.display());
            path
        }
        None if common_dir.join("gitfold").exists() => bail!(
            "{} is used with gitfold but not mounted: mount it (`gitfold mount`) for a \
             workspace, rather than a full checkout",
            common_dir.display()
        ),
        None => {
            // As Claude Code: below the top of the worktree the session is in.
            // From `cwd`: with only `--git-dir`, git takes the current directory
            // for the top.
            let out = Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["rev-parse", "--show-toplevel"])
                .output()?;
            ensure!(out.status.success(), "{} is in no worktree", cwd.display());
            let top = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim_end());
            let path = top.join(".claude/worktrees").join(&name);
            if !path.join(".git").exists() {
                let mut add = Command::new("git");
                add.arg("-C").arg(&top).args(["worktree", "add"]);
                match git::branch_exists(&branch) {
                    true => add.arg(&path).arg(&branch),
                    false => add.args(["-b", &branch]).arg(&path),
                };
                quiet(&mut add)?;
            }
            path
        }
    };
    println!("{}", path.display());
    Ok(())
}

/// `WorktreeRemove`: removes the worktree at `worktree_path` (Claude Code asks
/// once the user chose to, or the worktree is clean), discarding its changes
/// as Claude Code does; its branch stays. A workspace in use (the session's
/// shell still in it) is waited for briefly.
pub fn worktree_remove() -> Result<()> {
    let input = stdin()?;
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
        return super::remove::remove_in(&common_dir, args);
    }
    let mut remove = git::cmd::command(&common_dir);
    remove.args(["worktree", "remove", "--force"]).arg(&path);
    quiet(&mut remove)
}

/// The hook's input: Claude Code's JSON on stdin.
fn stdin() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading the hook's input")?;
    Ok(input)
}

/// Runs `command` with its stdout on stderr: a `WorktreeCreate` hook's stdout
/// is the path.
fn quiet(command: &mut Command) -> Result<()> {
    std::io::stdout().flush()?;
    let stderr = std::io::stderr().as_fd().try_clone_to_owned()?;
    let status = command.stdout(Stdio::from(stderr)).status()?;
    ensure!(status.success(), "{command:?} failed ({status})");
    Ok(())
}

/// The string `key` holds at the top level of JSON object `json`, if it is
/// one; the hooks' input needs no more of JSON than this.
fn field(json: &str, key: &str) -> Option<String> {
    let mut p = Json {
        s: json.as_bytes(),
        i: 0,
    };
    p.expect(b'{')?;
    if p.peek() == Some(b'}') {
        return None;
    }
    loop {
        let k = p.string()?;
        p.expect(b':')?;
        if k == key {
            return p.string();
        }
        p.skip_value()?;
        match p.next()? {
            b',' => {}
            _ => return None,
        }
    }
}

/// A cursor over JSON text; every step skips the whitespace before it.
struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    fn peek(&mut self) -> Option<u8> {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        Some(b)
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        (self.next()? == b).then_some(())
    }

    /// A string, unescaped.
    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut units: Vec<u16> = Vec::new();
        let mut out = String::new();
        loop {
            let b = *self.s.get(self.i)?;
            self.i += 1;
            if b != b'\\' && !units.is_empty() {
                out.extend(char::decode_utf16(units.drain(..)).map(|c| c.unwrap_or('\u{FFFD}')));
            }
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    let c = match e {
                        b'u' => {
                            let hex = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
                            units.push(u16::from_str_radix(hex, 16).ok()?);
                            self.i += 4;
                            continue;
                        }
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'"' | b'\\' | b'/' => e as char,
                        _ => return None,
                    };
                    if !units.is_empty() {
                        out.extend(
                            char::decode_utf16(units.drain(..)).map(|c| c.unwrap_or('\u{FFFD}')),
                        );
                    }
                    out.push(c);
                }
                _ => {
                    // UTF-8 as is: copy the whole run up to the next quote or escape.
                    let start = self.i - 1;
                    let end = self.s[start..]
                        .iter()
                        .position(|c| *c == b'"' || *c == b'\\')
                        .map_or(self.s.len(), |n| start + n);
                    out.push_str(std::str::from_utf8(&self.s[start..end]).ok()?);
                    self.i = end;
                }
            }
        }
    }

    /// Any value: a string, an object or array (strings in it skipped as
    /// strings), or a literal (number, `true`, `false`, `null`).
    fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => self.string().map(drop),
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match self.peek()? {
                        b'"' => {
                            self.string()?;
                        }
                        b'{' | b'[' => {
                            depth += 1;
                            self.i += 1;
                        }
                        b'}' | b']' => {
                            depth -= 1;
                            self.i += 1;
                            if depth == 0 {
                                return Some(());
                            }
                        }
                        _ => self.i += 1,
                    }
                }
            }
            _ => {
                while !matches!(self.s.get(self.i), None | Some(b',' | b'}' | b']'))
                    && !self.s[self.i].is_ascii_whitespace()
                {
                    self.i += 1;
                }
                Some(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Top-level strings, unescaped, past values of every kind.
    #[test]
    fn hook_input_fields() {
        let json = r#" { "session_id":"s", "n": -1.5e3, "nested": {"name": "inner", "a": [1, "]}", {}]},
            "ok": true, "name" : "bold-oak", "cwd": "/a \"b\"\\c/\u017c\u00f3\u0142w/zażółć/\ud83d\ude00" } "#;
        assert_eq!(field(json, "name").as_deref(), Some("bold-oak"));
        assert_eq!(field(json, "session_id").as_deref(), Some("s"));
        assert_eq!(
            field(json, "cwd").as_deref(),
            Some("/a \"b\"\\c/żółw/zażółć/😀")
        );
        assert_eq!(field(json, "missing"), None);
        assert_eq!(field(json, "n"), None, "not a string");
        assert_eq!(field("{}", "name"), None);
        assert_eq!(field("[\"name\"]", "name"), None);
        assert_eq!(field(r#"{"name": "unterminated"#, "name"), None);
    }
}
