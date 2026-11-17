//! Shared table-rendering helpers: display-width-aware truncation and
//! terminal-width-aware LAUNCH-column clamping used by both the CLI
//! (`print_table`) and the TUI (`format_ls_response`).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Compute per-column widths as the maximum display width across headers and
/// all rows. Use these widths with `clamp_launch_column` (which expects
/// display-width inputs) and for `format!("{:<w$}")` padding (all known
/// table content is ASCII, so display width == char count == byte count).
pub(crate) fn compute_display_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = headers.iter().map(|h| UnicodeWidthStr::width(*h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }
    widths
}

/// Truncate `s` to fit within `target_width` display columns, appending a
/// single Unicode ellipsis `…` (U+2026) when truncated. Returns the original
/// string unchanged when it already fits.
///
/// When `target_width` is smaller than the ellipsis itself, returns an empty
/// string so the caller never receives a result wider than `target_width`.
pub(crate) fn truncate_to_width(s: &str, target_width: usize) -> String {
    let s_width = UnicodeWidthStr::width(s);
    if s_width <= target_width {
        return s.to_string();
    }
    let ellipsis = "…";
    let ellipsis_width = UnicodeWidthStr::width(ellipsis);
    // Can't even fit the ellipsis — return the narrowest possible result.
    if target_width < ellipsis_width {
        return String::new();
    }
    let available = target_width - ellipsis_width;
    let mut result = String::with_capacity(s.len());
    let mut current_width = 0usize;
    for ch in s.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > available {
            break;
        }
        current_width += ch_width;
        result.push(ch);
    }
    result.push_str(ellipsis);
    result
}

