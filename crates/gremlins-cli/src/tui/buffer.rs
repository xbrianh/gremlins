//! Display buffers for the LiveArea region model.
//!
//! The [`ScrollableBuffer`] is the single source of truth for scrollable
//! transcript content. When its wrapped height exceeds available rows,
//! [`overflow_split`](ScrollableBuffer::overflow_split) reports which prefix
//! lines to commit to terminal scrollback. The caller drains those lines,
//! commits them via [`LiveArea::commit_scrollback`], and renders the
//! remaining tail. No accumulators — the split is a fresh measurement each
//! frame.
//!
//! ## Pinned boundary
//!
//! [`overflow_split`](ScrollableBuffer::overflow_split) takes a pinned
//! boundary: lines at or after the boundary are never reported as overflow.
//! This keeps an in-progress turn from ever reaching terminal scrollback —
//! only completed turns and the separator before a new turn may overflow.

use ratatui::{
    text::Line,
    widgets::{Paragraph, Wrap},
};

/// Provenance of a line pushed into [`ScrollableBuffer`].
///
/// Tagging every push with its origin lets us trace whether content
/// committed to terminal scrollback came from the FIFO or was pushed
/// directly from another path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOrigin {
    Transcript,
    Prompt,
    Reasoning,
    Tool,
    Response,
    Separator,
}

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

/// A buffer of styled lines that may overflow into terminal scrollback.
///
/// The FIFO is the single source of truth. When content exceeds available
/// rows, [`overflow_split`](Self::overflow_split) reports which prefix
/// lines to commit to scrollback. The caller drains those lines, commits
/// them via `LiveArea::commit_scrollback`, and renders the remaining tail.
/// No accumulators — the split is measured fresh each frame.
pub struct ScrollableBuffer {
    lines: Vec<Line<'static>>,
    /// Provenance tags, one per line. Kept in lockstep with [`lines`](Self::lines).
    origins: Vec<LineOrigin>,
}

impl ScrollableBuffer {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            origins: Vec::new(),
        }
    }

    /// Append a styled line with its provenance tag.
    pub fn push(&mut self, line: Line<'static>, origin: LineOrigin) {
        self.lines.push(line);
        self.origins.push(origin);
    }

    /// Insert a styled line at `index`, keeping the origin tag in lockstep.
    pub fn insert(&mut self, index: usize, line: Line<'static>, origin: LineOrigin) {
        self.lines.insert(index, line);
        self.origins.insert(index, origin);
    }

    /// Remove the line at `index`, returning it (origin tag removed too).
    pub fn remove(&mut self, index: usize) -> Line<'static> {
        self.origins.remove(index);
        self.lines.remove(index)
    }

    /// Truncate the buffer to `len` lines (and origin tags).
    pub fn truncate(&mut self, len: usize) {
        self.lines.truncate(len);
        self.origins.truncate(len);
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

    /// Find the prefix of lines that do not fit within `available_rows`.
    ///
    /// `pinned` is a line index: lines at or after `pinned` are never
    /// reported as overflow, so an in-progress turn is never committed to
    /// terminal scrollback.
    ///
    /// Returns `(split_idx, overflow_rows)`:
    /// - `split_idx`: call [`drain_head`](Self::drain_head) with this count
    ///   to remove the overflowed lines.
    /// - `overflow_rows`: wrapped height of the head at `width` — the
    ///   number of terminal rows to commit to scrollback.
    ///
    /// Does not mutate the buffer.
    pub fn overflow_split(&self, available_rows: u16, width: u16, pinned: usize) -> (usize, u16) {
        let available = available_rows as usize;
        let total = self.wrapped_height(width);
        if total <= available {
            return (0, 0);
        }
        // Never overflow past the pinned boundary.
        let limit = pinned.min(self.lines.len());
        let mut dropped = 0usize;
        let mut drop_count = 0usize;
        while total - dropped > available && drop_count < limit {
            dropped += line_wrapped_height(&self.lines[drop_count], width);
            drop_count += 1;
        }

        #[cfg(feature = "telemetry")]
        {
            let dropped_origins: Vec<LineOrigin> =
                self.origins.iter().take(drop_count).copied().collect();
            let previews: Vec<String> = self
                .lines
                .iter()
                .take(drop_count)
                .map(|l| {
                    let s = l.to_string();
                    let truncated: String = s.chars().take(40).collect();
                    if s.len() > 40 {
                        format!("{truncated}...")
                    } else {
                        truncated
                    }
                })
                .collect();
            log::debug!(
                "OVERFLOW_SPLIT available={} width={} lines_total={} wrapped_total={} pinned={} split_idx={} overflow_rows={} origins={:?} previews={:?}",
                available_rows,
                width,
                self.lines.len(),
                total,
                pinned,
                drop_count,
                dropped,
                dropped_origins,
                previews,
            );
        }

        (drop_count, dropped.min(u16::MAX as usize) as u16)
    }

    /// Remove the first `count` lines (and their origin tags).
    pub fn drain_head(&mut self, count: usize) {
        self.lines.drain(..count);
        self.origins.drain(..count);
    }

    /// Drop all lines and origin tags, resetting to an empty buffer.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.origins.clear();
    }
}

