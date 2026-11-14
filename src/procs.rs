//! Processes using a mount, from `/proc`: for `gitfold unmount` (who keeps it
//! busy) and the daemon's removals (who uses a workspace), and the process's
//! own ancestors (a shell inside the mount running the command); this user's
//! processes (`own`, also searched for prewarm walkers).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A process using a mount.
pub struct User {
    pub pid: u32,
    /// Its command line, shortened.
    pub command: String,
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} {}", self.pid, self.command)
    }
}

/// The processes of this user (only they can use the mount) using something
/// at or below `path` in the mount at `mountpoint`: their working or root
/// directory, an open file or a mapped one. Read before detaching: after it,
/// their paths no longer resolve to the mount point. Nothing below the mount
/// point is looked up through the mount.
pub fn users(mountpoint: &Path, path: &Path) -> Vec<User> {
    // Only needed (and only stat-ed: through the mount) for links to the
    // mount point itself, never for a path below it.
    let mount_dev = std::cell::OnceCell::new();
    // A link to the mount point itself may be the directory under the mount
    // (entered before mounting): the device tells. Stat-ing it goes through
    // the mount root, which the daemon still serves.
    let in_mount = |link: &Path| {
        fs::read_link(link).is_ok_and(|p| {
            p.starts_with(path)
                && (p != mountpoint || {
                    let dev =
                        mount_dev.get_or_init(|| fs::metadata(mountpoint).map(|m| m.dev()).ok());
                    fs::metadata(link).is_ok_and(|m| Some(m.dev()) == *dev)
                })
        })
    };
    let mut users = Vec::new();
    for (pid, dir) in own() {
        let uses = ["cwd", "root", "exe"]
            .iter()
            .any(|n| in_mount(&dir.join(n)))
            || fs::read_dir(dir.join("fd"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|fd| in_mount(&fd.path()))
            || fs::read_to_string(dir.join("maps")).is_ok_and(|maps| {
                maps.lines()
                    .filter_map(|l| l.find('/').map(|at| Path::new(&l[at..])))
                    .any(|p| p.starts_with(path))
            });
        if uses {
            let cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
            let mut command = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            command = command.trim().to_string();
            if command.is_empty() {
                // It cleared its command line.
                command = fs::read_to_string(dir.join("comm")).unwrap_or_default();
                command = format!("[{}]", command.trim());
            }
            if command.chars().count() > 80 {
                command = command.chars().take(77).collect::<String>() + "...";
            }
            users.push(User { pid, command });
        }
    }
    users
}

/// The processes of this user: (pid, its `/proc` directory).
pub fn own() -> impl Iterator<Item = (u32, PathBuf)> {
    let uid = unsafe { libc::getuid() };
    let procs = fs::read_dir("/proc").into_iter().flatten().flatten();
    procs.filter_map(move |proc| {
        let pid = proc.file_name().to_str()?.parse().ok()?;
        let ours = proc.metadata().is_ok_and(|m| m.uid() == uid);
        ours.then(|| (pid, proc.path()))
    })
}

/// This process and its ancestors (its parent, the parent's parent, ...).
pub fn self_and_ancestors() -> Vec<u32> {
    let mut pid = std::process::id();
    let mut pids = vec![pid];
    while let Some(ppid) = parent(pid) {
        pids.push(ppid);
        pid = ppid;
    }
    pids
}

/// The parent of process `pid`, if it has one (and is visible here).
pub fn parent(pid: u32) -> Option<u32> {
    // pid (comm) state ppid ...; comm may contain spaces and parentheses.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let at = stat.rfind(')')?;
    let ppid = stat[at + 1..].split_whitespace().nth(1)?.parse().ok()?;
    (ppid > 0).then_some(ppid)
}

/// Programs that search files: what they read is a scan, not a read an agent
/// acted on, and isn't recorded (`fs::workspace::reads`).
const SCANNERS: &[&str] = &[
    "rg", "grep", "egrep", "fgrep", "ugrep", "ag", "ack", "ack-grep", "pt", "sift",
];

/// Whether a process named `comm` with command line `cmdline` (NUL-separated,
/// `None` if not read yet) is a file searcher: by its name, by `argv[0]`'s
/// file name (Claude Code's `grep` is its own binary run as `ugrep`), or as
/// `git grep`. `None` if that takes the command line.
fn scanner_named(comm: &str, cmdline: Option<&[u8]>) -> Option<bool> {
    if SCANNERS.contains(&comm) {
        return Some(true);
    }
    let mut args = cmdline?.split(|b| *b == 0);
    let argv0 = args.next().unwrap_or_default();
    let name = argv0.rsplit(|b| *b == b'/').next().unwrap_or_default();
    let scanner = SCANNERS.iter().any(|s| name == s.as_bytes())
        || (name == b"git" && git_command(args) == Some(b"grep"));
    Some(scanner)
}

/// git's command among its arguments `args`: the first that isn't one of
/// git's own options (`git -C <dir> -c <key=value> grep`).
fn git_command<'a>(mut args: impl Iterator<Item = &'a [u8]>) -> Option<&'a [u8]> {
    const VALUED: &[&[u8]] = &[b"-C", b"-c", b"--git-dir", b"--work-tree", b"--namespace"];
    while let Some(arg) = args.next() {
        if VALUED.contains(&arg) {
            args.next();
        } else if !arg.starts_with(b"-") {
            return Some(arg);
        }
    }
    None
}

