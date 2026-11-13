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

use crate::daemon::control;
use crate::daemon::{self, procs};

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
    let name = crate::state::workspace_id(&mountpoint.display().to_string());
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
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(daemon_args(std::env::args_os().skip(1)))
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
        if daemon::is_mounted(mountpoint) {
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

/// `gitfold version`: this build's version and, for `mountpoint`, its
/// daemon's. Returns the exit code: 0 the same build, 3 another build
/// (remount to upgrade), 4 a daemon too old to tell, 5 no answer (busy).
pub fn version(mountpoint: Option<&Path>) -> Result<i32> {
    println!("gitfold {}", crate::VERSION);
    let Some(mountpoint) = mountpoint else {
        return Ok(0);
    };
    let mountpoint = daemon::canonical(mountpoint)?;
    let shown = mountpoint.display();
    let Some(source) = daemon::gitfold_source(&mountpoint)? else {
        bail!("{shown} is not a gitfold mount");
    };
    let common_dir = daemon::common_dir_of_source(&source);
    let answer = control::request(&common_dir, &control::Request::Version, Some(WAIT));
    let (code, message) = match answer {
        Ok(Some(daemon)) => {
            println!("daemon {daemon}");
            if daemon == crate::VERSION {
                return Ok(0);
            }
            (
                3,
                "runs another gitfold build: unmount and mount again to upgrade".to_string(),
            )
        }
        // No socket answering: the daemon gone (killed, crashed) leaves its
        // mount answering ENOTCONN; else it is starting, exiting or older
        // than the control socket.
        Ok(None)
            if fs::metadata(&mountpoint)
                .is_err_and(|e| e.raw_os_error() == Some(libc::ENOTCONN)) =>
        {
            bail!(
                "the daemon of {shown} is gone (killed?): `gitfold unmount {shown}`, then mount again"
            )
        }
        Ok(None) => (
            5,
            "doesn't answer (starting or exiting, or from before the control socket)".to_string(),
        ),
        // Requests are answered one at a time: a removal may be running.
        Err(e) if timed_out(&e) => {
            let wait = WAIT.as_secs();
            (
                5,
                format!("didn't answer within {wait} s (busy with a removal?)"),
            )
        }
        Err(e) if control::is_older_daemon(&e) => (4, format!("can't tell its version: {e:#}")),
        Err(e) => return Err(e.context(format!("asking the daemon of {shown}"))),
    };
    eprintln!("the daemon of {shown} {message}");
    Ok(code)
}

/// How long `version` and `unmount` wait for the daemon to answer.
const WAIT: Duration = Duration::from_secs(3);

/// `gitfold unmount`: unmounts, detaching the mount if it is busy, and waits
/// for the daemon to exit (after it brought workspace bases up to date), so
/// `remove` and `prune` can follow right away. A busy mount is waited for too
/// (say a `git maintenance run --auto` started by a commit), unless this
/// process or one it runs under (its shell, `make`, ...) is inside it: that
/// would wait forever.
pub fn unmount(mountpoint: &Path) -> Result<()> {
    let mountpoint = daemon::canonical(mountpoint)?;
    let shown = mountpoint.display();
    let Some(source) = daemon::gitfold_source(&mountpoint)? else {
        bail!("{shown} is not a gitfold mount");
    };
    // Not one of the processes using it.
    std::env::set_current_dir("/")?;
    // Prewarm walkers keep the mount busy, which would make this only detach it.
    // The daemon stops them, and starts no more; killing them from here is for
    // daemons without the request (it would start queued ones meanwhile).
    let common_dir = daemon::common_dir_of_source(&source);
    // Briefly: a daemon that is stopped, hung or busy with a removal must not
    // keep it from unmounting.
    let stop = control::Request::StopPrewarm;
    let _ = control::request(&common_dir, &stop, Some(WAIT));
    daemon::prewarm::stop_below(&mountpoint);
    // If it doesn't go ahead, the mount stays: let the daemon walk again.
    let resume = |e: anyhow::Error| {
        let resume = control::Request::ResumePrewarm;
        let _ = control::request(&common_dir, &resume, Some(WAIT));
        e
    };
    let detached = !daemon::unmount(&mountpoint).map_err(resume)?;
    if detached {
        // Read while still mounted, so their paths still resolve.
        let users = procs::users(&mountpoint, &mountpoint);
        daemon::detach(&mountpoint).map_err(resume)?;
        let ours = procs::self_and_ancestors();
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
            println!("  {u}");
        }
        if users.is_empty() {
            println!(
                "  (none visible here: in another mount namespace, a container or a sandbox?)"
            );
        }
        println!("Close them or `cd` out; Ctrl-C is safe: gitfold still exits once they leave.");
    }
    // The daemon holds the lock of its repository until it has exited.
    if !common_dir.is_dir() {
        println!("unmounted {shown} (can't tell when gitfold exits: {source} not found)");
        return Ok(());
    }
    let started = Instant::now();
    // After detaching, the wait is for the processes listed.
    let mut told = if detached { 2 } else { 0 };
    while crate::lock::lock(&common_dir)?.is_none() {
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

/// Whether `e` is a read that timed out.
fn timed_out(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
}

/// The daemon's arguments, from `gitfold mount`'s: `--foreground` right after the
/// subcommand (so a `--` among the arguments can't turn it into a positional
/// one), and without `-C`, already applied: the daemon starts in this process's
/// directory.
fn daemon_args(args: impl Iterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut out = Vec::new();
    let (mut args, mut mount_seen) = (args.peekable(), false);
    while let Some(arg) = args.next() {
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(arg.as_os_str());
        match bytes {
            // Everything after it is positional.
            b"--" => {
                out.push(arg);
                out.extend(args);
                break;
            }
            b"-C" => {
                args.next();
            }
            _ if bytes.starts_with(b"-C") => {}
            b"mount" if !mount_seen => {
                mount_seen = true;
                out.push(arg);
                out.push("--foreground".into());
            }
            _ => out.push(arg),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::daemon_args;

    fn args(s: &str) -> Vec<String> {
        let input = s.split(' ').map(std::ffi::OsString::from);
        daemon_args(input)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn foreground_after_mount_without_c() {
        assert_eq!(args("mount r m"), ["mount", "--foreground", "r", "m"]);
        assert_eq!(args("-C x mount r m"), ["mount", "--foreground", "r", "m"]);
        assert_eq!(args("mount -C x r m"), ["mount", "--foreground", "r", "m"]);
        assert_eq!(
            args("-Cx mount --log l r m"),
            ["mount", "--foreground", "--log", "l", "r", "m"]
        );
        assert_eq!(
            args("mount -- -C mount"),
            ["mount", "--foreground", "--", "-C", "mount"]
        );
    }
}
