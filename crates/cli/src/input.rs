//! Bounded line input from standard input.
//!
//! A dedicated thread reads standard input, because a blocking read cannot
//! be cancelled and would otherwise hold up the runtime's shutdown while a
//! terminal waits for a line. Lines are split by [`LinesCodec`] with a
//! length limit: an overlong line is discarded up to its newline and
//! reported, never buffered whole. The channel to the session is bounded,
//! so a session busy with a command makes the reader wait instead of
//! queueing input.

use std::io::{self, Read};

use bytes::BytesMut;
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, LinesCodec, LinesCodecError};

/// Longest accepted line in bytes: a command word and a URL of the
/// protocol's maximum length with room to spare.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// Bytes read from standard input at once.
const READ_CHUNK: usize = 8 * 1024;
/// Lines read ahead of the session.
const LINES_AHEAD: usize = 4;

/// One line of input.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A line, without its line ending.
    Text(String),
    /// A line longer than [`MAX_LINE_BYTES`]; it was discarded.
    TooLong,
    /// A line that is not UTF-8; it was discarded.
    NotUtf8,
}

/// Starts reading standard input. The channel closes at end of input or
/// on a read error.
pub fn spawn() -> mpsc::Receiver<Line> {
    let (send, receive) = mpsc::channel(LINES_AHEAD);
    std::thread::Builder::new()
        .name("wayfetch-stdin".into())
        .spawn(move || read_lines(io::stdin(), &send))
        .expect("spawn the input thread");
    receive
}

/// Splits `input` into lines and sends them until end of input, a read
/// error, or the receiver goes away.
fn read_lines(mut input: impl Read, send: &mpsc::Sender<Line>) {
    let mut codec = LinesCodec::new_with_max_length(MAX_LINE_BYTES);
    let mut buf = BytesMut::with_capacity(READ_CHUNK);
    let mut chunk = [0; READ_CHUNK];
    loop {
        while let Some(line) = line(codec.decode(&mut buf)) {
            if send.blocking_send(line).is_err() {
                return;
            }
        }
        match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // A last line without a newline.
    while let Some(line) = line(codec.decode_eof(&mut buf)) {
        if send.blocking_send(line).is_err() {
            return;
        }
    }
}

fn line(decoded: Result<Option<String>, LinesCodecError>) -> Option<Line> {
    match decoded {
        Ok(text) => text.map(Line::Text),
        Err(LinesCodecError::MaxLineLengthExceeded) => Some(Line::TooLong),
        // The codec reads no I/O itself; its only I/O error is bad UTF-8.
        Err(LinesCodecError::Io(_)) => Some(Line::NotUtf8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8]) -> Vec<Line> {
        let (send, mut receive) = mpsc::channel(1024);
        read_lines(input, &send);
        drop(send);
        std::iter::from_fn(|| receive.try_recv().ok()).collect()
    }

    fn text(s: &str) -> Line {
        Line::Text(s.into())
    }

    #[test]
    fn splits_lines_and_keeps_an_unterminated_last_one() {
        assert_eq!(
            lines(b"list\r\n\npause 1\nquit"),
            [text("list"), text(""), text("pause 1"), text("quit")]
        );
    }

    #[test]
    fn overlong_and_invalid_lines_are_dropped_and_reading_continues() {
        let mut input = vec![b'a'; MAX_LINE_BYTES * 3];
        input.extend_from_slice(b"\nbad \xff\xfe line\nlist\n");
        input.extend_from_slice(&vec![b'b'; MAX_LINE_BYTES + 1]);
        assert_eq!(
            lines(&input),
            [Line::TooLong, Line::NotUtf8, text("list"), Line::TooLong]
        );
    }

    #[test]
    fn a_line_at_the_limit_is_accepted() {
        let long = "x".repeat(MAX_LINE_BYTES);
        assert_eq!(lines(format!("{long}\n").as_bytes()), [text(&long)]);
    }
}
