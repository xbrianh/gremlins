//! LiveArea rendering widgets.
//!
//! The LiveArea screen is composed of a transcript widget (the single
//! styled FIFO), an input bar, an info bar, and overlay widgets. Each
//! widget is behind the [`LiveAreaWidget`] trait, owns its own rendering
//! buffers, and knows how to lay itself out (via
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
use crate::tui::buffer::{wrapped_height, LineOrigin, ScrollableBuffer};
use crate::tui::markdown;

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
    /// content widgets (transcript) collapse to zero height so the blocking
    /// overlay occupies the whole content region.
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

/// Style for reasoning and tool lines: dark-gray italic.
fn dim_italic() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

/// The transcript: a single styled FIFO plus the current turn's draft state.
///
/// This is the only widget that can overflow. Its `render` splits the FIFO
/// head off into overflow lines when the combined content exceeds the
/// region's height, respecting the pinned turn boundary so an in-progress
/// turn never reaches terminal scrollback.
pub struct TranscriptWidget {
    /// The single styled FIFO — the only thing `render` reads.
    scrollable: ScrollableBuffer,
    /// Raw response accumulator (for reconciliation).
    response_accumulator: String,
    /// FIFO index where the current turn starts (the pinned boundary).
    turn_start: Option<usize>,
    /// FIFO index where the response region starts.
    response_start: Option<usize>,
    /// Whether the response region needs re-rendering into the FIFO.
    response_dirty: bool,
    /// Partial reasoning line accumulator.
    reasoning_partial: String,
    /// FIFO index of the live partial reasoning line, if one is present.
    reasoning_partial_idx: Option<usize>,
}

impl TranscriptWidget {
    pub fn new() -> Self {
        Self {
            scrollable: ScrollableBuffer::new(),
            response_accumulator: String::new(),
            turn_start: None,
            response_start: None,
            response_dirty: false,
            reasoning_partial: String::new(),
            reasoning_partial_idx: None,
        }
    }

    /// Append a system message. When a turn is active, the message is
    /// inserted before the pinned turn; otherwise it is appended with
    /// blank-line padding from the previous entry.
    pub fn push_transcript_line(&mut self, line: String) {
        let styled = Line::from(Span::styled(line, Style::default()));
        match self.turn_start {
            Some(t) => {
                let mut inserted = 0usize;
                self.scrollable.insert(t, styled, LineOrigin::Transcript);
                inserted += 1;
                if t > 0 {
                    self.scrollable
                        .insert(t, Line::from(""), LineOrigin::Separator);
                    inserted += 1;
                }
                // Shift the markers by the number of lines inserted before
                // the pinned turn.
                self.turn_start = Some(t + inserted);
                if let Some(r) = &mut self.response_start {
                    *r += inserted;
                }
                if let Some(ri) = &mut self.reasoning_partial_idx {
                    if *ri >= t {
                        *ri += inserted;
                    }
                }
            }
            None => {
                if !self.scrollable.is_empty() {
                    self.scrollable.push(Line::from(""), LineOrigin::Separator);
                    self.scrollable.push(Line::from(""), LineOrigin::Separator);
                }
                self.scrollable.push(styled, LineOrigin::Transcript);
            }
        }
    }

    /// Start a new turn with the given prompt: push a separator and the
    /// markdown-rendered prompt, then pin the turn start.
    pub fn submit_prompt(&mut self, prompt: String) {
        self.flush_reasoning();
        if !self.scrollable.is_empty() {
            self.scrollable.push(Line::from(""), LineOrigin::Separator);
        }
        // Pin the turn at the first prompt line, so the leading separator
        // (which belongs to the *previous* turn) may still overflow.
        let start = self.scrollable.len();
        for line in render_prompt(&prompt) {
            self.scrollable.push(line, LineOrigin::Prompt);
        }
        self.turn_start = Some(start);
        self.response_start = None;
        self.response_accumulator.clear();
        self.response_dirty = false;
        self.reasoning_partial.clear();
        self.reasoning_partial_idx = None;
    }

    /// Append reasoning text (may be a partial chunk). Completed lines are
    /// pushed into the FIFO; the in-progress partial is kept as a live FIFO
    /// line so it is visible while streaming.
    pub fn push_reasoning_text(&mut self, text: &str) {
        self.reasoning_partial.push_str(text);
        // Drop the old partial line; it will be re-added after the complete
        // lines so ordering stays correct.
        self.remove_reasoning_partial();
        while let Some(newline_pos) = self.reasoning_partial.find('\n') {
            let line: String = self.reasoning_partial[..newline_pos].into();
            self.reasoning_partial = self.reasoning_partial[newline_pos + 1..].into();
            self.insert_before_response(
                Line::from(Span::styled(format!("  {line}"), dim_italic())),
                LineOrigin::Reasoning,
            );
        }
        self.add_reasoning_partial();
    }

    /// Flush the reasoning partial buffer (if any) into a completed line.
    pub fn flush_reasoning(&mut self) {
        // The partial line is already in the FIFO; stop tracking it as
        // replaceable so it becomes a permanent line.
        self.reasoning_partial.clear();
        self.reasoning_partial_idx = None;
    }

