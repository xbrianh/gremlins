//! Shared table-rendering helpers: display-width-aware truncation and
//! terminal-width-aware LAUNCH-column clamping used by both the CLI
//! (`print_table`) and the TUI (`format_ls_response`).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of the Unicode ellipsis character (`…`, U+2026).
pub(crate) const ELLIPSIS_DISPLAY_WIDTH: usize = 1;

/// Compute per-column widths as the maximum display width across headers and
/// all rows.
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

/// Right-pad `s` with spaces so its display width equals `target`.
/// Returns `s` unchanged when it already meets or exceeds `target`.
pub(crate) fn pad_to_display_width(s: &str, target: usize) -> String {
    let current = UnicodeWidthStr::width(s);
    if current >= target {
        return s.to_string();
    }
    let pad = target - current;
    let mut result = String::with_capacity(s.len() + pad);
    result.push_str(s);
    for _ in 0..pad {
        result.push(' ');
    }
    result
}

/// Extract the definition-name prefix from a launch string — the first
/// whitespace-delimited token. This is the part users recognize and use to
/// distinguish definitions.
#[allow(dead_code)]
pub(crate) fn definition_name_prefix(launch: &str) -> &str {
    launch.split_whitespace().next().unwrap_or("")
}

/// Compute a preferred minimum LAUNCH display width from the longest
/// definition-name prefix across all rows. The returned width includes room
/// for the ellipsis so similarly-prefixed definitions stay distinguishable
/// even when truncated.
///
/// Returns `None` when no rows carry a recognizable prefix.
#[allow(dead_code)]
pub(crate) fn compute_min_launch_width_from_rows(
    rows: &[Vec<String>],
    launch_col: usize,
) -> Option<usize> {
    let max_name_width = rows
        .iter()
        .filter_map(|row| row.get(launch_col))
        .map(|cell| definition_name_prefix(cell))
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    if max_name_width == 0 {
        return None;
    }
    Some(max_name_width + ELLIPSIS_DISPLAY_WIDTH)
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
/// * `widths` — column widths in display columns (from
///   `compute_display_widths`). Updated in-place; after truncation
///   `widths[launch_col]` retains its display-width semantics so callers
///   must use `pad_to_display_width` for alignment.
/// * `rows` — table rows; LAUNCH-column cells may be truncated in-place.
/// * `launch_col` — index of the LAUNCH column in `widths` / `rows`.
/// * `col_gap` — number of display columns between adjacent columns.
/// * `term_width` — available terminal width in display columns. `None`
///   means "not a terminal"; no clamping or truncation is performed.
pub(crate) fn clamp_launch_column(
    widths: &mut [usize],
    rows: &mut [Vec<String>],
    launch_col: usize,
    col_gap: usize,
    term_width: Option<usize>,
) {
    let term_cols = match term_width {
        Some(w) => w,
        None => return,
    };

    if launch_col >= widths.len() {
        return;
    }

    let gap_total = widths.len().saturating_sub(1) * col_gap;
    let total: usize = widths.iter().sum::<usize>() + gap_total;

    if total <= term_cols {
        return;
    }

    let without_launch = total.saturating_sub(widths[launch_col]);
    let available = term_cols.saturating_sub(without_launch);

    // Respect the soft floor when there is room; shrink further when the
    // terminal is too narrow. When even a single-cell ellipsis won't fit,
    // empty the column entirely so the table never wraps.
    if available >= ELLIPSIS_DISPLAY_WIDTH {
        // Soft floor: use full available width (never force min_launch_width
        // to exceed terminal constraints).
        let launch_display = available;

        let mut max_display = 0usize;
        for row in rows.iter_mut() {
            if let Some(cell) = row.get_mut(launch_col) {
                if UnicodeWidthStr::width(cell.as_str()) > launch_display {
                    *cell = truncate_to_width(cell, launch_display);
                }
                max_display = max_display.max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
        widths[launch_col] = max_display;
    } else {
        for row in rows.iter_mut() {
            if let Some(cell) = row.get_mut(launch_col) {
                cell.clear();
            }
        }
        widths[launch_col] = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pad_to_display_width ──

    #[test]
    fn pad_noop_when_already_at_target() {
        assert_eq!(pad_to_display_width("hello", 5), "hello");
        assert_eq!(pad_to_display_width("hello", 3), "hello");
    }

    #[test]
    fn pad_adds_spaces_to_reach_target() {
        let result = pad_to_display_width("hi", 5);
        assert_eq!(UnicodeWidthStr::width(result.as_str()), 5);
        assert!(result.starts_with("hi"), "expected hi prefix: {result}");
        assert!(result.ends_with("   "), "expected 3 spaces: {result:?}");
    }

    #[test]
    fn pad_with_wide_chars() {
        // "界" is 2 display columns wide.
        let result = pad_to_display_width("界", 5);
        assert_eq!(UnicodeWidthStr::width(result.as_str()), 5);
        assert!(result.starts_with('界'));
        // Should have 3 trailing spaces (2 + 3 = 5).
        assert_eq!(result.len(), "界".len() + 3);
    }

    #[test]
    fn pad_wide_cell_in_column() {
        // Seven 界 chars + ellipsis = 15 display cells, 8 chars.
        // Padding to 15 display cells should add 0 spaces (already 15).
        let cell = "界界界界界界界…";
        assert_eq!(UnicodeWidthStr::width(cell), 15);
        let padded = pad_to_display_width(cell, 15);
        assert_eq!(UnicodeWidthStr::width(padded.as_str()), 15);
        assert_eq!(padded, cell);
    }

    // ── definition_name_prefix ──

    #[test]
    fn prefix_extracts_first_token() {
        assert_eq!(definition_name_prefix("gremlins run --verbose"), "gremlins");
        assert_eq!(
            definition_name_prefix("long-definition-name --flag value"),
            "long-definition-name"
        );
    }

    #[test]
    fn prefix_returns_empty_for_blank() {
        assert_eq!(definition_name_prefix(""), "");
        assert_eq!(definition_name_prefix("   "), "");
    }

    #[test]
    fn prefix_single_token() {
        assert_eq!(definition_name_prefix("solo"), "solo");
    }

    // ── compute_min_launch_width_from_rows ──

    #[test]
    fn min_launch_from_definition_prefixes() {
        let rows = vec![
            vec![
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
                "short-name --flag".into(),
            ],
            vec![
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
                "very-long-definition-name --verbose --flag value".into(),
            ],
        ];
        let min = compute_min_launch_width_from_rows(&rows, 5);
        // "very-long-definition-name" = 25 + 1 ellipsis = 26
        assert_eq!(min, Some(26));
    }

    #[test]
    fn min_launch_returns_none_for_empty_launch_col() {
        let rows = vec![vec![
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
            "".into(),
        ]];
        assert_eq!(compute_min_launch_width_from_rows(&rows, 5), None);
    }

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
        assert_eq!(truncate_to_width("hello", 0), "");
    }

    #[test]
    fn truncate_exactly_ellipsis_width() {
        let ellipsis_w = UnicodeWidthStr::width("…");
        let result = truncate_to_width("hello", ellipsis_w);
        assert!(
            UnicodeWidthStr::width(result.as_str()) <= ellipsis_w,
            "result wider than target: {result}"
        );
    }

    #[test]
    fn truncate_preserves_wide_characters_that_fit() {
        let result = truncate_to_width("éa world", 3);
        let w = UnicodeWidthStr::width(result.as_str());
        assert!(w <= 3, "result width {w} > 3: {result}");
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

    #[test]
    fn truncate_wide_cjk_characters() {
        // Seven 界 (2 cells each) = 14 cells + ellipsis (1) = 15 cells
        let s: String = std::iter::repeat_n('界', 10).collect();
        let result = truncate_to_width(&s, 15);
        assert!(UnicodeWidthStr::width(result.as_str()) <= 15);
        assert!(result.ends_with('…'));
        // Should be 7 界 + ellipsis
        let chars: Vec<char> = result.chars().collect();
        assert_eq!(chars.len(), 8, "expected 7 界 + ellipsis: {result}");
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

        clamp_launch_column(&mut widths, &mut rows, 5, 2, None);

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

        clamp_launch_column(&mut widths, &mut rows, 5, 2, Some(80));

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
        clamp_launch_column(&mut widths, &mut rows, 5, 2, Some(80));

        // widths[5] is now a display width (not char count).
        let launch = &rows[0][5];
        let launch_dw = UnicodeWidthStr::width(launch.as_str());
        assert!(
            launch_dw <= 32,
            "launch too wide: {launch} (width {launch_dw})"
        );
        assert_eq!(
            widths[5], launch_dw,
            "widths[5] should be display width of truncated cell"
        );
        assert!(launch.ends_with('…'), "expected ellipsis: {launch}");
    }

    #[test]
    fn clamp_soft_floor_falls_below_min() {
        // Other columns alone consume nearly all of the terminal.
        let mut widths = vec![4, 6, 5, 4, 7, 60, 10];
        let mut rows = vec![vec![
            "id1".into(),
            "done".into(),
            "chat".into(),
            "date".into(),
            "/p".into(),
            "some-launch-command --flag".into(),
            "1,000".into(),
        ]];
        // Total = 96 + 12 gap = 108. At 50 cols, without_launch = 48.
        // Available = 50 - 48 = 2 < min_launch_width=15.
        // Soft floor: use available (2) instead of forcing 15.
        clamp_launch_column(&mut widths, &mut rows, 5, 2, Some(50));

        let launch = &rows[0][5];
        let launch_dw = UnicodeWidthStr::width(launch.as_str());
        // Should be at most 2 (just the ellipsis or empty).
        assert!(
            launch_dw <= 2,
            "launch should be ≤ 2 cells when terminal is tight: {launch}"
        );
        assert_eq!(widths[5], launch_dw);
    }

    #[test]
    fn clamp_empties_column_when_no_room() {
        let mut widths = vec![4, 6, 5, 4, 7, 60, 10];
        let mut rows = vec![vec![
            "id1".into(),
            "done".into(),
            "chat".into(),
            "date".into(),
            "/p".into(),
            "launch".into(),
            "1,000".into(),
        ]];
        // 108 total > 48 cols → without_launch = 48. Available = 0.
        clamp_launch_column(&mut widths, &mut rows, 5, 2, Some(48));

        assert!(
            rows[0][5].is_empty(),
            "launch should be empty: {:?}",
            rows[0][5]
        );
        assert_eq!(widths[5], 0);
    }

    #[test]
    fn clamp_with_wide_chars_in_launch() {
        // Seven 界 chars: 7 × 2 = 14 display cells. LAUNCH col says width 20.
        let mut widths = vec![4, 6, 5, 4, 7, 20, 10];
        let s: String = std::iter::repeat_n('界', 10).collect();
        let mut rows = vec![vec![
            "id1".into(),
            "done".into(),
            "chat".into(),
            "date".into(),
            "/p".into(),
            s,
            "1,000".into(),
        ]];
        // Total: 4+6+5+4+7+20+10=56 + gaps 12 = 68. Fits in 80, no clamp.
        let before = widths.clone();
        clamp_launch_column(&mut widths, &mut rows, 5, 2, Some(80));
        assert_eq!(widths, before); // no change — already fits

        // Now force a clamp to 58 cols. Total=68, without_launch=48, available=10.
        // launch_display=10 (since 10 < min_launch_width=15, soft floor uses 10).
        clamp_launch_column(&mut widths, &mut rows, 5, 2, Some(58));
        let launch = &rows[0][5];
        let launch_dw = UnicodeWidthStr::width(launch.as_str());
        assert!(
            launch_dw <= 10,
            "truncated wide-char launch too wide: {launch} (width {launch_dw})"
        );
        // widths[5] must be a display width (not a char count).
        assert_eq!(widths[5], launch_dw);
        // Padding to widths[5] with pad_to_display_width should be a no-op.
        assert_eq!(
            pad_to_display_width(launch, widths[5]),
            launch.clone(),
            "pad_to_display_width should be no-op when cell fills column width"
        );
    }

    #[test]
    fn clamp_skips_when_launch_col_out_of_bounds() {
        let mut widths = vec![4, 6, 5];
        let mut rows = vec![vec!["a".into(), "b".into(), "c".into()]];
        let original_widths = widths.clone();
        let original_rows = rows.clone();

        clamp_launch_column(&mut widths, &mut rows, 5, 2, Some(40));

        assert_eq!(widths, original_widths);
        assert_eq!(rows, original_rows);
    }
}
