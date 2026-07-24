//! Line-boundary detection over the captured `cp -v` stream (docs/capture-and-verbose.md).
//!
//! cprog does **not** parse `-v` content (no `'src' -> 'dst'` extraction). It looks at exactly
//! one thing: line boundaries. Each completed line (terminated by `\n`) is one "new item"
//! pulse, which the slow-file timer (docs/architecture.md `slowfile.rs`) uses to decide
//! whether the current file is slow.
//!
//! Because only `\n` matters, arbitrary bytes inside a line — NULs, control chars, even
//! embedded newlines in a file name — never confuse the logic (docs/testing.md B4). A line
//! split across read chunks is held until its terminating `\n` arrives (docs/testing.md B3).

/// Streaming line-boundary counter over the captured `cp -v` byte stream.
///
/// Feed captured chunks as they arrive; each completed line (`\n`) is one "new item" pulse.
/// The parser holds no line content — it only tracks whether an unterminated line is pending
/// so a boundary straddling two chunks pulses exactly once, when the `\n` finally arrives.
#[derive(Debug, Default)]
pub struct LinePulse {
    /// True when bytes have been received since the last `\n` (an incomplete line is buffered).
    pending: bool,
}

impl LinePulse {
    /// Create a parser with no buffered line.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next captured chunk. Returns the number of "new item" pulses it completed
    /// (i.e. the count of `\n` bytes in `chunk`). Content is never inspected beyond that.
    pub fn feed(&mut self, chunk: &[u8]) -> usize {
        if chunk.is_empty() {
            // An empty read must not clear a pending partial line.
            return 0;
        }
        // A line is pending iff this chunk did not end on a boundary; any bytes after the
        // last `\n` (or a chunk with no `\n` at all) leave an unterminated line buffered.
        self.pending = chunk.last() != Some(&b'\n');
        bytecount_newlines(chunk)
    }

    /// Whether an unterminated line is currently buffered (bytes seen since the last `\n`).
    /// Test-only: production relays on `feed`'s return value, not this internal-state probe.
    #[cfg(test)]
    pub fn pending(&self) -> bool {
        self.pending
    }
}

/// Count `\n` bytes in a slice.
fn bytecount_newlines(chunk: &[u8]) -> usize {
    chunk.iter().filter(|&&b| b == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_complete_line_is_one_pulse() {
        let mut p = LinePulse::new();
        assert_eq!(p.feed(b"'a.iso' -> '/mnt/a.iso'\n"), 1);
        assert!(!p.pending());
    }

    #[test]
    fn multiple_lines_in_one_chunk() {
        let mut p = LinePulse::new();
        assert_eq!(p.feed(b"line1\nline2\nline3\n"), 3);
        assert!(!p.pending());
    }

    #[test]
    fn newline_split_across_chunks_pulses_when_completed() {
        // docs/testing.md B3: a `-v` line straddling a read-chunk boundary.
        let mut p = LinePulse::new();
        assert_eq!(p.feed(b"'a.iso' -> "), 0); // partial line, no pulse yet
        assert!(p.pending());
        assert_eq!(p.feed(b"'/mnt/a.iso'\n"), 1); // completes on the next chunk
        assert!(!p.pending());
    }

    #[test]
    fn trailing_partial_after_completed_line_is_held() {
        let mut p = LinePulse::new();
        assert_eq!(p.feed(b"done\nnext"), 1); // one full line, "next" held
        assert!(p.pending());
        assert_eq!(p.feed(b"\n"), 1); // "next\n" completes
        assert!(!p.pending());
    }

    #[test]
    fn arbitrary_bytes_only_newlines_count() {
        // docs/testing.md B4: NULs / control bytes / an embedded newline in a name.
        // Only `\n` bytes are pulses; content is never parsed.
        let mut p = LinePulse::new();
        assert_eq!(p.feed(b"weird\x00\x1b[31m\tbytes\nmore"), 1);
        assert!(p.pending());
        // A raw embedded newline just produces one extra pulse — harmless, not parsed.
        assert_eq!(p.feed(b"na\nme\n"), 2);
        assert!(!p.pending());
    }

    #[test]
    fn no_trailing_newline_is_no_pulse() {
        let mut p = LinePulse::new();
        assert_eq!(p.feed(b"partial without newline"), 0);
        assert!(p.pending());
    }

    #[test]
    fn empty_chunk_is_no_pulse_and_preserves_pending() {
        let mut p = LinePulse::new();
        assert_eq!(p.feed(b""), 0);
        assert!(!p.pending());
        assert_eq!(p.feed(b"held"), 0);
        assert_eq!(p.feed(b""), 0); // empty chunk must not clear pending
        assert!(p.pending());
    }
}
