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
