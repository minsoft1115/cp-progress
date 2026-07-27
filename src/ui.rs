//! Footer layout, bar rendering, and width-based field shedding (docs/ui.md).
//!
//! The footer is a single line composed left-to-right as `bar  percent  size  (rate)  eta`,
//! e.g. `█████░░░░░  62.34 %  0.9/1.4 GiB  (1.8 GiB/s)  ⏳ 00:00`. The file name is **not**
//! repeated here — cp's `-v` line, printed immediately above, already names the file. Percent
//! is two decimals, right-aligned to a fixed width so later fields never shift; size is the
//! copied/total bytes in an adaptive unit; the rate is parenthesized as a qualifier. When the
//! terminal is too narrow, fields are shed in priority order `eta -> rate -> size -> bar ->
//! percent`; percent survives longest.
//!
//! The bar snaps to a quantized width (a divisor of 100: 10/20/50/100 cells) — the largest
//! that fits. This keeps a clean percent-per-cell, scales resolution to the terminal, stops
//! the bar from spanning the whole screen, and keeps it stable as the trailing rate/eta
//! fields change width (docs/ui.md). Below 10 cells the bar is shed.
//!
//! Colour and glyphs follow [`Style`]: the bar fill is green when colour is enabled, and a
//! non-UTF-8 terminal falls back to an ASCII bar (`[###---]`). Layout is always computed on
//! the plain text, so colour escapes never affect field widths.

use crate::progress::{percent_of, ProgressState};
use crate::term::TerminalSize;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// Log rows always left visible above the footer (docs/ui.md `min_log_rows`, default 2).
const MIN_LOG_ROWS: u16 = 2;
/// The footer occupies a single row.
const FOOTER_ROWS: u16 = 1;
/// Column separator between footer fields.
const SEP: &str = "  ";
/// Quantized bar widths — all divisors of 100, so each cell is a clean percent (10/5/2/1 %).
/// The bar uses the largest that fits; below the smallest (10) it is shed entirely.
const BAR_QUANTA: [usize; 4] = [10, 20, 50, 100];
/// SGR: green foreground / reset.
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// Colour and glyph capabilities for rendering (docs/ui.md "색/글리프 정책").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Emit ANSI colour (bar fill green).
    pub color: bool,
    /// Use Unicode block glyphs; false falls back to ASCII.
    pub unicode: bool,
}

impl Style {
    /// No colour, Unicode glyphs — the plain baseline used for layout tests. Test-only:
    /// production builds a [`Style`] from the environment via `term::detect_style`.
    #[cfg(test)]
    pub fn plain() -> Self {
        Style { color: false, unicode: true }
    }
}

/// The per-tick render decision: the footer to draw right now, or `None` to show nothing.
///
/// A footer appears only while the current file is slow *and* a progress sample exists, and
/// only when the terminal can spare the row (docs/ui.md). This is the pure core the managed
/// render loop calls each tick, composing slow-file timing, the latest sample, and the size.
pub fn footer_for(
    is_slow: bool,
    state: Option<&ProgressState>,
    size: TerminalSize,
    style: Style,
) -> Option<String> {
    if !is_slow {
        return None;
    }
    render_footer(size, state?, style)
}

