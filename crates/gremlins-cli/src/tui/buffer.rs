//! Display buffers for the LiveArea region model.
//!
//! Three buffer types replace the old single-giant-`Paragraph` approach:
//!
//! | Buffer | Type | Bound | Scrollback? |
//! |---|---|---|---|
//! | Reasoning | [`BoundedBuffer`] | 5 visible rows | No |
//! | Tools | [`BoundedBuffer`] | 5 visible rows | No |
//! | Scrollable | [`ScrollableBuffer`] | Available screen rows | Yes |
//!
//! Content streams into these buffers as chunks arrive from the model.
//! ratatui's [`Paragraph::line_count`] handles all wrapping measurement, so
//! bounds are expressed in *visible rows*, not logical lines.
//!
//! ## Scrollable FIFO model
//!
//! The [`ScrollableBuffer`] is the single source of truth for scrollable
//! content. When its wrapped height exceeds available rows,
//! [`overflow_split`](ScrollableBuffer::overflow_split) reports which prefix
//! lines to commit to terminal scrollback. The caller drains those lines,
//! commits them via [`LiveArea::commit_scrollback`], and renders the
//! remaining tail. No accumulators — the split is a fresh measurement each
//! frame.

use ratatui::{
    text::Line,
    widgets::{Paragraph, Wrap},
};

/// Provenance of a line pushed into [`ScrollableBuffer`].
///
/// Tagging every push with its origin lets us trace whether content
/// committed to terminal scrollback came from the FIFO
/// ([`BoundedBuffer::lines`]) or was pushed directly from another path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOrigin {
    Transcript,
    FlattenedPrompt,
    FlattenedReasoning,
    FlattenedTools,
    FlattenedResponse,
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

/// A buffer that holds at most `max_visible` visible rows of styled lines.
///
/// Oldest lines are silently dropped when the buffer exceeds its bound. It
/// never interacts with terminal scrollback.
///
/// The FIFO invariant: only the tail that fits within `max_visible` visible
/// rows is retained.  The head is permanently dropped — it is never moved to
/// scrollback or a scrollable transcript.  [`lines`](Self::lines) always
/// returns the tail, so flattening a completed turn preserves only what was
/// actually displayed.
pub struct BoundedBuffer {
    lines: Vec<Line<'static>>,
    max_visible: u16,
}

impl BoundedBuffer {
    pub fn new(max_visible: u16) -> Self {
        Self {
            lines: Vec::new(),
            max_visible,
        }
    }

    /// Append a styled line.
    ///
    /// A cheap logical-line bound keeps the buffer from growing without
    /// limit; precise visible-row trimming happens in [`visible_lines`]
    /// and [`lines`].
    pub fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
        // Safety ceiling: never keep more than max_visible * 2 logical
        // lines, so a burst of single-row lines can't balloon
        // unboundedly.  Row-accurate trimming is width-dependent and
        // deferred to [`lines`].
        while self.lines.len() > self.max_visible as usize * 2 {
            self.lines.remove(0);
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Remove all buffered lines.
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// The tail of buffered lines that fits within `max_visible` visible
    /// rows at `width`, oldest first.
    ///
    /// This is the FIFO tail — the same slice that [`visible_lines`] would
    /// return for display.  Lines that do not fit are permanently dropped
    /// from the buffer.  Callers that flatten a completed turn into a
    /// scrollable transcript use this to avoid re-displaying content that
    /// was already scrolled off the ephemeral region.
    pub fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let tail = self.visible_lines_with_extra(&[], width);
        self.lines = tail.clone();
        tail
    }

