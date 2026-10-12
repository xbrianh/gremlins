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

use std::collections::HashMap;

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::tui::app::{ActiveRun, Overlay};
use crate::tui::buffer::{wrapped_height, BoundedBuffer, LineOrigin, ScrollableBuffer};

/// Shared read-only state handed to each widget's [`LiveAreaWidget::render`]
/// and [`LiveAreaWidget::height_constraint`].
///
/// Stateless widgets (input bar, info bar) read their content from here
/// rather than owning buffers of their own. Overlay widgets observe what is
/// on screen (the active overlay mode and the daemon-derived run table).
pub struct RenderContext<'a> {
    /// Current input-bar text.
    pub input: &'a str,
    /// Cursor byte index into `input`.
    pub cursor: usize,
    /// Gremlin id currently being followed, if any.
    pub following_log: Option<&'a str>,
    /// Cached active-gremlin count string.
    pub gremlin_count: &'a str,
    /// Cached project name.
    pub project_name: &'a str,
    /// The active overlay mode, if any. Overlay widgets read this to decide
    /// whether they are occluded by a blocking overlay.
    pub overlay: Option<&'a Overlay>,
    /// The daemon-derived run table (source of truth for the watch widget).
    pub active_runs: &'a HashMap<String, ActiveRun>,
    /// Height of the content region (viewport minus the fixed bars).
    pub content_height: u16,
}

impl<'a> RenderContext<'a> {
    /// Whether a blocking overlay is currently active. When true, the
    /// content widgets (transcript, reasoning, tools) collapse to zero
    /// height so the blocking overlay occupies the whole content region.
    pub fn blocking_overlay(&self) -> bool {
        self.overlay.as_ref().is_some_and(|o| o.is_blocking())
    }
}

/// A single LiveArea rendering region.
///
/// Widgets are laid out top-to-bottom in the order they appear in the
/// widget vec. Each reports its desired height via
/// [`height_constraint`](Self::height_constraint) and paints itself into the
/// region it is assigned by [`render`](Self::render).
pub trait LiveAreaWidget {
    /// The layout constraint for this widget's region at the given width,
    /// given the current render context.
    fn height_constraint(&self, width: u16, ctx: &RenderContext) -> Constraint;

    /// Render this widget into `area`, returning any overflow lines that
    /// must be committed to terminal scrollback by the caller.
    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) -> Vec<Line<'static>>;

    /// Downcast helper for locating a concrete widget in the stack.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Mutable downcast helper.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Layout priority when the viewport is too short for every widget's
    /// requested height. **Lower value is downsized first.** The input bar
    /// returns the highest value so it is the last widget to lose rows.
    fn size_priority(&self) -> i32 {
        0
    }

    /// Ordering for committing overflow to scrollback. **Lower value is
    /// committed first.** Only the transcript produces overflow today, but
    /// the integer gives `compose` a deterministic ordering if more than
    /// one widget ever does.
    fn overflow_priority(&self) -> i32 {
        0
    }
}