/// Render the footer for `state` at terminal `size`, or `None` when the terminal is too
/// short to spare a footer row without eating into the log region (docs/testing.md C3).
pub fn render_footer(size: TerminalSize, state: &ProgressState, style: Style) -> Option<String> {
    if size.rows < MIN_LOG_ROWS + FOOTER_ROWS {
        return None;
    }
    let cols = size.cols as usize;

    let pct = percent_of(state.done, state.total);
    let pct_s = format_percent(pct);
    let size_s = format_size(state.done, state.total);
    let rate_s = format!("({})", format_rate(state.rate)); // parenthesized: speed as a qualifier
    let eta_s = if style.unicode {
        format!("⏳ {}", format_eta(state.eta))
    } else {
        format_eta(state.eta)
    };

    // Attempts in decreasing richness; each drops the next field per the shedding order
    // eta -> rate -> size -> bar -> (percent always kept). First one that fits wins.
    // Flags: (bar, size, rate, eta) — percent is always present.
    const ATTEMPTS: [(bool, bool, bool, bool); 5] = [
        (true, true, true, true),      // full: bar + size + rate + eta
        (true, true, true, false),     // drop eta
        (true, true, false, false),    // drop rate
        (true, false, false, false),   // drop size
        (false, false, false, false),  // drop bar -> percent only
    ];
    for (show_bar, show_size, show_rate, show_eta) in ATTEMPTS {
        let fields = Fields {
            pct: &pct_s,
            size: show_size.then_some(size_s.as_str()),
            rate: show_rate.then_some(rate_s.as_str()),
            eta: show_eta.then_some(eta_s.as_str()),
        };
        if let Some(line) = compose(cols, show_bar, pct, fields, style) {
            return Some(line);
        }
    }
    // Last resort on an absurdly narrow terminal: percent alone, even if it overflows.
    Some(pct_s)
}

/// A footer segment: a fixed-width text field or the progress bar.
enum Seg {
    Text(String),
    Bar,
}

/// The non-bar field texts for one compose attempt (percent is always shown).
struct Fields<'a> {
    pct: &'a str,
    size: Option<&'a str>,
    rate: Option<&'a str>,
    eta: Option<&'a str>,
}

/// Compose one footer line for the given present fields, or `None` if it cannot fit in `cols`
/// (the bar needs at least the smallest quantum; fixed fields must not overflow). Display
/// order: `bar · percent · size · rate · eta`.
fn compose(cols: usize, show_bar: bool, pct: Option<f64>, fields: Fields, style: Style) -> Option<String> {
    let mut segs: Vec<Seg> = Vec::new();
    if show_bar {
        segs.push(Seg::Bar);
    }
    segs.push(Seg::Text(fields.pct.to_string()));
    if let Some(s) = fields.size {
        segs.push(Seg::Text(s.to_string()));
    }
    if let Some(r) = fields.rate {
        segs.push(Seg::Text(r.to_string()));
    }
    if let Some(e) = fields.eta {
        segs.push(Seg::Text(e.to_string()));
    }

    let sep_total = SEP.width() * segs.len().saturating_sub(1);
    // Widths are measured on plain text only; the bar's colour escapes are added afterwards.
    let fixed_width: usize = segs
        .iter()
        .map(|s| match s {
            Seg::Text(t) => t.width(),
            Seg::Bar => 0,
        })
        .sum();

    let bar = if show_bar {
        let remaining = cols.checked_sub(fixed_width + sep_total)?;
        let cells = bar_cells(remaining)?; // fewer than the smallest quantum -> shed the bar
        render_bar(pct, cells, style)
    } else {
        if fixed_width + sep_total > cols {
            return None;
        }
        String::new()
    };

    let parts: Vec<String> = segs
        .into_iter()
        .map(|s| match s {
            Seg::Text(t) => t,
            Seg::Bar => bar.clone(),
        })
        .collect();
    Some(parts.join(SEP))
}

/// The bar width for `available` columns: the largest [`BAR_QUANTA`] value that fits, or `None`
/// when even the smallest does not (the bar is then shed).
fn bar_cells(available: usize) -> Option<usize> {
    BAR_QUANTA.iter().rev().copied().find(|&q| q <= available)
}

