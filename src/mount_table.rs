//! The mount table: which gitfold mount is where, and the source name a mount
//! shows (its repository), written by the daemon and read back by `unmount`
//! and the daemon's removals.

use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::Result;

/// The source of the gitfold mount at `path` (`fs_name` of the
/// repository's git common directory), or `None` if no gitfold mount is there.
pub fn gitfold_source(path: &Path) -> Result<Option<String>> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let want = path.display().to_string();
    for line in mountinfo.lines() {
        // id parent dev root mount-point options ... - fstype source super-options
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        if fields.get(dash + 1) == Some(&"fuse.gitfold")
            && fields.get(4).is_some_and(|m| unescape(m) == want)
        {
            return Ok(fields.get(dash + 2).map(|s| unescape(s)));
        }
    }
    Ok(None)
}

/// Undoes the mount table's escapes: the kernel octal-escapes space, tab,
/// newline and backslash (the backslash last, so `\134040` stays `\040`).
fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// The source a mount of `common_dir` shows in the mount table (`FSName`): the
/// path, with `%`, `,` (mount options are comma-separated), `\` (fusermount
/// strips it) and non-ASCII bytes percent-escaped, so it reads back exactly.
/// `gitfold unmount` reads it back to wait for the daemon.
pub fn fs_name(common_dir: &Path) -> String {
    let mut name = String::new();
    for &b in common_dir.as_os_str().as_bytes() {
        if matches!(b, b'%' | b',' | b'\\') || !b.is_ascii() {
            name.push_str(&format!("%{b:02X}"));
        } else {
            name.push(b as char);
        }
    }
    name
}

/// The git common directory of a mount with source `source` (see `fs_name`).
pub fn common_dir_of_source(source: &str) -> PathBuf {
    let s = source.as_bytes();
    let mut bytes = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let hex = source
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) if s[i] == b'%' => {
                bytes.push(b);
                i += 3;
            }
            _ => {
                bytes.push(s[i]);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mount's source names its repository exactly, whatever the path holds.
    #[test]
    fn fs_names_read_back() {
        for path in [
            "/plain/.git",
            "/a b/100%/x,y\\z/.git",
            "/zażółć/.git",
            "/t\tn\n/.git",
        ] {
            let name = fs_name(Path::new(path));
            assert!(!name.contains(',') && !name.contains('\\'), "{name}");
            assert_eq!(common_dir_of_source(&name), PathBuf::from(path));
        }
    }

    /// The kernel's mount-table escapes, the backslash last.
    #[test]
    fn mount_table_unescaped() {
        assert_eq!(unescape(r"/a\040b\011c\012d\134e"), "/a b\tc\nd\\e");
        assert_eq!(unescape(r"\134040"), r"\040");
    }
}
