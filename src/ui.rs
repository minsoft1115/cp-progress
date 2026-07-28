//! Footer layout, bar rendering, and width-based field shedding (docs/ui.md).
//!
//! The footer is two rows. Row one names the destination being copied — but only when the
//! user did not pass `-v`; with `-v` the log line just above already says it, and the row
//! stays blank as a separator (#20). Row two is the bar, composed left-to-right as
//! `bar  percent  size  (rate)  eta`, e.g.
//! `█████░░░░░  62.34 %  0.9/1.4 GiB  (1.8 GiB/s)  ⏳ 00:00`. Percent
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
/// The footer always occupies two rows: the file name (or a blank separator) and the bar.
const FOOTER_ROWS: u16 = 2;
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
/// `state` is what the sampler published, and it is the single source of truth: the sampler
/// alone decides whether the current file is slow, publishing `Some` when there is something to
/// draw and `None` otherwise. Re-deriving that here would mean two places agreeing on the same
/// question — and would cost the render path a clock read and a mutex on every wake-up, which is
/// where the overhead on many-small-file copies came from (docs/architecture.md "동시성").
pub fn footer_for(
    state: Option<&ProgressState>,
    size: TerminalSize,
    style: Style,
    show_name: bool,
) -> Option<Footer> {
    let state = state?;
    let bar = render_footer(size, state, style)?;
    // Row one names the file, or is blank when `-v` already did it just above. Blank rather than
    // absent so the footer keeps one height, one erase count and one code path — and the empty
    // row is what makes the two-region model visible (docs/ui.md "왜 항상 2줄인가").
    let name = if show_name {
        name_row(&state.name, size.cols as usize, style)
    } else {
        String::new()
    };
    Some(Footer { name, bar })
}

/// The footer's two rows, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    /// Row one: the destination path, or empty when `-v` is showing it in the log instead.
    pub name: String,
    /// Row two: the progress bar and its figures.
    pub bar: String,
}

