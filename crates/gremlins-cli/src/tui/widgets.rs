//! LiveArea rendering widgets.
//!
//! The LiveArea screen is composed of a transcript widget (the single
//! styled FIFO), an input bar, an info bar, and overlay widgets. Each
//! widget is behind the [`LiveAreaWidget`] trait, owns its own rendering
//! buffers, and knows how to lay itself out (via
//! [`LiveAreaWidget::min_height`] and [`LiveAreaWidget::flexible`]) and paint
//! itself (via [`LiveAreaWidget::render`]).
//!
//! [`LiveArea::compose`](crate::tui::screen::LiveArea::compose) iterates the
//! widgets top-to-bottom, splits the screen by their minimum heights and
//! flexibility, and collects any overflow lines returned by the transcript
//! widget. Widgets
//! never commit scrollback themselves — that remains the caller's
//! responsibility.

use std::cell::Cell;
use std::collections::HashMap;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::tui::app::{ActiveRun, Overlay};
use crate::tui::buffer::{line_wrapped_height, wrapped_height, LineBuffer};
use crate::tui::markdown;
use crate::tui::motif::{self, glyph};
use crate::tui::theme::DEFAULT_THEME;
/// Shared read-only state handed to each widget's [`LiveAreaWidget::render`]
/// and [`LiveAreaWidget::min_height`].
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
/// widget vec. Each reports its minimum height via
/// [`min_height`](Self::min_height), whether it may absorb leftover rows via
/// [`flexible`](Self::flexible), and paints itself into the region it is
/// assigned by [`render`](Self::render).
pub trait LiveAreaWidget {
    /// The fewest rows this widget can tolerate at the given width and render
    /// context. This is the floor the priority system shrinks down to.
    fn min_height(&self, width: u16, ctx: &RenderContext) -> u16;

    /// Whether this widget may absorb leftover rows in the expand pass.
    /// Defaults to `false`.
    fn flexible(&self, _ctx: &RenderContext) -> bool {
        false
    }

    /// Bring this widget's buffer into its final state for the frame. This
    /// is the single place content is mutated before measurement: the
    /// transcript rebuilds a dirty response region into its FIFO, and the
    /// reasoning widget flushes its trailing partial into a completed line.
    /// Defaults to a no-op.
    fn finalize(&mut self, _width: u16) {}

