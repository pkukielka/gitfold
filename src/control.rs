//! The daemon's control socket: how `gitfold remove` asks a running daemon to
//! remove a workspace or snapshot, instead of requiring an unmount.
//!
//! A Unix socket at `.git/gitfold/.control`, so the repository's permissions
//! guard it (an abstract socket name could be taken by any local user), and
//! only the daemon holding the repository lock binds it (replacing one left by
//! a crash). It is reached as `/proc/self/fd/<dir>/.control`: socket paths are
//! limited to 107 bytes. The daemon only answers processes of its own user.
//! One request per connection: the client writes it and shuts its side down;
//! the daemon answers `ok` or `error`, a newline, then the message.

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::worktree;
use anyhow::{Context, Result, bail};
use log::{info, warn};

/// How long the daemon waits for a client to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub enum Request {
    /// Remove workspace `name`, or snapshot `@<rev>`.
    Remove { name: String, force: bool },
}

impl Request {
    fn encode(&self) -> Vec<u8> {
        match self {
            Request::Remove { name, force } => format!("remove\n{}\n{name}\n", *force as u8),
        }
        .into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Request> {
        let text = std::str::from_utf8(bytes).context("request not UTF-8")?;
        match text.split('\n').collect::<Vec<_>>()[..] {
            ["remove", force, name, ""] => Ok(Request::Remove {
                name: name.to_string(),
                force: force == "1",
            }),
            _ => bail!("unknown request"),
        }
    }
}

/// Runs `f` with a short path to the control socket of the repository with git
/// common directory `common_dir` (valid while `f` runs).
fn with_path<T>(
    common_dir: &Path,
    f: impl FnOnce(&Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let dir = File::open(worktree::state_root(common_dir))?;
    f(&PathBuf::from(format!(
        "/proc/self/fd/{}/.control",
        dir.as_raw_fd()
    )))
}

/// Sends `request` to the daemon of `common_dir`: `Ok(None)` if no daemon
/// listens, else its answer, or its refusal as the error.
pub fn request(common_dir: &Path, request: &Request) -> Result<Option<String>> {
    let mut stream = match with_path(common_dir, |path| UnixStream::connect(path)) {
        Ok(stream) => stream,
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
            return Ok(None);
        }
        Err(e) => return Err(e).context("connecting to the gitfold daemon"),
    };
    stream.write_all(&request.encode())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .context("reading the gitfold daemon's answer")?;
    match answer.split_once('\n') {
        Some(("ok", message)) => Ok(Some(message.to_string())),
        Some(("error", message)) => bail!("{message}"),
        _ => bail!("the gitfold daemon gave no answer (see its log)"),
    }
}

/// Creates the control socket of `common_dir`. The caller holds the
/// repository lock: a socket left there is stale.
pub fn bind(common_dir: &Path) -> Result<UnixListener> {
    fs::create_dir_all(worktree::state_root(common_dir))?;
    let listener = with_path(common_dir, |path| {
        if let Err(e) = fs::remove_file(path)
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e);
        }
        UnixListener::bind(path)
    });
    listener.context("creating the control socket")
}

/// Answers requests on `listener` on a thread of its own, one at a time, with
/// `handle`.
pub fn serve(
    listener: UnixListener,
    handle: impl Fn(Request) -> Result<String> + Send + 'static,
) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    // Out of file descriptors, say: don't spin.
                    thread::sleep(Duration::from_millis(100));
                    continue;
                };
                if peer_uid(&stream) != Some(uid) {
                    warn!("control socket: refused a connection from another user");
                    continue;
                }
                let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                let mut bytes = Vec::new();
                let answer = match stream.read_to_end(&mut bytes) {
                    Ok(_) => Request::decode(&bytes).and_then(&handle),
                    Err(e) => Err(e.into()),
                };
                let reply = match answer {
                    Ok(message) => {
                        info!("{message}");
                        format!("ok\n{message}")
                    }
                    Err(e) => format!("error\n{e:#}"),
                };
                // The client may have gone (Ctrl-C); nothing to do then.
                let _ = stream.write_all(reply.as_bytes());
            }
        })?;
    Ok(())
}

/// The user id of the process at the other end of `stream`.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}
