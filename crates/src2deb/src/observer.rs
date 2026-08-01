//! A line-assembling [`Observer`] over the cage's raw
//! byte streams.
//!
//! ferroday-cage delivers a command's output as raw chunks that are not lines
//! and not guaranteed UTF-8; assembling lines is the consumer's job. This
//! observer buffers each stream and calls a sink once per completed line, with
//! any trailing partial line flushed on [`finish`](LineObserver::finish).

use ferroday_cage::Observer;

/// Which standard stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// An [`Observer`] that reassembles the cage's byte chunks into lines and hands
/// each to a sink.
pub struct LineObserver<F> {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    sink: F,
}

impl<F: FnMut(Stream, &str)> LineObserver<F> {
    /// Creates an observer that calls `sink` once per completed line.
    pub fn new(sink: F) -> LineObserver<F> {
        LineObserver {
            stdout: Vec::new(),
            stderr: Vec::new(),
            sink,
        }
    }

    /// Flushes any buffered partial lines. Call once after the wait returns, so
    /// output not terminated by a newline is not dropped.
    pub fn finish(&mut self) {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let buf = match stream {
                Stream::Stdout => &mut self.stdout,
                Stream::Stderr => &mut self.stderr,
            };
            if !buf.is_empty() {
                let line = String::from_utf8_lossy(buf);
                (self.sink)(stream, &line);
                buf.clear();
            }
        }
    }

    fn feed(&mut self, stream: Stream, chunk: &[u8]) {
        let buf = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        buf.extend_from_slice(chunk);
        // Emit every complete line the buffer now holds, retaining the tail.
        while let Some(newline) = buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..newline]).into_owned();
            buf.drain(..=newline);
            (self.sink)(stream, &line);
        }
    }
}

impl<F: FnMut(Stream, &str)> Observer for LineObserver<F> {
    fn stdout(&mut self, chunk: &[u8]) {
        self.feed(Stream::Stdout, chunk);
    }

    fn stderr(&mut self, chunk: &[u8]) {
        self.feed(Stream::Stderr, chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroday_cage::Observer;

    #[test]
    fn lines_are_reassembled_across_chunk_boundaries() {
        let mut lines: Vec<(Stream, String)> = Vec::new();
        {
            let mut observer =
                LineObserver::new(|stream, line: &str| lines.push((stream, line.to_string())));
            observer.stdout(b"foo\nba");
            observer.stdout(b"r\nbaz\n");
            observer.finish();
        }
        assert_eq!(
            lines,
            [
                (Stream::Stdout, "foo".to_string()),
                (Stream::Stdout, "bar".to_string()),
                (Stream::Stdout, "baz".to_string()),
            ],
        );
    }

    #[test]
    fn a_trailing_partial_line_is_flushed_on_finish() {
        let mut lines: Vec<(Stream, String)> = Vec::new();
        {
            let mut observer =
                LineObserver::new(|stream, line: &str| lines.push((stream, line.to_string())));
            // No newline has arrived, so nothing is emitted until finish; the
            // single flushed line below is the whole output.
            observer.stdout(b"no newline");
            observer.finish();
        }
        assert_eq!(lines, [(Stream::Stdout, "no newline".to_string())]);
    }

    #[test]
    fn a_multibyte_char_split_across_chunks_is_decoded_whole() {
        // The snowman U+2603 is three UTF-8 bytes; deliver them one per chunk.
        let mut lines: Vec<(Stream, String)> = Vec::new();
        {
            let mut observer =
                LineObserver::new(|stream, line: &str| lines.push((stream, line.to_string())));
            observer.stdout(&[0xE2]);
            observer.stdout(&[0x98]);
            observer.stdout(&[0x83, b'\n']);
            observer.finish();
        }
        assert_eq!(lines, [(Stream::Stdout, "\u{2603}".to_string())]);
    }

    #[test]
    fn invalid_utf8_becomes_the_replacement_character_without_panicking() {
        let mut lines: Vec<(Stream, String)> = Vec::new();
        {
            let mut observer =
                LineObserver::new(|stream, line: &str| lines.push((stream, line.to_string())));
            observer.stdout(&[0xFF, b'\n']);
            observer.finish();
        }
        assert_eq!(lines, [(Stream::Stdout, "\u{FFFD}".to_string())]);
    }

    #[test]
    fn stdout_and_stderr_are_buffered_separately() {
        let mut lines: Vec<(Stream, String)> = Vec::new();
        {
            let mut observer =
                LineObserver::new(|stream, line: &str| lines.push((stream, line.to_string())));
            // Interleaved partial writes on each stream must not bleed together.
            observer.stdout(b"out-");
            observer.stderr(b"err-");
            observer.stdout(b"1\n");
            observer.stderr(b"2\n");
            observer.finish();
        }
        assert_eq!(
            lines,
            [
                (Stream::Stdout, "out-1".to_string()),
                (Stream::Stderr, "err-2".to_string()),
            ],
        );
    }
}
