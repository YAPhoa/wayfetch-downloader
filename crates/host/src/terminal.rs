//! Non-blocking terminal output: text for people, or one JSON object per
//! line for scripts.

use std::cell::Cell;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;

/// Something a host prints.
pub trait Message: Serialize {
    /// The text form, or `None` for output only scripts need.
    fn text(&self) -> Option<String>;
    /// Whether the text form goes to standard error.
    fn is_problem(&self) -> bool;
    /// The message saying `count` lines were dropped because nothing read
    /// them fast enough.
    fn dropped(count: u64) -> Self;
}

/// Output could not be written: standard output is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

#[derive(Debug, Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// The process's standard output and error, written by a dedicated thread
/// through a bounded queue.
///
/// Nothing that prints ever waits for the terminal: a reader that stops
/// reading fills the queue, and further lines are dropped and counted
/// instead of blocking the host, its signal handling, or shutdown. A write
/// blocked in the thread holds up neither the runtime nor process exit.
#[derive(Debug)]
pub struct Terminal {
    queue: SyncSender<(Stream, String)>,
    /// Disconnects when the writer thread ends.
    done: Receiver<()>,
    /// Closes when the writer thread ends, waking async waiters.
    ended: watch::Receiver<()>,
    /// Lines dropped since the last one queued.
    dropped: Cell<u64>,
}

/// Lines queued for the terminal at once.
const QUEUE_LINES: usize = 256;
/// How long [`Terminal::finish`] lets queued lines drain before exiting.
pub const DRAIN: Duration = Duration::from_secs(1);

impl Terminal {
    /// Starts the writer thread.
    pub fn start() -> Self {
        let (queue, lines) = mpsc::sync_channel(QUEUE_LINES);
        let (alive, done) = mpsc::sync_channel(0);
        let (running, ended) = watch::channel(());
        std::thread::Builder::new()
            .name("wayfetch-output".into())
            .spawn(move || {
                // Dropped when the thread ends, however it ends.
                let _alive = (alive, running);
                write_lines(&lines);
            })
            .expect("spawn the output thread");
        Self {
            queue,
            done,
            ended,
            dropped: Cell::new(0),
        }
    }

    /// Completes once the writer thread has ended, which before
    /// [`Terminal::finish`] means standard output failed, for example
    /// because its reader closed it.
    pub async fn closed(&self) {
        let mut ended = self.ended.clone();
        // Nothing is ever sent, so this returns only once the sender is
        // gone.
        let _ = ended.changed().await;
    }

    /// Queues `text` as a line on `stream`. Returns whether it was queued
    /// rather than dropped because the queue is full.
    fn try_write(&self, stream: Stream, text: String) -> Result<bool, Closed> {
        match self.queue.try_send((stream, text)) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(Closed),
        }
    }

    /// Reports an error that ends the program.
    pub fn fatal(&self, message: &str) {
        let _ = self.try_write(Stream::Err, format!("error: {message}"));
    }

    /// Prints a plain line on standard output.
    pub fn line(&self, text: String) {
        let _ = self.try_write(Stream::Out, text);
    }

    /// Lets queued lines drain for at most `wait`, then gives up on a
    /// reader that stopped reading.
    pub fn finish(self, wait: Duration) {
        drop(self.queue);
        let _ = self.done.recv_timeout(wait);
    }
}

/// Writes queued lines until the queue closes or standard output fails.
fn write_lines(lines: &Receiver<(Stream, String)>) {
    for (stream, text) in lines {
        match stream {
            Stream::Out => {
                let mut out = io::stdout().lock();
                if writeln!(out, "{text}").and_then(|()| out.flush()).is_err() {
                    return;
                }
            }
            // Nothing useful is left to do if standard error is gone.
            Stream::Err => {
                let _ = writeln!(io::stderr(), "{text}");
            }
        }
    }
}

/// Prints [`Message`]s to a [`Terminal`].
#[derive(Debug, Clone, Copy)]
pub struct Printer<'a> {
    json: bool,
    terminal: &'a Terminal,
}

impl<'a> Printer<'a> {
    /// Text, or JSON lines if `json`.
    pub fn new(json: bool, terminal: &'a Terminal) -> Self {
        Self { json, terminal }
    }

    /// See [`Terminal::closed`].
    pub async fn closed(&self) {
        self.terminal.closed().await;
    }

    /// Queues `message` without waiting. In text mode, problems go to
    /// standard error. Output that does not fit is dropped and reported
    /// once there is room again. Fails only once standard output is closed.
    pub fn print<M: Message>(&self, message: &M) -> Result<(), Closed> {
        let dropped = &self.terminal.dropped;
        if dropped.get() > 0 {
            if !self.write(&M::dropped(dropped.get()))? {
                dropped.set(dropped.get() + 1);
                return Ok(());
            }
            dropped.set(0);
        }
        if !self.write(message)? {
            dropped.set(dropped.get() + 1);
        }
        Ok(())
    }

    /// Queues `message`; returns whether it fit.
    fn write<M: Message>(&self, message: &M) -> Result<bool, Closed> {
        if self.json {
            let line = serde_json::to_string(message).expect("output serializes");
            return self.terminal.try_write(Stream::Out, line);
        }
        let Some(text) = message.text() else {
            return Ok(true);
        };
        let stream = if message.is_problem() {
            Stream::Err
        } else {
            Stream::Out
        };
        self.terminal.try_write(stream, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Line {
        Listed { count: u64 },
        Dropped { count: u64 },
    }

    impl Message for Line {
        fn text(&self) -> Option<String> {
            None
        }

        fn is_problem(&self) -> bool {
            false
        }

        fn dropped(count: u64) -> Self {
            Self::Dropped { count }
        }
    }

    #[test]
    fn a_full_queue_drops_output_and_reports_it_when_there_is_room() {
        let (queue, lines) = mpsc::sync_channel(2);
        let (_alive, done) = mpsc::sync_channel(0);
        let (_running, ended) = watch::channel(());
        let terminal = Terminal {
            queue,
            done,
            ended,
            dropped: Cell::new(0),
        };
        let printer = Printer::new(true, &terminal);
        let listed = |count| Line::Listed { count };
        for count in 0..5 {
            assert_eq!(printer.print(&listed(count)), Ok(()));
        }
        let read = || -> Vec<String> { lines.try_iter().map(|(_, text)| text).collect() };
        assert_eq!(read().len(), 2);
        printer.print(&listed(9)).unwrap();
        assert_eq!(
            read(),
            [
                r#"{"type":"dropped","count":3}"#,
                r#"{"type":"listed","count":9}"#
            ]
        );
        drop(lines);
        assert_eq!(printer.print(&listed(1)), Err(Closed));
    }
}