    /// Render this widget into `area`. This is paint-only: overflow is
    /// reported separately via [`take_overflow`](Self::take_overflow).
    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext);

    /// Drain and return any lines that do not fit in `available_rows` at
    /// the given width. The widget decides what happens to its head.
    fn take_overflow(&mut self, _available_rows: u16, _width: u16) -> Vec<Line<'static>> {
        Vec::new()
    }

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

    /// The height this widget *wants* at the given width, given the current
    /// render context. Flexible widgets distinguish "how much I want" from
    /// "how little I can tolerate" (their minimum height): the desired
    /// height is what they report when space is plentiful, while the
    /// minimum is what they shrink down to when space is contested. Defaults
    /// to the widget's minimum height.
    fn desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        self.min_height(width, ctx)
    }

    /// Ordering for committing overflow to scrollback. **Lower value is
    /// committed first.** Only the transcript produces overflow today, but
    /// the integer gives `compose` a deterministic ordering if more than
    /// one widget ever does.
    fn overflow_priority(&self) -> i32 {
        0
    }

    /// The maximum height this widget may be allocated, as a first-class
    /// layout constraint. Defaults to `u16::MAX` (uncapped). The input bar
    /// caps at 5 rows and the watch table caps at half the content region.
    fn max_height(&self, _ctx: &RenderContext) -> u16 {
        u16::MAX
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

/// The transcript: a single styled FIFO plus the current turn's draft state.
///
/// This is the only widget that can overflow. Its
/// [`take_overflow`](TranscriptWidget::take_overflow) splits the FIFO head
/// off into overflow lines whenever the combined content exceeds the
/// region's height. The transcript is a plain FIFO: it drains its head and
/// renders its tail regardless of whether a turn is in progress.
pub struct TranscriptWidget {
    /// The single styled FIFO — the only thing `render` reads.
    scrollable: LineBuffer,
    /// Raw response accumulator (for reconciliation).
    response_accumulator: String,
    /// FIFO index where the current turn starts (the pinned boundary).
    turn_start: Option<usize>,
    /// FIFO index where the response region starts.
    response_start: Option<usize>,
    /// FIFO index of the turn separator rule (regenerated at `finalize`
    /// width so the glint rule spans the full content width).
    separator_index: Option<usize>,
    /// Last-known content width (from `finalize`), used to size rules when
    /// the response region is rebuilt outside a finalize pass.
    last_width: u16,
    /// Whether the response region needs re-rendering into the FIFO.
    response_dirty: bool,
    /// Byte offset into `response_accumulator` of text already rendered
    /// into the FIFO. When overflow drains response lines, this offset
    /// stays put so the next rebuild only renders the uncommitted suffix
    /// — preventing drained lines from reappearing.
    response_committed_offset: usize,
    /// Cached wrapped desired height `(width, height)` for the FIFO,
    /// invalidated whenever the FIFO, accumulator, or width changes.
    desired_cache: Cell<Option<(u16, u16)>>,
}

impl TranscriptWidget {
    pub fn new() -> Self {
        Self {
            scrollable: LineBuffer::new(),
            response_accumulator: String::new(),
            turn_start: None,
            response_start: None,
            separator_index: None,
            last_width: 40,
            response_dirty: false,
            response_committed_offset: 0,
            desired_cache: Cell::new(None),
        }
    }

    /// Invalidate the cached desired height after any content mutation.
    fn invalidate_desired_cache(&self) {
        self.desired_cache.set(None);
    }

    /// Append a system message. When a turn is active, the message is
    /// inserted before the pinned turn; otherwise it is appended with
    /// blank-line padding from the previous entry.
    pub fn push_transcript_line(&mut self, line: String) {
        self.invalidate_desired_cache();
        let styled = Line::from(Span::styled(line, Style::default().fg(DEFAULT_THEME.text)));
        self.push_styled_line(styled);
    }

    /// Push a pre-styled line into the FIFO with the same insertion logic
    /// as `push_transcript_line`.
    pub fn push_styled_line(&mut self, styled: Line<'static>) {
        self.invalidate_desired_cache();
        match self.turn_start {
            Some(t) => {
                let mut inserted = 0usize;
                self.scrollable.insert(t, styled);
                inserted += 1;
                if t > 0 {
                    self.scrollable.insert(t, Line::from(""));
                    inserted += 1;
                }
                self.turn_start = Some(t + inserted);
                if let Some(r) = &mut self.response_start {
                    *r += inserted;
                }
            }
            None => {
                if !self.scrollable.is_empty() {
                    self.scrollable.push(Line::from(""));
                    self.scrollable.push(Line::from(""));
                }
                self.scrollable.push(styled);
            }
        }
    }

    /// Start a new turn with the given prompt: push a separator rule and the
    /// markdown-rendered prompt, then pin the turn start.
    pub fn submit_prompt(&mut self, prompt: String) {
        self.invalidate_desired_cache();
        let mut sep_index = None;
        if !self.scrollable.is_empty() {
            // Placeholder width; regenerated at `finalize` with the real
            // content width.
            self.scrollable.push(Line::from(Span::styled(
                "─".repeat(40),
                Style::default().fg(DEFAULT_THEME.separator),
            )));
            sep_index = Some(self.scrollable.len() - 1);
        }
        // Pin the turn at the first prompt line, so the leading separator
        // (which belongs to the *previous* turn) may still overflow.
        let start = self.scrollable.len();
        // The prompt prefix (▍ + » ) adds 3 cells; subtract those plus
        // the transcript's 1-cell outer margin so thematic breaks and
        // fenced-code borders never exceed the inner content width.
        for line in render_prompt(&prompt, self.last_width.saturating_sub(4)) {
            self.scrollable.push(line);
        }
        self.turn_start = Some(start);
        self.separator_index = sep_index;
        self.response_start = None;
        self.response_accumulator.clear();
        self.response_dirty = false;
        self.response_committed_offset = 0;
    }

    /// Append streaming response text.
    pub fn push_response(&mut self, text: &str) {
        self.invalidate_desired_cache();
        if self.response_start.is_none() {
            self.response_start = Some(self.scrollable.len());
        }
        self.response_accumulator.push_str(text);
        self.response_dirty = true;
    }

    /// Replace the entire response text.
    pub fn replace_response(&mut self, text: &str) {
        self.invalidate_desired_cache();
        if self.response_start.is_none() {
            self.response_start = Some(self.scrollable.len());
        }
        self.response_accumulator.clear();
        self.response_accumulator.push_str(text);
        self.response_dirty = true;
        self.response_committed_offset = 0;
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
        self.invalidate_desired_cache();

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
        self.response_committed_offset = 0;
    }

    /// Whether a current turn is active (pinned).
    pub fn has_current_turn(&self) -> bool {
        self.turn_start.is_some()
    }

    /// Unpin the current turn and clear its draft state. The content is
    /// already in the FIFO.
    pub fn finish_turn(&mut self) {
        self.rebuild_response();
        self.invalidate_desired_cache();
        self.turn_start = None;
        self.response_start = None;
        self.separator_index = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.response_committed_offset = 0;
    }

    /// Discard the current turn: truncate the FIFO back to the turn start
    /// and clear the draft state.
    pub fn clear_current_turn(&mut self) {
        self.invalidate_desired_cache();
        if let Some(t) = self.turn_start {
            self.scrollable.truncate(t);
            // The separator rule before the turn (pushed by `submit_prompt`)
            // is left behind by the truncation; drop it so Esc leaves no
            // stray rule.
            if t > 0 && self.scrollable.len() == t {
                if let Some(last) = self.scrollable.visible_lines().last() {
                    if is_separator_rule(last) {
                        self.scrollable.truncate(t - 1);
                    }
                }
            }
        }
        self.turn_start = None;
        self.response_start = None;
        self.separator_index = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.response_committed_offset = 0;
    }

    /// Clear the scrollable FIFO.
    pub fn clear_scrollable(&mut self) {
        self.invalidate_desired_cache();
        self.scrollable.clear();
        self.turn_start = None;
        self.response_start = None;
        self.separator_index = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.response_committed_offset = 0;
    }

    /// Re-render the response region into the FIFO if dirty.
    fn rebuild_response(&mut self) {
        if !self.response_dirty {
            return;
        }
        self.invalidate_desired_cache();
        self.response_dirty = false;
        let start = self.response_start.unwrap_or(self.scrollable.len());
        self.scrollable.truncate(start);
        // Only render the suffix that hasn't been committed yet. After
        // overflow drains response lines from the FIFO head,
        // response_committed_offset stays put so drained text is never
        // re-rendered.
        let suffix = &self.response_accumulator[self.response_committed_offset..];
        for line in markdown::render(
            suffix,
            Style::default().fg(DEFAULT_THEME.text),
            self.last_width.saturating_sub(1),
        ) {
            self.scrollable.push(line);
        }
        self.response_committed_offset = self.response_accumulator.len();
    }

    /// Regenerate the turn separator at the current content width.
    fn rebuild_separator(&mut self, width: u16) {
        let Some(idx) = self.separator_index else {
            return;
        };
        let rule = motif::glint_rule(width.saturating_sub(1));
        if idx < self.scrollable.len() {
            self.scrollable.remove(idx);
            self.scrollable.insert(idx, rule);
        }
    }

    /// Wrapped height of the full FIFO at the given width. Measurement
    /// happens on finalized content, so no prediction of a dirty response
    /// region is needed. The transcript paints with a one-cell left margin,
    /// so measurement uses `width - 1`.
    fn desired_height(&self, width: u16) -> u16 {
        let width = width.saturating_sub(1);
        if let Some((w, h)) = self.desired_cache.get() {
            if w == width {
                return h;
            }
        }
        let h = (self.scrollable.wrapped_height(width).min(u16::MAX as usize)) as u16;
        self.desired_cache.set(Some((width, h)));
        h
    }

    /// Drain the head of the transcript FIFO that does not fit in
    /// `available_rows`. The FIFO is drained unconditionally: an in-progress
    /// turn overflows into terminal scrollback exactly like completed
    /// history.
    pub fn take_overflow(&mut self, available_rows: u16, width: u16) -> Vec<Line<'static>> {
        self.invalidate_desired_cache();
        let width = width.saturating_sub(1);
        let (_overflow_rows, overflow_lines): (_, Vec<_>) =
            split_overflow(&self.scrollable, available_rows, width);
        if overflow_lines.is_empty() {
            return Vec::new();
        }

        let drained = overflow_lines.len();
        self.scrollable.drain_head(drained);
        if let Some(t) = &mut self.turn_start {
            *t = t.saturating_sub(drained);
        }
        if let Some(r) = &mut self.response_start {
            *r = r.saturating_sub(drained);
        }
        if let Some(s) = &mut self.separator_index {
            if *s < drained {
                // The separator was drained; drop the index so a later
                // `finalize` cannot overwrite a real transcript line.
                self.separator_index = None;
            } else {
                *s -= drained;
            }
        }
        overflow_lines
    }
}

