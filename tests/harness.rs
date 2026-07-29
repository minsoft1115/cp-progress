#![cfg(feature = "integration")]
//! Tests for the shared integration harness itself (#60).
//!
//! `tests/common/mod.rs` is the lens every PTY test looks through: if it models a screen the
//! terminal never showed, every assertion made on that screen is answered by a fiction. The one
//! helper with real logic in it is [`common::render_screen`], and nothing checked it.

mod common;

use common::render_screen;

/// The exact byte sequence `FooterGuard` emits: draw two rows, then erase them.
///
/// The erase is `\r CSI K CSI A \r CSI K` — the `CSI A` is what walks back up to the footer's
/// *first* row so it can be cleared too. A replay that ignores cursor movement clears the second
/// row twice and leaves the first one standing, so the modelled screen keeps a row the terminal
/// had already wiped.
#[test]
fn a_two_row_footer_erase_clears_both_rows() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"'a.iso' -> 'b.iso'\r\n"); // a relayed log line
    bytes.extend_from_slice(b"\r/dst/big.iso\x1b[K"); // footer row 1: the name
    bytes.extend_from_slice(b"\n\r####  62.34 %\x1b[K"); // footer row 2: the bar
    bytes.extend_from_slice(b"\r\x1b[K\x1b[A\r\x1b[K"); // FooterGuard::erase

    let screen = render_screen(&bytes);
    assert!(
        !screen.iter().any(|l| l.contains("big.iso")),
        "the erased name row must not survive the replay: {screen:#?}"
    );
    assert!(
        !screen.iter().any(|l| l.contains('%')),
        "nor the erased bar row: {screen:#?}"
    );
    assert!(
        screen.iter().any(|l| l.contains("'a.iso' -> 'b.iso'")),
        "the log line above the footer is untouched: {screen:#?}"
    );
}

/// Cursor-up must not swallow the log line the footer was drawn over: after walking up and
/// erasing, a new log line lands on the row the footer occupied, not on the line above it.
#[test]
fn text_written_after_a_cursor_up_lands_on_that_row() {
    let bytes = b"first\r\nsecond\x1b[A\rONE\x1b[K".to_vec();
    let screen = render_screen(&bytes);
    assert_eq!(
        screen.first().map(String::as_str),
        Some("ONE"),
        "CSI A moved back onto row 0, so `ONE` overwrote `first`: {screen:#?}"
    );
    assert!(screen.iter().any(|l| l == "second"), "row 1 is untouched: {screen:#?}");
}