/// Clamp the LAUNCH column width and truncate its cells so the table fits
/// within `term_width` display columns.
///
/// * `widths` — column widths (must be display-width based — see
///   `compute_display_widths`). Updated in-place when clamping occurs.
///   After truncation, `widths[launch_col]` is adjusted to the maximum
///   *character count* across all rows so that `format!("{:<w$}")` padding
///   produces correct alignment.
/// * `rows` — table rows; the LAUNCH-column cell in each row may be
///   truncated in-place.
/// * `launch_col` — index of the LAUNCH column in `widths` / `rows`.
/// * `min_launch_width` — floor for the LAUNCH column in display columns
///   (guarantees the definition name is never completely hidden).
/// * `col_gap` — number of display columns between adjacent columns.
/// * `term_width` — available terminal width in display columns. `None`
///   means "not a terminal"; no clamping or truncation is performed.
pub(crate) fn clamp_launch_column(
    widths: &mut [usize],
    rows: &mut [Vec<String>],
    launch_col: usize,
    min_launch_width: usize,
    col_gap: usize,
    term_width: Option<usize>,
) {
    let term_cols = match term_width {
        Some(w) => w,
        None => return,
    };

    // Nothing to do when the table doesn't have a LAUNCH column.
    if launch_col >= widths.len() {
        return;
    }

    let gap_total = widths.len().saturating_sub(1) * col_gap;
    let total: usize = widths.iter().sum::<usize>() + gap_total;

    if total <= term_cols {
        return; // already fits
    }

    let without_launch = total.saturating_sub(widths[launch_col]);
    let available = term_cols.saturating_sub(without_launch);
    let launch_display = available.max(min_launch_width);

    // Truncate cells that exceed the clamped display width, then back-fill
    // `widths[launch_col]` with the maximum char count so `{:<w$}` padding
    // stays consistent.
    let mut max_chars = 0usize;
    for row in rows.iter_mut() {
        if let Some(cell) = row.get_mut(launch_col) {
            if UnicodeWidthStr::width(cell.as_str()) > launch_display {
                *cell = truncate_to_width(cell, launch_display);
            }
            max_chars = max_chars.max(cell.chars().count());
        }
    }
    widths[launch_col] = max_chars.max(min_launch_width);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate_to_width ──

    #[test]
    fn truncate_noop_when_fits() {
        assert_eq!(truncate_to_width("hello", 5), "hello");
        assert_eq!(truncate_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_appends_ellipsis() {
        let result = truncate_to_width("hello world", 8);
        assert!(result.ends_with('…'), "expected ellipsis: {result}");
        assert!(
            UnicodeWidthStr::width(result.as_str()) <= 8,
            "result too wide: {result} (width {})",
            UnicodeWidthStr::width(result.as_str())
        );
    }

    #[test]
    fn truncate_narrow_target_returns_empty() {
        // target_width 0 is smaller than the ellipsis → empty
        assert_eq!(truncate_to_width("hello", 0), "");
    }

    #[test]
    fn truncate_exactly_ellipsis_width() {
        let ellipsis_w = UnicodeWidthStr::width("…");
        // When target equals ellipsis width, the only thing we can return is
        // the ellipsis itself (contract: fit within target_width).
        let result = truncate_to_width("hello", ellipsis_w);
        assert!(
            UnicodeWidthStr::width(result.as_str()) <= ellipsis_w,
            "result wider than target: {result}"
        );
    }

    #[test]
    fn truncate_preserves_wide_characters_that_fit() {
        // "é" is 1 display column; "a" is 1. Together they fit in 2.
        let result = truncate_to_width("éa world", 3);
        let w = UnicodeWidthStr::width(result.as_str());
        assert!(w <= 3, "result width {w} > 3: {result}");
        // "éa" should be preserved (2 columns) + ellipsis (1) = 3.
        assert!(result.starts_with("éa"), "expected éa prefix: {result}");
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_to_wide_target() {
        let result = truncate_to_width("a-really-long-launch-command --flag value", 30);
        let w = UnicodeWidthStr::width(result.as_str());
        assert!(w <= 30, "result width {w} > 30");
        assert!(result.ends_with('…'));
    }

    // ── clamp_launch_column ──

    #[test]
    fn clamp_noop_when_no_terminal() {
        let mut widths = vec![4, 6, 5, 4, 7, 30, 10];
        let mut rows = vec![vec![
            "id1".into(),
            "done".into(),
            "chat".into(),
            "date".into(),
            "/p".into(),
            "a-very-long-launch-command".into(),
            "100".into(),
        ]];
        let original_widths = widths.clone();
        let original_rows = rows.clone();

        clamp_launch_column(&mut widths, &mut rows, 5, 15, 2, None);

        assert_eq!(widths, original_widths);
        assert_eq!(rows, original_rows);
    }

    #[test]
    fn clamp_noop_when_fits_terminal() {
        let mut widths = vec![4, 6, 5, 4, 7, 10, 10];
        let mut rows = vec![vec![
            "id1".into(),
            "done".into(),
            "chat".into(),
            "date".into(),
            "/p".into(),
            "short".into(),
            "100".into(),
        ]];
        let original = widths.clone();

        // 7 columns × widths + 6 gaps × 2 = 46 + 12 = 58, well under 80.
        clamp_launch_column(&mut widths, &mut rows, 5, 15, 2, Some(80));

        assert_eq!(widths, original);
    }

    #[test]
    fn clamp_shrinks_and_truncates_launch() {
        let mut widths = vec![4, 6, 5, 4, 7, 60, 10];
        let mut rows = vec![vec![
            "id1".into(),
            "done".into(),
            "chat".into(),
            "date".into(),
            "/p".into(),
            "a-very-very-very-long-launch-command-that-needs-clipping".into(),
            "1,000".into(),
        ]];

        // Total without gaps: 4+6+5+4+7+60+10 = 96; gaps: 6*2=12; total=108.
        // At 80 cols, LAUNCH gets 80 - (108-60) = 80 - 48 = 32 display cols.
        // After truncation `widths[5]` is the char count (≥ MIN_LAUNCH_WIDTH=15).
        clamp_launch_column(&mut widths, &mut rows, 5, 15, 2, Some(80));

        // width is now max char count of truncated cell, ≥ 15
        assert!(widths[5] >= 15, "widths[5] = {}", widths[5]);
        let launch = &rows[0][5];
        assert!(
            UnicodeWidthStr::width(launch.as_str()) <= 32,
            "truncated launch too wide: {launch} (width {})",
            UnicodeWidthStr::width(launch.as_str())
        );
        assert!(launch.ends_with('…'), "expected ellipsis: {launch}");
    }

    #[test]
    fn clamp_respects_min_launch_width() {
        // Other columns alone consume nearly all of 80 cols.
        let mut widths = vec![4, 6, 5, 4, 7, 60, 10];
        let mut rows = vec![vec![
            "id1".into(),
            "done".into(),
            "chat".into(),
            "date".into(),
            "/p".into(),
            "a-very-very-very-long-launch-command".into(),
            "1,000".into(),
        ]];
        // Total = 96 + 12 gap = 108. At 50 cols, without_launch = 48.
        // Available = 50 - 48 = 2, but min is 15 → LAUNCH display = 15.
        // After truncation, widths[5] = max char count of truncated cells
        // (capped at min 15).
        clamp_launch_column(&mut widths, &mut rows, 5, 15, 2, Some(50));

        // After truncation the char count is at least min_launch_width.
        assert!(widths[5] >= 15, "widths[5] = {}", widths[5]);
        let launch = &rows[0][5];
        assert!(
            UnicodeWidthStr::width(launch.as_str()) <= 15,
            "launch too wide: {launch}"
        );
    }

    #[test]
    fn clamp_skips_when_launch_col_out_of_bounds() {
        let mut widths = vec![4, 6, 5];
        let mut rows = vec![vec!["a".into(), "b".into(), "c".into()]];
        let original_widths = widths.clone();
        let original_rows = rows.clone();

        // LAUNCH_COL=5 but only 3 columns — should be a no-op.
        clamp_launch_column(&mut widths, &mut rows, 5, 15, 2, Some(40));

        assert_eq!(widths, original_widths);
        assert_eq!(rows, original_rows);
    }
}
