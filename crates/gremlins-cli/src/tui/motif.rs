//! Art-deco motifs: rules, titles, and the startup spire.
//!
//! Every glyph here is single-width so the wrapping math in `widgets.rs` and
//! `markdown.rs` (which already uses `unicode_width`) stays correct. Rules
//! are built by repeating a short pattern to a target width, so they survive
//! any terminal width without wide-glyph surprises.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::tui::theme::DEFAULT_THEME;

/// Single-width glyphs shared across the TUI.
pub mod glyph {
    /// Radiator-red / gold chevron used as the prompt marker.
    pub const CHEVRON: &str = "»";
    /// Gold center diamond for glint rules.
    pub const DIAMOND: &str = "◆";
    /// Blockquote bar (gold).
    pub const QUOTE_BAR: &str = "║";
    /// List bullet (brass).
    pub const BULLET: &str = "»";
    /// Unchecked task marker.
    pub const TASK_OPEN: &str = "▢";
    /// Checked task marker.
    pub const TASK_DONE: &str = "▣";
    /// Running status glyph.
    pub const RUNNING: &str = "▲";
    /// Done status glyph.
    pub const DONE: &str = "◆";
    /// Failed status glyph.
    pub const FAILED: &str = "✖";
    /// Stopped status glyph.
    pub const STOPPED: &str = "■";
    /// Column separator.
    pub const COLUMN: &str = "│";
}

/// Repeat a single-width pattern to fill `width` cells.
///
/// The pattern is repeated and then truncated to exactly `width` cells, so
/// the result is always `width` display columns wide (never wider).
fn fill(pattern: &str, width: u16) -> String {
    let w = width as usize;
    if w == 0 || pattern.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(w);
    while out.chars().count() < w {
        out.push_str(pattern);
    }
    out.chars().take(w).collect()
}

/// A width-aware deco rule line.
///
/// `pattern` is a repeating single-width motif (e.g. `"─═─═"`). When
/// `center` is `Some`, a single centered glyph replaces the middle cell.
pub fn deco_rule(width: u16, pattern: &str, center: Option<&str>, style: Style) -> Line<'static> {
    let w = width as usize;
    if w == 0 {
        return Line::from("");
    }
    let mut body = fill(pattern, width);
    if let Some(c) = center {
        let cw = c.chars().count();
        if cw <= w {
            let start = (w - cw) / 2;
            // Rebuild char-by-char so the replacement is always on a char
            // boundary (the center glyph may be multi-byte).
            let mut chars: Vec<char> = body.chars().collect();
            for (i, ch) in c.chars().enumerate() {
                chars[start + i] = ch;
            }
            body = chars.into_iter().collect();
        }
    }
    Line::from(Span::styled(body, style))
}

/// The turn separator: a glint rule, nickel with a gold center diamond.
pub fn glint_rule(width: u16) -> Line<'static> {
    let w = width as usize;
    if w == 0 {
        return Line::from("");
    }
    let nickel = Style::default().fg(DEFAULT_THEME.separator);
    let gold = Style::default().fg(DEFAULT_THEME.heading_primary);
    let center = glyph::DIAMOND;
    let cw = center.chars().count();
    if cw >= w {
        // Too narrow for a flanked center: just the gold diamond.
        return Line::from(Span::styled(
            center.chars().take(w).collect::<String>(),
            gold,
        ));
    }
    let left = (w - cw) / 2;
    let right = w - cw - left;
    Line::from(vec![
        Span::styled(fill("──", left as u16), nickel),
        Span::styled(center.to_string(), gold),
        Span::styled(fill("──", right as u16), nickel),
    ])
}

/// The thematic break (`---`): a streamline rule in nickel.
pub fn streamline_rule(width: u16) -> Line<'static> {
    deco_rule(
        width,
        "─═",
        None,
        Style::default().fg(DEFAULT_THEME.separator),
    )
}

/// A brass streamline rule for table headers and other brass accents.
pub fn streamline_rule_brass(width: u16) -> Line<'static> {
    deco_rule(
        width,
        "─═",
        None,
        Style::default().fg(DEFAULT_THEME.code_border),
    )
}

