//! The mount table: which gitfold mount is where, and the source name a mount
//! shows (its repository), written by the daemon and read back by commands
//! (`unmount`, `version`) and the daemon's removals.

use std::fmt::Write;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::Result;

/// The source of the gitfold mount at `path` (`fs_name` of the
/// repository's git common directory), or `None` if no gitfold mount is there.
pub fn gitfold_source(path: &Path) -> Result<Option<String>> {
    // Bytes: other mounts' paths need not be UTF-8.
    let mountinfo = fs::read("/proc/self/mountinfo")?;
    Ok(source_in(&mountinfo, path.as_os_str().as_bytes()))
}

/// Where the repository of git common directory `common_dir` is mounted by
/// gitfold, if it is.
pub fn mount_of(common_dir: &Path) -> Result<Option<PathBuf>> {
    let mountinfo = fs::read("/proc/self/mountinfo")?;
    let ours = fs::canonicalize(common_dir).ok();
    Ok(mounts_in(&mountinfo)
        .find(|(_, source)| {
            let theirs = common_dir_of_source(source);
            theirs == common_dir || ours.is_some() && fs::canonicalize(&theirs).ok() == ours
        })
        .map(|(mount, _)| PathBuf::from(std::ffi::OsString::from_vec(mount))))
}

/// The source of the gitfold mount at `path` in mount table `mountinfo`.
fn source_in(mountinfo: &[u8], path: &[u8]) -> Option<String> {
    mounts_in(mountinfo)
        .find(|(mount, _)| mount == path)
        .map(|(_, source)| source)
}

/// The gitfold mounts in mount table `mountinfo`: mount point and source.
fn mounts_in(mountinfo: &[u8]) -> impl Iterator<Item = (Vec<u8>, String)> + '_ {
    mountinfo.split(|b| *b == b'\n').filter_map(|line| {
        // id parent dev root mount-point options ... - fstype source super-options
        let fields: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
        let dash = fields.iter().position(|f| *f == b"-")?;
        if fields.get(dash + 1) != Some(&b"fuse.gitfold".as_slice()) {
            return None;
        }
        let source = unescape(fields.get(dash + 2)?);
        Some((
            unescape(fields.get(4)?),
            String::from_utf8_lossy(&source).into_owned(),
        ))
    })
}

/// Undoes the mount table's escapes: the kernel octal-escapes space, tab,
/// newline and backslash (the backslash last, so `\134040` stays `\040`).
fn unescape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let byte = match s.get(i..i + 4) {
            Some(b"\\040") => Some(b' '),
            Some(b"\\011") => Some(b'\t'),
            Some(b"\\012") => Some(b'\n'),
            Some(b"\\134") => Some(b'\\'),
            _ => None,
        };
        match byte {
            Some(b) => {
                out.push(b);
                i += 4;
            }
            None => {
                out.push(s[i]);
                i += 1;
            }
        }
    }
    out
}

/// The source a mount of `common_dir` shows in the mount table (`FSName`): the
/// path, with `%`, `,` (mount options are comma-separated), `\` (fusermount
/// strips it) and non-ASCII bytes percent-escaped, so it reads back exactly.
/// `gitfold unmount` reads it back to wait for the daemon.
pub fn fs_name(common_dir: &Path) -> String {
    let mut name = String::new();
    for &b in common_dir.as_os_str().as_bytes() {
        if matches!(b, b'%' | b',' | b'\\') || !b.is_ascii() {
            let _ = write!(name, "%{b:02X}");
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
        assert_eq!(unescape(br"/a\040b\011c\012d\134e"), b"/a b\tc\nd\\e");
        assert_eq!(unescape(br"\134040"), br"\040");
    }

    /// Found by its path's bytes, whatever other mounts' paths hold.
    #[test]
    fn source_of_mount() {
        let table: &[u8] = b"36 1 0:30 / /x\xff/m rw - ext4 /dev/sda rw\n\
            37 1 0:31 / /a\\040b\xfe rw,nosuid - fuse.gitfold /r/.git rw\n";
        assert_eq!(source_in(table, b"/a b\xfe").as_deref(), Some("/r/.git"));
        assert_eq!(source_in(table, b"/x\xff/m"), None);
        assert_eq!(source_in(table, b"/elsewhere"), None);
    }
}