/// Emit the `as_any`/`as_any_mut` trait method bodies for a widget impl.
/// Place inside an `impl LiveAreaWidget for X { ... }` block.
macro_rules! as_any_methods {
    () => {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    };
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
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        0
    }

    fn overflow_priority(&self) -> i32 {
        0
    }

    fn height_constraint(&self, _width: u16, ctx: &RenderContext) -> Constraint {
        // When a blocking overlay is active, collapse to zero so the overlay
        // occupies the whole content region.
        if ctx.blocking_overlay() {
            return Constraint::Length(0);
        }
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

        // Blank separators. `sep_pr` (prompt/response) is independent of
        // the drain; `sep_tp` (transcript/prompt) is decided against the
        // *post-drain* transcript state, since that is what `render` emits.
        let sep_pr = if !prompt_lines.is_empty() && !response_lines.is_empty() {
            1u16
        } else {
            0
        };

        // ── Overflow: split the FIFO head ─────────────────────────
        //
        // The separator between transcript and prompt is present exactly
        // when both the post-drain transcript and the prompt are non-empty.
        // We cannot know the post-drain emptiness before draining, so we
        // drain in two steps: first without reserving the separator row,
        // then, if the transcript survives, reserve the separator row and
        // drain any additional overflow it forces. The single `sep_tp`
        // decision below is then used for both the reserved height and the
        // rendered line list, so they always agree.
        let avail = area.height.saturating_sub(
            prompt_height
                .saturating_add(response_height)
                .saturating_add(sep_pr),
        );
        let (split_idx, _overflow_rows) = self.scrollable.overflow_split(avail, width);

        let mut overflow_lines: Vec<Line<'static>> = if split_idx > 0 {
            let head: Vec<Line<'static>> = self.scrollable.visible_lines()[..split_idx].to_vec();
            self.scrollable.drain_head(split_idx);
            head
        } else {
            Vec::new()
        };

        // If the transcript survived the first drain and a prompt is
        // present, reserve one row for the separator and drain the extra
        // overflow it forces. The separator is emitted only when the
        // transcript survives that reservation; if reserving the separator
        // would empty the FIFO, we skip it and keep the first split's
        // suffix with no separator (otherwise a row is reserved but never
        // emitted, producing a stray blank / dropped fitting line).
        let mut sep_tp = 0u16;
        if !self.scrollable.is_empty() && !prompt_lines.is_empty() {
            let (extra_idx, _) = self
                .scrollable
                .overflow_split(avail.saturating_sub(1), width);
            if extra_idx < self.scrollable.visible_lines().len() {
                if extra_idx > 0 {
                    let head: Vec<Line<'static>> =
                        self.scrollable.visible_lines()[..extra_idx].to_vec();
                    self.scrollable.drain_head(extra_idx);
                    overflow_lines.extend(head);
                }
                sep_tp = 1;
            }
        }

        // ── Build combined scrollable lines (post-drain) ──────────
        let mut combined: Vec<Line<'static>> = self.scrollable.visible_lines().to_vec();
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
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        10
    }

    fn height_constraint(&self, width: u16, ctx: &RenderContext) -> Constraint {
        if ctx.blocking_overlay() {
            return Constraint::Length(0);
        }
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
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        10
    }

    fn height_constraint(&self, width: u16, ctx: &RenderContext) -> Constraint {
        if ctx.blocking_overlay() {
            return Constraint::Length(0);
        }
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

/// The input bar. Wraps its text at the current width and grows up to 5
/// rows. Owns the vertical scroll offset so the cursor stays visible within
/// the wrapped view. Text is wrapped at the terminal width (the only mode
/// `Paragraph` supports with `Wrap { trim: false }`), so there is no
/// horizontal scroll: a row is never wider than the viewport.
pub struct InputBarWidget {
    /// Vertical scroll offset (in wrapped rows) across the wrapped view.
    v_scroll: u16,
}

impl InputBarWidget {
    pub fn new() -> Self {
        Self { v_scroll: 0 }
    }

    /// The wrapped rows of the input text (prompt + text) at `width`.
    pub fn wrapped_rows(&self, width: u16, input: &str) -> u16 {
        let text = Self::full_line(input);
        let rows = wrapped_height(&[text], width).max(1);
        rows.min(5) as u16
    }

    /// Build the full styled line: prompt + input text.
    fn full_line(input: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(input.to_string()),
        ])
    }

    /// Compute the cursor's (row, column) within the wrapped view.
    ///
    /// The column is a *display* column (using `unicode-width`), matching the
    /// wrapping `Paragraph` uses, so wide characters advance the cursor by
    /// their full cell width. When the cursor sits exactly at a wrap boundary
    /// at the end of the input (no following character to occupy the next
    /// row), it is parked on the last cell of the current row so it is always
    /// rendered.
    fn cursor_position(cursor: usize, input: &str, width: u16) -> (u16, u16) {
        if width == 0 {
            return (0, 0);
        }
        let w = width as usize;
        // Prompt is 2 cells wide.
        let col = 2usize + unicode_width::UnicodeWidthStr::width(&input[..cursor]);
        let row = (col / w) as u16;
        let col_in_row = (col % w) as u16;

        // At the end of input on an exact wrap boundary, the cursor would
        // address a row past the wrapped content. Park it on the previous
        // row's last cell instead.
        if cursor == input.len() && col_in_row == 0 && row > 0 {
            (row - 1, width - 1)
        } else {
            (row, col_in_row)
        }
    }

    /// Reconcile the vertical scroll offset so the cursor (at byte index
    /// `cursor` into `input`) is visible within a `rows`-tall window.
    pub fn ensure_cursor_visible(&mut self, cursor: usize, input: &str, width: u16, rows: u16) {
        if width == 0 || rows == 0 {
            return;
        }
        let (row, _) = Self::cursor_position(cursor, input, width);

        // Keep the cursor's row within [v_scroll, v_scroll + rows).
        if row < self.v_scroll {
            self.v_scroll = row;
        } else if row >= self.v_scroll + rows {
            self.v_scroll = row.saturating_sub(rows - 1);
        }
    }

    /// Reset the vertical scroll offset (e.g. after history recall or resize).
    pub fn reset_scroll(&mut self) {
        self.v_scroll = 0;
    }
}