/// Render a determinate/indeterminate bar `width` display columns wide.
///
/// Unicode uses `█`/`░`; ASCII falls back to `[###---]`. Every cell fills by the same floor
/// rule, so the bar reaches full only at a true 100% ("가짜 100 방지", ui.md). When colour is
/// enabled the fill is wrapped in green. The visible width is always exactly `width` (colour
/// escapes aside).
fn render_bar(pct: Option<f64>, width: usize, style: Style) -> String {
    if width == 0 {
        return String::new();
    }
    let color = |fill: String| {
        if style.color && !fill.is_empty() {
            format!("{GREEN}{fill}{RESET}")
        } else {
            fill
        }
    };

    if style.unicode {
        match pct {
            // Unknown size stays indeterminate rather than showing a fake full bar.
            None => "░".repeat(width),
            Some(p) => {
                let f = fill_cells(p, width);
                format!("{}{}", color("█".repeat(f)), "░".repeat(width - f))
            }
        }
    } else {
        // ASCII: [###---] with brackets consuming two columns.
        if width < 2 {
            return "-".repeat(width);
        }
        let inner = width - 2;
        match pct {
            None => format!("[{}]", "-".repeat(inner)),
            Some(p) => {
                let f = fill_cells(p, inner);
                format!("[{}{}]", color("#".repeat(f)), "-".repeat(inner - f))
            }
        }
    }
}

/// Number of filled cells for `percent` across `cells`, flooring uniformly: every cell fills only
/// once fully earned, so a completely filled bar means `percent` truly reached 100 — never a
/// premature full (mirrors the percent field's floor, "가짜 100 방지").
fn fill_cells(percent: f64, cells: usize) -> usize {
    (((percent / 100.0) * cells as f64).floor() as usize).min(cells)
}

/// Format the current-file percentage to two decimals, right-aligned to a fixed width (that
/// of `100.00 %`) so the fields after it never shift as the number grows. Truncates (floors)
/// so large-file progress is visible without a premature `100.00 %` before done; `--` unknown.
fn format_percent(pct: Option<f64>) -> String {
    let num = match pct {
        Some(p) => {
            let hundredths = (p * 100.0) as u64; // p is clamped to 0..=100; this floors
            format!("{}.{:02}", hundredths / 100, hundredths % 100)
        }
        None => "--".to_string(),
    };
    // Pad the numeric part to the width of "100.00" (6) so the field is a constant 8 columns.
    format!("{num:>6} %")
}

/// Binary size units.
const SIZE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// Divisor and unit index so `bytes` reads in a sensible unit (largest where value >= 1).
fn scale_unit(bytes: u64) -> (f64, usize) {
    let mut u = 0;
    let mut div = 1.0_f64;
    while (bytes as f64) / div >= 1024.0 && u < SIZE_UNITS.len() - 1 {
        div *= 1024.0;
        u += 1;
    }
    (div, u)
}

/// Format copied/total sizes in a unit chosen by the total's magnitude, e.g. `36.2/40.0 GiB`
/// (byte scale shows integers). When the total is unknown, just the copied size (`36.2 GiB`).
fn format_size(done: u64, total: Option<u64>) -> String {
    match total {
        Some(t) => {
            let (div, u) = scale_unit(t);
            if u == 0 {
                format!("{done}/{t} {}", SIZE_UNITS[u])
            } else {
                format!("{:.1}/{:.1} {}", done as f64 / div, t as f64 / div, SIZE_UNITS[u])
            }
        }
        None => {
            let (div, u) = scale_unit(done);
            if u == 0 {
                format!("{done} {}", SIZE_UNITS[u])
            } else {
                format!("{:.1} {}", done as f64 / div, SIZE_UNITS[u])
            }
        }
    }
}

/// Format a throughput in bytes/sec with a binary unit, or `-- MiB/s` when unknown.
fn format_rate(rate: Option<f64>) -> String {
    let Some(r) = rate.filter(|r| *r >= 0.0) else {
        return "-- MiB/s".to_string();
    };
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = r;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}/s", v.round() as u64, UNITS[u])
    } else if (v * 10.0).round() < 100.0 {
        // Shown value stays below 10 -> one decimal (e.g. "1.5", "9.9"). The check is on the
        // rounded display value so 9.95 becomes "10", not "10.0".
        format!("{v:.1} {}/s", UNITS[u])
    } else {
        format!("{} {}/s", v.round() as u64, UNITS[u])
    }
}