impl Default for TranscriptWidget {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a line is the turn separator rule (a single span of rule glyphs).
fn is_separator_rule(line: &Line<'static>) -> bool {
    line.spans.len() == 1
        && !line.spans[0].content.is_empty()
        && line.spans[0]
            .content
            .chars()
            .all(|c| c == '─' || c == '═' || c == '◆')
}

/// Render a prompt as markdown with a two-tone `» ` chevron on the first
/// line and continuation lines indented. `rule_width` is the content width
/// used to size any thematic breaks or fenced-code borders in the prompt.
fn render_prompt(prompt: &str, rule_width: u16) -> Vec<Line<'static>> {
    let marker_style = Style::default()
        .fg(DEFAULT_THEME.prompt_marker)
        .bg(DEFAULT_THEME.prompt_bg);
    let text_style = Style::default()
        .fg(DEFAULT_THEME.prompt_text)
        .bg(DEFAULT_THEME.prompt_bg)
        .add_modifier(Modifier::BOLD);
    let margin_style = Style::default()
        .fg(DEFAULT_THEME.prompt_margin)
        .bg(DEFAULT_THEME.prompt_bg);
    let lines = markdown::render(prompt, text_style, rule_width);
    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.into_iter().enumerate() {
        let mut spans = Vec::with_capacity(line.spans.len() + 2);
        spans.push(Span::styled(if i == 0 { "▍" } else { " " }, margin_style));
        if i == 0 {
            spans.push(Span::styled("» ", marker_style));
        } else {
            spans.push(Span::styled("  ", marker_style));
        }
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    out
}

/// A single reasoning line: brass telemetry diamond + nickel italic text.
/// `hollow` alternates the diamond glyph so streaming motion comes free from
/// the stream itself (no frame timer).
fn reasoning_line(text: &str, hollow: bool) -> Line<'static> {
    let g = if hollow {
        glyph::TELEMETRY_HOLLOW
    } else {
        glyph::TELEMETRY
    };
    Line::from(vec![
        Span::styled(
            format!("{g} "),
            Style::default().fg(DEFAULT_THEME.code_border),
        ),
        Span::styled(
            text.to_string(),
            Style::default()
                .fg(DEFAULT_THEME.subtle)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// A single tool-event line: chrome diamond + nickel italic name.
fn tool_line(name: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{} ", glyph::DIAMOND),
            Style::default().fg(DEFAULT_THEME.chrome),
        ),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(DEFAULT_THEME.subtle)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// Find a prefix of `buffer` that does not fit within `available_rows`.
///
/// The head is drained unconditionally; an in-progress turn is treated like
/// any other FIFO content. Returns the number of overflowing wrapped rows
/// and the actual head lines (in render order).
fn split_overflow(
    buffer: &LineBuffer,
    available_rows: u16,
    width: u16,
) -> (usize, Vec<Line<'static>>) {
    // Zero-area regions have no place to put overflow, and the transcript
    // may be collapsed by a blocking overlay or a very short viewport. Keep
    // the FIFO intact in those cases.
    if width == 0 || available_rows == 0 {
        return (0, Vec::new());
    }

    let available = available_rows as usize;
    let total = buffer.wrapped_height(width);
    if total <= available {
        return (0, Vec::new());
    }

    let lines = buffer.visible_lines();
    let mut dropped = 0usize;
    let mut drop_count = 0usize;
    // Drain the head until the remaining content fits, but never drain the
    // final line: a single line that wraps taller than the viewport must
    // stay in the FIFO so the tail-anchored renderer can show its wrapped
    // tail. Overflowing preceding lines is enough to make room.
    while total - dropped > available && drop_count + 1 < lines.len() {
        dropped += line_wrapped_height(&lines[drop_count], width);
        drop_count += 1;
    }

    if drop_count == 0 {
        return (0, Vec::new());
    }

    let overflow_lines = lines[..drop_count].to_vec();
    (dropped, overflow_lines)
}

/// Trims a live-tail buffer to the rows that fit in `available_rows`,
/// wrapping-aware, discarding the head. Returns nothing: the head is never
/// committed to terminal scrollback.
fn trim_to_tail(buffer: &mut LineBuffer, available_rows: u16, width: u16) {
    if available_rows == 0 || width == 0 {
        buffer.clear();
        return;
    }

    let available = available_rows as usize;
    let total = buffer.wrapped_height(width);
    if total <= available {
        return;
    }

    let mut drop_count = 0usize;
    let mut dropped = 0usize;
    while total - dropped > available && drop_count < buffer.len() {
        dropped += line_wrapped_height(&buffer.visible_lines()[drop_count], width);
        drop_count += 1;
    }
    buffer.drain_head(drop_count);
}

/// Live tail for reasoning text. Accumulates partial chunks into complete
/// lines and renders them unstyled, growing freely with its content.
pub struct ReasoningWidget {
    lines: LineBuffer,
    partial: String,
    /// Number of completed lines pushed, used to alternate the telemetry
    /// diamond glyph for free streaming motion.
    line_count: usize,
}

impl ReasoningWidget {
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
            partial: String::new(),
            line_count: 0,
        }
    }

    /// Append reasoning text (may be a partial chunk). Completed lines are
    /// pushed into the buffer; the trailing partial is kept as a live line.
    /// Each line is prefixed with a brass telemetry diamond.
    pub fn push_reasoning_text(&mut self, text: &str) {
        self.partial.push_str(text);
        while let Some(newline_pos) = self.partial.find('\n') {
            let line: String = self.partial[..newline_pos].into();
            self.partial = self.partial[newline_pos + 1..].into();
            let hollow = self.line_count % 2 == 1;
            self.lines.push(reasoning_line(&line, hollow));
            self.line_count += 1;
        }
    }

    /// Flush the trailing partial (if any) into a completed line.
    #[allow(dead_code)]
    pub fn flush_reasoning(&mut self) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            let hollow = self.line_count % 2 == 1;
            self.lines.push(reasoning_line(&line, hollow));
            self.line_count += 1;
        }
    }