impl Default for InputBarWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAreaWidget for InputBarWidget {
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        100
    }

    fn height_constraint(&self, width: u16, ctx: &RenderContext) -> Constraint {
        Constraint::Length(self.wrapped_rows(width, ctx.input))
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) -> Vec<Line<'static>> {
        let width = area.width;
        if width == 0 {
            return Vec::new();
        }

        // Build the wrapped paragraph, then slice out the visible vertical
        // window so the cursor stays in view.
        let paragraph = Paragraph::new(Self::full_line(ctx.input)).wrap(Wrap { trim: false });

        // Render into a full-width buffer at the paragraph's natural height,
        // then copy the visible window into `area`.
        let full_rows = wrapped_height(&[Self::full_line(ctx.input)], width).max(1) as u16;
        let full_area = Rect::new(0, 0, width, full_rows);
        let mut full_buf = Buffer::empty(full_area);
        paragraph.render(full_area, &mut full_buf);

        // Copy the visible vertical window.
        for y in 0..area.height {
            let src_y = self.v_scroll + y;
            if src_y >= full_rows {
                break;
            }
            for x in 0..width {
                let cell = full_buf[(x, src_y)].clone();
                buf[(area.x + x, area.y + y)] = cell;
            }
        }

        // Draw the cursor cell at the current cursor position, only when it
        // falls inside the visible window.
        let cursor = ctx.cursor.min(ctx.input.len());
        // Reconcile the vertical scroll against the *actual* allocated height
        // (which may be smaller than the requested 5-row cap when the viewport
        // is short), so the cursor stays visible even after shrinking.
        self.ensure_cursor_visible(cursor, ctx.input, width, area.height);
        let (row, col_in_row) = Self::cursor_position(cursor, ctx.input, width);
        let vis_row = row.checked_sub(self.v_scroll);
        if let Some(vis_row) = vis_row {
            if vis_row < area.height && col_in_row < width {
                let cell = &mut buf[(area.x + col_in_row, area.y + vis_row)];
                cell.set_style(Style::default().bg(Color::White));
            }
        }

        Vec::new()
    }
}

/// The info bar. Stateless — reads display state from the render context.
pub struct InfoBarWidget;

