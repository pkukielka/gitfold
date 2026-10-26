//! Running the mount in the background: `gitfold mount` starts itself again with
//! `--foreground`, detached from the terminal (its own session, so closing the
//! terminal doesn't stop it; output to a log file), and returns once the mount
//! is up, or with the daemon's error. `gitfold unmount` stops it.

use std::fs::{self, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::admin;
use crate::mountpoint;
use crate::worktree;

/// How long `mount` waits for the daemon to mount.
const MOUNT_WAIT: Duration = Duration::from_secs(60);

/// The default log file of a mount at `mountpoint`:
/// `$XDG_STATE_HOME/gitfold/<escaped mount point>.log`, by default under
/// `~/.local/state`.
pub fn default_log(mountpoint: &Path) -> Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
            Some(PathBuf::from(home).join(".local/state"))
        })
        .context("neither XDG_STATE_HOME nor HOME is set; pass --log")?;
    let name = worktree::workspace_id(&mountpoint.display().to_string());
    Ok(state.join("gitfold").join(format!("{name}.log")))
}

/// Runs this command again with `--foreground`, detached and logging to `log`,
/// and waits until `mountpoint` is mounted. The daemon resolves its arguments
/// in this process's working directory, then moves to `/`.
pub fn start(mountpoint: &Path, log: &Path) -> Result<()> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let ours_from = out.metadata()?.len() as usize;
    // `--foreground` right after the subcommand, so a `--` among the arguments
    // can't turn it into a positional one.
    let mut args = std::env::args_os().skip(1);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(args.next())
        .arg("--foreground")
        .args(args)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    // A session of its own: no controlling terminal, so no SIGHUP when it closes.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("starting the daemon")?;
    let started = Instant::now();
    loop {
        if mountpoint::is_mounted(mountpoint) {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            // It failed: its error is the last thing it logged.
            let logged = fs::read(log).unwrap_or_default();
            let ours = String::from_utf8_lossy(logged.get(ours_from..).unwrap_or_default());
            let error = match ours.rfind("Error: ") {
                Some(at) => ours[at + "Error: ".len()..].trim(),
                None => ours.trim(),
            };
            if error.is_empty() {
                bail!("the daemon exited ({status}); see {}", log.display());
            }
            bail!("{error}");
        }
        if started.elapsed() > MOUNT_WAIT {
            // SIGTERM, not SIGKILL: a mount that comes up meanwhile is unmounted.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            bail!(
                "the mount didn't come up within {} s; see {}",
                MOUNT_WAIT.as_secs(),
                log.display()
            );
        }
        sleep(Duration::from_millis(20));
    }
}

/// `gitfold unmount`: unmounts, detaching the mount if it is busy, and waits
/// for the daemon to exit (after it brought workspace bases up to date), so
/// `remove` and `prune` can follow right away. A busy mount is waited for too
/// (say a `git maintenance run --auto` started by a commit), unless this
/// process or one it runs under (its shell, `make`, ...) is inside it: that
/// would wait forever.
pub fn unmount(mountpoint: &Path) -> Result<()> {
    let mountpoint = mountpoint::canonical(mountpoint)?;
    let shown = mountpoint.display();
    let Some(source) = mountpoint::gitfold_source(&mountpoint)? else {
        bail!("{shown} is not a gitfold mount");
    };
    // Not one of the processes using it.
    std::env::set_current_dir("/")?;
    // Prewarm walkers keep the mount busy, which would make this only detach it.
    crate::prewarm::stop_below(&mountpoint);
    let detached = !mountpoint::unmount(&mountpoint)?;
    if detached {
        // Read while still mounted, so their paths still resolve.
        let users = mountpoint::users(&mountpoint);
        mountpoint::detach(&mountpoint)?;
        let ours = mountpoint::self_and_ancestors();
        if users.iter().any(|u| ours.contains(&u.pid)) {
            println!(
                "{shown} is in use by this command or a process it runs under: detached it. \
                 gitfold exits, and `gitfold remove` works, once the last process \
                 using it leaves (`cd` out)"
            );
            return Ok(());
        }
        println!("{shown} is in use: detached it; waiting for gitfold to exit once these leave:");
        for u in &users {
            println!("  {} {}", u.pid, u.command);
        }
        if users.is_empty() {
            println!(
                "  (none visible here: in another mount namespace, a container or a sandbox?)"
            );
        }
        println!("Close them or `cd` out; Ctrl-C is safe: gitfold still exits once they leave.");
    }
    // The daemon holds the lock of its repository until it has exited.
    let common_dir = admin::common_dir_of_source(&source);
    if !common_dir.is_dir() {
        println!("unmounted {shown} (can't tell when gitfold exits: {source} not found)");
        return Ok(());
    }
    let started = Instant::now();
    // After detaching, the wait is for the processes listed.
    let mut told = if detached { 2 } else { 0 };
    while worktree::lock(&common_dir)?.is_none() {
        let waited = started.elapsed();
        if told == 0 && waited > Duration::from_secs(2) {
            println!("waiting for gitfold to exit (bringing workspace bases up to date)...");
            told = 1;
        } else if told == 1 && waited > Duration::from_secs(15) {
            println!(
                "still waiting: the repository is locked. The mount may still be in use in \
                 another mount namespace (a container, a sandbox), or a `gitfold remove` or \
                 `prune` holds the lock. It is unmounted here; Ctrl-C is safe."
            );
            told = 2;
        }
        sleep(Duration::from_millis(50));
    }
    println!("unmounted {shown}");
    Ok(())
}