    /// Reset the live region for a new turn.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.partial.clear();
        self.line_count = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.partial.is_empty()
    }

    /// Desired height: the full wrapped content height (uncapped).
    fn desired_rows(&self, width: u16) -> u16 {
        if self.is_empty() {
            return 0;
        }
        (wrapped_height(&self.rendered_lines(), width).min(u16::MAX as usize)) as u16
    }

    fn rendered_lines(&self) -> Vec<Line<'static>> {
        let mut out = self.lines.visible_lines().to_vec();
        if !self.partial.is_empty() {
            let hollow = self.line_count % 2 == 1;
            out.push(reasoning_line(&self.partial, hollow));
        }
        out
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
        5
    }

    fn min_height(&self, width: u16, _ctx: &RenderContext) -> u16 {
        self.desired_rows(width).min(5)
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        true
    }

    fn desired_height(&self, width: u16, _ctx: &RenderContext) -> u16 {
        self.desired_rows(width)
    }

    fn max_height(&self, _ctx: &RenderContext) -> u16 {
        u16::MAX
    }

    fn take_overflow(&mut self, available_rows: u16, width: u16) -> Vec<Line<'static>> {
        // Keep the visible tail; the head is discarded, never committed to
        // terminal scrollback.
        let rendered = self.rendered_lines();
        let total = wrapped_height(&rendered, width);
        if total > available_rows as usize {
            // Rebuild `lines` from the rendered tail.
            let mut drop_count = 0usize;
            let mut dropped = 0usize;
            while total - dropped > available_rows as usize && drop_count < rendered.len() {
                dropped += line_wrapped_height(&rendered[drop_count], width);
                drop_count += 1;
            }
            if drop_count == 0 {
                return Vec::new();
            }
            // Drop the first `drop_count` source lines. Partial lines are
            // always at the tail, so only buffer lines are dropped.
            let drop_from_buffer = drop_count.min(self.lines.len());
            self.lines.drain_head(drop_from_buffer);
            if drop_count > drop_from_buffer {
                self.partial.clear();
            }
        }
        Vec::new()
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) {
        let lines = self.rendered_lines();
        if !lines.is_empty() {
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .render(area, buf);
        }
    }
}

/// Live tail for tool-call events. Owns a FIFO of unstyled tool-name lines,
/// growing freely with its content.
pub struct ToolEventsWidget {
    lines: LineBuffer,
}

impl ToolEventsWidget {
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
        }
    }

    /// Append a tool-name line.
    pub fn push_tool(&mut self, name: &str) {
        self.lines.push(tool_line(name));
    }

    /// Reset the live region for a new turn.
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Desired height: the full wrapped content height (uncapped).
    fn desired_rows(&self, width: u16) -> u16 {
        if self.is_empty() {
            return 0;
        }
        (self.lines.wrapped_height(width).min(u16::MAX as usize)) as u16
    }
}

impl Default for ToolEventsWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAreaWidget for ToolEventsWidget {
    as_any_methods!();

    fn size_priority(&self) -> i32 {
        1
    }

    fn min_height(&self, width: u16, _ctx: &RenderContext) -> u16 {
        self.desired_rows(width).min(5)
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        true
    }

    fn desired_height(&self, width: u16, _ctx: &RenderContext) -> u16 {
        self.desired_rows(width)
    }

    fn max_height(&self, _ctx: &RenderContext) -> u16 {
        u16::MAX
    }

    fn take_overflow(&mut self, available_rows: u16, width: u16) -> Vec<Line<'static>> {
        trim_to_tail(&mut self.lines, available_rows, width);
        Vec::new()
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) {
        let tail = self.lines.visible_lines().to_vec();
        if !tail.is_empty() {
            Paragraph::new(tail)
                .wrap(Wrap { trim: false })
                .render(area, buf);
        }
    }
}

