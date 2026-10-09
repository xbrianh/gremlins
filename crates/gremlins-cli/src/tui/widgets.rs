use ratatui::{
    prelude::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

/// Minimum height of the stream section when response is present.
const STREAM_MIN: u16 = 5;
/// Number of stream lines to keep when freezing.
const FREEZE_STREAM_LINES: usize = 3;

/// Frozen content: `(response_lines, stream_tail)`.
pub type FrozenLines = (Vec<(String, Style)>, Vec<(String, Style)>);

/// A widget whose height can change dynamically during streaming.
///
/// Implementations drive viewport resizing: when `height()` changes,
/// the event loop calls `set_viewport_height` to grow or shrink the
/// inline terminal viewport.
pub trait DynamicWidget {
    /// Current height in rows given the terminal width and available height (0 when empty).
    fn height(&self, width: u16, available: u16) -> u16;
    /// True when there is no content to render.
    fn is_empty(&self) -> bool;
    /// Render into the given area. The widget may use internal scrolling
    /// when content exceeds the allocated height.
    fn render(&self, frame: &mut Frame, area: Rect);
    /// Freeze content for scrollback, then clear both buffers.
    ///
    /// Returns `(response_lines, stream_tail)` where response lines carry
    /// plain [`Style::default()`] and stream tail lines carry dark gray
    /// italic style with 2-space indent.
    fn freeze(&mut self) -> FrozenLines;
}

/// A dual-section streaming widget that routes model output into two
/// independent buffers:
///
/// - **Stream section** (top): reasoning chunks and tool results, styled
///   dark gray italic with 2-space indent.
/// - **Response section** (bottom): model text deltas, plain terminal style.
///
/// The response section gets priority in fluid height allocation. The
/// stream section never goes below [`STREAM_MIN`] when the response
/// section is present.
pub struct SplitWidget {
    /// Reasoning + tool result lines (already styled).
    stream_lines: Vec<Line<'static>>,
    /// Model text lines (plain style applied at render time).
    response_lines: Vec<Line<'static>>,
    /// Partial-line accumulator for `push_response_text`.
    partial: String,
    /// Partial-line accumulator for `push_stream_text`.
    stream_partial: String,
}

/// Wrapped-row helper: computes total wrapped rows for a buffer of [`Line`]s
/// plus an optional partial string, at the given width. Uses the same
/// `chars.div_ceil(width)` formula as [`App::scrollback_height`].
fn wrapped_rows(lines: &[Line], partial: &str, width: u16) -> u16 {
    let wrap_width = (width as usize).max(1);
    let mut total: u16 = 0;
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let chars = text.chars().count();
        let rows = if chars == 0 {
            1
        } else {
            chars.div_ceil(wrap_width)
        };
        total = total.saturating_add(rows as u16);
    }
    if !partial.is_empty() {
        let chars = partial.chars().count();
        let rows = if chars == 0 {
            1
        } else {
            chars.div_ceil(wrap_width)
        };
        total = total.saturating_add(rows as u16);
    }
    total
}

impl SplitWidget {
    pub fn new() -> Self {
        Self {
            stream_lines: Vec::new(),
            response_lines: Vec::new(),
            partial: String::new(),
            stream_partial: String::new(),
        }
    }

    /// Compute (stream_h, response_h) given terminal width and available height.
    ///
    /// Response gets priority. Stream never goes below `STREAM_MIN` when
    /// response is present. Returns (0, 0) when both buffers are empty.
    pub fn layout(&self, width: u16, available: u16) -> (u16, u16) {
        let stream_content = wrapped_rows(&self.stream_lines, &self.stream_partial, width);
        let response_content = wrapped_rows(&self.response_lines, &self.partial, width);

        if stream_content == 0 && response_content == 0 {
            return (0, 0);
        }

        if available == 0 {
            return (0, 0);
        }

        if response_content == 0 {
            // Only stream content: give it what we have.
            return (stream_content.min(available), 0);
        }

        if stream_content == 0 {
            // Only response content: give it what we have.
            return (0, response_content.min(available));
        }

        // Both present. Response gets priority.
        // Stream floor: STREAM_MIN, but not more than content or
        // available-1 (leave at least 1 row for response).
        let stream_floor = STREAM_MIN
            .min(stream_content)
            .min(available.saturating_sub(1));

        // Give response as much as possible after reserving stream floor.
        let response_h = response_content.min(available.saturating_sub(stream_floor));
        let stream_h = (available.saturating_sub(response_h)).min(stream_content);

        (stream_h, response_h)
    }

    /// Push a styled line to the stream section.
    ///
    /// Flushes any pending partial stream text first so tool-result
    /// lines appear after preceding reasoning fragments, not before.
    pub fn push_stream(&mut self, line: Line<'static>) {
        self.flush_stream_partial();
        self.stream_lines.push(line);
    }

