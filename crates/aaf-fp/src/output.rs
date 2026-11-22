//! A child's output read up to the child's own end, not up to the end of the
//! pipe.
//!
//! A pipe ends when the last copy of its write end closes, and on macOS a
//! copy can leak into an unrelated child: `std` makes the pipe first and
//! marks it close-on-exec in a second step, and a `Command` spawned by
//! another thread in between inherits it (measured: a `true` whose stdout
//! was read to its end took 3.0 s, the lifetime of a `sleep 3` spawned at
//! the same moment by another thread). The reader then waits for a process
//! that never writes, and a git call that succeeded failed as "output pipe
//! left open" (`aaf` runs gate commands, git and `ps` on several threads at
//! once). A detached helper of the command holds it the same way.
//!
//! So the stream is one end of a socket pair, read with a short timeout:
//! once the child has been reaped, everything it wrote is already waiting
//! in the socket, and the first read that finds the socket empty is the
//! end. A starved reader loses nothing: what it has not read stays queued.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often a reader with nothing to read looks whether the child ended.
const SLICE: Duration = Duration::from_millis(20);

/// Set by whoever reaps the child, once it has been reaped. Dropping it sets
/// it too, so an early return never leaves a reader waiting for ever.
#[derive(Debug, Default)]
pub struct Exited(Arc<AtomicBool>);

impl Exited {
    pub fn new() -> Exited {
        Exited::default()
    }

    /// The child has been reaped: it will write nothing more.
    pub fn mark(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Drop for Exited {
    fn drop(&mut self) {
        self.mark();
    }
}

/// One output stream of a child, read in this process.
#[derive(Debug)]
pub struct ChildOutput {
    stream: UnixStream,
    exited: Arc<AtomicBool>,
}

impl Read for ChildOutput {
    /// `Ok(0)` at the end of the stream, or once the child has been reaped
    /// and nothing it wrote is left.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // Looked at before the read: a child reaped by then wrote all it
            // ever will before the read began, so an empty read is the end.
            let gone = self.exited.load(Ordering::SeqCst);
            match self.stream.read(buf) {
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if gone {
                        return Ok(0);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

/// A stream for the child's stdout or stderr (pass the [`Stdio`] to the
/// `Command`) and its reading end, which ends with `exited`. Drop the
/// `Command` once spawned: it holds this process's copy of the write end.
pub fn output(exited: &Exited) -> io::Result<(Stdio, ChildOutput)> {
    let (ours, theirs) = UnixStream::pair()?;
    ours.set_read_timeout(Some(SLICE))?;
    let reader = ChildOutput {
        stream: ours,
        exited: Arc::clone(&exited.0),
    };
    Ok((Stdio::from(OwnedFd::from(theirs)), reader))
}

/// [`output`] for two streams written in the order the child wrote them
/// (`2>&1`): both [`Stdio`] values are the same write end.
pub fn merged_output(exited: &Exited) -> io::Result<(Stdio, Stdio, ChildOutput)> {
    let (ours, theirs) = UnixStream::pair()?;
    ours.set_read_timeout(Some(SLICE))?;
    let second = theirs.try_clone()?;
    let reader = ChildOutput {
        stream: ours,
        exited: Arc::clone(&exited.0),
    };
    Ok((
        Stdio::from(OwnedFd::from(theirs)),
        Stdio::from(OwnedFd::from(second)),
        reader,
    ))
}

/// A stream for the child's stdin and the writing end. [`ChildInput::finish`]
/// ends it for the child even when a copy of the writing end leaked into
/// another process: a socket's end is a state of the socket, not of the
/// last descriptor closing.
pub fn input() -> io::Result<(Stdio, ChildInput)> {
    let (ours, theirs) = UnixStream::pair()?;
    Ok((Stdio::from(OwnedFd::from(theirs)), ChildInput(ours)))
}

#[derive(Debug)]
pub struct ChildInput(pub(crate) UnixStream);

impl ChildInput {
    /// Writes `bytes`, then tells the child there is no more.
    pub fn finish(mut self, bytes: &[u8]) -> io::Result<()> {
        let written = self.0.write_all(bytes);
        let ended = self.0.shutdown(Shutdown::Write);
        written.and(ended)
    }
}