impl LiveAreaWidget for TranscriptWidget {
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        10
    }

    fn overflow_priority(&self) -> i32 {
        0
    }

    fn min_height(&self, _width: u16, ctx: &RenderContext) -> u16 {
        // The transcript is a plain FIFO with a one-row floor: it always
        // reserves a single row so a streaming response stays visible even
        // when reasoning/tool widgets contest the space, but the floor no
        // longer depends on where a turn happens to start. Content that does
        // not fit is drained to scrollback. A blocking overlay collapses it
        // to zero by the same logic.
        if ctx.blocking_overlay() {
            0
        } else {
            1
        }
    }

    fn flexible(&self, ctx: &RenderContext) -> bool {
        // When a blocking overlay is active the transcript collapses to zero
        // and must not absorb leftover rows, mirroring the old `Length(0)`
        // (inflexible) vs `Min` (flexible) split.
        !ctx.blocking_overlay()
    }

    fn desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        if ctx.blocking_overlay() {
            return 0;
        }
        self.desired_height(width)
    }

    fn finalize(&mut self, width: u16) {
        self.last_width = width;
        self.rebuild_separator(width);
        self.rebuild_response();
    }

    fn take_overflow(&mut self, available_rows: u16, width: u16) -> Vec<Line<'static>> {
        self.take_overflow(available_rows, width)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) {
        // Clear the region first so a shorter frame cannot leave stale cells
        // from a previous, taller transcript.
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)].reset();
            }
        }
        if area.width == 0 || area.height == 0 {
            return;
        }

        // One-cell left margin: paint into an inner rect.
        let inner = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(1),
            area.height,
        );
        let width = inner.width;
        if width == 0 {
            return;
        }

        let lines = self.scrollable.visible_lines();
        if lines.is_empty() {
            return;
        }

        let full_rows = wrapped_height(lines, width);
        if full_rows == 0 {
            return;
        }

        // Tail-anchored, wrapping-aware paint. When the FIFO fits, paint it
        // top-anchored directly. Otherwise skip the wrapped rows above the
        // visible window and render only the tail slice, so the most recent
        // content is always visible without materializing the whole history.
        let skip = full_rows.saturating_sub(inner.height as usize);
        if skip == 0 {
            Paragraph::new(lines.to_vec())
                .wrap(Wrap { trim: false })
                .render(inner, buf);
            return;
        }

        // Find the first line that contributes wrapped rows to the window.
        let mut cum = 0usize;
        let mut start = 0usize;
        for (i, line) in lines.iter().enumerate() {
            let h = line_wrapped_height(line, width);
            if cum + h > skip {
                start = i;
                break;
            }
            cum += h;
        }
        // Wrapped rows of `lines[start]` that sit above the visible window.
        let partial = skip - cum;

        // Render the tail slice into a buffer tall enough to hold the window
        // plus the partial head, then copy the last `area.height` rows.
        let render_rows = (partial + inner.height as usize) as u16;
        let render_area = Rect::new(0, 0, width, render_rows);
        let mut render_buf = Buffer::empty(render_area);
        Paragraph::new(lines[start..].to_vec())
            .wrap(Wrap { trim: false })
            .render(render_area, &mut render_buf);

        for y in 0..inner.height {
            let src_y = partial as u16 + y;
            if src_y >= render_rows {
                break;
            }
            for x in 0..width {
                buf[(inner.x + x, inner.y + y)] = render_buf[(x, src_y)].clone();
            }
        }
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

    /// The wrapped rows of the input text (prompt + text) at `width`,
    /// uncapped.
    pub fn wrapped_rows(&self, width: u16, input: &str) -> u16 {
        let text = Self::full_line(input, width);
        (wrapped_height(&[text], width).max(1).min(u16::MAX as usize)) as u16
    }

    /// Build the full styled line: prompt + input text (or a faint hint when
    /// the input is empty). The hint is truncated to `width` cells so an empty
    /// bar never wraps onto a second row on a narrow terminal.
    fn full_line(input: &str, width: u16) -> Line<'static> {
        if input.is_empty() {
            let hint = "say it like you mean it — /help for the brass";
            // Reserve two cells for the `» ` chevron prefix.
            let budget = width.saturating_sub(2) as usize;
            let hint: String = hint.chars().take(budget).collect();
            return Line::from(vec![
                Span::styled(
                    format!("{} ", glyph::CHEVRON),
                    Style::default().fg(DEFAULT_THEME.accent_primary),
                ),
                Span::styled(
                    hint,
                    Style::default()
                        .fg(DEFAULT_THEME.subtle)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
        }
        Line::from(vec![
            Span::styled(
                format!("{} ", glyph::CHEVRON),
                Style::default().fg(DEFAULT_THEME.accent_primary),
            ),
            Span::raw(input.to_string()),
        ])
    }

    /// Compute the cursor's (row, column) within the wrapped view.
    ///
    /// The cursor cell is located by rendering the input through the same
    /// `Paragraph::wrap(Wrap { trim: false })` path the input bar paints with,
    /// with the grapheme at the cursor's byte offset styled with a marker
    /// style that cannot occur in the input. Because the text is unchanged,
    /// the wrapping — including word breaks at spaces — is identical to the
    /// real line, and the styled grapheme lands exactly on the cell where the
    /// next keystroke inserts. At end of input a single-cell sentinel is
    /// appended and parked on the last cell of the final row so the cursor
    /// never addresses a row past the wrapped content. When the grapheme at
    /// the cursor is dropped by the wrapper (a space at a wrap boundary, or a
    /// zero-width character), the cursor falls back to the end of the
    /// preceding text.
    fn cursor_position(cursor: usize, input: &str, width: u16) -> (u16, u16) {
        if width == 0 {
            return (0, 0);
        }

        if cursor == input.len() {
            return Self::end_of_text_position(input, width);
        }

        let ch = input[cursor..]
            .chars()
            .next()
            .expect("cursor < input.len()");
        let len = ch.len_utf8();

        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Self::prompt_span());
        if cursor > 0 {
            spans.push(Span::raw(input[..cursor].to_string()));
        }
        spans.push(Span::styled(ch.to_string(), Self::marker_style()));
        spans.push(Span::raw(input[cursor + len..].to_string()));

        if let Some(pos) = Self::locate_marker(Line::from(spans), width) {
            return pos;
        }

        // The styled grapheme was dropped by the wrapper. The cursor sits just
        // before it, i.e. at the end of the preceding text.
        Self::end_of_text_position(&input[..cursor], width)
    }

    /// Cursor position at the very end of `input` (just past the last char).
    fn end_of_text_position(input: &str, width: u16) -> (u16, u16) {
        let full_rows = wrapped_height(&[Self::full_line(input, width)], width).max(1) as u16;

        let spans: Vec<Span<'static>> = vec![
            Self::prompt_span(),
            Span::raw(input.to_string()),
            Span::styled("\u{E000}", Self::marker_style()),
        ];

        let marker =
            Self::locate_marker(Line::from(spans), width).unwrap_or((full_rows - 1, width - 1));

        // Park at the end of the last row if the sentinel wrapped past the
        // wrapped content (the cursor would otherwise address a row that does
        // not exist in the input bar).
        if marker.0 >= full_rows {
            (full_rows - 1, width - 1)
        } else {
            marker
        }
    }

    /// The prompt span shared by the input bar's real rendering.
    fn prompt_span() -> Span<'static> {
        Span::styled(
            format!("{} ", glyph::CHEVRON),
            Style::default().fg(DEFAULT_THEME.accent_primary),
        )
    }

    /// A style that cannot occur in the input (raw/default) or the prompt.
    fn marker_style() -> Style {
        Style::default().fg(DEFAULT_THEME.prompt_marker)
    }

    /// Render `line` at `width` and return the (row, col) of the marker-style
    /// cell, if present.
    fn locate_marker(line: Line<'static>, width: u16) -> Option<(u16, u16)> {
        let rows = wrapped_height(std::slice::from_ref(&line), width).max(1) as u16;
        let area = Rect::new(0, 0, width, rows);
        let mut buf = Buffer::empty(area);
        Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);

        // Compare on the foreground color rather than full `Style` equality:
        // ratatui patches the cell style with `bg(Color::Reset)` and
        // `underline_color(Color::Reset)` defaults, so the rendered style never
        // equals the marker style verbatim. The prompt-marker color is unique
        // to the marker within the input bar.
        for y in 0..rows {
            for x in 0..width {
                if buf[(x, y)].style().fg == Some(DEFAULT_THEME.prompt_marker) {
                    return Some((y, x));
                }
            }
        }
        None
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

    fn min_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        // Minimum: the wrapped content, capped at 5 rows. The bar is not
        // shrunk below its visible text unless the viewport cannot fit it.
        self.wrapped_rows(width, ctx.input).min(5)
    }

    fn desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        self.wrapped_rows(width, ctx.input)
    }

    fn max_height(&self, _ctx: &RenderContext) -> u16 {
        5
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        let width = area.width;
        if width == 0 {
            return;
        }

        // Build the wrapped paragraph, then slice out the visible vertical
        // window so the cursor stays in view.
        let paragraph =
            Paragraph::new(Self::full_line(ctx.input, width)).wrap(Wrap { trim: false });

        // Render into a full-width buffer at the paragraph's natural height,
        // then copy the visible window into `area`.
        let full_rows = wrapped_height(&[Self::full_line(ctx.input, width)], width).max(1) as u16;
        let full_area = Rect::new(0, 0, width, full_rows);
        let mut full_buf = Buffer::empty(full_area);
        paragraph.render(full_area, &mut full_buf);

        // Copy the visible vertical window. Unwritten cells stay `Reset` so
        // the input bar keeps the surrounding screen background.
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
                cell.set_style(Style::default().bg(DEFAULT_THEME.cursor));
            }
        }
    }
}