/// A brass groove rule for code-block edges, with corner caps.
///
/// The result is always exactly `width` cells wide: when the caps alone
/// exceed `width`, the caps are truncated (left cap first, then right) so
/// the rule never overflows its area.
pub fn groove_rule(width: u16, caps: (&str, &str)) -> Line<'static> {
    groove_rule_with_fill(width, caps, "▔")
}

/// A brass groove rule with a custom fill glyph (e.g. `▁` for a lower bar).
pub fn groove_rule_with_fill(width: u16, caps: (&str, &str), fill_glyph: &str) -> Line<'static> {
    let w = width as usize;
    if w == 0 {
        return Line::from("");
    }
    let (left, right) = caps;
    let lw = left.chars().count();
    let rw = right.chars().count();
    let inner = if w >= lw + rw {
        fill(fill_glyph, (w - lw - rw) as u16)
    } else {
        String::new()
    };
    let body = format!("{left}{inner}{right}");
    // Truncate to exactly `w` cells on a char boundary (caps are single-width
    // but may be multi-byte, so rebuild char-by-char).
    let body: String = body.chars().take(w).collect();
    Line::from(Span::styled(
        body,
        Style::default().fg(DEFAULT_THEME.code_border),
    ))
}

/// The startup spire banner, gold → chrome → brass.
///
/// Seven lines, all single-width, so it never pushes the first prompt off a
/// 24-row terminal.
pub fn spire_banner() -> Vec<Line<'static>> {
    let gold = Style::default().fg(DEFAULT_THEME.heading_primary);
    let sunburst = Style::default().fg(DEFAULT_THEME.sunburst);
    let chrome = Style::default().fg(DEFAULT_THEME.chrome);
    let brass = Style::default().fg(DEFAULT_THEME.code_border);

    vec![
        Line::from(Span::styled("        ▲", sunburst)),
        Line::from(Span::styled("       ▲▲", gold)),
        Line::from(Span::styled("      ▲▲▲", gold)),
        Line::from(Span::styled("     ▲▲▲▲", gold)),
        Line::from(Span::styled("   ═════◆═════", chrome)),
        Line::from(vec![Span::styled("   G R E M L I N S", chrome)]),
        Line::from(Span::styled("   machine-age pipeline works", brass)),
    ]
}

/// The one-line tagline under the wordmark, in nickel italic.
pub fn tagline() -> Line<'static> {
    Line::from(Span::styled(
        "   type /help for commands — Ctrl+C clears/cancels (repeat within 1s to exit)",
        Style::default()
            .fg(DEFAULT_THEME.subtle)
            .add_modifier(ratatui::style::Modifier::ITALIC),
    ))
}

/// The one-line `/clear` confirmation, in nickel italic.
pub fn marble_line() -> Line<'static> {
    Line::from(Span::styled(
        "fresh marble.",
        Style::default()
            .fg(DEFAULT_THEME.subtle)
            .add_modifier(ratatui::style::Modifier::ITALIC),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_truncates_to_exact_width() {
        assert_eq!(fill("─═", 5), "─═─═─");
        assert_eq!(fill("──", 5), "─────");
        assert_eq!(fill("──", 0), "");
    }

    #[test]
    fn deco_rule_centers_diamond() {
        let line = deco_rule(9, "──", Some("◆"), Style::default());
        let s = line.to_string();
        assert_eq!(s.chars().count(), 9);
        assert_eq!(s.chars().nth(4), Some('◆'));
    }

    #[test]
    fn groove_rule_has_corner_caps() {
        let line = groove_rule(8, ("◤", "◥"));
        let s = line.to_string();
        assert!(s.starts_with('◤'));
        assert!(s.ends_with('◥'));
        assert_eq!(s.chars().count(), 8);
    }

    #[test]
    fn spire_banner_is_seven_lines_single_width() {
        let lines = spire_banner();
        assert_eq!(lines.len(), 7);
        for l in &lines {
            let s = l.to_string();
            assert!(
                s.chars()
                    .all(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) <= 1),
                "non-single-width glyph in banner line: {s:?}"
            );
        }
    }
}
