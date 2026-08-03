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

/// Absolute addressing (`CUP`) — the sequence a footer pinned outside a scrolling region uses.
///
/// Without it the replay leaves the cursor wherever the previous write ended, so the footer lands
/// on top of the log instead of at the row it named. That is the #60 failure in a new spelling:
/// a screen the terminal never showed, and every assertion made on it worth less than it looks.
#[test]
fn absolute_addressing_puts_text_on_the_row_it_names() {
    // Three log lines, then a footer written to rows 5 and 6 by absolute position.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"one\r\ntwo\r\nthree\r\n");
    bytes.extend_from_slice(b"\x1b[5;1H/dst/big.iso\x1b[K");
    bytes.extend_from_slice(b"\x1b[6;1H####  62.34 %\x1b[K");

    let screen = render_screen(&bytes);
    assert_eq!(screen.first().map(String::as_str), Some("one"), "log untouched: {screen:#?}");
    assert_eq!(screen.get(4).map(String::as_str), Some("/dst/big.iso"), "row 5: {screen:#?}");
    assert_eq!(screen.get(5).map(String::as_str), Some("####  62.34 %"), "row 6: {screen:#?}");
    assert_eq!(screen.get(3).map(String::as_str), Some(""), "row 4 was skipped: {screen:#?}");
}

/// `ESC 7` / `ESC 8` (DECSC/DECRC) — save the log cursor, jump away to repaint the footer, come
/// back. Deliberately not `CSI s` / `CSI u`: that spelling is ANSI.SYS and does not appear in
/// terminfo, which is why apt moved off it (Debian #772521).
#[test]
fn a_saved_cursor_returns_to_where_the_log_left_off() {
    // A partial log line, a footer repaint at an absolute row, then the rest of the log line.
    let bytes = b"'a.iso' -> \x1b7\x1b[4;1Hbar\x1b[K\x1b8'b.iso'\r\n".to_vec();
    let screen = render_screen(&bytes);
    assert_eq!(
        screen.first().map(String::as_str),
        Some("'a.iso' -> 'b.iso'"),
        "the log line continued where it was interrupted: {screen:#?}"
    );
    assert_eq!(screen.get(3).map(String::as_str), Some("bar"), "and the detour drew row 4: {screen:#?}");
}

/// A restore with nothing saved must not move the cursor. `FooterGuard` only ever emits `ESC 8`
/// after its own `ESC 7`, but a replay that treated the pair as optional would hide a regression
/// where the save is dropped and every repaint lands at the top of the screen.
#[test]
fn a_restore_without_a_save_leaves_the_cursor_alone() {
    let bytes = b"one\r\ntwo\x1b8three".to_vec();
    let screen = render_screen(&bytes);
    assert_eq!(screen.get(1).map(String::as_str), Some("twothree"), "{screen:#?}");
}
