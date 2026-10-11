//! LiveArea rendering widgets.
//!
//! The five regions of the LiveArea screen are each owned by a dedicated
//! widget behind the [`LiveAreaWidget`] trait. Each widget owns its own
//! rendering buffers and knows how to lay itself out (via
//! [`LiveAreaWidget::height_constraint`]) and paint itself (via
//! [`LiveAreaWidget::render`]).
//!
//! [`LiveArea::compose`](crate::tui::screen::LiveArea::compose) iterates the
//! widgets top-to-bottom, splits the screen by their height constraints, and
//! collects any overflow lines returned by the transcript widget. Widgets
//! never commit scrollback themselves — that remains the caller's
//! responsibility.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::tui::buffer::{wrapped_height, BoundedBuffer, LineOrigin, ScrollableBuffer};

/// Shared read-only state handed to each widget's [`LiveAreaWidget::render`].
///
/// Stateless widgets (input bar, info bar) read their content from here
/// rather than owning buffers of their own.
pub struct RenderContext<'a> {
    /// Current input-bar text.
    pub input: &'a str,
    /// Gremlin id currently being followed, if any.
    pub following_log: Option<&'a str>,
    /// Cached active-gremlin count string.
    pub gremlin_count: &'a str,
    /// Cached project name.
    pub project_name: &'a str,
}

/// A single LiveArea rendering region.
///
/// Widgets are laid out top-to-bottom in the order they appear in the
/// widget vec. Each reports its desired height via
/// [`height_constraint`](Self::height_constraint) and paints itself into the
/// region it is assigned by [`render`](Self::render).
pub trait LiveAreaWidget {
    /// The layout constraint for this widget's region at the given width.
    fn height_constraint(&self, width: u16) -> Constraint;

    /// Render this widget into `area`, returning any overflow lines that
    /// must be committed to terminal scrollback by the caller.
    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) -> Vec<Line<'static>>;
}

/// The scrollable transcript region: completed turns, the current prompt,
/// and the streaming response.
///
/// This is the only widget that can overflow. Its `render` splits the FIFO
/// head off into overflow lines when the combined content exceeds the
/// region's height.
pub struct TranscriptWidget {
    scrollable: ScrollableBuffer,
    current_prompt: Option<String>,
    response_text: String,
}

impl TranscriptWidget {
    pub fn new() -> Self {
        Self {
            scrollable: ScrollableBuffer::new(),
            current_prompt: None,
            response_text: String::new(),
        }
    }

    /// Append a system message with blank-line padding from the previous
    /// entry.
    pub fn push_transcript_line(&mut self, line: String) {
        if !self.scrollable.is_empty() {
            self.scrollable.push(Line::from(""), LineOrigin::Separator);
            self.scrollable.push(Line::from(""), LineOrigin::Separator);
        }
        self.scrollable.push(
            Line::from(Span::styled(line, Style::default())),
            LineOrigin::Transcript,
        );
    }

    /// Push a styled line into the scrollable FIFO with its provenance tag.
    pub fn push_line(&mut self, line: Line<'static>, origin: LineOrigin) {
        self.scrollable.push(line, origin);
    }

    /// Push a blank separator line.
    pub fn push_separator(&mut self) {
        self.scrollable.push(Line::from(""), LineOrigin::Separator);
    }

    /// Whether the scrollable FIFO is empty.
    pub fn is_empty(&self) -> bool {
        self.scrollable.is_empty()
    }

    /// Take the current prompt, leaving `None`.
    pub fn take_prompt(&mut self) -> Option<String> {
        self.current_prompt.take()
    }

    /// Set the current prompt.
    pub fn set_prompt(&mut self, prompt: String) {
        self.current_prompt = Some(prompt);
    }

    /// Clear the current prompt.
    pub fn clear_prompt(&mut self) {
        self.current_prompt = None;
    }

    /// Whether a current prompt is set.
    pub fn has_prompt(&self) -> bool {
        self.current_prompt.is_some()
    }

    /// Borrow the streaming response text.
    pub fn response_text(&self) -> &str {
        &self.response_text
    }

    /// Whether the streaming response is empty.
    pub fn response_is_empty(&self) -> bool {
        self.response_text.is_empty()
    }

    /// Append streaming response text.
    pub fn push_response(&mut self, text: &str) {
        self.response_text.push_str(text);
    }

    /// Replace the entire response text.
    pub fn replace_response(&mut self, text: &str) {
        self.response_text.clear();
        self.response_text.push_str(text);
    }

    /// Clear the response text.
    pub fn clear_response(&mut self) {
        self.response_text.clear();
    }

    /// Clear the scrollable FIFO.
    pub fn clear_scrollable(&mut self) {
        self.scrollable.clear();
    }