impl LiveAreaWidget for InfoBarWidget {
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        90
    }

    fn height_constraint(&self, _width: u16, _ctx: &RenderContext) -> Constraint {
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

/// The non-blocking gremlins watch table overlay.
///
/// Renders a table of active gremlin runs (id, status, stage, definition,
/// project) plus a status-count footer. It owns no mutable buffers — all
/// data is read from the render context. Its constraint reserves an upper
/// portion of the layout and it returns no overflow.
pub struct WatchTableWidget;

impl WatchTableWidget {
    pub fn new() -> Self {
        Self
    }

    /// Desired height: the table header + one row per run + footer, capped
    /// at half the content region so the transcript below stays visible.
    fn desired_height(&self, ctx: &RenderContext) -> u16 {
        let rows = ctx.active_runs.len().max(1) as u16;
        // header + rows + footer, capped at half the content region.
        let half = ctx.content_height / 2;
        (rows + 2).min(half.max(1))
    }
}

impl Default for WatchTableWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAreaWidget for WatchTableWidget {
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        50
    }

    fn height_constraint(&self, _width: u16, ctx: &RenderContext) -> Constraint {
        if ctx.blocking_overlay() {
            return Constraint::Length(0);
        }
        Constraint::Length(self.desired_height(ctx))
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();

        let header_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        lines.push(Line::from(Span::styled(
            "  ID        STATUS    STAGE       DEFINITION",
            header_style,
        )));

        let mut runs: Vec<(&String, &ActiveRun)> = ctx.active_runs.iter().collect();
        runs.sort_by(|a, b| a.0.cmp(b.0));

        for (id, run) in runs {
            let status_style = match run.status.as_str() {
                "running" => Style::default().fg(Color::Green),
                "failed" => Style::default().fg(Color::Red),
                "stopped" => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::DarkGray),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(id.clone(), Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(run.status.clone(), status_style),
                Span::raw("  "),
                Span::styled(run.stage.clone(), Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::styled(run.definition.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }

        let running = ctx
            .active_runs
            .values()
            .filter(|r| r.status == "running")
            .count();
        let total = ctx.active_runs.len();
        lines.push(Line::from(Span::styled(
            format!("  {running} running · {total} total"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }

        Vec::new()
    }
}

/// The blocking single-gremlin log viewer overlay.
///
/// Owns the target gremlin id and its log-line buffer. Renders a header and
/// a tail-anchored log view. Its constraint fills the content area above the
/// fixed input/info bars and it returns no overflow.
pub struct WatchSingleWidget {
    id: String,
    log_lines: Vec<String>,
}

impl WatchSingleWidget {
    pub fn new(id: String) -> Self {
        Self {
            id,
            log_lines: Vec::new(),
        }
    }

    /// Append a log line.
    pub fn push_line(&mut self, line: String) {
        self.log_lines.push(line);
    }
}

impl LiveAreaWidget for WatchSingleWidget {
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        50
    }

    fn height_constraint(&self, _width: u16, _ctx: &RenderContext) -> Constraint {
        // Fills the entire content area above the fixed bars.
        Constraint::Min(0)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) -> Vec<Line<'static>> {
        let header = Line::from(vec![
            Span::styled(
                "following: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.id.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]);

        let mut lines: Vec<Line<'static>> = vec![header];
        lines.push(Line::from(""));

        // Tail-anchored: keep only the last `area.height - 2` lines.
        let body_rows = area.height.saturating_sub(2) as usize;
        let start = self.log_lines.len().saturating_sub(body_rows);
        for line in &self.log_lines[start..] {
            lines.push(Line::from(Span::styled(
                line.clone(),
                Style::default().fg(Color::White),
            )));
        }

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }

        Vec::new()
    }
}

/// The blocking interactive debug session overlay.
///
/// Owns the target id, prompt text, output scrollback, and connection state
/// (`connecting`/`connected`). Renders a header, output scrollback, and the
/// debug prompt above the fixed input/info bars. It returns no overflow.
pub struct DebugWidget {
    id: String,
    input: String,
    history: Vec<String>,
    connecting: bool,
    connected: bool,
}

impl DebugWidget {
    pub fn new(id: String) -> Self {
        Self {
            id,
            input: String::new(),
            history: Vec::new(),
            connecting: true,
            connected: false,
        }
    }

    /// Mutable debug-prompt input.
    pub fn prompt_mut(&mut self) -> &mut String {
        &mut self.input
    }

    /// Take the debug prompt for submission.
    pub fn take_prompt(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    /// Append a line to the debug output scrollback.
    pub fn push_output(&mut self, line: String) {
        self.history.push(line);
    }

    /// Whether the debug socket is open but `ready` has not arrived yet.
    pub fn connecting(&self) -> bool {
        self.connecting
    }

    /// Whether the debug socket session is still connected.
    pub fn connected(&self) -> bool {
        self.connected
    }

    /// Set the connected flag. Setting `false` also clears the connecting
    /// flag so a session that never became ready is reported as ended.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
        if !connected {
            self.connecting = false;
        }
    }

    /// Mark the session as connected (clears the connecting flag).
    pub fn mark_ready(&mut self) {
        self.connecting = false;
        self.connected = true;
    }
}

impl LiveAreaWidget for DebugWidget {
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        50
    }

    fn height_constraint(&self, _width: u16, _ctx: &RenderContext) -> Constraint {
        // Fills the entire content area above the fixed bars.
        Constraint::Min(0)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) -> Vec<Line<'static>> {
        let status = if self.connecting {
            "connecting…"
        } else if self.connected {
            "connected"
        } else {
            "ended"
        };
        let status_style = if self.connecting {
            Style::default().fg(Color::Yellow)
        } else if self.connected {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };

        let header = Line::from(vec![
            Span::styled(
                "debug: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.id.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(status, status_style),
        ]);

        let mut lines: Vec<Line<'static>> = vec![header];
        lines.push(Line::from(""));

        // Output scrollback, tail-anchored. Reserve 2 rows for the prompt
        // line at the bottom.
        let prompt_rows = 2u16;
        let body_rows = area.height.saturating_sub(2 + prompt_rows) as usize;
        let start = self.history.len().saturating_sub(body_rows);
        for line in &self.history[start..] {
            lines.push(Line::from(Span::styled(
                line.clone(),
                Style::default().fg(Color::White),
            )));
        }

        // Debug prompt line.
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("debug> ", Style::default().fg(Color::Cyan)),
            Span::styled(self.input.clone(), Style::default().fg(Color::White)),
        ]));

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }

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

    // ── separator accounting ────────────────────────────────────

    /// Render a transcript into a buffer and return trimmed rows.
    fn render_rows(transcript: &mut TranscriptWidget, cols: u16, rows: u16) -> Vec<String> {
        let area = Rect::new(0, 0, cols, rows);
        let mut buf = Buffer::empty(area);
        let ctx = RenderContext {
            input: "",
            cursor: 0,
            following_log: None,
            gremlin_count: "0",
            project_name: "",
            overlay: None,
            active_runs: &HashMap::new(),
            content_height: rows,
        };
        transcript.render(area, &mut buf, &ctx);
        (0..rows)
            .map(|y| {
                (0..cols)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn separator_accounting_no_stray_blank_when_drain_empties_fifo() {
        // A transcript whose content overflows a tight area such that the
        // extra drain would empty the FIFO. The fitting transcript line
        // must be kept, the prompt/response must not be clipped, and no
        // stray blank line may appear.
        let mut transcript = TranscriptWidget::new();
        transcript.push_line(
            Line::from(Span::raw("transcript line one")),
            LineOrigin::Transcript,
        );
        transcript.push_line(
            Line::from(Span::raw("transcript line two")),
            LineOrigin::Transcript,
        );
        transcript.set_prompt("hi".to_string());
        transcript.replace_response("resp");

        // 4 rows: transcript (1) + prompt (1) + sep_pr (1) + response (1).
        // The separator between transcript and prompt must be omitted
        // because reserving it would empty the FIFO.
        let rows = render_rows(&mut transcript, 80, 4);

        assert_eq!(
            rows,
            vec![
                "transcript line two".to_string(),
                "> hi".to_string(),
                String::new(),
                "resp".to_string(),
            ],
            "Expected the fitting transcript line kept with no stray blank"
        );
    }

    #[test]
    fn separator_accounting_reserves_separator_when_transcript_survives() {
        // When the transcript survives the drain, the separator between
        // transcript and prompt must be present and nothing clipped.
        let mut transcript = TranscriptWidget::new();
        transcript.push_line(
            Line::from(Span::raw("transcript line one")),
            LineOrigin::Transcript,
        );
        transcript.set_prompt("hi".to_string());
        transcript.replace_response("resp");

        // 5 rows: transcript (1) + sep_tp (1) + prompt (1) + sep_pr (1)
        // + response (1) = 5, fits exactly.
        let rows = render_rows(&mut transcript, 80, 5);

        assert!(rows[0].contains("transcript line one"), "{rows:?}");
        assert!(rows[1].is_empty(), "separator should be blank, {rows:?}");
        assert!(rows[2].contains("> hi"), "{rows:?}");
        assert!(rows[3].is_empty(), "separator should be blank, {rows:?}");
        assert!(rows[4].contains("resp"), "{rows:?}");
    }

    // ── input bar cursor positioning ────────────────────────────

    fn input_ctx<'a>(input: &'a str, cursor: usize) -> RenderContext<'a> {
        static EMPTY: std::sync::OnceLock<HashMap<String, ActiveRun>> = std::sync::OnceLock::new();
        RenderContext {
            input,
            cursor,
            following_log: None,
            gremlin_count: "0",
            project_name: "",
            overlay: None,
            active_runs: EMPTY.get_or_init(HashMap::new),
            content_height: 0,
        }
    }

    #[test]
    fn cursor_position_uses_display_width_for_wide_chars() {
        // "😀" is 2 cells wide; "é" is 1 cell. Prompt is 2 cells.
        // input = "😀" → cursor at end = col 2 + 2 = 4.
        assert_eq!(InputBarWidget::cursor_position(4, "😀", 10), (0, 4));
        // input = "a😀" → cursor after 'a' (byte 1) = col 2 + 1 = 3.
        assert_eq!(InputBarWidget::cursor_position(1, "a😀", 10), (0, 3));
        // cursor after 😀 (byte 5) = col 2 + 1 + 2 = 5.
        assert_eq!(InputBarWidget::cursor_position(5, "a😀", 10), (0, 5));
    }

    #[test]
    fn cursor_position_wraps_at_width() {
        // width 5, prompt 2 cells. "abc" → col 5 → row 1 col 0 (mid-text).
        assert_eq!(InputBarWidget::cursor_position(1, "abcd", 5), (0, 3));
        // Cursor after "abc" (byte 3) but more text follows → row 1 col 0.
        assert_eq!(InputBarWidget::cursor_position(3, "abcd", 5), (1, 0));
        // "abcd" → col 6 → row 1 col 1.
        assert_eq!(InputBarWidget::cursor_position(4, "abcd", 5), (1, 1));
    }

    #[test]
    fn cursor_position_parks_at_end_of_full_row() {
        // width 5, prompt 2 cells, input "abc" fills exactly 5 cells.
        // Cursor at end-of-input on a wrap boundary must park on (0, 4),
        // not address a non-existent row 1.
        assert_eq!(InputBarWidget::cursor_position(3, "abc", 5), (0, 4));
    }

    #[test]
    fn cursor_position_zero_width_does_not_panic() {
        assert_eq!(InputBarWidget::cursor_position(0, "", 0), (0, 0));
        assert_eq!(InputBarWidget::cursor_position(3, "abc", 0), (0, 0));
    }

    #[test]
    fn ensure_cursor_visible_scrolls_past_five_rows() {
        let mut bar = InputBarWidget::new();
        // width 5, prompt 2 cells. 30 chars → col 32 → row 6 (col_in_row 2).
        let input = "x".repeat(30);
        bar.ensure_cursor_visible(input.len(), &input, 5, 5);
        // Cursor row 6 must be within [v_scroll, v_scroll + 5).
        assert!(bar.v_scroll <= 6 && 6 < bar.v_scroll + 5);
        assert_eq!(bar.v_scroll, 2);
    }

    #[test]
    fn ensure_cursor_visible_zero_width_does_not_panic() {
        let mut bar = InputBarWidget::new();
        bar.ensure_cursor_visible(0, "", 0, 5);
        bar.ensure_cursor_visible(3, "abc", 0, 5);
    }

    #[test]
    fn render_draws_cursor_only_when_visible() {
        let mut bar = InputBarWidget::new();
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        let ctx = input_ctx("abc", 3); // cursor at end, parked at (0,4)
        bar.render(area, &mut buf, &ctx);
        assert_eq!(buf[(4, 0)].style().bg, Some(Color::White));
        assert_ne!(buf[(0, 0)].style().bg, Some(Color::White));
    }
}