impl Footer {
    /// The rows top to bottom, as [`crate::render::FooterGuard::draw`] wants them.
    pub fn rows(&self) -> [&str; 2] {
        [&self.name, &self.bar]
    }
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

/// The footer's first row: the destination path, fitted to `cols` display columns.
///
/// Without `-v` this row is the only thing on screen naming the file, so the **tail** is what
/// matters — the front is dropped and marked with an ellipsis rather than the other way round
/// (docs/ui.md "footer 1행 — 파일 이름").
///
/// Two properties are load-bearing rather than cosmetic, because the footer is two rows and its
/// erase path moves the cursor up one line: a row wider than the terminal gets folded, which
/// desynchronises that arithmetic (docs/ui.md invariant 7).
///
/// * Control characters are stripped first. A path read back from `/proc/<pid>/fd` may legally
///   contain a newline, and emitting one would add a row nobody is counting.
/// * Fitting is by display width and stops on character boundaries, so a two-column CJK glyph is
///   never half-emitted and never spills over.
fn name_row(path: &str, cols: usize, style: Style) -> String {
    let clean: String = path.chars().filter(|c| !c.is_control()).collect();
    if cols == 0 || clean.is_empty() {
        return String::new();
    }
    if clean.width() <= cols {
        return clean;
    }
    let ellipsis = if style.unicode { "…" } else { "..." };
    let Some(room) = cols.checked_sub(ellipsis.width()) else {
        return String::new(); // not even the marker fits
    };
    // Walk back from the end, taking whole characters while they still fit.
    let mut taken = 0usize;
    let mut start = clean.len();
    for (idx, ch) in clean.char_indices().rev() {
        let w = ch.to_string().width();
        if taken + w > room {
            break;
        }
        taken += w;
        start = idx;
    }
    format!("{ellipsis}{}", &clean[start..])
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
///
/// "Does not fit" is exclusive: a layout that uses the last available column is kept, because
/// filling the width is not exceeding it (exceptions F4).
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
///
/// The index stops at the last entry of [`SIZE_UNITS`], so anything past a TiB keeps counting in
/// TiB rather than walking off the table — which would be an out-of-bounds panic, and a panic
/// takes cp's exit code with it (exceptions F18).
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

/// Format a throughput in bytes/sec with a binary unit, or `-- MiB/s` when unknown. Saturates at
/// the last unit for the same reason [`scale_unit`] does (exceptions F18).
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

/// Format a remaining time as `MM:SS` (or `H:MM:SS` from one hour on — 3600 s reads `1:00:00`,
/// not `60:00`), or `--:--` when unknown.
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
mod name_row_tests {
    use super::*;

    fn plain(path: &str, cols: usize) -> String {
        name_row(path, cols, Style::plain())
    }

    #[test]
    fn a_path_that_fits_is_untouched() {
        assert_eq!(plain("/mnt/a.iso", 40), "/mnt/a.iso");
        assert_eq!(plain("/mnt/a.iso", 10), "/mnt/a.iso", "exactly the width still fits");
    }

    #[test]
    fn a_long_path_loses_its_front_not_its_tail() {
        // The tail is the part worth keeping — that is where the file name is.
        let got = plain("/mnt/data/backup/archives/dataset.tar.zst", 26);
        assert_eq!(got, "…/archives/dataset.tar.zst");
        assert_eq!(got.width(), 26, "fills the width exactly, never exceeds it");
        assert!(got.ends_with("dataset.tar.zst"), "the file name survives: {got:?}");
    }

    #[test]
    fn truncation_is_by_display_width_not_bytes() {
        // Hangul and CJK occupy two columns each; cutting by bytes would overflow the row and
        // the terminal would fold it, desynchronising the two-row erase (ui.md invariant 7).
        let path = "/사진/여행/제주도/한라산.jpg";
        for cols in 1..=path.width() + 4 {
            let got = name_row(path, cols, Style::plain());
            assert!(got.width() <= cols, "cols {cols}: {got:?} is {} wide", got.width());
        }
    }

    #[test]
    fn a_wide_character_is_never_split_in_half() {
        // Dropping half a character would emit an invalid sequence, not a narrower row.
        let got = name_row("/사진/한라산.jpg", 10, Style::plain());
        assert!(got.width() <= 10);
        assert!(got.chars().all(|c| c != '\u{fffd}'), "no replacement chars: {got:?}");
    }

    #[test]
    fn control_characters_are_removed_before_fitting() {
        // A path from /proc/<pid>/fd can legally contain a newline. Emitting one would push the
        // footer past its row count and corrupt the screen (ui.md invariant 7, exceptions C5).
        let got = name_row("/mnt/we\nird\ta\u{1b}[31m.iso", 40, Style::plain());
        assert!(!got.contains('\n') && !got.contains('\r'), "no newlines: {got:?}");
        assert!(!got.chars().any(|c| c.is_control()), "no control characters: {got:?}");
        assert!(got.ends_with(".iso"), "the readable part survives: {got:?}");
    }

    #[test]
    fn the_ellipsis_follows_the_glyph_style() {
        let unicode = name_row("/very/long/path/to/a.iso", 12, Style::plain());
        assert!(unicode.starts_with('…'), "one column when UTF-8: {unicode:?}");

        let ascii = Style { color: false, unicode: false };
        let fallback = name_row("/very/long/path/to/a.iso", 12, ascii);
        assert!(fallback.starts_with("..."), "three columns otherwise: {fallback:?}");
        assert!(fallback.width() <= 12);
    }

    #[test]
    fn a_width_too_small_for_anything_yields_nothing() {
        assert_eq!(plain("/mnt/a.iso", 0), "");
        // Not even the ellipsis fits in a single column? It does — it is one column wide.
        assert_eq!(plain("/mnt/a.iso", 1), "…");
    }

    #[test]
    fn an_empty_path_stays_empty() {
        assert_eq!(plain("", 40), "");
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
    fn size_saturates_at_the_largest_unit() {
        // exceptions F18. Past the end of SIZE_UNITS the number keeps growing in TiB rather than
        // the unit index walking off the table — which would be an out-of-bounds panic, and a
        // panic is exit 101 over cp's own status. A petabyte is not exotic: `truncate -s 1P`
        // makes one instantly and cprog measures the logical length (exceptions E4).
        assert_eq!(format_size(0, Some(1 << 50)), "0.0/1024.0 TiB", "1 PiB total");
        assert_eq!(format_size(1 << 50, Some(1 << 51)), "1024.0/2048.0 TiB");
        assert_eq!(format_size(1 << 60, None), "1048576.0 TiB", "1 EiB copied");
        assert_eq!(format_size(u64::MAX, None), "16777216.0 TiB", "and the largest u64 there is");
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
    fn rate_saturates_at_the_largest_unit() {
        // exceptions F18, the throughput half: UNITS stops at GiB/s, so a faster figure reads as
        // more GiB/s rather than indexing past the end of the table. tmpfs and page-cache reads
        // reach this; the out-of-bounds panic it prevents would reach cp's exit code.
        assert_eq!(format_rate(Some((1u64 << 40) as f64)), "1024 GiB/s", "1 TiB/s");
        assert_eq!(format_rate(Some((1u64 << 50) as f64)), "1048576 GiB/s", "1 PiB/s");
        assert!(format_rate(Some(f64::MAX)).ends_with(" GiB/s"), "no unit above GiB/s exists");
    }

    #[test]
    fn eta_formats_mmss_hhmmss_and_unknown() {
        assert_eq!(format_eta(Some(Duration::from_secs(5))), "00:05");
        assert_eq!(format_eta(Some(Duration::from_secs(65))), "01:05");
        assert_eq!(format_eta(Some(Duration::from_secs(3665))), "1:01:05");
        assert_eq!(format_eta(None), "--:--");
        // The hour boundary itself: 3600 s is the first value written as H:MM:SS, so it is
        // `1:00:00` and never `60:00` (docs/ui.md).
        assert_eq!(format_eta(Some(Duration::from_secs(3599))), "59:59");
        assert_eq!(format_eta(Some(Duration::from_secs(3600))), "1:00:00");
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
        assert!(render_footer(TerminalSize::new(80, 4), &state(), Style::plain()).is_some());
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
    fn a_layout_that_exactly_fits_is_not_shed() {
        // exceptions F4: the shedding test is `fixed + separators > cols`, exclusive, so a layout
        // that uses the last available column is kept. Filling the width is not overflowing it —
        // invariant 7 asks the row not to exceed the terminal, not to leave slack.
        //
        // This is `compose`'s own boundary rather than one visible through `render_footer`: the
        // bar-less attempt is the last rung of the ladder, and the last-resort line below it
        // prints the same percent field either way (F5). Both spellings agree end to end, which
        // is exactly why nothing was pinning the rule down.
        let pct = Some(62.34);
        let pct_s = format_percent(pct);
        let fields = || Fields { pct: &pct_s, size: None, rate: None, eta: None };
        let width = pct_s.width();

        assert_eq!(
            compose(width, false, pct, fields(), Style::plain()).as_deref(),
            Some(pct_s.as_str()),
            "a row that fills the terminal exactly is kept"
        );
        assert_eq!(
            compose(width - 1, false, pct, fields(), Style::plain()),
            None,
            "one column short is a genuine overflow"
        );
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
    ///
    /// `tests/common/mod.rs` has a near-twin for the integration suite, which cannot reach into
    /// the crate. The difference is deliberate: that one additionally drops control characters,
    /// because it reads raw PTY output full of `\r`, `\n` and cursor movement. Here the input is
    /// a single composed footer row, where colour escapes are the only thing to remove — a
    /// control character reaching this function would be the bug, not noise to filter out.
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
        assert_eq!(footer_for(None, TerminalSize::new(80, 24), Style::plain(), true), None);
    }

    #[test]
    fn footer_for_hidden_when_slow_but_no_state_yet() {
        assert_eq!(footer_for(None, TerminalSize::new(80, 24), Style::plain(), true), None);
    }

    #[test]
    fn footer_for_shown_when_slow_with_state() {
        let line = footer_for(Some(&state()), TerminalSize::new(80, 24), Style::plain(), false)
            .map(|f| f.bar);
        assert!(line.is_some_and(|l| l.contains('%')));
    }

    #[test]
    fn footer_for_respects_size_suppression() {
        assert_eq!(footer_for(Some(&state()), TerminalSize::new(80, 2), Style::plain(), true), None);
    }
}