    /// Reconcile the streamed response against authoritative full text.
    pub fn reconcile_response(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        if self.response_text.ends_with(text) {
            return;
        }

        if self.response_text.is_empty() {
            self.response_text.push_str(text);
            return;
        }

        let overlap = (0..=self.response_text.len())
            .rev()
            .find(|&len| text.starts_with(&self.response_text[self.response_text.len() - len..]))
            .unwrap_or(0);
        self.response_text
            .truncate(self.response_text.len() - overlap);
        self.response_text.push_str(text);
    }
}

impl Default for TranscriptWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAreaWidget for TranscriptWidget {
    fn height_constraint(&self, _width: u16) -> Constraint {
        // Fills whatever remains after the fixed-height widgets below it.
        Constraint::Min(0)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) -> Vec<Line<'static>> {
        let width = area.width;

        let prompt_marker = Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD);
        let prompt_text = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);

        // ── Current-turn scrollable lines (prompt + response) ─────
        let prompt_lines: Vec<Line<'static>> = match &self.current_prompt {
            Some(prompt) => vec![Line::from(vec![
                Span::styled("> ", prompt_marker),
                Span::styled(prompt.clone(), prompt_text),
            ])],
            None => Vec::new(),
        };
        let prompt_height = wrapped_height(&prompt_lines, width) as u16;

        let response_lines: Vec<Line<'static>> = if self.response_text.is_empty() {
            Vec::new()
        } else {
            self.response_text
                .lines()
                .map(|s| Line::from(Span::styled(s.to_string(), Style::default())))
                .collect()
        };
        let response_height = wrapped_height(&response_lines, width) as u16;

        // Blank separators.
        let sep_tp_est = if !self.scrollable.is_empty() && !prompt_lines.is_empty() {
            1u16
        } else {
            0
        };
        let sep_pr = if !prompt_lines.is_empty() && !response_lines.is_empty() {
            1u16
        } else {
            0
        };

        // Fixed (non-transcript) scrollable height.
        let fixed = prompt_height
            .saturating_add(response_height)
            .saturating_add(sep_tp_est)
            .saturating_add(sep_pr);

        // ── Overflow: split the FIFO head ─────────────────────────
        let transcript_available = area.height.saturating_sub(fixed);
        let (split_idx, _overflow_rows) =
            self.scrollable.overflow_split(transcript_available, width);

        let overflow_lines: Vec<Line<'static>> = if split_idx > 0 {
            let head: Vec<Line<'static>> = self.scrollable.visible_lines()[..split_idx].to_vec();
            self.scrollable.drain_head(split_idx);
            head
        } else {
            Vec::new()
        };

        // ── Build combined scrollable lines (post-drain) ──────────
        let mut combined: Vec<Line<'static>> = self.scrollable.visible_lines().to_vec();
        let sep_tp = if !combined.is_empty() && !prompt_lines.is_empty() {
            1u16
        } else {
            0
        };
        if sep_tp > 0 {
            combined.push(Line::from(""));
        }
        combined.extend(prompt_lines.iter().cloned());
        if sep_pr > 0 {
            combined.push(Line::from(""));
        }
        combined.extend(response_lines.iter().cloned());

        if !combined.is_empty() {
            let paragraph = Paragraph::new(combined).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }

        overflow_lines
    }
}

/// The current-turn reasoning region (bounded to 5 visible rows).
pub struct ReasoningWidget {
    reasoning: BoundedBuffer,
    reasoning_partial: String,
}

impl ReasoningWidget {
    pub fn new() -> Self {
        Self {
            reasoning: BoundedBuffer::new(5),
            reasoning_partial: String::new(),
        }
    }

    /// Append reasoning text (may be a partial chunk). Text is accumulated
    /// and flushed to a reasoning line only when a newline is encountered.
    pub fn push_text(&mut self, text: &str) {
        let style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        self.reasoning_partial.push_str(text);
        while let Some(newline_pos) = self.reasoning_partial.find('\n') {
            let line: String = self.reasoning_partial[..newline_pos].into();
            self.reasoning_partial = self.reasoning_partial[newline_pos + 1..].into();
            self.reasoning
                .push(Line::from(Span::styled(format!("  {line}"), style)));
        }
    }

    /// Flush the reasoning partial buffer (if any) into a completed line.
    pub fn flush(&mut self) {
        if !self.reasoning_partial.is_empty() {
            let remaining = std::mem::take(&mut self.reasoning_partial);
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            self.reasoning
                .push(Line::from(Span::styled(format!("  {remaining}"), style)));
        }
    }

    /// Clear the reasoning buffer and partial accumulator.
    pub fn clear(&mut self) {
        self.reasoning.clear();
        self.reasoning_partial.clear();
    }

