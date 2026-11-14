//! The files Claude Code's Read tool and Bash commands read, for read
//! tracking: `gitfold track-reads --claude-hook pre-tool-use`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::cwd;
use crate::cli::hook::{field, field_at};
use crate::mount_table;
use crate::shell;

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
            .map(|p| shell::normalized(&cwd.join(p)))
            .collect(),
        Some("Bash") => shell::files_read(
            &field_at(input, &["tool_input", "command"]).unwrap_or_default(),
            cwd,
        ),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
