//! Line-boundary detection over the captured `cp -v` stream (docs/capture-and-verbose.md).
//!
//! cprog does **not** parse `-v` content (no `'src' -> 'dst'` extraction). It looks at exactly
//! one thing: line boundaries. Each completed line (terminated by `\n`) is one "new item"
//! pulse, which the slow-file timer (docs/architecture.md `slowfile.rs`) uses to decide
//! whether the current file is slow.
//!
//! Because only `\n` matters, arbitrary bytes inside a line — NULs, control chars, even
//! embedded newlines in a file name — never confuse the logic (docs/testing.md B4). A line
//! split across read chunks is held until its terminating `\n` arrives (docs/testing.md B3),
//! and that needs no state: the terminating `\n` lands in exactly one chunk, so counting
//! newlines per chunk already pulses such a line exactly once.

/// How many "new item" pulses this chunk completed — the count of `\n` bytes in it. Content is
/// never inspected beyond that.
///
/// A free function because there is nothing to remember between chunks. This was a `LinePulse`
/// struct carrying a `pending: bool`, and the flag's doc claimed it was what made a boundary
/// straddling two chunks pulse exactly once. It was not: the pulse count came from this newline
/// count alone, the flag had no production reader, and the empty-chunk guard that protected it
/// guarded nothing else — `capture.rs` breaks on a zero-length read, so an empty chunk never
/// arrived here in the first place. Field, guard, accessor and the one test that read them formed
/// a closed loop with no line out of it (#69 D).
///
/// "Is an unterminated line buffered" *is* a real question, but for the footer-withholding rule,
/// and it is asked of the bytes actually written to the terminal — `render.rs`'s
/// `FooterGuard::line_pending`, which is a different flag on the other side of the relay
/// (docs/ui.md invariant 11).
pub fn completed_lines(chunk: &[u8]) -> usize {
    chunk.iter().filter(|&&b| b == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_complete_line_is_one_pulse() {
        assert_eq!(completed_lines(b"'a.iso' -> '/mnt/a.iso'\n"), 1);
    }

    #[test]
    fn multiple_lines_in_one_chunk() {
        assert_eq!(completed_lines(b"line1\nline2\nline3\n"), 3);
        // A read that returned nothing is no pulse either. `capture.rs` never gets here with an
        // empty chunk (a zero-length read ends the relay), so this is the shape of the rule
        // rather than a branch of its own (#69 D).
        assert_eq!(completed_lines(b""), 0);
    }

    #[test]
    fn newline_split_across_chunks_pulses_when_completed() {
        // docs/testing.md B3: a `-v` line straddling a read-chunk boundary. The first assertion is
        // also the whole of "a chunk with no newline is no pulse" — that case was a separate test
        // pinning the same branch (#61 B).
        assert_eq!(completed_lines(b"'a.iso' -> "), 0); // partial line, no pulse yet
        assert_eq!(completed_lines(b"'/mnt/a.iso'\n"), 1); // completes on the next chunk
    }

    #[test]
    fn trailing_partial_after_completed_line_is_held() {
        assert_eq!(completed_lines(b"done\nnext"), 1); // one full line, "next" not yet pulsed
        assert_eq!(completed_lines(b"\n"), 1); // the chunk completing it pulses once
    }

    #[test]
    fn arbitrary_bytes_only_newlines_count() {
        // docs/testing.md B4: NULs / control bytes / an embedded newline in a name.
        // Only `\n` bytes are pulses; content is never parsed.
        assert_eq!(completed_lines(b"weird\x00\x1b[31m\tbytes\nmore"), 1);
        // A raw embedded newline just produces one extra pulse — harmless, not parsed.
        assert_eq!(completed_lines(b"na\nme\n"), 2);
    }
}
