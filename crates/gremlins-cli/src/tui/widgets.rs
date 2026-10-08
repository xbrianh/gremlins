use ratatui::{
    prelude::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// A widget whose height can change dynamically during streaming.
///
/// Implementations drive viewport resizing: when `height()` changes,
/// the event loop calls `set_viewport_height` to grow or shrink the
/// inline terminal viewport.
pub trait DynamicWidget {
    /// Current height in rows (0 when empty).
    fn height(&self) -> u16;
    /// Minimum height when non-empty.
    fn min_height(&self) -> u16;
    /// Maximum height before internal scrolling takes over.
    fn max_height(&self) -> u16;
    /// True when there is no content to render.
    fn is_empty(&self) -> bool;
    /// Render into the given area. The widget may use internal scrolling
    /// when content exceeds the allocated height.
    fn render(&self, frame: &mut Frame, area: Rect);
    /// Freeze the last few lines for scrollback, then clear the buffer.
    ///
    /// Returns up to N most-recent lines with their [`Style`]. After this
    /// call `is_empty()` returns true and `height()` returns 0.
    fn freeze(&mut self) -> Vec<(String, Style)>;
}

/// A streaming text widget that grows up to [`StreamWidget::max_height`]
/// rows and then scrolls internally.
///
/// Lines are pushed incrementally as reasoning chunks and tool results
/// arrive. When the turn ends the last 3 lines are frozen to scrollback
/// and the buffer is cleared.
pub struct StreamWidget {
    lines: Vec<String>,
    /// Partial line buffer — text received without a trailing newline.
    partial: String,
    style: Style,
}

impl StreamWidget {
    pub fn new(style: Style) -> Self {
        Self {
            lines: Vec::new(),
            partial: String::new(),
            style,
        }
    }

    /// Flush any remaining partial text as a final line.
    fn flush_partial(&mut self) {
        if !self.partial.is_empty() {
            self.lines.push(std::mem::take(&mut self.partial));
        }
    }

    /// Push a complete line.
    pub fn push(&mut self, line: &str) {
        self.flush_partial();
        self.lines.push(line.to_string());
    }

    /// Push text that may contain newlines or partial lines.
    pub fn push_str(&mut self, text: &str) {
        self.partial.push_str(text);
        while let Some(newline_pos) = self.partial.find('\n') {
            let line = self.partial[..newline_pos].to_string();
            self.partial = self.partial[newline_pos + 1..].to_string();
            self.lines.push(line);
        }
    }
}

impl DynamicWidget for StreamWidget {
    fn height(&self) -> u16 {
        let count = self.lines.len() + if self.partial.is_empty() { 0 } else { 1 };
        if count == 0 {
            0
        } else {
            (count as u16).max(self.min_height()).min(self.max_height())
        }
    }

    fn min_height(&self) -> u16 {
        1
    }

    fn max_height(&self) -> u16 {
        8
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.partial.is_empty()
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let mut rat_lines: Vec<Line> = Vec::new();
        for line in &self.lines {
            rat_lines.push(Line::from(Span::styled(line.clone(), self.style)));
        }
        if !self.partial.is_empty() {
            rat_lines.push(Line::from(Span::styled(self.partial.clone(), self.style)));
        }
        let line_count = rat_lines.len().max(1);
        let scroll = line_count.saturating_sub(area.height as usize) as u16;
        let paragraph = Paragraph::new(rat_lines).scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    }

    fn freeze(&mut self) -> Vec<(String, Style)> {
        self.flush_partial();
        let take_count = self.lines.len().min(3);
        let start = self.lines.len() - take_count;
        let result: Vec<(String, Style)> = self.lines[start..]
            .iter()
            .map(|l| (l.clone(), self.style))
            .collect();
        self.lines.clear();
        result
    }
}