impl Default for ScrollableBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    fn plain(s: &str) -> Line<'static> {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(Color::White),
        ))
    }

    #[test]
    fn scrollable_overflow_split_no_overflow() {
        let mut buf = ScrollableBuffer::new();
        buf.push(plain("one"), LineOrigin::Transcript);
        buf.push(plain("two"), LineOrigin::Transcript);
        let (split_idx, overflow_rows) = buf.overflow_split(10, 80, 2);
        assert_eq!(split_idx, 0);
        assert_eq!(overflow_rows, 0);
        assert_eq!(buf.visible_lines().len(), 2);
    }

    #[test]
    fn scrollable_overflow_split_and_drain() {
        let mut buf = ScrollableBuffer::new();
        for i in 0..6 {
            buf.push(plain(&format!("line {i}")), LineOrigin::Transcript);
        }
        // 6 lines at width 80 = 6 rows, available=4 → overflow 2 rows
        let (split_idx, overflow_rows) = buf.overflow_split(4, 80, 6);
        assert_eq!(split_idx, 2);
        assert_eq!(overflow_rows, 2);
        // Buffer unchanged before drain.
        assert_eq!(buf.visible_lines().len(), 6);
        buf.drain_head(split_idx);
        assert_eq!(buf.visible_lines().len(), 4);
        assert_eq!(buf.visible_lines()[0].to_string(), "line 2");
        assert_eq!(buf.visible_lines()[3].to_string(), "line 5");
        // No further overflow.
        let (split_idx2, _) = buf.overflow_split(4, 80, 4);
        assert_eq!(split_idx2, 0);
    }

    #[test]
    fn scrollable_overflow_split_wrapping() {
        let mut buf = ScrollableBuffer::new();
        buf.push(plain("abcdefghijklmnopqrstuvwxyz"), LineOrigin::Transcript); // 26 chars → 3 rows at width 10
        let (split_idx, overflow_rows) = buf.overflow_split(2, 10, 1);
        assert_eq!(overflow_rows, 3);
        assert_eq!(split_idx, 1);
        buf.drain_head(split_idx);
        assert!(buf.is_empty());
    }

    #[test]
    fn overflow_split_respects_pinned_boundary() {
        let mut buf = ScrollableBuffer::new();
        for i in 0..6 {
            buf.push(plain(&format!("line {i}")), LineOrigin::Transcript);
        }
        // Pinned at index 3: only lines 0..3 may overflow, so at most 3
        // lines are reported even though more would fit the budget.
        let (split_idx, overflow_rows) = buf.overflow_split(1, 80, 3);
        assert_eq!(split_idx, 3);
        assert_eq!(overflow_rows, 3);
        assert_eq!(buf.visible_lines().len(), 6);
    }

    #[test]
    fn insert_and_truncate_keep_origins_in_lockstep() {
        let mut buf = ScrollableBuffer::new();
        buf.push(plain("a"), LineOrigin::Transcript);
        buf.push(plain("c"), LineOrigin::Response);
        buf.insert(1, plain("b"), LineOrigin::Prompt);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.visible_lines()[1].to_string(), "b");
        buf.truncate(2);
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.visible_lines()[1].to_string(), "b");
        // Origins vector must be in lockstep (no panic on drain).
        buf.drain_head(2);
        assert!(buf.is_empty());
    }
}