/// The info bar. Stateless — reads display state from the render context.
pub struct InfoBarWidget;

impl LiveAreaWidget for InfoBarWidget {
    as_any_methods!();
    fn size_priority(&self) -> i32 {
        90
    }

    fn min_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        1
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        // Clear the region first so a shorter frame cannot leave stale cells
        // (mirrors the transcript's clear loop; the info bar paints a
        // background band).
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)].reset();
            }
        }
        if area.width == 0 || area.height == 0 {
            return;
        }

        let band = Style::default().bg(DEFAULT_THEME.midnight);
        let gold = band
            .fg(DEFAULT_THEME.heading_primary)
            .add_modifier(Modifier::BOLD);
        let red = band.fg(DEFAULT_THEME.danger);
        let nickel = band.fg(DEFAULT_THEME.subtle);
        let chrome = band.fg(DEFAULT_THEME.chrome);
        let ivory = band.fg(DEFAULT_THEME.ivory);
        let malachite = band.fg(DEFAULT_THEME.success);
        let brass = band.fg(DEFAULT_THEME.code_border);

        let running = ctx
            .active_runs
            .values()
            .filter(|r| r.status == "running")
            .count();
        let count_style = if running > 0 { malachite } else { brass };

        let mut spans = vec![
            Span::styled("◆ ", red),
            Span::styled("GREMLINS", gold),
            Span::styled(" ◆", red),
            Span::styled("  ", band),
            Span::styled(format!("{} running", ctx.gremlin_count), count_style),
            Span::styled("  ", band),
            Span::styled("project:", nickel),
            Span::styled(" ", band),
            Span::styled(ctx.project_name.to_string(), chrome),
        ];

        if let Some(id) = ctx.following_log {
            spans.push(Span::styled("  ", band));
            spans.push(Span::styled(
                format!("following: {id}"),
                Style::default()
                    .fg(DEFAULT_THEME.ivory)
                    .bg(DEFAULT_THEME.danger)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        spans.push(Span::styled("  ", band));
        spans.push(Span::styled(glyph::COLUMN, gold));
        spans.push(Span::styled("  ", band));
        spans.push(Span::styled("Ctrl+G", malachite));
        spans.push(Span::styled(" editor  ", ivory));
        spans.push(Span::styled("/help", malachite));

        let paragraph = Paragraph::new(Line::from(spans)).style(band);
        paragraph.render(area, buf);
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

    /// Desired height: the table header + one row per run + footer.
    fn desired_height(&self, ctx: &RenderContext) -> u16 {
        let rows = ctx.active_runs.len().max(1) as u16;
        // header + rows + footer.
        rows + 2
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

    fn min_height(&self, _width: u16, ctx: &RenderContext) -> u16 {
        if ctx.blocking_overlay() {
            return 0;
        }
        // Real minimum: just the header row. The half-region cap lives in
        // `max_height`, so the minimum can never exceed it.
        1
    }

    fn desired_height(&self, _width: u16, ctx: &RenderContext) -> u16 {
        self.desired_height(ctx)
    }

    fn max_height(&self, ctx: &RenderContext) -> u16 {
        // Cap at half the content region so the transcript below stays
        // visible.
        (ctx.content_height / 2).max(1)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        let mut lines: Vec<Line<'static>> = Vec::new();

        let header_style = Style::default()
            .fg(DEFAULT_THEME.heading_primary)
            .add_modifier(Modifier::BOLD);
        lines.push(Line::from(Span::styled(
            "  ID        STATUS    STAGE       DEFINITION",
            header_style,
        )));
        // Brass streamline rule under the header.
        lines.push(motif::streamline_rule_brass(area.width));

        let mut runs: Vec<(&String, &ActiveRun)> = ctx.active_runs.iter().collect();
        runs.sort_by(|a, b| a.0.cmp(b.0));

        for (id, run) in runs {
            let (glyph, status_style) = match run.status.as_str() {
                "running" => (glyph::RUNNING, Style::default().fg(DEFAULT_THEME.success)),
                "failed" => (glyph::FAILED, Style::default().fg(DEFAULT_THEME.danger)),
                "stopped" => (glyph::STOPPED, Style::default().fg(DEFAULT_THEME.subtle)),
                _ => (glyph::DONE, Style::default().fg(DEFAULT_THEME.chrome)),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(id.clone(), Style::default().fg(DEFAULT_THEME.text)),
                Span::raw("  "),
                Span::styled(format!("{glyph} "), status_style),
                Span::styled(run.status.clone(), status_style),
                Span::raw("  "),
                Span::styled(run.stage.clone(), Style::default().fg(DEFAULT_THEME.subtle)),
                Span::raw("  "),
                Span::styled(
                    run.definition.clone(),
                    Style::default().fg(DEFAULT_THEME.subtle),
                ),
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
                .fg(DEFAULT_THEME.accent_secondary)
                .add_modifier(Modifier::BOLD),
        )));

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }
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

    fn min_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        // Fills the entire content area above the fixed bars.
        0
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        true
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) {
        let header = Line::from(vec![
            Span::styled(
                "◆ following: ",
                Style::default()
                    .fg(DEFAULT_THEME.heading_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.id.clone(),
                Style::default()
                    .fg(DEFAULT_THEME.danger)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ◆",
                Style::default()
                    .fg(DEFAULT_THEME.heading_primary)
                    .add_modifier(Modifier::BOLD),
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
                Style::default().fg(DEFAULT_THEME.text),
            )));
        }

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }
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

    fn min_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        // Fills the entire content area above the fixed bars.
        0
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        true
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) {
        let status = if self.connecting {
            "connecting…"
        } else if self.connected {
            "connected"
        } else {
            "ended"
        };
        let status_style = if self.connecting {
            Style::default().fg(DEFAULT_THEME.accent_secondary)
        } else if self.connected {
            Style::default().fg(DEFAULT_THEME.success)
        } else {
            Style::default().fg(DEFAULT_THEME.danger)
        };

        let header = Line::from(vec![
            Span::styled(
                "◆ debug: ",
                Style::default()
                    .fg(DEFAULT_THEME.heading_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.id.clone(),
                Style::default()
                    .fg(DEFAULT_THEME.danger)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ◆",
                Style::default()
                    .fg(DEFAULT_THEME.heading_primary)
                    .add_modifier(Modifier::BOLD),
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
                Style::default().fg(DEFAULT_THEME.text),
            )));
        }

        // Debug prompt line.
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "debug» ",
                Style::default().fg(DEFAULT_THEME.heading_primary),
            ),
            Span::styled(self.input.clone(), Style::default().fg(DEFAULT_THEME.text)),
        ]));

        if !lines.is_empty() {
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            paragraph.render(area, buf);
        }
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
        assert_eq!(lines[0].to_string(), "▍» bold prompt");
    }

    #[test]
    fn clear_current_turn_truncates_to_turn_start() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_transcript_line("prior".to_string());
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("response");
        transcript.clear_current_turn();
        assert!(!transcript.has_current_turn());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["prior"]);
    }

    #[test]
    fn clear_current_turn_removes_rule_separator() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_transcript_line("prior".to_string());
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("response");
        transcript.clear_current_turn();
        assert!(!transcript.has_current_turn());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["prior"]);
        assert!(!text
            .iter()
            .any(|l| l.chars().all(|c| c == '─') && !l.is_empty()));
    }

    #[test]
    fn finish_turn_unpins_and_keeps_content_split_across_widgets() {
        let mut transcript = TranscriptWidget::new();
        let mut reasoning = ReasoningWidget::new();
        let mut tools = ToolEventsWidget::new();

        transcript.submit_prompt("hi".to_string());
        reasoning.push_reasoning_text("thinking\n");
        tools.push_tool("Done");
        transcript.push_response("**answer**");
        transcript.finish_turn();

        assert!(!transcript.has_current_turn());
        let transcript_text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(transcript_text, vec!["▍» hi", "answer"]);
        assert!(!transcript_text.contains(&"thinking".to_string()));
        assert!(!transcript_text.contains(&"Done".to_string()));

        let reasoning_text: Vec<String> = reasoning
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(reasoning_text, vec!["◈ thinking"]);

        let tool_text: Vec<String> = tools
            .lines
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(tool_text, vec!["◆ Done"]);
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
        assert!(text.contains(&"▍» hi".to_string()));
    }

    #[test]
    fn take_overflow_drains_in_progress_turn() {
        let mut transcript = TranscriptWidget::new();
        // Completed history.
        transcript.push_transcript_line("prior".to_string());
        // In-progress turn: pinned at the prompt line.
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("resp");
        LiveAreaWidget::finalize(&mut transcript, 80);

        // Only 1 row available. The FIFO drains its head unconditionally,
        // so the prompt is now scrollback and the tail "resp" stays on
        // screen.
        let overflow = transcript.take_overflow(1, 80);
        let overflow_text: Vec<String> = overflow.iter().map(|l| l.to_string()).collect();
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            overflow_text,
            vec![
                "prior".to_string(),
                motif::glint_rule(79).to_string(),
                "▍» hi".to_string()
            ]
        );
        assert_eq!(text, vec!["resp"]);
    }

    #[test]
    fn take_overflow_drains_wrapped_head_lines_unconditionally() {
        let mut transcript = TranscriptWidget::new();
        // A long prior line that wraps to many rows.
        transcript.push_transcript_line("abcdefghijklmnopqrstuvwxyz".to_string());
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("resp");
        LiveAreaWidget::finalize(&mut transcript, 10);

        // Width 10: the prior line wraps to 3 rows. With 1 available row,
        // the wrapped-aware overflow drain rolls through the head (long
        // prior, separator, prompt) until only one wrapped row remains: the
        // response tail.
        let overflow = transcript.take_overflow(1, 10);
        let overflow_text: Vec<String> = overflow.iter().map(|l| l.to_string()).collect();
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            overflow_text,
            vec![
                "abcdefghijklmnopqrstuvwxyz".to_string(),
                motif::glint_rule(9).to_string(),
                "▍» hi".to_string()
            ]
        );
        assert_eq!(text, vec!["resp"]);
    }

    #[test]
    fn clear_current_turn_still_truncates_to_recorded_start_after_overflow() {
        let mut transcript = TranscriptWidget::new();

        // A completed turn remains in the FIFO before the current one.
        transcript.submit_prompt("first".to_string());
        transcript.push_response("first response");
        transcript.finish_turn();

        transcript.push_transcript_line("completed system note".to_string());

        // The current turn starts after that completed content.
        transcript.submit_prompt("current".to_string());
        transcript.push_response("current response");
        LiveAreaWidget::finalize(&mut transcript, 80);

        // Drain the head far enough that the turn start index shifts but the
        // pre-turn completed content stays in the FIFO.
        let overflow = transcript.take_overflow(5, 80);
        assert!(!overflow.is_empty());

        transcript.clear_current_turn();

        assert!(!transcript.has_current_turn());
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(!text.contains(&"▍» current".to_string()));
        assert!(
            text.contains(&"completed system note".to_string()),
            "completed content before the turn start must survive: {text:?}"
        );
    }

    #[test]
    fn finalize_rebuilds_dirty_response_before_measuring() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("short");
        // Reconcile to a much longer response, leaving the region dirty.
        transcript.reconcile_response("a much longer response body that wraps");

        // Finalize rebuilds the dirty response into the FIFO before
        // measurement/overflow.
        LiveAreaWidget::finalize(&mut transcript, 10);
        let _ = transcript.take_overflow(2, 10);
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("longer response")),
            "dirty response must be rebuilt before overflow split: {text:?}"
        );
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
    fn reasoning_partial_is_visible_in_reasoning_widget() {
        let mut reasoning = ReasoningWidget::new();
        reasoning.push_reasoning_text("thinking");
        // The partial reasoning line must be visible before any newline.
        let text: Vec<String> = reasoning
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.contains(&"◈ thinking".to_string()), "{text:?}");
    }

    #[test]
    fn tool_line_after_response_is_preserved_in_separate_widget() {
        let mut transcript = TranscriptWidget::new();
        let mut tools = ToolEventsWidget::new();

        transcript.submit_prompt("hi".to_string());
        transcript.push_response("streamed");
        // A tool call arriving after response streaming began is stored in
        // the tool events widget and must not be erased when the response
        // region is rebuilt.
        tools.push_tool("grep");
        transcript.reconcile_response("streamed complete");
        transcript.finish_turn();

        let transcript_text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            transcript_text.contains(&"streamed complete".to_string()),
            "{transcript_text:?}"
        );
        assert!(!transcript_text.contains(&"grep".to_string()));

        let tool_text: Vec<String> = tools
            .lines
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(tool_text.contains(&"◆ grep".to_string()), "{tool_text:?}");
    }

    #[test]
    fn reasoning_widget_take_overflow_trims_wrapped_lines_to_cap() {
        let mut reasoning = ReasoningWidget::new();
        reasoning.push_reasoning_text("abcdefghijklmnopqrstuvwxyz\n"); // 26 chars → 3 rows at width 10
        reasoning.flush_reasoning();

        let overflow = reasoning.take_overflow(2, 10);
        assert!(overflow.is_empty());

        // The single long line wraps to 3 rows and cannot fit in 2 visible
        // rows, so the live tail discards it entirely.
        assert!(reasoning.rendered_lines().is_empty());
        assert_eq!(reasoning.desired_rows(10), 0);
    }

    #[test]
    fn tool_events_widget_trims_to_visible_tail_discarding_head() {
        let mut tools = ToolEventsWidget::new();
        for i in 0..6 {
            tools.push_tool(&format!("tool-{i}"));
        }

        let overflow = tools.take_overflow(2, 80);
        assert!(overflow.is_empty());

        let text: Vec<String> = tools
            .lines
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(text, vec!["◆ tool-4", "◆ tool-5"]);
    }

    #[test]
    fn live_tail_widgets_collapse_to_zero_height_when_empty() {
        let reasoning = ReasoningWidget::new();
        let tools = ToolEventsWidget::new();
        assert_eq!(reasoning.desired_rows(80), 0);
        assert_eq!(reasoning.min_height(80, &input_ctx("", 0)), 0);
        assert!(reasoning.flexible(&input_ctx("", 0)));
        assert_eq!(tools.min_height(80, &input_ctx("", 0)), 0);
        assert!(tools.flexible(&input_ctx("", 0)));
    }

    #[test]
    fn live_tail_widgets_report_uncapped_desired_height() {
        let mut reasoning = ReasoningWidget::new();
        for i in 0..10 {
            reasoning.push_reasoning_text(&format!("reasoning line {i}\n"));
        }
        // 10 lines, uncapped: desired height is 10, not 5.
        assert_eq!(reasoning.desired_rows(80), 10);
        assert_eq!(reasoning.min_height(80, &input_ctx("", 0)), 5);
        assert!(reasoning.flexible(&input_ctx("", 0)));
        assert_eq!(reasoning.max_height(&input_ctx("", 0)), u16::MAX);

        let mut tools = ToolEventsWidget::new();
        for i in 0..10 {
            tools.push_tool(&format!("tool-{i}"));
        }
        assert_eq!(tools.desired_rows(80), 10);
        assert_eq!(tools.min_height(80, &input_ctx("", 0)), 5);
        assert!(tools.flexible(&input_ctx("", 0)));
        assert_eq!(tools.max_height(&input_ctx("", 0)), u16::MAX);
    }

    #[test]
    fn live_tail_widgets_floor_is_wrapped_rows_capped_at_5() {
        // A single long line with a `◈ `/`◆ ` prefix: the prefix occupies
        // row 1 and the long word wraps onto the following rows, so a
        // 26-char word reports a floor of 4 (prefix row + 3 word rows).
        let mut reasoning = ReasoningWidget::new();
        reasoning.push_reasoning_text("abcdefghijklmnopqrstuvwxyz");
        assert_eq!(reasoning.desired_rows(10), 4);
        assert_eq!(reasoning.min_height(10, &input_ctx("", 0)), 4);

        let mut tools = ToolEventsWidget::new();
        tools.push_tool("abcdefghijklmnopqrstuvwxyz");
        assert_eq!(tools.desired_rows(10), 4);
        assert_eq!(tools.min_height(10, &input_ctx("", 0)), 4);

        // A long line wrapping past 5 rows caps the floor at 5.
        let mut reasoning = ReasoningWidget::new();
        reasoning.push_reasoning_text("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz");
        assert_eq!(reasoning.desired_rows(10), 7);
        assert_eq!(reasoning.min_height(10, &input_ctx("", 0)), 5);

        let mut tools = ToolEventsWidget::new();
        tools.push_tool("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz");
        assert_eq!(tools.desired_rows(10), 7);
        assert_eq!(tools.min_height(10, &input_ctx("", 0)), 5);
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
        // width 5, prompt 2 cells. "abcd" is a single word that word-wraps as
        // "> " / "abcd", so every character lands on row 1.
        assert_eq!(InputBarWidget::cursor_position(1, "abcd", 5), (1, 1));
        // Cursor after "abc" (byte 3) but more text follows → before 'd'.
        assert_eq!(InputBarWidget::cursor_position(3, "abcd", 5), (1, 3));
        // "abcd" → cursor at end, just past 'd'.
        assert_eq!(InputBarWidget::cursor_position(4, "abcd", 5), (1, 4));
    }

    #[test]
    fn cursor_position_word_wraps_at_space_boundary() {
        // "hello world" at width 7 wraps as "> hello" / "world" (word wrap),
        // not "> hello" / " world" (linear wrap): the space at the boundary
        // is dropped. The cursor at end of input lands immediately after
        // "world".
        assert_eq!(
            InputBarWidget::cursor_position(11, "hello world", 7),
            (1, 5)
        );
        // At width 5 it wraps to three rows; the cursor parks at the end of
        // the final "world" row.
        assert_eq!(
            InputBarWidget::cursor_position(11, "hello world", 5),
            (2, 4)
        );
    }

    #[test]
    fn cursor_position_on_wrap_space_parks_at_word_end() {
        // Cursor on the space that word-wrap drops (byte 5 in "hello world"
        // at width 7). The space occupies no cell, so the cursor sits at the
        // end of the preceding word "hello", which fills row 0 exactly.
        assert_eq!(InputBarWidget::cursor_position(5, "hello world", 7), (0, 6));
    }

    #[test]
    fn cursor_position_mid_word_after_wrap() {
        // Cursor inside the word that follows a wrap boundary (byte 8 is the
        // 'r' in "world"), which lands on row 1 col 2.
        assert_eq!(InputBarWidget::cursor_position(8, "hello world", 7), (1, 2));
    }

    #[test]
    fn cursor_position_ignores_marker_chars_in_input() {
        // The cursor is located by style, not symbol, so an input that already
        // contains the former marker characters ("😀", U+E000) cannot collide.
        assert_eq!(InputBarWidget::cursor_position(1, "a😀b", 10), (0, 3));
        assert_eq!(InputBarWidget::cursor_position(1, "a\u{E000}b", 10), (0, 3));
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
        assert_eq!(buf[(4, 0)].style().bg, Some(DEFAULT_THEME.cursor));
        assert_ne!(buf[(0, 0)].style().bg, Some(DEFAULT_THEME.cursor));
    }
}