    /// Record a tool call as a styled line.
    pub fn push_tool(&mut self, name: &str) {
        self.flush_reasoning();
        self.insert_before_response(
            Line::from(Span::styled(format!("  {name}"), dim_italic())),
            LineOrigin::Tool,
        );
    }

    /// Append streaming response text.
    pub fn push_response(&mut self, text: &str) {
        self.flush_reasoning();
        if self.response_start.is_none() {
            self.response_start = Some(self.scrollable.len());
        }
        self.response_accumulator.push_str(text);
        self.response_dirty = true;
    }

    /// Replace the entire response text.
    pub fn replace_response(&mut self, text: &str) {
        self.flush_reasoning();
        if self.response_start.is_none() {
            self.response_start = Some(self.scrollable.len());
        }
        self.response_accumulator.clear();
        self.response_accumulator.push_str(text);
        self.response_dirty = true;
    }

    /// Whether the streaming response is empty.
    pub fn response_is_empty(&self) -> bool {
        self.response_accumulator.is_empty()
    }

    /// Borrow the raw response accumulator.
    #[cfg(test)]
    pub fn response_accumulator(&self) -> &str {
        &self.response_accumulator
    }

    /// Reconcile the streamed response against authoritative full text.
    pub fn reconcile_response(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        if self.response_accumulator.ends_with(text) {
            return;
        }

        if self.response_start.is_none() {
            self.response_start = Some(self.scrollable.len());
        }

        if self.response_accumulator.is_empty() {
            self.response_accumulator.push_str(text);
            self.response_dirty = true;
            return;
        }

        // Walk char boundaries only (never slice mid-codepoint), finding the
        // smallest start whose suffix is a prefix of `text` — i.e. the
        // largest overlap.
        let overlap = self
            .response_accumulator
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(self.response_accumulator.len()))
            .find(|&start| text.starts_with(&self.response_accumulator[start..]))
            .map(|start| self.response_accumulator.len() - start)
            .unwrap_or(0);
        self.response_accumulator
            .truncate(self.response_accumulator.len() - overlap);
        self.response_accumulator.push_str(text);
        self.response_dirty = true;
    }

    /// Whether a current turn is active (pinned).
    pub fn has_current_turn(&self) -> bool {
        self.turn_start.is_some()
    }

    /// Unpin the current turn and clear its draft state. The content is
    /// already in the FIFO.
    pub fn finish_turn(&mut self) {
        self.flush_reasoning();
        self.rebuild_response();
        self.turn_start = None;
        self.response_start = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.reasoning_partial.clear();
        self.reasoning_partial_idx = None;
    }

    /// Discard the current turn: truncate the FIFO back to the turn start
    /// and clear the draft state.
    pub fn clear_current_turn(&mut self) {
        if let Some(t) = self.turn_start {
            self.scrollable.truncate(t);
            // The separator before the turn (pushed by `submit_prompt`) is
            // left behind by the truncation; drop it so Esc leaves no stray
            // blank line.
            if t > 0 && self.scrollable.len() == t {
                if let Some(last) = self.scrollable.visible_lines().last() {
                    if last.to_string().is_empty() {
                        self.scrollable.truncate(t - 1);
                    }
                }
            }
        }
        self.turn_start = None;
        self.response_start = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.reasoning_partial.clear();
        self.reasoning_partial_idx = None;
    }

    /// Clear the scrollable FIFO.
    pub fn clear_scrollable(&mut self) {
        self.scrollable.clear();
        self.turn_start = None;
        self.response_start = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.reasoning_partial.clear();
        self.reasoning_partial_idx = None;
    }

    /// Insert a line before the response region (or append when there is no
    /// response yet). Returns the FIFO index where the line landed.
    fn insert_before_response(&mut self, line: Line<'static>, origin: LineOrigin) -> usize {
        match self.response_start {
            Some(r) => {
                self.scrollable.insert(r, line, origin);
                self.response_start = Some(r + 1);
                r
            }
            None => {
                let idx = self.scrollable.len();
                self.scrollable.push(line, origin);
                idx
            }
        }
    }

    /// Remove the live partial reasoning line (if any) from the FIFO.
    fn remove_reasoning_partial(&mut self) {
        if let Some(idx) = self.reasoning_partial_idx.take() {
            self.scrollable.remove(idx);
            if let Some(r) = &mut self.response_start {
                if *r > idx {
                    *r -= 1;
                }
            }
        }
    }

    /// Add (or refresh) the live partial reasoning line in the FIFO.
    fn add_reasoning_partial(&mut self) {
        if self.reasoning_partial.is_empty() {
            return;
        }
        let line = Line::from(Span::styled(
            format!("  {}", self.reasoning_partial),
            dim_italic(),
        ));
        let idx = self.insert_before_response(line, LineOrigin::Reasoning);
        self.reasoning_partial_idx = Some(idx);
    }

    /// Re-render the response region into the FIFO if dirty.
    fn rebuild_response(&mut self) {
        if !self.response_dirty {
            return;
        }
        self.response_dirty = false;
        let start = self.response_start.unwrap_or(self.scrollable.len());
        self.scrollable.truncate(start);
        for line in markdown::render(&self.response_accumulator, Style::default()) {
            self.scrollable.push(line, LineOrigin::Response);
        }
    }
}