/// How long `Scanners` trusts what it found for a pid: a search's threads
/// look about once a second; a pid reused sooner (pids can wrap at 32768, and
/// builds start many) would be mistaken, and a read lost.
const SCANNER_TTL: std::time::Duration = std::time::Duration::from_secs(1);

/// Which processes are file searchers (`scanner_named`), from `/proc`,
/// remembered per pid (or thread id, as FUSE gives it) for `SCANNER_TTL`.
#[derive(Default)]
pub struct Scanners(
    parking_lot::RwLock<std::collections::HashMap<u32, (std::time::Instant, bool)>>,
);

impl Scanners {
    pub fn is(&self, pid: u32) -> bool {
        let now = std::time::Instant::now();
        if let Some((at, scanner)) = self.0.read().get(&pid)
            && now.duration_since(*at) < SCANNER_TTL
        {
            return *scanner;
        }
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let scanner = scanner_named(comm.trim_end(), None).unwrap_or_else(|| {
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            scanner_named(comm.trim_end(), Some(&cmdline)).unwrap_or_default()
        });
        let mut known = self.0.write();
        if known.len() > 4096 {
            known.retain(|_, (at, _)| now.duration_since(*at) < SCANNER_TTL);
        }
        known.insert(pid, (now, scanner));
        scanner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// By the process name or argv[0]'s file name.
    #[test]
    fn scanners_by_name() {
        let named = |comm, cmdline: &[u8]| scanner_named(comm, Some(cmdline)).unwrap();
        assert_eq!(scanner_named("rg", None), Some(true), "the name is enough");
        assert_eq!(scanner_named("cat", None), None, "else the command line");
        assert!(named(".claude-wrapped", b"ugrep\0-G\0x"));
        assert!(named("rg-thread", b"/nix/store/x-ripgrep/bin/rg\0x"));
        assert!(named("git", b"git\0grep\0-n\0x"));
        assert!(
            named("git", b"/usr/bin/git\0--no-pager\0grep\0x"),
            "after options"
        );
        assert!(
            named("git", b"git\0-C\0/m/ws\0-c\0a=b\0grep\0x"),
            "valued ones"
        );
        assert!(
            !named("git", b"git\0-C\0grep\0status"),
            "a directory named grep"
        );
        assert!(!named("git", b"git\0status"));
        assert!(!named("git", b"git\0log\0--grep\0x"));
        assert!(!named("cat", b"/usr/bin/cat\0f"));
        assert!(!named("Bun Pool 3", b"claude"));
        assert!(!named("rgx", b"rgx"), "the whole name");
    }
}