    /// Push raw text to the stream section, handling partial lines.
    /// Completed lines are styled with `style` and indented 2 spaces.
    pub fn push_stream_text(&mut self, text: &str, style: Style) {
        self.stream_partial.push_str(text);
        while let Some(newline_pos) = self.stream_partial.find('\n') {
            let line: String = self.stream_partial[..newline_pos].into();
            self.stream_partial = self.stream_partial[newline_pos + 1..].into();
            self.stream_lines
                .push(Line::from(Span::styled(format!("  {line}"), style)));
        }
    }

    /// Drain any remaining stream partial text as a final stream line.
    fn flush_stream_partial(&mut self) {
        if !self.stream_partial.is_empty() {
            let remaining = std::mem::take(&mut self.stream_partial);
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            self.stream_lines
                .push(Line::from(Span::styled(format!("  {remaining}"), style)));
        }
    }

    /// Push model text. Accumulates partial lines, emits completed
    /// `Line`s to `response_lines` with default style.
    pub fn push_response_text(&mut self, text: &str) {
        self.partial.push_str(text);
        while let Some(newline_pos) = self.partial.find('\n') {
            let line: String = self.partial[..newline_pos].into();
            self.partial = self.partial[newline_pos + 1..].into();
            self.response_lines
                .push(Line::from(Span::styled(line, Style::default())));
        }
    }

    /// Replace the entire response buffer with canonical text.
    /// Used after a broadcast lag — the streamed chunks are incomplete
    /// and the Done.text is authoritative.
    pub fn replace_response(&mut self, text: &str) {
        self.response_lines.clear();
        self.partial.clear();
        self.push_response_text(text);
    }

    /// Drain any remaining partial text as a final response line.
    pub fn flush_partial(&mut self) {
        if !self.partial.is_empty() {
            let remaining = std::mem::take(&mut self.partial);
            self.response_lines
                .push(Line::from(Span::styled(remaining, Style::default())));
        }
    }
}

impl DynamicWidget for SplitWidget {
    fn height(&self, width: u16, available: u16) -> u16 {
        let (sh, rh) = self.layout(width, available);
        sh + rh
    }

    fn is_empty(&self) -> bool {
        self.stream_lines.is_empty()
            && self.response_lines.is_empty()
            && self.partial.is_empty()
            && self.stream_partial.is_empty()
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let (stream_h, response_h) = self.layout(area.width, area.height);

        if stream_h == 0 && response_h == 0 {
            return;
        }

        // Split area vertically: stream on top, response on bottom.
        let mut y = area.y;
        let mut remaining_h = area.height;

        if stream_h > 0 {
            let mut lines = self.stream_lines.clone();
            if !self.stream_partial.is_empty() {
                lines.push(Line::from(Span::styled(
                    self.stream_partial.as_str(),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
            let stream_area = Rect {
                x: area.x,
                y,
                width: area.width,
                height: stream_h.min(remaining_h),
            };
            let total_wrapped =
                wrapped_rows(&self.stream_lines, &self.stream_partial, area.width) as usize;
            let scroll = total_wrapped.saturating_sub(stream_area.height as usize) as u16;
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(paragraph, stream_area);
            y += stream_area.height;
            remaining_h = remaining_h.saturating_sub(stream_area.height);
        }

        if response_h > 0 && remaining_h > 0 {
            let mut lines = self.response_lines.clone();
            if !self.partial.is_empty() {
                lines.push(Line::from(Span::styled(
                    self.partial.as_str(),
                    Style::default(),
                )));
            }
            let response_area = Rect {
                x: area.x,
                y,
                width: area.width,
                height: response_h.min(remaining_h),
            };
            let total_wrapped =
                wrapped_rows(&self.response_lines, &self.partial, area.width) as usize;
            let scroll = total_wrapped.saturating_sub(response_area.height as usize) as u16;
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(paragraph, response_area);
        }
    }

    fn freeze(&mut self) -> FrozenLines {
        self.flush_partial();
        self.flush_stream_partial();

        // Response lines: plain style.
        let response_lines: Vec<(String, Style)> = std::mem::take(&mut self.response_lines)
            .into_iter()
            .map(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                (text, Style::default())
            })
            .collect();

        // Stream tail: last FREEZE_STREAM_LINES, dark gray italic.
        // Lines are already indented by the caller; do not add extra indent.
        let stream_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        let take_count = self.stream_lines.len().min(FREEZE_STREAM_LINES);
        let start = self.stream_lines.len() - take_count;
        let stream_tail: Vec<(String, Style)> = self.stream_lines[start..]
            .iter()
            .map(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                (text, stream_style)
            })
            .collect();

        self.stream_lines.clear();
        (response_lines, stream_tail)
    }
}
