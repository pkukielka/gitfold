//! Commands' standard output, ending quietly when its reader does
//! (`gitfold list | head`), as git does.
//!
//! Rust ignores SIGPIPE, so a write to a pipe nobody reads fails, and
//! `println!` panics on it. Prints here (`out!`, `outln!`, `buffered`) treat
//! a reader gone as no error: they note it and print nothing more, and the
//! command carries on, so it still does all it would have, cleanup included,
//! and its result stands. A command that otherwise succeeded then exits with
//! 141, what a shell reports for git killed by SIGPIPE, saying nothing
//! (`main`). Where printing is the point, as before deleting the messages an
//! `inbox` printed, `check` makes a reader gone an error (`StdoutClosed`).
//! Other write errors (a full disk) stay errors.

// The one place stdout is written (`clippy.toml`).
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::io::{self, BufWriter, StdoutLock, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// The exit code for a reader gone: 128 + SIGPIPE, as a shell reports git's.
pub const CLOSED_EXIT: i32 = 141;

/// Whether stdout's reader has gone.
static GONE: AtomicBool = AtomicBool::new(false);

/// Whether stdout's reader has gone: what is printed since was dropped.
pub fn gone() -> bool {
    GONE.load(Ordering::Relaxed)
}

/// `r`, with a broken pipe noted (see the module) and taken as done.
fn note<T>(r: io::Result<T>, done: T) -> io::Result<T> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            GONE.store(true, Ordering::Relaxed);
            Ok(done)
        }
        r => r,
    }
}

/// Prints `args` (`out!`, `outln!`): nothing once the reader is gone.
pub fn write(args: fmt::Arguments) -> io::Result<()> {
    if gone() {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    note(stdout.write_fmt(args), ())?;
    // Text without a newline would wait in the line buffer for the flush at
    // exit, whose error std ignores.
    note(stdout.flush(), ())
}

/// Standard output's reader is gone, where printing was the point (`check`).
#[derive(Debug)]
pub struct StdoutClosed;

impl fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("standard output closed")
    }
}

impl std::error::Error for StdoutClosed {}

/// An error if stdout's reader is gone: for a command that must not go on
/// as if what it printed was read.
pub fn check() -> Result<(), StdoutClosed> {
    match gone() {
        true => Err(StdoutClosed),
        false => Ok(()),
    }
}

/// Whether `e` is (or was caused by) `check`'s.
pub fn closed(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<StdoutClosed>())
}

/// Buffered standard output, for many lines; what is left in it is written
/// when dropped, a reader gone noted then too.
pub fn buffered() -> BufWriter<Stdout> {
    BufWriter::new(Stdout(io::stdout().lock()))
}

/// Standard output, a reader gone noted (see the module).
pub struct Stdout(StdoutLock<'static>);

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if gone() {
            return Ok(buf.len());
        }
        // Through std's line buffer at once: what waits there for the flush at
        // exit (output without a newline) would meet a reader gone unnoted.
        let written = self.0.write_all(buf).and_then(|()| self.0.flush());
        note(written.map(|()| buf.len()), buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if gone() {
            return Ok(());
        }
        note(self.0.flush(), ())
    }
}

/// `print!`, returning a write error other than a reader gone (see the
/// module).
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::out::write(format_args!($($arg)*))
    };
}

/// `println!`, returning a write error other than a reader gone (see the
/// module).
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::out::write(format_args!("{}\n", format_args!($($arg)*)))
    };
}