    /// The suffix of lines that fits within `max_visible` visible rows at
    /// `width`, oldest first. A single line that wraps past the bound is
    /// dropped entirely.
    pub fn visible_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.visible_lines_with_extra(&[], width)
    }

    /// Like [`visible_lines`](Self::visible_lines), but treats `extra` as
    /// additional trailing lines (e.g. an un-flushed partial) when applying
    /// the visible-row bound. The bound is enforced over the combined set, so
    /// a trailing line that does not fit within the remaining rows is dropped
    /// entirely.
    pub fn visible_lines_with_extra(
        &self,
        extra: &[Line<'static>],
        width: u16,
    ) -> Vec<Line<'static>> {
        let max = self.max_visible as usize;
        let mut result = Vec::new();
        let mut height = 0usize;
        // Newest-first: trailing `extra` lines, then the buffered lines.
        for line in extra.iter().rev().chain(self.lines.iter().rev()) {
            let h = line_wrapped_height(line, width);
            if h > max {
                // Line alone exceeds the bound — drop it entirely.
                continue;
            }
            if height + h > max {
                break;
            }
            height += h;
            result.push(line.clone());
        }
        result.reverse();
        result
    }
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

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
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
    /// Returns `(split_idx, overflow_rows)`:
    /// - `split_idx`: call [`drain_head`](Self::drain_head) with this count
    ///   to remove the overflowed lines.
    /// - `overflow_rows`: wrapped height of the head at `width` — the
    ///   number of terminal rows to commit to scrollback.
    ///
    /// Does not mutate the buffer.
    pub fn overflow_split(&self, available_rows: u16, width: u16) -> (usize, u16) {
        let available = available_rows as usize;
        let total = self.wrapped_height(width);
        if total <= available {
            return (0, 0);
        }
        let mut dropped = 0usize;
        let mut drop_count = 0usize;
        while total - dropped > available && drop_count < self.lines.len() {
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
                "OVERFLOW_SPLIT available={} width={} lines_total={} wrapped_total={} split_idx={} overflow_rows={} origins={:?} previews={:?}",
                available_rows,
                width,
                self.lines.len(),
                total,
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
    fn bounded_buffer_drops_oldest_beyond_bound() {
        let mut buf = BoundedBuffer::new(3);
        for i in 0..5 {
            buf.push(plain(&format!("line {i}")));
        }
        let lines = buf.visible_lines(80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].to_string(), "line 2");
        assert_eq!(lines[2].to_string(), "line 4");
    }

    #[test]
    fn bounded_buffer_lines_drops_head_permanently() {
        let mut buf = BoundedBuffer::new(3);
        for i in 0..6 {
            buf.push(plain(&format!("line {i}")));
        }
        let tail = buf.lines(80);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].to_string(), "line 3");
        assert_eq!(tail[2].to_string(), "line 5");
        let tail2 = buf.lines(80);
        assert_eq!(tail2.len(), 3);
        assert_eq!(tail2[0].to_string(), "line 3");
    }

    #[test]
    fn bounded_buffer_lines_drops_wrapping_head() {
        let mut buf = BoundedBuffer::new(5);
        for i in 0..4 {
            buf.push(plain(&format!("line {i}")));
        }
        buf.push(plain("abcdefghijklmnopqrstuvwxyz"));
        let tail = buf.lines(80);
        assert_eq!(tail.len(), 5);
        let mut buf2 = BoundedBuffer::new(5);
        for i in 0..4 {
            buf2.push(plain(&format!("line {i}")));
        }
        buf2.push(plain("abcdefghijklmnopqrstuvwxyz"));
        let tail = buf2.lines(10);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].to_string(), "line 2");
        assert_eq!(tail[1].to_string(), "line 3");
        assert_eq!(tail[2].to_string(), "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn bounded_buffer_empty() {
        let buf = BoundedBuffer::new(5);
        assert!(buf.is_empty());
        assert!(buf.visible_lines(80).is_empty());
    }

    #[test]
    fn bounded_buffer_drops_line_that_wraps_past_bound() {
        let mut buf = BoundedBuffer::new(5);
        buf.push(plain("abcdefghijklmnopqrstuvwxyz"));
        assert_eq!(buf.visible_lines(10).len(), 1);

        let mut buf = BoundedBuffer::new(5);
        buf.push(plain(&"x".repeat(60)));
        assert!(buf.visible_lines(10).is_empty());
    }

    #[test]
    fn bounded_buffer_extra_is_bounded_too() {
        let mut buf = BoundedBuffer::new(5);
        for i in 0..4 {
            buf.push(plain(&format!("line {i}")));
        }
        let extra = vec![plain(&"x".repeat(60))];
        let lines = buf.visible_lines_with_extra(&extra, 10);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].to_string(), "line 0");
        assert_eq!(lines[3].to_string(), "line 3");
    }

    #[test]
    fn scrollable_overflow_split_no_overflow() {
        let mut buf = ScrollableBuffer::new();
        buf.push(plain("one"), LineOrigin::Transcript);
        buf.push(plain("two"), LineOrigin::Transcript);
        let (split_idx, overflow_rows) = buf.overflow_split(10, 80);
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
        let (split_idx, overflow_rows) = buf.overflow_split(4, 80);
        assert_eq!(split_idx, 2);
        assert_eq!(overflow_rows, 2);
        // Buffer unchanged before drain.
        assert_eq!(buf.visible_lines().len(), 6);
        buf.drain_head(split_idx);
        assert_eq!(buf.visible_lines().len(), 4);
        assert_eq!(buf.visible_lines()[0].to_string(), "line 2");
        assert_eq!(buf.visible_lines()[3].to_string(), "line 5");
        // No further overflow.
        let (split_idx2, _) = buf.overflow_split(4, 80);
        assert_eq!(split_idx2, 0);
    }

    #[test]
    fn scrollable_overflow_split_wrapping() {
        let mut buf = ScrollableBuffer::new();
        buf.push(
            plain("abcdefghijklmnopqrstuvwxyz"),
            LineOrigin::Transcript,
        ); // 26 chars → 3 rows at width 10
        let (split_idx, overflow_rows) = buf.overflow_split(2, 10);
        assert_eq!(overflow_rows, 3);
        assert_eq!(split_idx, 1);
        buf.drain_head(split_idx);
        assert!(buf.is_empty());
    }
}