impl Default for TranscriptWidget {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a prompt as markdown with a `> ` marker on the first line and
/// continuation lines indented.
fn render_prompt(prompt: &str) -> Vec<Line<'static>> {
    let marker_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let text_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let lines = markdown::render(prompt, text_style);
    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.into_iter().enumerate() {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        if i == 0 {
            spans.push(Span::styled("> ", marker_style));
        } else {
            spans.push(Span::styled("  ", marker_style));
        }
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    out
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

        // Rebuild the response region if dirty.
        self.rebuild_response();

        // Split overflow with the current turn pinned.
        let pinned = self.turn_start.unwrap_or(self.scrollable.len());
        let (split_idx, _overflow_rows) =
            self.scrollable.overflow_split(area.height, width, pinned);

        let overflow_lines: Vec<Line<'static>> = if split_idx > 0 {
            let head: Vec<Line<'static>> = self.scrollable.visible_lines()[..split_idx].to_vec();
            self.scrollable.drain_head(split_idx);
            // Adjust the markers by the drained count.
            if let Some(t) = &mut self.turn_start {
                *t = t.saturating_sub(split_idx);
            }
            if let Some(r) = &mut self.response_start {
                *r = r.saturating_sub(split_idx);
            }
            if let Some(ri) = &mut self.reasoning_partial_idx {
                *ri = ri.saturating_sub(split_idx);
            }
            head
        } else {
            Vec::new()
        };

        let tail = self.scrollable.visible_lines().to_vec();
        if !tail.is_empty() {
            let paragraph = Paragraph::new(tail).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }

        overflow_lines
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
            transcript.response_accumulator,
            "first turn\nsecond turn complete"
        );
    }

    #[test]
    fn reconcile_response_keeps_complete_streamed_text() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_response("first turn\nsecond turn complete");

        transcript.reconcile_response("second turn complete");

        assert_eq!(
            transcript.response_accumulator,
            "first turn\nsecond turn complete"
        );
    }

    #[test]
    fn reconcile_response_appends_when_buffer_is_empty() {
        let mut transcript = TranscriptWidget::new();

        transcript.reconcile_response("complete turn");

        assert_eq!(transcript.response_accumulator, "complete turn");
    }

    #[test]
    fn reconcile_response_ignores_empty_authoritative_text() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_response("streamed");

        transcript.reconcile_response("");

        assert_eq!(transcript.response_accumulator, "streamed");
    }

    #[test]
    fn submit_prompt_pins_turn_and_renders_markdown() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("**bold** prompt".to_string());
        assert!(transcript.has_current_turn());
        let lines = transcript.scrollable.visible_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "> bold prompt");
    }

    #[test]
    fn clear_current_turn_truncates_to_turn_start() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_transcript_line("prior".to_string());
        transcript.submit_prompt("hi".to_string());
        transcript.push_reasoning_text("thinking\n");
        transcript.push_response("response");
        transcript.clear_current_turn();
        assert!(!transcript.has_current_turn());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["prior"]);
    }

    #[test]
    fn finish_turn_unpins_and_keeps_content() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_reasoning_text("thinking\n");
        transcript.push_tool("Done");
        transcript.push_response("**answer**");
        transcript.finish_turn();
        assert!(!transcript.has_current_turn());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["> hi", "  thinking", "  Done", "answer"]);
    }

    #[test]
    fn transcript_line_inserted_before_pinned_turn() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("resp");
        transcript.push_transcript_line("system note".to_string());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        // "system note" appears above the pinned turn.
        assert_eq!(text[0], "system note");
        assert!(text.contains(&"> hi".to_string()));
    }

    #[test]
    fn reconcile_response_handles_multibyte_char_boundaries() {
        let mut transcript = TranscriptWidget::new();
        // Stream a partial response ending mid-multibyte, then reconcile with
        // authoritative text that shares a multibyte suffix.
        transcript.push_response("héllo wörld");
        transcript.reconcile_response("wörld done");
        assert_eq!(transcript.response_accumulator, "héllo wörld done");
    }

    #[test]
    fn reasoning_partial_is_visible_in_fifo() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_reasoning_text("thinking");
        // The partial reasoning line must be in the FIFO before any newline.
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.contains(&"  thinking".to_string()), "{text:?}");
    }

    #[test]
    fn tool_line_after_response_is_preserved_by_rebuild() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("streamed");
        // A tool call arriving after response streaming began must not be
        // erased when the response region is rebuilt.
        transcript.push_tool("grep");
        transcript.reconcile_response("streamed complete");
        transcript.finish_turn();
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.contains(&"  grep".to_string()), "{text:?}");
        assert!(text.contains(&"streamed complete".to_string()), "{text:?}");
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