/// Format a remaining time as `MM:SS` (or `H:MM:SS` past an hour), or `--:--` when unknown.
fn format_eta(eta: Option<Duration>) -> String {
    let Some(d) = eta else {
        return "--:--".to_string();
    };
    let secs = d.as_secs();
    if secs < 3600 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::ProgressState;
    use crate::term::TerminalSize;
    use std::time::Duration;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn state() -> ProgressState {
        // 3 of 8 GiB -> size "3.0/8.0 GiB" (has "GiB"), rate "(142 MiB/s)" (has "/s"): distinct.
        ProgressState {
            name: "big.iso".into(),
            total: Some(8 * GIB),
            done: 3 * GIB,
            rate: Some(142.0 * 1024.0 * 1024.0),
            eta: Some(Duration::from_secs(5)),
        }
    }

    const ASCII: Style = Style { color: false, unicode: false };
    const COLOR: Style = Style { color: true, unicode: true };

    // ---- field formatters ------------------------------------------------------------

    #[test]
    fn percent_formats_two_decimals_right_aligned() {
        // Right-aligned to the width of "100.00" so the decimal point and " %" stay put.
        assert_eq!(format_percent(Some(62.4)), " 62.40 %");
        assert_eq!(format_percent(Some(100.0)), "100.00 %");
        assert_eq!(format_percent(Some(0.0)), "  0.00 %");
        // Floors, so a nearly-done file never shows a premature 100.00.
        assert_eq!(format_percent(Some(99.999)), " 99.99 %");
        assert_eq!(format_percent(None), "    -- %");
    }

    #[test]
    fn percent_field_is_constant_width() {
        // Fixed 8 columns so the fields after it never shift as the value grows.
        for p in [Some(0.0), Some(5.5), Some(62.34), Some(100.0), None] {
            assert_eq!(format_percent(p).chars().count(), 8, "for {p:?}");
        }
    }

    #[test]
    fn size_pair_adapts_unit_to_total() {
        // Unit chosen by the total's magnitude; both sides in that unit.
        assert_eq!(format_size(3 * GIB, Some(8 * GIB)), "3.0/8.0 GiB");
        assert_eq!(format_size(512 * 1024 * 1024, Some(8 * GIB)), "0.5/8.0 GiB");
        assert_eq!(format_size(2 * 1024 * 1024, Some(8 * 1024 * 1024)), "2.0/8.0 MiB");
        // Byte scale shows integers, not decimals.
        assert_eq!(format_size(128, Some(500)), "128/500 B");
        // Unknown total -> just the copied size in its own unit.
        assert_eq!(format_size(3 * GIB, None), "3.0 GiB");
        assert_eq!(format_size(0, None), "0 B");
    }

    #[test]
    fn rate_scales_units_and_unknown() {
        assert_eq!(format_rate(Some(512.0)), "512 B/s");
        assert_eq!(format_rate(Some(142.0 * 1024.0 * 1024.0)), "142 MiB/s");
        assert_eq!(format_rate(Some(1536.0)), "1.5 KiB/s");
        assert_eq!(format_rate(None), "-- MiB/s");
        // No "10.0": values that display as 10+ use no decimal, consistently across the 10 line.
        assert_eq!(format_rate(Some(9.95 * 1024.0 * 1024.0)), "10 MiB/s");
        assert_eq!(format_rate(Some(9.4 * 1024.0 * 1024.0)), "9.4 MiB/s");
    }

    #[test]
    fn eta_formats_mmss_hhmmss_and_unknown() {
        assert_eq!(format_eta(Some(Duration::from_secs(5))), "00:05");
        assert_eq!(format_eta(Some(Duration::from_secs(65))), "01:05");
        assert_eq!(format_eta(Some(Duration::from_secs(3665))), "1:01:05");
        assert_eq!(format_eta(None), "--:--");
    }

    // ---- quantized bar width ---------------------------------------------------------

    #[test]
    fn bar_snaps_to_largest_divisor_of_100() {
        assert_eq!(bar_cells(9), None, "below 10 -> shed");
        assert_eq!(bar_cells(10), Some(10));
        assert_eq!(bar_cells(19), Some(10));
        assert_eq!(bar_cells(20), Some(20));
        assert_eq!(bar_cells(49), Some(20));
        assert_eq!(bar_cells(50), Some(50));
        assert_eq!(bar_cells(99), Some(50));
        assert_eq!(bar_cells(100), Some(100));
        assert_eq!(bar_cells(500), Some(100), "capped at 100");
    }

    // ---- bar rendering ---------------------------------------------------------------

    #[test]
    fn bar_fill_proportional() {
        assert_eq!(render_bar(Some(50.0), 10, Style::plain()), "█████░░░░░");
        assert_eq!(render_bar(Some(0.0), 20, Style::plain()), "░".repeat(20));
        assert_eq!(render_bar(Some(100.0), 10, Style::plain()), "██████████");
    }

    #[test]
    fn bar_indeterminate_is_not_fake_full() {
        assert_eq!(render_bar(None, 10, Style::plain()), "░".repeat(10));
    }

    #[test]
    fn bar_floors_uniformly_full_only_at_100() {
        // Every cell obeys the same floor rule, so the bar reaches full only at a true 100%:
        // 90% and 99.99% of a 10-cell bar both floor to 9 cells (never the 10th before done).
        assert_eq!(render_bar(Some(90.0), 10, Style::plain()), "█████████░");
        assert_eq!(render_bar(Some(99.99), 10, Style::plain()), "█████████░");
        assert_eq!(render_bar(Some(100.0), 10, Style::plain()), "██████████");
        // Floor tradeoff: sub-one-cell progress reads empty on a coarse bar (4.3% of 10 -> 0).
        assert_eq!(render_bar(Some(4.30), 10, Style::plain()), "░".repeat(10));
    }

    #[test]
    fn bar_ascii_fallback() {
        // inner = width - 2 (brackets); 50% of 18 inner cells -> 9 filled.
        assert_eq!(render_bar(Some(50.0), 20, ASCII), format!("[{}{}]", "#".repeat(9), "-".repeat(9)));
        assert_eq!(render_bar(None, 10, ASCII), format!("[{}]", "-".repeat(8)));
    }

    #[test]
    fn bar_color_wraps_fill_in_green() {
        let b = render_bar(Some(50.0), 10, COLOR);
        assert!(b.starts_with("\x1b[32m"), "green fill: {b:?}");
        assert!(b.contains("█████"));
        assert!(b.contains("\x1b[0m"));
        assert!(b.ends_with("░░░░░"), "empty stays uncoloured");
    }

    // ---- footer suppression (C3) -----------------------------------------------------

    #[test]
    fn footer_suppressed_when_terminal_too_short() {
        assert_eq!(render_footer(TerminalSize::new(80, 2), &state(), Style::plain()), None);
        assert_eq!(render_footer(TerminalSize::new(80, 1), &state(), Style::plain()), None);
        assert!(render_footer(TerminalSize::new(80, 3), &state(), Style::plain()).is_some());
    }

    // ---- width-based shedding order (measured on plain style) -------------------------

    /// Presence of each field: (eta, rate, size, bar, percent). Markers are distinct given
    /// `state()`: eta `⏳`, rate `/s` (in `(142 MiB/s)`), size `GiB`, bar glyph, percent `%`.
    fn fields(line: &str) -> (bool, bool, bool, bool, bool) {
        (
            line.contains('⏳'),
            line.contains("/s"),
            line.contains("GiB"),
            line.contains('█') || line.contains('░'),
            line.contains('%'),
        )
    }

    #[test]
    fn wide_terminal_shows_all_fields() {
        let line = render_footer(TerminalSize::new(80, 24), &state(), Style::plain()).unwrap();
        let (eta, rate, size, bar, pct) = fields(&line);
        assert!(eta && rate && size && bar && pct, "all fields at width 80: {line:?}");
        assert!(!line.contains("big.iso"), "name must not appear: {line:?}");
    }

    #[test]
    fn shedding_respects_survival_priority_across_widths() {
        // Survival priority (drop order reversed): percent > bar > size > rate > eta.
        for cols in 12u16..=80 {
            let line = render_footer(TerminalSize::new(cols, 24), &state(), Style::plain()).unwrap();
            let (eta, rate, size, bar, pct) = fields(&line);
            assert!(pct, "percent always survives (cols {cols}): {line:?}");
            if eta {
                assert!(rate && size && bar && pct, "eta implies all (cols {cols}): {line:?}");
            }
            if rate {
                assert!(size && bar && pct, "rate implies size+bar+pct (cols {cols}): {line:?}");
            }
            if size {
                assert!(bar && pct, "size implies bar+pct (cols {cols}): {line:?}");
            }
            if bar {
                assert!(pct, "bar implies pct (cols {cols}): {line:?}");
            }
        }
    }

    #[test]
    fn very_narrow_keeps_only_percent() {
        let line = render_footer(TerminalSize::new(6, 24), &state(), Style::plain()).unwrap();
        let (eta, rate, size, bar, pct) = fields(&line);
        assert!(pct && !eta && !rate && !size && !bar, "only percent at width 6: {line:?}");
    }

    #[test]
    fn footer_never_exceeds_terminal_width() {
        // Plain style has no colour escapes, so display width == byte layout width.
        for cols in 12u16..=200 {
            let line = render_footer(TerminalSize::new(cols, 24), &state(), Style::plain()).unwrap();
            assert!(line.width() <= cols as usize, "cols {cols}: {line:?}");
        }
    }

    #[test]
    fn bar_width_is_stable_as_rate_text_changes() {
        // The quantized bar must not jiggle when only the trailing rate field's width changes.
        let bar_len = |rate: Option<f64>| {
            let s = ProgressState { rate, ..state() };
            let line = render_footer(TerminalSize::new(80, 24), &s, Style::plain()).unwrap();
            line.chars().filter(|&c| c == '█' || c == '░').count()
        };
        // "-- MiB/s" (8), "142 MiB/s" (9), "1.5 GiB/s" (9) — different rate widths, same bar.
        let a = bar_len(None);
        let b = bar_len(Some(142.0 * 1024.0 * 1024.0));
        let c = bar_len(Some(1.5 * 1024.0 * 1024.0 * 1024.0));
        assert_eq!((a, b), (b, c), "bar cells stayed {a}/{b}/{c} across rate widths");
    }

    /// Strip ANSI CSI (SGR) sequences so the visible width can be measured.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break; // final byte of the CSI sequence
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn colored_footer_visible_width_fits_terminal() {
        // Layout is computed on plain text; colour escapes must not push the *visible* width
        // past the terminal.
        for cols in 12u16..=80 {
            let line = render_footer(TerminalSize::new(cols, 24), &state(), COLOR).unwrap();
            let visible = strip_sgr(&line);
            assert!(visible.width() <= cols as usize, "cols {cols}: {visible:?} from {line:?}");
        }
        let wide = render_footer(TerminalSize::new(80, 24), &state(), COLOR).unwrap();
        assert!(wide.contains("\x1b[32m"), "green bar fill at width 80: {wide:?}");
    }

    // ---- footer_for: the per-tick render decision ------------------------------------

    #[test]
    fn footer_for_hidden_when_not_slow() {
        assert_eq!(footer_for(false, Some(&state()), TerminalSize::new(80, 24), Style::plain()), None);
    }

    #[test]
    fn footer_for_hidden_when_slow_but_no_state_yet() {
        assert_eq!(footer_for(true, None, TerminalSize::new(80, 24), Style::plain()), None);
    }

    #[test]
    fn footer_for_shown_when_slow_with_state() {
        let line = footer_for(true, Some(&state()), TerminalSize::new(80, 24), Style::plain());
        assert!(line.is_some_and(|l| l.contains('%')));
    }

    #[test]
    fn footer_for_respects_size_suppression() {
        assert_eq!(footer_for(true, Some(&state()), TerminalSize::new(80, 2), Style::plain()), None);
    }
}