    /// The visible tail of reasoning lines (permanently dropping the head),
    /// used when flattening a completed turn into the transcript.
    pub fn take_tail(&mut self, width: u16) -> Vec<Line<'static>> {
        self.reasoning.lines(width)
    }

    fn extra_lines(&self) -> Vec<Line<'static>> {
        if self.reasoning_partial.is_empty() {
            Vec::new()
        } else {
            let reason_style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            vec![Line::from(Span::styled(
                format!("  {}", self.reasoning_partial),
                reason_style,
            ))]
        }
    }
}

impl Default for ReasoningWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAreaWidget for ReasoningWidget {
    fn height_constraint(&self, width: u16) -> Constraint {
        let extra = self.extra_lines();
        let lines = self.reasoning.visible_lines_with_extra(&extra, width);
        Constraint::Length(wrapped_height(&lines, width) as u16)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) -> Vec<Line<'static>> {
        let width = area.width;
        let extra = self.extra_lines();
        let lines = self.reasoning.visible_lines_with_extra(&extra, width);

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }

        Vec::new()
    }
}

/// The current-turn active-tools region (bounded to 5 visible rows).
pub struct ToolsWidget {
    tools: BoundedBuffer,
}

impl ToolsWidget {
    pub fn new() -> Self {
        Self {
            tools: BoundedBuffer::new(5),
        }
    }

    /// Record a tool call. Oldest entries are discarded once the region
    /// exceeds its visible-row bound.
    pub fn push(&mut self, name: &str) {
        let style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        self.tools
            .push(Line::from(Span::styled(format!("  {name}"), style)));
    }

    /// Clear the tools buffer.
    pub fn clear(&mut self) {
        self.tools.clear();
    }

    /// The visible tail of tool lines (permanently dropping the head), used
    /// when flattening a completed turn into the transcript.
    pub fn take_tail(&mut self, width: u16) -> Vec<Line<'static>> {
        self.tools.lines(width)
    }
}

impl Default for ToolsWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAreaWidget for ToolsWidget {
    fn height_constraint(&self, width: u16) -> Constraint {
        let lines = self.tools.visible_lines(width);
        Constraint::Length(wrapped_height(&lines, width) as u16)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) -> Vec<Line<'static>> {
        let lines = self.tools.visible_lines(area.width);

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }

        Vec::new()
    }
}

/// The input bar. Stateless — reads the current input from the render
/// context.
pub struct InputBarWidget;

impl LiveAreaWidget for InputBarWidget {
    fn height_constraint(&self, _width: u16) -> Constraint {
        Constraint::Length(1)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) -> Vec<Line<'static>> {
        let prompt = Span::styled("> ", Style::default().fg(Color::Cyan));

        let text = Line::from(vec![
            prompt,
            Span::raw(ctx.input),
            Span::styled(" ", Style::default().bg(Color::White)),
        ]);

        let paragraph = Paragraph::new(text);
        paragraph.render(area, buf);

        Vec::new()
    }
}

/// The info bar. Stateless — reads display state from the render context.
pub struct InfoBarWidget;

impl LiveAreaWidget for InfoBarWidget {
    fn height_constraint(&self, _width: u16) -> Constraint {
        Constraint::Length(1)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) -> Vec<Line<'static>> {
        let mut spans = vec![
            Span::styled("gremlins tui", Style::default().fg(Color::Cyan)),
            Span::raw(" · "),
            Span::styled(
                format!("{} gremlins", ctx.gremlin_count),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" · "),
            Span::styled(
                format!("project: {}", ctx.project_name),
                Style::default().fg(Color::Magenta),
            ),
        ];

        if let Some(id) = ctx.following_log {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                format!("following: {id}"),
                Style::default().fg(Color::Red),
            ));
        }

        spans.push(Span::raw("  |  "));
        spans.push(Span::styled("Ctrl+G", Style::default().fg(Color::Green)));
        spans.push(Span::raw(" editor  "));
        spans.push(Span::styled("/help", Style::default().fg(Color::Green)));

        let paragraph = Paragraph::new(Line::from(spans));
        paragraph.render(area, buf);

        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_response_fills_dropped_suffix_without_touching_prior_turns() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_response("first turn\n");
        transcript.push_response("second tur");

        transcript.reconcile_response("second turn complete");

        assert_eq!(
            transcript.response_text(),
            "first turn\nsecond turn complete"
        );
    }

    #[test]
    fn reconcile_response_keeps_complete_streamed_text() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_response("first turn\nsecond turn complete");

        transcript.reconcile_response("second turn complete");

        assert_eq!(
            transcript.response_text(),
            "first turn\nsecond turn complete"
        );
    }

    #[test]
    fn reconcile_response_appends_when_buffer_is_empty() {
        let mut transcript = TranscriptWidget::new();

        transcript.reconcile_response("complete turn");

        assert_eq!(transcript.response_text(), "complete turn");
    }

    #[test]
    fn reconcile_response_ignores_empty_authoritative_text() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_response("streamed");

        transcript.reconcile_response("");

        assert_eq!(transcript.response_text(), "streamed");
    }
}
