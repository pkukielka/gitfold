//! The daemon's filter socket, `.git/gitfold/.filter`: where `gitfold
//! filter-process`, the filter git runs in a workspace with union files
//! (`git::filter`), sends git's clean and smudge requests, for the open
//! workspace to answer from its view as it is now.
//!
//! A connection serves one filter process (one git command): its first line
//! names the protocol (`git::filter::HELLO`), the second the workspace's id;
//! then requests and answers alternate (`git::filter`'s frames). Each
//! connection gets a thread of its own, so git commands in different
//! workspaces don't wait on each other. As for the control socket, the
//! daemon answers only processes of its own user.

use std::io::{BufRead, BufReader, BufWriter, ErrorKind};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};
use log::warn;

use super::control::{peer_uid, with_socket};
use crate::git::filter::{self, Op};

const SOCKET: &str = ".filter";

/// How a request is answered: workspace id, operation, path, content.
type Answer = dyn Fn(&str, Op, &[u8], Vec<u8>) -> Result<Vec<u8>> + Send + Sync;

/// Creates the filter socket of `common_dir` (the caller holds the
/// repository lock: one there is stale).
pub fn bind(common_dir: &Path) -> Result<UnixListener> {
    std::fs::create_dir_all(crate::state::state_root(common_dir))?;
    let listener = with_socket(common_dir, SOCKET, |path| {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e);
        }
        UnixListener::bind(path)
    });
    listener.context("creating the filter socket")
}

/// Answers filter connections on `listener`, each on a thread of its own,
/// with `answer(workspace id, op, path, content)`.
pub fn serve(
    listener: UnixListener,
    answer: impl Fn(&str, Op, &[u8], Vec<u8>) -> Result<Vec<u8>> + Send + Sync + 'static,
) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let answer = Arc::new(answer);
    thread::Builder::new()
        .name("filter".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                };
                if peer_uid(&stream) != Some(uid) {
                    warn!("filter socket: refused a connection from another user");
                    continue;
                }
                let answer = answer.clone();
                let spawned = thread::Builder::new()
                    .name("filter conn".into())
                    .spawn(move || {
                        if let Err(e) = connection(stream, &*answer) {
                            warn!("filter socket: {e:#}");
                        }
                    });
                if let Err(e) = spawned {
                    warn!("filter socket: no thread for a connection: {e}");
                }
            }
        })?;
    Ok(())
}

fn connection(stream: UnixStream, answer: &Answer) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_end() != filter::HELLO {
        bail!("not a gitfold filter process: {line:?}");
    }
    line.clear();
    reader.read_line(&mut line)?;
    let id = line.trim_end().to_string();
    while let Some((op, path, content)) = filter::read_request(&mut reader)? {
        let out = answer(&id, op, &path, content);
        filter::write_answer(&mut writer, &out)?;
    }
    Ok(())
}

/// `gitfold filter-process <common dir> <id>`: the filter process git
/// starts (`filter.<driver>.process`), serving git's protocol on stdin and
/// stdout, each request answered by the daemon of `common_dir` for
/// workspace `id`. Without a daemon answering, every request fails (git
/// then fails the command: the filter is required).
pub fn relay(common_dir: &Path, id: &str) -> Result<()> {
    let stream = with_socket(common_dir, SOCKET, |p| UnixStream::connect(p));
    let mut daemon = match stream {
        Ok(stream) => {
            let mut writer = BufWriter::new(stream.try_clone()?);
            std::io::Write::write_all(
                &mut writer,
                format!("{}\n{id}\n", filter::HELLO).as_bytes(),
            )?;
            Some((BufReader::new(stream), writer))
        }
        Err(e) => {
            eprintln!(
                "gitfold: the gitfold daemon of {} isn't answering ({e}): is it mounted?",
                common_dir.display()
            );
            None
        }
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    filter::serve_git(&mut input, &mut output, |op, path, content| {
        let Some((reader, writer)) = daemon.as_mut() else {
            bail!("the gitfold daemon isn't answering");
        };
        filter::write_request(writer, op, path, &content)?;
        filter::read_answer(reader)?
    })
}
