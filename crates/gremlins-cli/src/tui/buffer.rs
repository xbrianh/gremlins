//! Display buffers for the LiveArea region model.
//!
//! [`LineBuffer`] is a dumb FIFO of styled lines. It has no head policy: it
//! never drops lines on its own and never reports overflow. What happens to
//! the head (commit to terminal scrollback, discard, etc.) is decided by the
//! widget that owns the buffer.

use ratatui::{
    text::Line,
    widgets::{Paragraph, Wrap},
};

/// Wrapped height (in rows) of a set of lines at the given width.
pub fn wrapped_height(lines: &[Line<'static>], width: u16) -> usize {
    if lines.is_empty() {
        return 0;
    }
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Wrapped height of a single line at the given width.
pub fn line_wrapped_height(line: &Line<'static>, width: u16) -> usize {
    Paragraph::new(vec![line.clone()])
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// A FIFO of styled lines.
pub struct LineBuffer {
    lines: Vec<Line<'static>>,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// Append a styled line.
    pub fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    /// Insert a styled line at `index`.
    pub fn insert(&mut self, index: usize, line: Line<'static>) {
        self.lines.insert(index, line);
    }

    /// Remove the line at `index`, returning it.
    #[allow(dead_code)]
    pub fn remove(&mut self, index: usize) -> Line<'static> {
        self.lines.remove(index)
    }

    /// Remove the first `count` lines.
    pub fn drain_head(&mut self, count: usize) {
        self.lines.drain(..count);
    }

    /// Truncate the buffer to `len` lines.
    pub fn truncate(&mut self, len: usize) {
        self.lines.truncate(len);
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Number of lines currently held.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// The lines that could be visible, oldest first.
    pub fn visible_lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// Wrapped height of the currently-held lines at `width`.
    pub fn wrapped_height(&self, width: u16) -> usize {
        wrapped_height(&self.lines, width)
    }

    /// Drop all lines, resetting to an empty buffer.
    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::DEFAULT_THEME;
    use ratatui::style::Style;
    use ratatui::text::Span;

    fn plain(s: &str) -> Line<'static> {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(DEFAULT_THEME.text),
        ))
    }

    #[test]
    fn line_buffer_push_and_visible_lines() {
        let mut buf = LineBuffer::new();
        buf.push(plain("one"));
        buf.push(plain("two"));
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.visible_lines()[0].to_string(), "one");
        assert_eq!(buf.wrapped_height(80), 2);
    }

    #[test]
    fn line_buffer_drain_head() {
        let mut buf = LineBuffer::new();
        for i in 0..6 {
            buf.push(plain(&format!("line {i}")));
        }
        buf.drain_head(2);
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.visible_lines()[0].to_string(), "line 2");
    }

    #[test]
    fn line_buffer_wrapping_uses_wrapped_rows() {
        let mut buf = LineBuffer::new();
        buf.push(plain("abcdefghijklmnopqrstuvwxyz")); // 26 chars → 3 rows at width 10
        assert_eq!(buf.wrapped_height(10), 3);
        assert_eq!(buf.visible_lines().len(), 1);
    }

    #[test]
    fn insert_and_truncate_keep_lines_in_order() {
        let mut buf = LineBuffer::new();
        buf.push(plain("a"));
        buf.push(plain("c"));
        buf.insert(1, plain("b"));
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.visible_lines()[1].to_string(), "b");
        buf.truncate(2);
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.visible_lines()[1].to_string(), "b");
        buf.drain_head(2);
        assert!(buf.is_empty());
    }

    #[test]
    fn line_buffer_clear() {
        let mut buf = LineBuffer::new();
        buf.push(plain("one"));
        buf.clear();
        assert!(buf.is_empty());
    }
}
