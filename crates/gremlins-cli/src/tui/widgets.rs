//! LiveArea rendering widgets.
//!
//! The LiveArea screen is composed of a transcript widget (the single
//! styled FIFO with embedded reasoning and live-tool slots), an input bar,
//! an info bar, and the full-screen debug view widget. Each
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
use std::collections::{BTreeMap, HashMap, HashSet};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::tui::app::ActiveRun;
use crate::tui::buffer::{line_wrapped_height, tail_window, wrapped_height, LineBuffer};
use crate::tui::markdown;
use crate::tui::motif::{self, glyph};
use crate::tui::theme::DEFAULT_THEME;
/// Shared read-only state handed to each widget's [`LiveAreaWidget::render`]
/// and [`LiveAreaWidget::min_height`].
///
/// Stateless widgets (input bar, info bar) read their content from here
/// rather than owning buffers of their own.
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
    /// The daemon-derived run table (source of truth for the runs view).
    pub active_runs: &'a HashMap<String, ActiveRun>,
    /// Highlighted gremlin id on the `/watch` screen.
    pub watch_selection: Option<&'a str>,
    /// Gremlin whose inline watch stream pane is open, if any.
    pub watch_following: Option<&'a str>,
    /// Error from the current watch follow, if the dedicated socket died.
    pub watch_follow_error: Option<&'a str>,
    /// Inline completion preview suffix (paint-only, never submitted).
    pub completion_preview: Option<&'a str>,
}

/// A single LiveArea rendering region.
///
/// Widgets may be nested. A container's
/// [`min_height`](Self::min_height), [`desired_height`](Self::desired_height),
/// and [`max_height`](Self::max_height) are the sums of its children's
/// answers; width travels down as the already-reduced inner width, while
/// height travels up. Layout is driven by a shared recursive engine over a
/// slice of `&mut dyn LiveAreaWidget`.
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
    /// transcript rebuilds a dirty response region into its FIFO and
    /// finalizes its embedded streaming children. Defaults to a no-op.
    fn finalize(&mut self, _width: u16) {}

    /// Render this widget into `area`. This is paint-only: overflow is
    /// reported separately via [`take_overflow`](Self::take_overflow).
    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext);

    /// Drain and return any lines that do not fit in `available_rows` at
    /// the given width. The widget decides what happens to its head.
    fn take_overflow(
        &mut self,
        _available_rows: u16,
        _width: u16,
        _ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        Vec::new()
    }

    /// Short, stable label for telemetry. Concrete widgets that benefit
    /// from a stable name override this; the fallback is the full type
    /// path, which is unique but verbose.
    #[cfg(feature = "telemetry")]
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

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
    /// the integer gives the shared engine a deterministic ordering if more
    /// than one widget ever does.
    fn overflow_priority(&self) -> i32 {
        0
    }

    /// The maximum height this widget may be allocated, as a first-class
    /// layout constraint. Defaults to `u16::MAX` (uncapped). The input bar
    /// caps at 5 rows, and a streaming widget caps at its title rows plus
    /// the stream row cap.
    fn max_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        u16::MAX
    }
}

/// The transcript: a single styled FIFO plus the current turn's draft state.
///
/// The FIFO is the only overflow producer in the widget model. The
/// ephemeral reasoning and live-tool bands are nested [`StreamingWidget`]
/// children: the transcript re-runs the shared recursive layout engine over
/// those children inside the height the outer engine allocated to the
/// transcript, and paints top-to-bottom pre-reasoning FIFO head, reasoning,
/// FIFO tool-call events, live tool output, then the streaming-response
/// tail. The FIFO head drains into terminal scrollback when the combined
/// content exceeds the region's height.
pub struct TranscriptWidget {
    /// The single styled FIFO — the only transcript content that overflows.
    scrollable: LineBuffer,
    /// The current turn's ephemeral reasoning slot, rendered between the
    /// pre-reasoning transcript head and the FIFO tool-call-event band.
    reasoning: StreamingWidget,
    /// The current turn's ephemeral live tool-output streams, keyed by
    /// tool-call id. A stream is created lazily on its first output chunk and
    /// removed when its tool commits.
    tools: BTreeMap<String, StreamingWidget>,
    /// Formatted tool call (`format_tool_call` output) for each live stream.
    tool_calls_by_id: HashMap<String, String>,
    /// Tool-call ids already committed to the FIFO, guaranteeing exactly-once
    /// representation even if a duplicate completion event arrives.
    committed_tool_ids: HashSet<String>,
    /// Formatted tool calls from `TurnComplete` still awaiting their result.
    /// ToolResult events remove entries as they commit; anything left when
    /// the turn finishes is reconciled so a dropped completion event cannot
    /// silently lose a tool's permanent entry.
    pending_tool_calls: Vec<String>,
    /// Committed formatted tool calls that have not yet been matched against
    /// a `TurnComplete` entry. Counts make duplicate identical calls safe:
    /// when a later `TurnComplete` arrives it consumes these before adding a
    /// new pending entry.
    committed_unmatched: HashMap<String, usize>,
    /// Raw response accumulator (for reconciliation).
    response_accumulator: String,
    /// FIFO index where the current turn starts (the pinned boundary).
    turn_start: Option<usize>,
    /// FIFO index where the response region starts.
    response_start: Option<usize>,
    /// FIFO index where the rendered prompt ends (one past the last prompt
    /// line). When the response boundary has been clamped by overflow this
    /// stays behind as a lower bound so tool lines can never be inserted
    /// into the prompt region.
    prompt_end: usize,
    /// FIFO index of the turn separator rule (regenerated at `finalize`
    /// width so the glint rule spans the full content width).
    separator_index: Option<usize>,
    /// Last-known content width (from `finalize`), used to size rules when
    /// the response region is rebuilt outside a finalize pass.
    last_width: u16,
    /// Whether the response region needs re-rendering into the FIFO.
    response_dirty: bool,
    /// Number of rendered response lines already committed to terminal
    /// scrollback. When the response region is rebuilt, these leading lines
    /// are skipped so drained content is never emitted twice.
    response_drained_lines: usize,
    /// Cached wrapped desired height `(width, height)` for the FIFO,
    /// invalidated whenever the FIFO, accumulator, or width changes.
    desired_cache: Cell<Option<(u16, u16)>>,
}

impl TranscriptWidget {
    /// Shared row cap for the live streaming bands and their committed
    /// tails: reasoning band height, live tool slot height, and committed
    /// reasoning/tool tail length.
    const STREAM_MAX_ROWS: u16 = 5;

    pub fn new() -> Self {
        Self {
            scrollable: LineBuffer::new(),
            reasoning: StreamingWidget::reasoning(Self::STREAM_MAX_ROWS),
            tools: BTreeMap::new(),
            tool_calls_by_id: HashMap::new(),
            committed_tool_ids: HashSet::new(),
            pending_tool_calls: Vec::new(),
            committed_unmatched: HashMap::new(),
            response_accumulator: String::new(),
            turn_start: None,
            response_start: None,
            prompt_end: 0,
            separator_index: None,
            last_width: 40,
            response_dirty: false,
            response_drained_lines: 0,
            desired_cache: Cell::new(None),
        }
    }

    /// Invalidate the cached desired height after any content mutation.
    fn invalidate_desired_cache(&self) {
        self.desired_cache.set(None);
    }

    /// Append reasoning text (may be a partial chunk) to the current turn's
    /// ephemeral reasoning slot.
    pub fn push_reasoning_text(&mut self, text: &str) {
        self.reasoning.push_text(text);
    }

    /// Flush the reasoning slot's trailing partial into a completed line.
    #[allow(dead_code)]
    pub fn flush_reasoning(&mut self) {
        self.reasoning.flush();
    }

    /// Append live tool output (may be a partial chunk) to the stream for
    /// the given tool-call id, creating the stream lazily on first use.
    pub fn push_tool_output_chunk(&mut self, id: &str, call: &str, text: &str) {
        if id.is_empty() && call.is_empty() {
            return;
        }
        let stream = self
            .tools
            .entry(id.to_string())
            .or_insert_with(|| StreamingWidget::tool(Self::STREAM_MAX_ROWS));
        if !call.is_empty() {
            stream.set_title(call);
        }
        stream.push_text(text);
        self.tool_calls_by_id
            .insert(id.to_string(), call.to_string());
    }

    /// Commit a finished tool call into the transcript's permanent tool
    /// segment. Streaming tools with a live slot commit their title plus the
    /// retained 5-row tail; every other tool commits a single `◆` event line.
    /// Each tool commits at most once, even if a completion event is repeated.
    pub fn commit_tool_result(&mut self, id: &str, call: &str) {
        if self.turn_start.is_none() || (id.is_empty() && call.is_empty()) {
            return;
        }
        if self.committed_tool_ids.contains(id) {
            return;
        }
        if !id.is_empty() {
            self.committed_tool_ids.insert(id.to_string());
        }
        self.note_committed_call(call);

        if let Some(stream) = self.tools.remove(id) {
            self.tool_calls_by_id.remove(id);
            self.commit_tool_stream_block(call, stream);
        } else if !call.is_empty() {
            self.insert_tool_lines(vec![tool_line(call)]);
        }
    }

    /// Seed the current turn's tool-call list for end-of-turn reconciliation.
    /// Read/Edit calls stay filtered exactly as they were before. Later
    /// `TurnComplete` events merge into the existing list, consuming any
    /// already-committed result first so exactly-once holds regardless of
    /// whether the result or the completion event arrived first.
    pub fn reconcile_tool_calls(&mut self, calls: &[String]) {
        if self.turn_start.is_none() {
            return;
        }
        for call in calls {
            if call.starts_with("Read") || call.starts_with("Edit") {
                continue;
            }
            if let Some(count) = self.committed_unmatched.get_mut(call) {
                if *count > 0 {
                    *count -= 1;
                    continue;
                }
            }
            self.pending_tool_calls.push(call.clone());
        }
        self.committed_unmatched.retain(|_, count| *count > 0);
    }

    /// Clear every live tool stream at a turn boundary.
    pub fn clear_tool_output(&mut self) {
        self.clear_live_tools();
    }

    fn clear_live_tools(&mut self) {
        for stream in self.tools.values_mut() {
            stream.clear();
        }
        self.tools.clear();
        self.tool_calls_by_id.clear();
    }

    fn clear_tool_state(&mut self) {
        self.clear_live_tools();
        self.committed_tool_ids.clear();
        self.pending_tool_calls.clear();
        self.committed_unmatched.clear();
    }

    /// Account for a committed tool result in the reconciliation multiset.
    ///
    /// If a `TurnComplete` already seeded this formatted call, consume that
    /// pending entry. Otherwise remember it so a later `TurnComplete` does
    /// not re-add an already-committed tool as pending.
    fn note_committed_call(&mut self, call: &str) {
        if let Some(pos) = self.pending_tool_calls.iter().position(|c| c == call) {
            self.pending_tool_calls.remove(pos);
        } else if !call.is_empty() {
            *self
                .committed_unmatched
                .entry(call.to_string())
                .or_insert(0) += 1;
        }
    }

    /// Commit any tool calls still pending when the turn finishes. ToolResult
    /// events normally commit each call as it completes; this safety net keeps
    /// a bounded-event-channel drop from silently losing a permanent entry.
    fn reconcile_pending_tools(&mut self) {
        if self.turn_start.is_none() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_tool_calls);
        for call in pending {
            let stream_id = self.tools.iter().find_map(|(stream_id, _)| {
                self.tool_calls_by_id
                    .get(stream_id)
                    .is_some_and(|stream_call| stream_call == &call)
                    .then_some(stream_id.clone())
            });
            if let Some(stream_id) = stream_id {
                let stream = self.tools.remove(&stream_id).unwrap();
                self.committed_tool_ids.insert(stream_id.clone());
                self.tool_calls_by_id.remove(&stream_id);
                self.commit_tool_stream_block(&call, stream);
            } else if !call.is_empty() {
                self.insert_tool_lines(vec![tool_line(&call)]);
            }
        }
    }

    /// Build a committed stream block: flush the trailing partial, prepend
    /// an optional title row, then append the retained tail capped at the
    /// shared stream row count. Tools invoke this with their `◆` title;
    /// reasoning invokes it without one.
    fn commit_stream_block(
        &mut self,
        title: Option<&str>,
        mut stream: StreamingWidget,
    ) -> Vec<Line<'static>> {
        stream.flush();
        let mut block: Vec<Line<'static>> = Vec::new();
        if let Some(title) = title.filter(|title| !title.is_empty()) {
            block.push(tool_line(title));
        }
        let width = self.last_width.saturating_sub(1);
        let rendered = stream.commit_lines();
        if !rendered.is_empty() {
            let window = tail_window(&rendered, width, Self::STREAM_MAX_ROWS as usize);
            block.extend_from_slice(&rendered[window.start..]);
        }
        block
    }

    /// Commit a finished tool stream as title + retained tail above the
    /// streaming response region.
    fn commit_tool_stream_block(&mut self, call: &str, stream: StreamingWidget) {
        let block = self.commit_stream_block(Some(call), stream);
        self.insert_tool_lines(block);
    }

    /// Commit the reasoning stream's retained tail into the transcript just
    /// after the prompt and before any committed tool blocks/events.
    fn commit_reasoning(&mut self) {
        if self.turn_start.is_none() {
            return;
        }
        let stream = std::mem::replace(
            &mut self.reasoning,
            StreamingWidget::reasoning(Self::STREAM_MAX_ROWS),
        );
        let block = self.commit_stream_block(None, stream);
        self.insert_reasoning_lines(block);
    }

    /// Insert a contiguous run of styled lines into the active turn directly
    /// above the streaming response region, advancing the response boundary.
    fn insert_tool_lines(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() || self.turn_start.is_none() {
            return;
        }
        self.invalidate_desired_cache();
        let idx = self
            .response_start
            .unwrap_or(self.scrollable.len())
            .max(self.prompt_end);
        let mut inserted = 0usize;
        for line in lines {
            self.scrollable.insert(idx + inserted, line);
            inserted += 1;
        }
        self.response_start = Some(idx + inserted);
    }

    /// Insert a contiguous run of styled reasoning lines directly after the
    /// prompt, before the tool-event band. This advances both the prompt and
    /// response boundaries so later tool commits still land above the
    /// streaming response.
    fn insert_reasoning_lines(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() || self.turn_start.is_none() {
            return;
        }
        self.invalidate_desired_cache();
        let idx = self.prompt_end.min(self.scrollable.len());
        let mut inserted = 0usize;
        for line in lines {
            self.scrollable.insert(idx + inserted, line);
            inserted += 1;
        }
        self.prompt_end += inserted;
        if let Some(response_start) = &mut self.response_start {
            *response_start += inserted;
        }
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
        self.push_styled_block(vec![styled]);
    }

    /// Append a block of pre-styled lines with the same insertion logic as
    /// `push_styled_line`, but applying the blank-line padding exactly once
    /// before the first line of the block. Lines within the block are
    /// appended contiguously (no blank lines between them), and the blank
    /// separator inserted during an active turn is placed after the block.
    pub fn push_styled_block(&mut self, block: Vec<Line<'static>>) {
        self.invalidate_desired_cache();
        if block.is_empty() {
            return;
        }
        match self.turn_start {
            Some(t) => {
                let mut inserted = 0usize;
                for styled in block {
                    self.scrollable.insert(t + inserted, styled);
                    inserted += 1;
                }
                if t > 0 {
                    self.scrollable.insert(t + inserted, Line::from(""));
                    inserted += 1;
                }
                self.turn_start = Some(t + inserted);
                self.prompt_end += inserted;
                if let Some(r) = &mut self.response_start {
                    *r += inserted;
                }
            }
            None => {
                if !self.scrollable.is_empty() {
                    self.scrollable.push(Line::from(""));
                    self.scrollable.push(Line::from(""));
                }
                for styled in block {
                    self.scrollable.push(styled);
                }
            }
        }
    }

    /// Insert a tool-call line into the active turn, directly above the
    /// streaming response region. Tool lines stack contiguously (no blank-line
    /// padding) and must survive response rebuilds and reconciliations, so
    /// they are inserted at the response-boundary marker, clamped to be no
    /// earlier than the end of the rendered prompt, and that marker is
    /// advanced past the inserted line.
    ///
    /// Outside a turn (degenerate — tool calls only arrive during a turn) the
    /// line is appended as ordinary styled scrollback content.
    /// Insert a single bare tool-call line into the active turn.
    #[cfg(test)]
    pub fn push_tool_line(&mut self, name: &str) {
        let line = tool_line(name);
        self.invalidate_desired_cache();
        match self.turn_start {
            Some(_) => self.insert_tool_lines(vec![line]),
            None => {
                self.push_styled_line(line);
            }
        }
    }

    /// Append a block of system messages with the default body text style,
    /// forwarding to [`push_styled_block`](Self::push_styled_block).
    pub fn push_transcript_block(&mut self, block: Vec<String>) {
        let styled = block
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(DEFAULT_THEME.text))))
            .collect();
        self.push_styled_block(styled);
    }

    /// Start a new turn with the given prompt: push a separator rule and the
    /// markdown-rendered prompt, then pin the turn start. Any reasoning from
    /// the previous turn is cleared before the new turn begins.
    pub fn submit_prompt(&mut self, prompt: String) {
        self.invalidate_desired_cache();
        self.reasoning.clear();
        self.clear_tool_state();
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
        self.prompt_end = self.scrollable.len();
        self.separator_index = sep_index;
        self.response_start = None;
        self.response_accumulator.clear();
        self.response_dirty = false;
        self.response_drained_lines = 0;
    }

    /// Append streaming response text.
    pub fn push_response(&mut self, text: &str) {
        self.invalidate_desired_cache();
        if self.response_start.is_none() {
            self.response_start = Some(self.scrollable.len());
        }
        self.response_accumulator.push_str(text);
        // Keep the drained-line watermark: appended text re-renders the full
        // accumulator, but the lines already committed to scrollback stay
        // committed and must not be emitted again.
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
        self.response_drained_lines = 0;
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
        // Reconcile rewrites the accumulated text, so the drained-line
        // watermark no longer maps to the repaired accumulator.
        self.response_drained_lines = 0;
    }

    /// Whether a current turn is active (pinned).
    pub fn has_current_turn(&self) -> bool {
        self.turn_start.is_some()
    }

    /// Unpin the current turn and clear its draft state. The retained
    /// reasoning tail is committed to the FIFO before tool reconciliation,
    /// then the rest of the content is already in the FIFO.
    pub fn finish_turn(&mut self) {
        self.commit_reasoning();
        self.reconcile_pending_tools();
        self.rebuild_response();
        self.invalidate_desired_cache();
        self.reasoning.clear();
        self.clear_tool_state();
        self.turn_start = None;
        self.response_start = None;
        self.prompt_end = 0;
        self.separator_index = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.response_drained_lines = 0;
    }

    /// Discard the current turn: truncate the FIFO back to the turn start
    /// and clear the draft state.
    pub fn clear_current_turn(&mut self) {
        self.invalidate_desired_cache();
        self.reasoning.clear();
        self.clear_tool_state();
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
        self.prompt_end = 0;
        self.separator_index = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.response_drained_lines = 0;
    }

    /// Clear the scrollable FIFO.
    pub fn clear_scrollable(&mut self) {
        self.invalidate_desired_cache();
        self.reasoning.clear();
        self.clear_tool_state();
        self.scrollable.clear();
        self.turn_start = None;
        self.response_start = None;
        self.prompt_end = 0;
        self.separator_index = None;
        self.response_dirty = false;
        self.response_accumulator.clear();
        self.response_drained_lines = 0;
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
        // Re-render the full accumulator so appended chunks merge into the
        // retained response instead of replacing it, but skip the leading
        // lines already committed to scrollback by overflow.
        let lines = markdown::render(
            &self.response_accumulator,
            Style::default().fg(DEFAULT_THEME.text),
            self.last_width.saturating_sub(1),
        );
        for line in lines.into_iter().skip(self.response_drained_lines) {
            self.scrollable.push(line);
        }
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
    fn fifo_desired_height(&self, width: u16) -> u16 {
        if let Some((w, h)) = self.desired_cache.get() {
            if w == width {
                return h;
            }
        }
        let h = (self.scrollable.wrapped_height(width).min(u16::MAX as usize)) as u16;
        self.desired_cache.set(Some((width, h)));
        h
    }

    /// Wrapped height of the composite transcript: the FIFO plus the
    /// reasoning stream plus every live tool stream's desired height (each
    /// child caps itself at its row cap).
    fn desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        let inner = width.saturating_sub(1);
        let fifo = self.fifo_desired_height(inner);
        fifo.saturating_add(self.reasoning.desired_height(inner, ctx))
            .saturating_add(self.tools_desired_height(inner, ctx))
    }

    fn tools_desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        self.tools.values().fold(0u16, |acc, stream| {
            acc.saturating_add(stream.desired_height(width, ctx))
        })
    }

    fn tools_min_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        self.tools.values().fold(0u16, |acc, stream| {
            acc.saturating_add(stream.min_height(width, ctx))
        })
    }

    /// Re-run the recursive layout engine over the transcript's ephemeral
    /// children, constrained to the outer allocation minus the FIFO's one-row
    /// floor. Returns the reasoning height followed by each live tool height
    /// in sorted-id order.
    fn ephemeral_heights(
        &mut self,
        available_rows: u16,
        width: u16,
        ctx: &RenderContext,
    ) -> (u16, Vec<u16>) {
        let child_budget = available_rows.saturating_sub(1);
        let mut children: Vec<&mut dyn LiveAreaWidget> = Vec::with_capacity(1 + self.tools.len());
        children.push(&mut self.reasoning);
        for stream in self.tools.values_mut() {
            children.push(stream);
        }
        let layout = layout_widgets(&mut children, width, child_budget, ctx);
        (layout.heights[0], layout.heights[1..].to_vec())
    }

    /// Drain the head of the transcript FIFO that does not fit in
    /// `available_rows`. The embedded streaming bands are allocated first by
    /// the recursive layout engine, and the FIFO is drained unconditionally:
    /// an in-progress turn overflows into terminal scrollback exactly like
    /// completed history. Reasoning and tool output are never returned as
    /// overflow.
    pub fn take_overflow(
        &mut self,
        available_rows: u16,
        width: u16,
        ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        self.invalidate_desired_cache();
        let inner = width.saturating_sub(1);
        let (reasoning_rows, tool_rows) = self.ephemeral_heights(available_rows, inner, ctx);
        let tool_rows = tool_rows
            .into_iter()
            .fold(0u16, |acc, h| acc.saturating_add(h));
        let fifo_rows = available_rows
            .saturating_sub(reasoning_rows)
            .saturating_sub(tool_rows);
        let (_overflow_rows, overflow_lines): (_, Vec<_>) =
            split_overflow(&self.scrollable, fifo_rows, inner);
        if overflow_lines.is_empty() {
            return Vec::new();
        }

        let drained = overflow_lines.len();
        // Count the drained lines that belonged to the response region so a
        // later rebuild skips them instead of emitting them to scrollback a
        // second time.
        let response_start_old = self.response_start.unwrap_or(self.scrollable.len());
        self.response_drained_lines += drained.saturating_sub(response_start_old);
        self.scrollable.drain_head(drained);
        if let Some(t) = &mut self.turn_start {
            *t = t.saturating_sub(drained);
        }
        if let Some(r) = &mut self.response_start {
            *r = r.saturating_sub(drained);
        }
        self.prompt_end = self.prompt_end.saturating_sub(drained);
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
    // Render each physical line independently so a user-entered `\n` is
    // preserved as a hard break. The markdown renderer would otherwise
    // collapse a bare newline into a `SoftBreak` (a space), silently
    // flattening multi-line prompts. Inline markdown still applies per line.
    let mut out = Vec::new();
    let mut first = true;
    for raw_line in prompt.split('\n') {
        let lines = markdown::render(raw_line, text_style, rule_width);
        if lines.is_empty() {
            // Preserve blank lines as empty lines, but keep first-line
            // prompt styling when the prompt starts with a newline.
            let margin = if first { "▍" } else { " " };
            let marker = if first { "» " } else { "  " };
            out.push(Line::from(vec![
                Span::styled(margin, margin_style),
                Span::styled(marker, marker_style),
            ]));
            first = false;
            continue;
        }
        for line in lines {
            let mut spans = Vec::with_capacity(line.spans.len() + 2);
            spans.push(Span::styled(if first { "▍" } else { " " }, margin_style));
            if first {
                spans.push(Span::styled("» ", marker_style));
            } else {
                spans.push(Span::styled("  ", marker_style));
            }
            spans.extend(line.spans);
            out.push(Line::from(spans));
            first = false;
        }
    }
    out
}

/// A single reasoning line: four-space indent + nickel italic text.
fn reasoning_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
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
    tool_chrome_line(&format!("{} ", glyph::DIAMOND), name)
}

/// Chrome-prefixed live header/body line matching the transcript's tool-event
/// styling: colored prefix + nickel italic text.
fn tool_chrome_line(prefix: &str, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            prefix.to_string(),
            Style::default().fg(DEFAULT_THEME.chrome),
        ),
        Span::styled(
            text.to_string(),
            Style::default()
                .fg(DEFAULT_THEME.subtle)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// Streaming styler: reasoning lines use the four-space indent.
fn reasoning_stream_line(text: &str, _index: usize) -> Line<'static> {
    reasoning_line(text)
}

/// Streaming styler: live tool output is ANSI-stripped before it is painted.
///
/// The tool slot's body indent is applied by [`StreamingWidget`] itself (via
/// its configured `line_prefix`); this styler only styles the raw text.
fn tool_stream_line(text: &str, _index: usize) -> Line<'static> {
    Line::from(vec![Span::styled(
        strip_ansi(text),
        Style::default()
            .fg(DEFAULT_THEME.subtle)
            .add_modifier(Modifier::ITALIC),
    )])
}

/// Strip common ANSI control sequences from live tool output.
///
/// This is display-only tidying: the tool result returned to the model is
/// never passed through here. The stripper is deliberately simple — CSI,
/// OSC, and single-character escapes — and drops any trailing escape prefix
/// so an interrupted sequence cannot leak escape garbage into a rendered
/// line.
fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            let ch = text[i..].chars().next().expect("valid utf-8 boundary");
            // Keep printable characters and tab; drop carriage returns and
            // other C0/C1 control characters so progress-style output cannot
            // become a single control-laden line or skew wrap math.
            if ch == '\t' || !ch.is_control() {
                out.push(ch);
            }
            i += ch.len_utf8();
            continue;
        }
        if i + 1 >= bytes.len() {
            // Trailing escape prefix; drop it.
            break;
        }
        match bytes[i + 1] {
            b'[' => {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            b']' => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                // A bare ESC plus the following character (for example ESC )
                // or ESC c); neither carries printable payload. Advance by the
                // following character's UTF-8 width so a raw ESC before a
                // multibyte character cannot split inside that character.
                i += 1;
                if i < bytes.len() {
                    let next = text[i..].chars().next().expect("valid utf-8 boundary");
                    i += next.len_utf8();
                }
            }
        }
    }
    out
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
    // may be collapsed by a very short viewport. Keep the FIFO intact in
    // those cases.
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

/// Paint only the maintained wrapped tail of `lines` into `area`.
///
/// `visible_rows` is the tail window maintained by the widget, recomputed for
/// `area.width` by the caller. Rendering exactly that window — rather than the
/// last `area.height` rows of the full content — keeps rows that were already
/// scrolled off hidden even when the allocated area is temporarily taller than
/// the requested height.
pub(crate) fn render_wrapped_tail(
    lines: &[Line<'static>],
    area: Rect,
    buf: &mut Buffer,
    visible_rows: usize,
) {
    // Clear the region first so a shorter frame cannot leave stale cells from
    // a previous, taller live tail.
    for y in 0..area.height {
        for x in 0..area.width {
            buf[(area.x + x, area.y + y)].reset();
        }
    }

    let height = (visible_rows.min(area.height as usize)) as u16;
    if lines.is_empty() || area.width == 0 || height == 0 {
        return;
    }

    let window = tail_window(lines, area.width, height as usize);
    if window.skip == 0 {
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .render(area, buf);
        return;
    }

    // Render the tail slice into a scratch buffer tall enough to hold the
    // partial head plus the visible window, then copy the visible rows. The
    // scratch height is capped at `u16::MAX`; pathological single lines that
    // wrap past that many rows cannot be addressed by a `u16`-tall buffer,
    // and the copy loop below simply stops before the unavailable rows.
    let render_rows = (window.partial + height as usize).min(u16::MAX as usize) as u16;
    let render_area = Rect::new(0, 0, area.width, render_rows);
    let mut render_buf = Buffer::empty(render_area);
    Paragraph::new(lines[window.start..].to_vec())
        .wrap(Wrap { trim: false })
        .render(render_area, &mut render_buf);

    for y in 0..height {
        let Some(src_y) = window.partial.checked_add(y as usize) else {
            break;
        };
        if src_y > u16::MAX as usize || src_y >= render_rows as usize {
            break;
        }
        let src_y = src_y as u16;
        for x in 0..area.width {
            buf[(area.x + x, area.y + y)] = render_buf[(x, src_y)].clone();
        }
    }
}
/// Result of a recursive layout pass: allocated heights (in child order) and
/// priority-ordered overflow lines.
pub(crate) struct WidgetLayout {
    pub(crate) heights: Vec<u16>,
    pub(crate) overflow: Vec<(i32, Line<'static>)>,
}

/// The shared recursive layout engine.
///
/// This performs the measurement/allocate/drain portion of the frame over an
/// ordered slice of `&mut dyn LiveAreaWidget`:
///
/// 1. finalize every child at `width`,
/// 2. measure min/desired/max from finalized content,
/// 3. allocate viewport rows with priority-aware shrink/expand,
/// 4. drain each child's overflow, ordered by `overflow_priority`.
///
/// Painting is deliberately left to the caller only because containers such
/// as [`TranscriptWidget`] paint their children between bands that are not
/// themselves widgets. The returned `overflow` is sorted low-priority first.
pub(crate) fn layout_widgets(
    widgets: &mut [&mut dyn LiveAreaWidget],
    width: u16,
    viewport_rows: u16,
    ctx: &RenderContext,
) -> WidgetLayout {
    for w in widgets.iter_mut() {
        w.finalize(width);
    }

    let n = widgets.len();
    let mut min_heights = vec![0u16; n];
    let mut desired_heights = vec![0u16; n];
    let mut max_heights = vec![0u16; n];

    for (i, w) in widgets.iter().enumerate() {
        let h = w.min_height(width, ctx);
        let max = w.max_height(width, ctx);
        // Force the minimum into [0, max] so a widget whose minimum exceeds
        // its cap cannot drive below-minimum shrinking.
        min_heights[i] = h.min(max);
        max_heights[i] = max;
        // Clamp desired height to [min, max] so allocation honors both
        // bounds uniformly.
        desired_heights[i] = w.desired_height(width, ctx).max(h).min(max);
    }

    // Shrink pass: when the sum of desired heights exceeds the viewport,
    // shrink in ascending size_priority order (lowest first) from desired
    // down to minimum, then below minimum if still over budget. The total is
    // tracked in `u32` so uncapped desired heights cannot overflow a `u16`
    // sum before being clamped to the viewport.
    let mut heights = desired_heights.clone();
    let mut total: u32 = heights.iter().map(|&h| h as u32).sum();
    let rows = viewport_rows as u32;
    if total > rows {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| widgets[i].size_priority());
        for &i in &order {
            if total <= rows {
                break;
            }
            let excess = total - rows;
            let shrinkable = heights[i].saturating_sub(min_heights[i]) as u32;
            let shrink = shrinkable.min(excess);
            heights[i] -= shrink as u16;
            total -= shrink;
        }
        for &i in &order {
            if total <= rows {
                break;
            }
            let excess = total - rows;
            let shrink = (heights[i] as u32).min(excess);
            heights[i] -= shrink as u16;
            total -= shrink;
        }
    }

    // Expand pass: give remaining rows to the highest-priority flexible
    // widget.
    if total < rows {
        let remaining = (rows - total) as u16;
        let mut best: Option<usize> = None;
        let mut best_prio = i32::MIN;
        for (i, widget) in widgets.iter().enumerate() {
            if widget.flexible(ctx) && widget.size_priority() > best_prio {
                best_prio = widget.size_priority();
                best = Some(i);
            }
        }
        if let Some(i) = best {
            heights[i] += remaining;
        }
    }

    // Cap every allocated height at its widget's max so the expand pass
    // cannot exceed a first-class cap (e.g. the input bar's 5-row limit or a
    // streaming widget's row cap).
    for (i, height) in heights.iter_mut().enumerate() {
        *height = (*height).min(max_heights[i]);
    }

    #[cfg(feature = "telemetry")]
    {
        let segments: Vec<String> = widgets
            .iter()
            .enumerate()
            .map(|(i, w)| {
                format!(
                    "{} min={} desired={} max={} allocated={} flexible={} priority={}",
                    w.name(),
                    min_heights[i],
                    desired_heights[i],
                    max_heights[i],
                    heights[i],
                    w.flexible(ctx),
                    w.size_priority()
                )
            })
            .collect();
        log::info!("WIDGET_LAYOUT {}", segments.join("; "));
    }

    let mut overflow: Vec<(i32, Line<'static>)> = Vec::new();
    for (i, w) in widgets.iter_mut().enumerate() {
        let h = heights[i];
        let prio = w.overflow_priority();
        for line in w.take_overflow(h, width, ctx) {
            overflow.push((prio, line));
        }
    }
    overflow.sort_by_key(|(prio, _)| *prio);

    WidgetLayout { heights, overflow }
}

/// The shared streaming primitive for live tails.
///
/// A `StreamingWidget` owns a [`LineBuffer`] of completed styled lines plus a
/// trailing partial string, tail-anchors the visible portion with a
/// `scrolled` offset measured against `last_width`, and styles each logical
/// line with a caller-supplied styler (raw text + running line index →
/// styled [`Line`]). An optional owned title row may be set with
/// [`set_title`](Self::set_title); a per-line body prefix and a title-row
/// prefix are carried as configuration so callers can match the transcript's
/// tool chrome without hardcoding it in the styler. The stream trims its own
/// head to the configured row cap (lossy tail), collapses to zero when no
/// stream content is present, and never reports overflow.
pub struct StreamingWidget {
    lines: LineBuffer,
    partial: String,
    /// Number of completed lines pushed; passed to the styler as the running
    /// line index so a styler can alternate glyphs for free streaming motion.
    line_count: usize,
    /// Wrapped rows scrolled off the top of the current remaining live tail.
    /// This is measured against `last_width`, and only ever moves forward
    /// within a turn (allocation or width changes never reduce the hidden
    /// head window).
    scrolled: Cell<usize>,
    /// Width `scrolled` is expressed in. A width change re-anchors the
    /// visible tail at the same number of visible rows before updating this.
    last_width: Cell<u16>,
    styler: fn(&str, usize) -> Line<'static>,
    /// Optional title row text. `None` (or an empty title) renders no row.
    title: Option<String>,
    /// Prefix rendered before the title row (e.g. the `◆ ` tool chrome).
    title_prefix: String,
    /// Prefix rendered before every streamed body line (e.g. a four-space
    /// indent for the tool slot).
    line_prefix: String,
    row_cap: u16,
    size_priority: i32,
    /// When true, the widget keeps a sliding tail of completed lines (capped
    /// at `row_cap` logical lines) for a later permanent commit. The live
    /// viewport may still trim its own head independently, so a transiently
    /// short allocation never shrinks the commit tail.
    retain_tail: bool,
    /// Completed lines reserved for a later commit, independent of the live
    /// viewport's head drain.
    retained_tail: LineBuffer,
}

impl StreamingWidget {
    pub fn new(styler: fn(&str, usize) -> Line<'static>, row_cap: u16, size_priority: i32) -> Self {
        Self {
            lines: LineBuffer::new(),
            partial: String::new(),
            line_count: 0,
            scrolled: Cell::new(0),
            last_width: Cell::new(0),
            styler,
            title: None,
            title_prefix: String::new(),
            line_prefix: String::new(),
            row_cap,
            size_priority,
            retain_tail: false,
            retained_tail: LineBuffer::new(),
        }
    }

    /// Configure this stream to retain a sliding commit tail, independent of
    /// the live viewport allocation.
    pub fn retain_tail(mut self) -> Self {
        self.retain_tail = true;
        self
    }

    /// Configure the prefix rendered before the title row.
    pub fn with_title_prefix(mut self, prefix: &str) -> Self {
        self.title_prefix = prefix.to_string();
        self
    }

    /// Configure the prefix rendered before every streamed body line.
    pub fn with_line_prefix(mut self, prefix: &str) -> Self {
        self.line_prefix = prefix.to_string();
        self
    }

    /// Set or clear the title row. An empty title clears the row.
    ///
    /// The setter is idempotent: setting the same title is a no-op, so the
    /// TUI can set it on every output chunk without invalidating layout or
    /// repaint work.
    pub fn set_title(&mut self, title: &str) {
        let next = if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        };
        if self.title != next {
            self.title = next;
        }
    }

    /// The configured reasoning stream: four-space body indent, nickel
    /// italic text, and a retained commit tail.
    pub fn reasoning(row_cap: u16) -> Self {
        Self::new(reasoning_stream_line, row_cap, 20).retain_tail()
    }

    /// The configured live tool-output stream: four-space body indent, no
    /// bullet glyph, a `◆ `-prefixed title row, and a retained commit tail.
    /// Text is ANSI-stripped.
    pub fn tool(row_cap: u16) -> Self {
        Self::new(tool_stream_line, row_cap, 10)
            .with_title_prefix(&format!("{} ", glyph::DIAMOND))
            .with_line_prefix("    ")
            .retain_tail()
    }

    /// Style one logical line, applying the configured body prefix.
    fn style_line(&self, text: &str, index: usize) -> Line<'static> {
        let styled = (self.styler)(text, index);
        if self.line_prefix.is_empty() {
            return styled;
        }
        let mut spans = Vec::with_capacity(styled.spans.len() + 1);
        spans.push(Span::raw(self.line_prefix.clone()));
        spans.extend(styled.spans);
        Line::from(spans)
    }

    /// Append text (may be a partial chunk). Completed lines are pushed into
    /// the buffer; the trailing partial is kept as a live line.
    pub fn push_text(&mut self, text: &str) {
        self.partial.push_str(text);
        while let Some(newline_pos) = self.partial.find('\n') {
            // `drain` moves the remainder once per completed line and yields
            // the line (including the newline) in a single allocation, rather
            // than reallocating the trailing partial on every line.
            let mut line: String = self.partial.drain(..=newline_pos).collect();
            line.pop(); // drop the newline `drain` included
            let idx = self.line_count;
            let styled = self.style_line(&line, idx);
            if self.retain_tail {
                self.retained_tail.push(styled.clone());
                self.trim_retained_tail();
            }
            self.lines.push(styled);
            self.line_count += 1;
        }
    }

    /// Flush the trailing partial (if any) into a completed line.
    pub fn flush(&mut self) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            let idx = self.line_count;
            let styled = self.style_line(&line, idx);
            if self.retain_tail {
                self.retained_tail.push(styled.clone());
                self.trim_retained_tail();
            }
            self.lines.push(styled);
            self.line_count += 1;
        }
    }

    /// Keep only the most recent `row_cap` logical lines in the commit tail.
    /// A line wraps to at least one row, so the last `row_cap` logical lines
    /// always contain the last `row_cap` wrapped rows.
    fn trim_retained_tail(&mut self) {
        let cap = self.row_cap as usize;
        if self.retained_tail.len() > cap {
            self.retained_tail
                .drain_head(self.retained_tail.len() - cap);
        }
    }

    /// Reset the live region for a new turn.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.partial.clear();
        self.retained_tail.clear();
        self.line_count = 0;
        self.scrolled.set(0);
        self.last_width.set(0);
        self.title = None;
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.partial.is_empty()
    }

    /// Re-anchor `scrolled` when the wrap width changes.
    fn normalize_scroll(&self, width: u16) {
        if width == 0 {
            return;
        }
        let last = self.last_width.get();
        if last == 0 {
            self.last_width.set(width);
            return;
        }
        if last == width {
            return;
        }

        let scrolled = self.scrolled.get();
        if scrolled == 0 {
            self.last_width.set(width);
            return;
        }

        // Re-anchor the already-hidden prefix of the head line at the new
        // width. `scrolled` is a partial wrapped-row offset into the head
        // line at `last_width`; converting that offset back into a character
        // prefix and re-wrapping it preserves monotonicity.
        let rendered = self.rendered_lines();
        let Some(head) = rendered.first() else {
            self.scrolled.set(0);
            self.last_width.set(width);
            return;
        };
        self.scrolled
            .set(Self::reanchor_scrolled(head, scrolled, last, width));
        self.last_width.set(width);
    }

    /// Re-anchor a partial wrapped-row offset across a width change.
    fn reanchor_scrolled(
        line: &Line<'static>,
        scrolled: usize,
        old_width: u16,
        new_width: u16,
    ) -> usize {
        if scrolled == 0 || old_width == 0 || new_width == 0 {
            return 0;
        }
        let content = line.to_string();
        if content.is_empty() {
            return 0;
        }

        let mut boundary = 0usize;
        for (idx, _) in content.char_indices() {
            if idx == 0 {
                continue;
            }
            let prefix = &content[..idx];
            let prefix_rows =
                line_wrapped_height(&Line::from(Span::raw(prefix.to_string())), old_width);
            if prefix_rows == scrolled {
                boundary = idx;
            } else if prefix_rows > scrolled {
                break;
            }
        }

        if boundary == 0 {
            return 0;
        }
        let prefix = &content[..boundary];
        line_wrapped_height(&Line::from(Span::raw(prefix.to_string())), new_width)
    }

    /// Advance the scrolled offset and drop leading logical lines that have
    /// been completely consumed. Returns the scrolled offset remaining on the
    /// new head line.
    fn consume_head(&mut self, rendered: &[Line<'static>], mut hidden: usize, width: u16) -> usize {
        if rendered.is_empty() {
            return 0;
        }

        let mut drop_count = 0usize;
        while drop_count + 1 < rendered.len() {
            let line_rows = line_wrapped_height(&rendered[drop_count], width);
            if hidden < line_rows {
                break;
            }
            hidden -= line_rows;
            drop_count += 1;
        }

        // Never consume the final line entirely: at least its last wrapped
        // row must stay visible.
        if drop_count + 1 >= rendered.len() {
            if let Some(last) = rendered.last() {
                let rows = line_wrapped_height(last, width);
                hidden = hidden.min(rows.saturating_sub(1));
            }
        }

        let drop_from_buffer = drop_count.min(self.lines.len());
        self.lines.drain_head(drop_from_buffer);
        if drop_count > drop_from_buffer {
            self.partial.clear();
        }
        hidden
    }

    /// Desired stream height, uncapped: the full wrapped height of the
    /// remaining live tail minus the wrapped rows already scrolled off.
    pub fn desired_rows(&self, width: u16) -> u16 {
        if self.is_empty() || width == 0 {
            return 0;
        }
        self.normalize_scroll(width);
        let rendered = self.rendered_lines();
        let total = wrapped_height(&rendered, width);
        total
            .saturating_sub(self.scrolled.get())
            .min(u16::MAX as usize) as u16
    }

    /// Completed lines plus the trailing partial, in render order.
    pub fn rendered_lines(&self) -> Vec<Line<'static>> {
        let mut out = self.lines.visible_lines().to_vec();
        if !self.partial.is_empty() {
            out.push(self.style_line(&self.partial, self.line_count));
        }
        out
    }

    /// Lines available for a permanent commit tail. Tool streams return the
    /// sliding retained tail; other streams fall back to the live viewport.
    pub fn commit_lines(&self) -> Vec<Line<'static>> {
        if self.retain_tail {
            self.retained_tail.visible_lines().to_vec()
        } else {
            self.rendered_lines()
        }
    }

    /// Keep only the visible tail of the live stream; the head is discarded,
    /// never committed to terminal scrollback. Zero-area viewports are
    /// transient, so defer trimming rather than dropping content that had no
    /// chance to paint.
    pub fn trim_to(&mut self, available_rows: u16, width: u16) {
        if width == 0 || available_rows == 0 {
            return;
        }

        self.normalize_scroll(width);
        let rendered = self.rendered_lines();
        let total = wrapped_height(&rendered, width);
        let visible = total.saturating_sub(self.scrolled.get());
        let excess = visible.saturating_sub(available_rows as usize);
        if excess == 0 {
            return;
        }

        let next_scrolled = self.consume_head(&rendered, self.scrolled.get() + excess, width);
        self.scrolled.set(next_scrolled);
        self.last_width.set(width);
    }

    fn title_line(&self) -> Option<Line<'static>> {
        let title = self.title.as_deref()?;
        if title.is_empty() {
            return None;
        }
        Some(tool_chrome_line(&self.title_prefix, title))
    }

    /// Wrapped title rows. Idle streams collapse to zero even when a title
    /// is configured, so the title never allocates screen space on its own.
    fn title_rows(&self, width: u16, _ctx: &RenderContext) -> u16 {
        if self.is_empty() || width == 0 {
            return 0;
        }
        match self.title_line() {
            None => 0,
            Some(line) => (wrapped_height(&[line], width).min(u16::MAX as usize)) as u16,
        }
    }
}

impl Default for StreamingWidget {
    fn default() -> Self {
        Self::new(
            |text, _| Line::from(Span::raw(text.to_string())),
            u16::MAX,
            10,
        )
    }
}

impl LiveAreaWidget for StreamingWidget {
    #[cfg(feature = "telemetry")]
    fn name(&self) -> &'static str {
        "streaming"
    }

    fn size_priority(&self) -> i32 {
        self.size_priority
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        false
    }

    fn min_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        self.title_rows(width, ctx)
    }

    fn desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        let title_rows = self.title_rows(width, ctx);
        let stream_rows = if self.is_empty() || width == 0 {
            0
        } else {
            self.desired_rows(width).min(self.row_cap)
        };
        title_rows.saturating_add(stream_rows)
    }

    fn max_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        if self.is_empty() {
            0
        } else {
            self.title_rows(width, ctx).saturating_add(self.row_cap)
        }
    }

    fn finalize(&mut self, width: u16) {
        if self.row_cap < u16::MAX {
            self.trim_to(self.row_cap, width);
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        // Clear the region first so a shorter frame cannot leave stale cells
        // from a previous, taller live tail.
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)].reset();
            }
        }
        if area.width == 0 || area.height == 0 {
            return;
        }

        let mut y = area.y;
        if let Some(line) = self.title_line() {
            let title_rows = self.title_rows(area.width, ctx).min(area.height);
            if title_rows > 0 {
                let title_area = Rect::new(area.x, y, area.width, title_rows);
                render_wrapped_tail(&[line], title_area, buf, title_rows as usize);
                y += title_rows;
            }
        }

        let stream_height = area.height.saturating_sub(y.saturating_sub(area.y));
        if stream_height == 0 || self.is_empty() {
            return;
        }
        let stream_area = Rect::new(area.x, y, area.width, stream_height);
        self.trim_to(stream_area.height, stream_area.width);
        let lines = self.rendered_lines();
        let total = wrapped_height(&lines, stream_area.width);
        let visible_rows = total.saturating_sub(self.scrolled.get());
        render_wrapped_tail(&lines, stream_area, buf, visible_rows);
    }

    fn take_overflow(
        &mut self,
        _available_rows: u16,
        _width: u16,
        _ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        Vec::new()
    }
}

impl LiveAreaWidget for TranscriptWidget {
    #[cfg(feature = "telemetry")]
    fn name(&self) -> &'static str {
        "transcript"
    }

    fn size_priority(&self) -> i32 {
        self.tools
            .values()
            .map(|stream| stream.size_priority())
            .fold(self.reasoning.size_priority(), |acc, p| acc.min(p))
    }

    fn overflow_priority(&self) -> i32 {
        self.tools
            .values()
            .map(|stream| stream.overflow_priority())
            .fold(self.reasoning.overflow_priority(), |acc, p| acc.min(p))
    }

    fn min_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        let inner = width.saturating_sub(1);
        1u16.saturating_add(self.reasoning.min_height(inner, ctx))
            .saturating_add(self.tools_min_height(inner, ctx))
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        true
    }

    fn desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        self.desired_height(width, ctx)
    }

    fn max_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        u16::MAX
    }

    fn finalize(&mut self, width: u16) {
        self.last_width = width;
        self.rebuild_separator(width);
        self.rebuild_response();
        let inner = width.saturating_sub(1);
        self.reasoning.finalize(inner);
        for stream in self.tools.values_mut() {
            stream.finalize(inner);
        }
    }

    fn take_overflow(
        &mut self,
        available_rows: u16,
        width: u16,
        ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        self.take_overflow(available_rows, width, ctx)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
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

        // One-cell left margin: all bands paint inside the same inner rect so
        // reasoning/tool align with prompt and response.
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

        // Allocate the ephemeral band before borrowing the FIFO lines.
        let (reasoning_rows, tool_rows) = self.ephemeral_heights(area.height, width, ctx);
        let tool_rows_total = tool_rows.iter().fold(0u16, |acc, h| acc.saturating_add(*h));
        let fifo_rows = area
            .height
            .saturating_sub(reasoning_rows)
            .saturating_sub(tool_rows_total);

        let lines = self.scrollable.visible_lines();
        if lines.is_empty() {
            return;
        }

        // Three-slice split around the two pinned boundaries. Everything
        // before `prompt_end` is the top-anchored pre-reasoning head (prior
        // history + current prompt); the middle slice is the permanent
        // tool-call-event band; everything from `response_start` onward is
        // the streaming response.
        let prompt_end = self.prompt_end.min(lines.len());
        let response_start = self
            .response_start
            .unwrap_or(lines.len())
            .min(lines.len())
            .max(prompt_end);
        let pre = &lines[..prompt_end];
        let events = &lines[prompt_end..response_start];
        let tail = &lines[response_start..];

        let pre_rows = wrapped_height(pre, width);
        let event_rows = wrapped_height(events, width);
        let tail_rows = wrapped_height(tail, width);

        let (pre_band, event_band, tail_band) = if tail.is_empty() {
            // No response yet: paint the pre-reasoning head and then the
            // tool-call-event band, both top-anchored.
            let pre_band = (pre_rows as u16).min(fifo_rows);
            let event_band = (event_rows as u16).min(fifo_rows.saturating_sub(pre_band));
            (pre_band, event_band, 0)
        } else {
            // The response keeps at least one FIFO row after the ephemeral
            // bands. After overflow the only line that can still exceed the
            // remaining FIFO rows is the final over-tall response line; the
            // head slices are painted top-anchored and the tail renderer
            // keeps the response's wrapped tail.
            let tail_floor = fifo_rows.min(1);
            let head_budget = fifo_rows.saturating_sub(tail_floor);
            let pre_band = (pre_rows as u16).min(head_budget);
            let event_band = (event_rows as u16).min(head_budget.saturating_sub(pre_band));
            let tail_band = fifo_rows
                .saturating_sub(pre_band)
                .saturating_sub(event_band);
            (pre_band, event_band, tail_band)
        };

        let mut y = inner.y;
        if pre_band > 0 && !pre.is_empty() {
            let pre_area = Rect::new(inner.x, y, width, pre_band);
            Paragraph::new(pre.to_vec())
                .wrap(Wrap { trim: false })
                .render(pre_area, buf);
        }

        y = inner.y + pre_band;
        if reasoning_rows > 0 {
            let reasoning_area = Rect::new(inner.x, y, width, reasoning_rows);
            LiveAreaWidget::render(&mut self.reasoning, reasoning_area, buf, ctx);
        }

        y = inner.y + pre_band + reasoning_rows;
        if event_band > 0 && !events.is_empty() {
            let event_area = Rect::new(inner.x, y, width, event_band);
            Paragraph::new(events.to_vec())
                .wrap(Wrap { trim: false })
                .render(event_area, buf);
        }

        y = inner.y + pre_band + reasoning_rows + event_band;
        for (stream, stream_rows) in self.tools.values_mut().zip(tool_rows) {
            if stream_rows == 0 {
                continue;
            }
            let tool_area = Rect::new(inner.x, y, width, stream_rows);
            LiveAreaWidget::render(stream, tool_area, buf, ctx);
            y += stream_rows;
        }

        if tail_band > 0 && !tail.is_empty() {
            if tail_rows <= tail_band as usize && (reasoning_rows > 0 || tool_rows_total > 0) {
                // Bottom-anchor the short response inside its allocated band
                // so the blank rows sit between the ephemeral slots and
                // response.
                let tail_area = Rect::new(
                    inner.x,
                    inner.y + area.height.saturating_sub(tail_rows as u16),
                    width,
                    tail_rows as u16,
                );
                Paragraph::new(tail.to_vec())
                    .wrap(Wrap { trim: false })
                    .render(tail_area, buf);
            } else {
                // Without ephemeral output the short response streams
                // directly below the tool-call-event band with no blank gap;
                // a tall response keeps its tail.
                let tail_area = Rect::new(inner.x, y, width, tail_band);
                render_wrapped_tail(tail, tail_area, buf, tail_band as usize);
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

    /// The wrapped rows of the input text (prompt + text + optional preview)
    /// at `width`, uncapped.
    pub fn wrapped_rows(&self, width: u16, input: &str, preview: Option<&str>) -> u16 {
        let lines = Self::preview_lines(Self::full_lines(input), preview);
        (wrapped_height(&lines, width).max(1).min(u16::MAX as usize)) as u16
    }

    /// Build the styled logical lines: the prompt chevron on the first line,
    /// with each `'\n'` in the input producing a hard line break and
    /// continuation lines indented two cells to align past the chevron.
    fn full_lines(input: &str) -> Vec<Line<'static>> {
        if input.is_empty() {
            return vec![Line::from(vec![Span::styled(
                format!("{} ", glyph::CHEVRON),
                Style::default().fg(DEFAULT_THEME.accent_primary),
            )])];
        }
        input
            .split('\n')
            .enumerate()
            .map(|(i, part)| {
                if i == 0 {
                    Line::from(vec![
                        Span::styled(
                            format!("{} ", glyph::CHEVRON),
                            Style::default().fg(DEFAULT_THEME.accent_primary),
                        ),
                        Span::raw(part.to_string()),
                    ])
                } else {
                    Line::from(vec![Span::raw("  "), Span::raw(part.to_string())])
                }
            })
            .collect()
    }

    /// Extend the final logical line with a dimmed inline preview suffix.
    ///
    /// The preview is paint-only: it never changes the buffer, cursor, or
    /// wrapping measurements beyond what the full candidate path would
    /// occupy. It stays on the last logical line and is never split or
    /// duplicated across lines.
    fn preview_lines(mut lines: Vec<Line<'static>>, preview: Option<&str>) -> Vec<Line<'static>> {
        let Some(preview) = preview.filter(|p| !p.is_empty()) else {
            return lines;
        };
        let last = lines
            .last_mut()
            .expect("full_lines always yields at least one line");
        last.spans.push(Span::styled(
            preview.to_string(),
            Style::default()
                .fg(DEFAULT_THEME.subtle)
                .add_modifier(Modifier::DIM),
        ));
        lines
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
        // A cursor parked on a `'\n'` has no visible cell of its own: it sits
        // at the end of the logical line that the newline terminates.
        if ch == '\n' {
            return Self::end_of_text_position(&input[..cursor], width);
        }

        let lines = Self::lines_with_marker(input, cursor);
        if let Some(pos) = Self::locate_marker(&lines, width) {
            return pos;
        }

        // The styled grapheme was dropped by the wrapper. The cursor sits just
        // before it, i.e. at the end of the preceding text.
        Self::end_of_text_position(&input[..cursor], width)
    }

    /// Build the logical lines of `input`, styling the grapheme at byte offset
    /// `marker_at` with the marker style. The offset must lie strictly inside
    /// a logical line (never on a `'\n'` or past the end).
    fn lines_with_marker(input: &str, marker_at: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut offset = 0usize;
        for (i, part) in input.split('\n').enumerate() {
            let line_start = offset;
            let line_end = offset + part.len();
            let mut spans = Vec::new();
            if i == 0 {
                spans.push(Self::prompt_span());
            } else {
                spans.push(Span::raw("  "));
            }
            if marker_at >= line_start && marker_at < line_end {
                let local = marker_at - line_start;
                let ch = part[local..].chars().next().expect("marker within line");
                let len = ch.len_utf8();
                if local > 0 {
                    spans.push(Span::raw(part[..local].to_string()));
                }
                spans.push(Span::styled(ch.to_string(), Self::marker_style()));
                spans.push(Span::raw(part[local + len..].to_string()));
            } else {
                spans.push(Span::raw(part.to_string()));
            }
            lines.push(Line::from(spans));
            offset = line_end + 1;
        }
        lines
    }

    /// Cursor position at the very end of `input` (just past the last char).
    fn end_of_text_position(input: &str, width: u16) -> (u16, u16) {
        let mut lines = Self::full_lines(input);
        let full_rows = wrapped_height(&lines, width).max(1) as u16;

        lines
            .last_mut()
            .expect("full_lines always yields at least one line")
            .spans
            .push(Span::styled("\u{E000}", Self::marker_style()));

        let marker = Self::locate_marker(&lines, width).unwrap_or((full_rows - 1, width - 1));

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

    /// Render `lines` at `width` and return the (row, col) of the marker-style
    /// cell, if present.
    fn locate_marker(lines: &[Line<'static>], width: u16) -> Option<(u16, u16)> {
        let rows = wrapped_height(lines, width).max(1) as u16;
        let area = Rect::new(0, 0, width, rows);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines.to_vec())
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
    #[cfg(feature = "telemetry")]
    fn name(&self) -> &'static str {
        "input"
    }

    fn size_priority(&self) -> i32 {
        100
    }

    fn min_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        // Minimum: the wrapped content, capped at 5 rows. The bar is not
        // shrunk below its visible text unless the viewport cannot fit it.
        self.wrapped_rows(width, ctx.input, ctx.completion_preview)
            .min(5)
    }

    fn desired_height(&self, width: u16, ctx: &RenderContext) -> u16 {
        self.wrapped_rows(width, ctx.input, ctx.completion_preview)
    }

    fn max_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        5
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        let width = area.width;
        if width == 0 {
            return;
        }

        // Build the styled logical lines, adding the paint-only preview suffix
        // immediately after the real input on the final line. Measurement now
        // includes the preview so the extended line is allocated enough rows;
        // cursor position is still computed from `ctx.input` alone.
        let lines = Self::preview_lines(Self::full_lines(ctx.input), ctx.completion_preview);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });

        // Render into a full-width buffer at the paragraph's natural height,
        // then copy the visible window into `area`.
        let full_rows = self
            .wrapped_rows(width, ctx.input, ctx.completion_preview)
            .max(1);
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
    #[cfg(feature = "telemetry")]
    fn name(&self) -> &'static str {
        "info"
    }

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
        spans.push(Span::styled("Tab", malachite));
        spans.push(Span::styled(" filepath  ", ivory));
        spans.push(Span::styled("/help", malachite));

        let paragraph = Paragraph::new(Line::from(spans)).style(band);
        paragraph.render(area, buf);
    }
}

/// The full-screen interactive debug session view.
///
/// Owns the target id, prompt text, output scrollback, and connection state
/// (`connecting`/`connected`). Renders a header, output scrollback, and its
/// own prompt. It fills the whole screen and returns no overflow.
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
    #[cfg(feature = "telemetry")]
    fn name(&self) -> &'static str {
        "debug"
    }

    fn size_priority(&self) -> i32 {
        50
    }

    fn min_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        // Fills the whole terminal while active.
        0
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        true
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &RenderContext) {
        // Clear the full-screen region first so a shorter frame never leaves
        // stale cells behind.
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)].reset();
            }
        }

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
    fn submit_prompt_preserves_hard_line_breaks() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("one\ntwo".to_string());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["▍» one", "   two"]);
    }

    #[test]
    fn submit_prompt_preserves_blank_lines() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("one\n\ntwo".to_string());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["▍» one", "   ", "   two"]);
    }

    #[test]
    fn submit_prompt_leading_newline_keeps_first_line_marker() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("\none".to_string());
        let lines = transcript.scrollable.visible_lines();
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["▍» ", "   one"]);
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
    fn finish_turn_keeps_tool_lines_in_transcript_between_prompt_and_response() {
        let mut transcript = TranscriptWidget::new();
        let mut reasoning = StreamingWidget::reasoning(5);

        transcript.submit_prompt("hi".to_string());
        reasoning.push_text("thinking\n");
        transcript.push_tool_line("Done");
        transcript.push_response("**answer**");
        transcript.finish_turn();

        assert!(!transcript.has_current_turn());
        let transcript_text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        // Tool line sits between the prompt and the response, in order.
        assert_eq!(transcript_text, vec!["▍» hi", "◆ Done", "answer"]);
        assert!(!transcript_text.contains(&"thinking".to_string()));

        let reasoning_text: Vec<String> = reasoning
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(reasoning_text, vec!["    thinking"]);
    }

    #[test]
    fn tool_line_inserted_above_streaming_response_survives_rebuild() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("streamed");
        // Finalize renders the response into the FIFO.
        LiveAreaWidget::finalize(&mut transcript, 80);

        // A tool call arriving after the response was rendered is inserted
        // above the response region and must survive a later rebuild.
        transcript.push_tool_line("grep");
        transcript.reconcile_response("streamed complete");
        transcript.finish_turn();

        let transcript_text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            transcript_text.contains(&"◆ grep".to_string()),
            "{transcript_text:?}"
        );
        assert!(
            transcript_text
                .iter()
                .any(|l| l.contains("streamed complete")),
            "{transcript_text:?}"
        );
        // Ordering: prompt, tool line, response.
        assert_eq!(transcript_text[0], "▍» hi");
        assert_eq!(transcript_text[1], "◆ grep");
    }

    #[test]
    fn tool_line_never_inserts_before_pinned_prompt_after_overflow() {
        let mut transcript = TranscriptWidget::new();
        // Completed history above the in-progress turn.
        transcript.push_transcript_line("prior".to_string());
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("resp");
        LiveAreaWidget::finalize(&mut transcript, 80);

        // A one-row viewport forces the history and the pinned prompt into
        // terminal scrollback; both markers clamp to the FIFO head.
        let overflow: Vec<String> = transcript
            .take_overflow(1, 80, &input_ctx("", 0))
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(overflow.contains(&"▍» hi".to_string()), "{overflow:?}");

        // A tool call arriving after the overflow must land after the
        // prompt in terminal scrollback order and before the response line
        // left in the FIFO.
        transcript.push_tool_line("grep");

        let mut order = overflow;
        order.extend(
            transcript
                .scrollable
                .visible_lines()
                .iter()
                .map(|l| l.to_string()),
        );
        let prompt = order.iter().position(|l| l == "▍» hi").unwrap();
        let tool = order.iter().position(|l| l == "◆ grep").unwrap();
        let response = order.iter().position(|l| l == "resp").unwrap();
        assert!(
            tool > prompt,
            "tool line landed before the prompt: {order:?}"
        );
        assert!(
            tool < response,
            "tool line must sit above the streaming response: {order:?}"
        );
    }

    #[test]
    fn tool_line_stays_below_prompt_when_response_marker_clamps_to_turn_start() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());

        // Simulate the degenerate post-overflow marker state while the
        // prompt is still the FIFO head. The prompt end is tracked
        // separately, so a tool line must be inserted after the prompt even
        // when both the turn and response markers point at index 0.
        transcript.turn_start = Some(0);
        transcript.response_start = Some(0);

        transcript.push_tool_line("grep");

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text,
            vec!["▍» hi".to_string(), "◆ grep".to_string()],
            "tool line must not be inserted before a prompt still in the FIFO"
        );
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
    fn transcript_block_no_active_turn_is_contiguous() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_transcript_block(vec![
            "line one".to_string(),
            "line two".to_string(),
            "line three".to_string(),
        ]);
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        // No blank lines between the block's lines.
        assert_eq!(text, vec!["line one", "line two", "line three"]);
    }

    #[test]
    fn transcript_block_no_active_turn_pads_once_before_block() {
        let mut transcript = TranscriptWidget::new();
        transcript.push_transcript_line("prior".to_string());
        transcript.push_transcript_block(vec!["line one".to_string(), "line two".to_string()]);
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        // Two blank lines before the block (completed-history padding), then
        // the block lines contiguously.
        assert_eq!(text, vec!["prior", "", "", "line one", "line two"]);
    }

    #[test]
    fn transcript_block_inserted_before_pinned_turn() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("resp");
        transcript.push_transcript_block(vec!["block one".to_string(), "block two".to_string()]);
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        // The block lines appear above the pinned turn, contiguously. The
        // turn starts at index 0 (no prior content), so no blank separator
        // is inserted (mirroring `push_styled_line`). The response is still
        // in the accumulator (not yet rebuilt into the FIFO).
        assert_eq!(text, vec!["block one", "block two", "▍» hi"]);
    }

    #[test]
    fn transcript_block_inserted_before_pinned_turn_with_prior_content() {
        // Prior content means `submit_prompt` pushes a separator rule, so the
        // turn is pinned at index > 0 and the `t > 0` blank-separator branch
        // runs. The block lines stay contiguous above the pinned turn, with
        // exactly one blank separator between the block and the prompt.
        let mut transcript = TranscriptWidget::new();
        transcript.push_transcript_line("prior".to_string());
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("resp");
        transcript.push_transcript_block(vec!["block one".to_string(), "block two".to_string()]);
        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text,
            vec![
                "prior",
                "────────────────────────────────────────",
                "block one",
                "block two",
                "",
                "▍» hi"
            ]
        );
    }

    #[test]
    fn transcript_block_single_line_matches_single_line_insertion() {
        // A one-line block inserted during a turn must produce the same FIFO
        // layout as `push_styled_line`, including when prior content pins the
        // turn at index > 0 (exercising the blank-separator branch).
        let mut block = TranscriptWidget::new();
        block.push_transcript_line("prior".to_string());
        block.submit_prompt("hi".to_string());
        block.push_response("resp");
        block.push_transcript_block(vec!["system note".to_string()]);

        let mut single = TranscriptWidget::new();
        single.push_transcript_line("prior".to_string());
        single.submit_prompt("hi".to_string());
        single.push_response("resp");
        single.push_transcript_line("system note".to_string());

        let block_text: Vec<String> = block
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        let single_text: Vec<String> = single
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(block_text, single_text);
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
        let overflow = transcript.take_overflow(1, 80, &input_ctx("", 0));
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
        let overflow = transcript.take_overflow(1, 10, &input_ctx("", 0));
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
        let overflow = transcript.take_overflow(5, 80, &input_ctx("", 0));
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
    fn streaming_response_accumulates_across_frames() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());

        // Stream the response word-by-word, finalizing after each chunk the
        // way `compose` does every frame. Each chunk must append to the
        // previously-rendered text, not replace it.
        for word in ["hello", " ", "world", " ", "again"] {
            transcript.push_response(word);
            LiveAreaWidget::finalize(&mut transcript, 80);
        }

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("hello world again")),
            "streamed words must accumulate into one response line: {text:?}"
        );
    }

    #[test]
    fn streaming_response_does_not_reemit_drained_lines() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());

        // A tall streamed response. List items render as distinct logical
        // lines, so re-emission into overflow is observable.
        transcript.push_response("- one\n- two\n- three\n- four");
        LiveAreaWidget::finalize(&mut transcript, 80);
        let first: Vec<String> = transcript
            .take_overflow(1, 80, &input_ctx("", 0))
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(!first.is_empty());

        // Stream another chunk. The rebuild must append to the retained tail
        // and skip the lines already committed to scrollback.
        transcript.push_response("\n- five");
        LiveAreaWidget::finalize(&mut transcript, 80);
        let second: Vec<String> = transcript
            .take_overflow(1, 80, &input_ctx("", 0))
            .iter()
            .map(|l| l.to_string())
            .collect();

        for line in &first {
            assert!(
                !second.contains(line),
                "line committed to scrollback was emitted again: {line:?}\nfirst={first:?}\nsecond={second:?}"
            );
        }

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("five")),
            "the newly streamed chunk must stay visible: {text:?}"
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
        let _ = transcript.take_overflow(2, 10, &input_ctx("", 0));
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
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("thinking");
        // The partial reasoning line must be visible before any newline.
        let text: Vec<String> = reasoning
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.contains(&"    thinking".to_string()), "{text:?}");
    }

    #[test]
    fn reasoning_trim_keeps_wrapped_tail() {
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("abcdefghijklmnopqrstuvwxyz\n");
        reasoning.flush();

        reasoning.trim_to(2, 10);

        // The single long line is taller than the 2 visible rows, but the
        // live tail keeps it and scrolls the visible tail one wrapped row at
        // a time instead of dropping the whole logical line.
        assert_eq!(reasoning.rendered_lines().len(), 1);
        assert_eq!(reasoning.desired_rows(10), 2);
    }

    #[test]
    fn reasoning_scrolled_tail_grows_with_streaming_chunks() {
        let mut reasoning = StreamingWidget::reasoning(5);
        for i in 0..5 {
            reasoning.push_text(&format!("reasoning line {i}\n"));
        }
        assert_eq!(reasoning.desired_rows(80), 5);

        reasoning.trim_to(2, 80);
        assert_eq!(reasoning.desired_rows(80), 2);

        // Chunks that arrive after the trim grow the visible tail instead of
        // being hidden behind a stale retained-visible-rows window.
        reasoning.push_text("reasoning line 5\n");
        reasoning.push_text("reasoning line 6\n");
        assert_eq!(reasoning.desired_rows(80), 4);
    }

    #[test]
    fn reasoning_taller_allocation_does_not_resurrect_scrolled_rows() {
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("abcdefghijklmnopqrstuvwxyz\n");
        reasoning.flush();

        reasoning.trim_to(2, 10);
        assert_eq!(reasoning.desired_rows(10), 2);

        // A later taller allocation must not bring back the wrapped rows
        // that already scrolled off the head of this live tail.
        reasoning.trim_to(4, 10);
        assert_eq!(reasoning.desired_rows(10), 2);
    }

    #[test]
    fn reasoning_short_trailing_line_does_not_unscroll_head() {
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("abcdefghijklmnopqrstuvwxyz\n");
        reasoning.push_text("tail\n");

        // The prefixed head line wraps to 3 rows at width 10; the short
        // trailing line is 1 row. With a 2-row allocation the scrolled
        // offset must stay 2 (two rows of the head line hidden), not be
        // clamped against the short tail line back to zero.
        reasoning.trim_to(2, 10);
        assert_eq!(reasoning.desired_rows(10), 2);
        assert_eq!(reasoning.scrolled.get(), 2);
    }

    #[test]
    fn reasoning_wider_viewport_does_not_resurrect_scrolled_head() {
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("abcdefghijklmnopqrstuvwxyz\n");
        reasoning.flush();

        reasoning.trim_to(2, 10);
        assert_eq!(reasoning.desired_rows(10), 2);
        assert_eq!(reasoning.scrolled.get(), 1);

        // At width 80 the whole line fits on a single row. The one row
        // hidden at width 10 corresponds to a prefix that still occupies
        // that one row, so the previously hidden characters must not
        // reappear; the tail asks for no visible rows until new content
        // arrives.
        assert_eq!(reasoning.desired_rows(80), 0);
        assert_eq!(reasoning.scrolled.get(), 1);
    }

    #[test]
    fn reasoning_narrower_viewport_does_not_hide_new_wraps() {
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("abcdefghijklmnopqrstuvwxyz\n");
        reasoning.flush();

        assert_eq!(reasoning.desired_rows(10), 3);
        // Nothing has scrolled off yet, so narrowing to width 5 must ask for
        // all six newly wrapped rows rather than clamping to the previous
        // three visible rows.
        assert_eq!(reasoning.desired_rows(5), 6);
    }

    #[test]
    fn reasoning_clear_resets_scrolled_tail() {
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("first\nsecond\nthird\n");
        reasoning.trim_to(1, 80);
        assert_eq!(reasoning.desired_rows(80), 1);

        reasoning.clear();
        assert!(reasoning.is_empty());
        assert_eq!(reasoning.desired_rows(80), 0);

        reasoning.push_text("fresh\n");
        assert_eq!(reasoning.desired_rows(80), 1);
    }

    #[test]
    fn reasoning_is_empty_when_never_fed() {
        let reasoning = StreamingWidget::reasoning(5);
        assert!(reasoning.is_empty());
        assert_eq!(reasoning.desired_rows(80), 0);
    }

    #[test]
    fn reasoning_reports_uncapped_desired_height() {
        let mut reasoning = StreamingWidget::reasoning(5);
        for i in 0..10 {
            reasoning.push_text(&format!("reasoning line {i}\n"));
        }
        // The child component itself is uncapped; the transcript enforces
        // the 5-row band cap during finalize/measurement.
        assert_eq!(reasoning.desired_rows(80), 10);
    }

    #[test]
    fn reasoning_desired_rows_are_wrapped_rows() {
        // The four-space indent plus a 26-char word is 30 columns, which
        // wraps to 3 rows at width 10.
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("abcdefghijklmnopqrstuvwxyz");
        assert_eq!(reasoning.desired_rows(10), 3);

        // A 56-column line (four-space indent + 52-char word) wraps to 6
        // rows, still reporting its natural wrapped height at the component
        // level even though the transcript band cap is 5 rows.
        let mut reasoning = StreamingWidget::reasoning(5);
        reasoning.push_text("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz");
        assert_eq!(reasoning.desired_rows(10), 6);
    }

    #[test]
    fn streaming_widget_with_title_renders_title_line() {
        let ctx = input_ctx("", 0);
        let mut widget = StreamingWidget::new(reasoning_stream_line, 5, 10)
            .with_title_prefix("◆ ")
            .with_line_prefix("    ");
        widget.set_title("LIVE TITLE");
        for i in 0..8 {
            widget.push_text(&format!("line {i}\n"));
        }

        assert_eq!(widget.desired_height(20, &ctx), 6);
        assert!(widget.take_overflow(1, 20, &ctx).is_empty());

        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf, &ctx);

        let rows = rendered_rows(&buf, 20, 5);
        assert!(rows[0].contains("◆ LIVE TITLE"), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("        line 7")),
            "{rows:?}"
        );
        assert!(!rows.iter().any(|r| r.contains("line 0")), "{rows:?}");
    }

    #[test]
    fn streaming_widget_title_collapses_when_idle() {
        let ctx = input_ctx("", 0);
        let mut widget = StreamingWidget::new(reasoning_stream_line, 5, 10).with_title_prefix("◆ ");
        widget.set_title("LIVE");

        assert!(widget.is_empty());
        assert_eq!(widget.min_height(20, &ctx), 0);
        assert_eq!(widget.desired_height(20, &ctx), 0);
        assert_eq!(widget.max_height(20, &ctx), 0);
    }

    #[test]
    fn streaming_widget_wrapping_title_paints_stream_after_measured_title_rows() {
        let ctx = input_ctx("", 0);
        let mut widget = StreamingWidget::new(reasoning_stream_line, 5, 10).with_title_prefix("◆ ");
        widget.set_title("THIS TITLE WRAPS");
        widget.push_text("one\ntwo\nthree\n");

        let width = 10;
        let title_rows = widget.title_rows(width, &ctx) as usize;
        assert!(
            title_rows > 1,
            "expected wrapping title, got {title_rows} row"
        );

        let area = Rect::new(0, 0, width, (title_rows + 3) as u16);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf, &ctx);

        let rows = rendered_rows(&buf, width, (title_rows + 3) as u16);
        assert!(
            rows[..title_rows].iter().any(|r| r.contains("TITLE")),
            "{rows:?}"
        );
        assert!(
            !rows[..title_rows].iter().any(|r| r.contains("one")),
            "{rows:?}"
        );
        assert!(
            rows[title_rows..].iter().any(|r| r.contains("one")),
            "{rows:?}"
        );
    }

    #[test]
    fn transcript_ephemeral_layout_shrinks_tool_before_reasoning_and_preserves_fifo_floor() {
        let ctx = input_ctx("", 0);
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");
        for i in 0..5 {
            transcript.push_reasoning_text(&format!("reasoning {i}\n"));
            transcript.push_tool_output_chunk("b1", "", &format!("tool {i}\n"));
        }

        let cols = 40;
        LiveAreaWidget::finalize(&mut transcript, cols);

        // The transcript re-runs the shared engine on its children with the
        // parent's already-reduced inner width. Both streams want 5 rows;
        // with only 5 ephemeral rows available the tool stream (priority 10)
        // shrinks before reasoning (priority 20), leaving the FIFO its
        // one-row floor.
        let (reasoning_rows, tool_rows) = transcript.ephemeral_heights(6, cols - 1, &ctx);
        assert_eq!(reasoning_rows, 5);
        assert_eq!(tool_rows, vec![0]);

        // With only 3 ephemeral rows available, reasoning also yields.
        let (reasoning_rows, tool_rows) = transcript.ephemeral_heights(4, cols - 1, &ctx);
        assert_eq!(reasoning_rows, 3);
        assert_eq!(tool_rows, vec![0]);
    }

    #[test]
    fn transcript_render_orders_prompt_reasoning_tool_events_tool_output_response() {
        let ctx = input_ctx("", 0);
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_reasoning_text("thinking\n");
        transcript.push_tool_line("grep");
        transcript.push_tool_output_chunk("b1", "bash", "stdout line\n");
        transcript.push_response("answer");

        let cols = 40;
        LiveAreaWidget::finalize(&mut transcript, cols);

        // Six rows are exactly enough for prompt + reasoning + tool-call
        // event + live tool title + live tool body + response. The short
        // response bottom-anchors below the live tool slot.
        let area = Rect::new(0, 0, cols, 6);
        let mut buf = Buffer::empty(area);
        LiveAreaWidget::render(&mut transcript, area, &mut buf, &ctx);
        let rows = rendered_rows(&buf, cols, area.height);

        let prompt_row = rows.iter().position(|r| r.contains("▍» hi")).unwrap();
        let reasoning_row = rows.iter().position(|r| r.contains("thinking")).unwrap();
        let tool_event_row = rows.iter().position(|r| r.contains("◆ grep")).unwrap();
        let tool_output_row = rows.iter().position(|r| r.contains("stdout line")).unwrap();
        let response_row = rows.iter().position(|r| r.contains("answer")).unwrap();

        assert!(
            prompt_row < reasoning_row,
            "prompt must render above reasoning: {rows:?}"
        );
        assert!(
            reasoning_row < tool_event_row,
            "reasoning must render above tool-call events: {rows:?}"
        );
        assert!(
            tool_event_row < tool_output_row,
            "tool-call events must render above live tool output: {rows:?}"
        );
        assert!(
            tool_output_row < response_row,
            "live tool output must render above the response: {rows:?}"
        );
    }

    // ── embedded live tool slot ─────────────────────────────────

    #[test]
    fn tool_slot_buffers_completed_lines_and_trailing_partial() {
        let mut live = StreamingWidget::tool(5);
        live.push_text("first\nsecond");
        let lines: Vec<String> = live
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            lines,
            vec!["    first".to_string(), "    second".to_string()]
        );
    }

    #[test]
    fn tool_slot_trims_head_to_visible_height() {
        let mut live = StreamingWidget::tool(5);
        for i in 0..8 {
            live.push_text(&format!("line {i}\n"));
        }
        assert_eq!(live.desired_rows(80), 8);

        live.trim_to(3, 80);
        assert_eq!(live.desired_rows(80), 3);
        let lines: Vec<String> = live
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            lines,
            vec![
                "    line 5".to_string(),
                "    line 6".to_string(),
                "    line 7".to_string(),
            ]
        );
    }

    #[test]
    fn tool_slot_clear_resets_tail() {
        let mut live = StreamingWidget::tool(5);
        live.push_text("one\ntwo\nthree\n");
        live.trim_to(1, 80);
        assert_eq!(live.desired_rows(80), 1);

        live.clear();
        assert!(live.is_empty());
        assert_eq!(live.desired_rows(80), 0);

        live.push_text("fresh\n");
        assert_eq!(live.desired_rows(80), 1);
    }

    #[test]
    fn tool_slot_strips_ansi_escape_sequences() {
        let mut live = StreamingWidget::tool(5);
        live.push_text("\u{1b}[31mred\u{1b}[0m normal\n");
        let lines: Vec<String> = live
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines, vec!["    red normal".to_string()]);
    }

    #[test]
    fn committed_stream_tool_uses_title_plus_five_row_tail_exactly_once() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");
        let calls = vec!["Bash(command=\"run\")".to_string()];
        transcript.reconcile_tool_calls(&calls);
        for i in 0..8 {
            transcript.push_tool_output_chunk(
                "c1",
                "Bash(command=\"run\")",
                &format!("line {i}\n"),
            );
        }

        // A repeated completion for the same call must be ignored.
        transcript.commit_tool_result("c1", "Bash(command=\"run\")");
        transcript.commit_tool_result("c1", "Bash(command=\"run\")");
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text.iter()
                .filter(|l| *l == "◆ Bash(command=\"run\")")
                .count(),
            1,
            "streaming tool must commit exactly one title row: {text:?}"
        );
        let title = text
            .iter()
            .position(|l| l == "◆ Bash(command=\"run\")")
            .unwrap();
        let answer = text.iter().position(|l| l == "answer").unwrap();
        assert_eq!(text[0], "▍» hi");
        assert!(
            title < answer,
            "tool block must precede the response: {text:?}"
        );
        assert_eq!(text[title + 1], "    line 3");
        assert!(text.contains(&"    line 7".to_string()), "{text:?}");
        assert!(!text.contains(&"    line 0".to_string()), "{text:?}");
    }

    #[test]
    fn dropped_tool_result_is_reconciled_at_finish_turn() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");
        let calls = vec!["Bash(command=\"run\")".to_string()];
        transcript.reconcile_tool_calls(&calls);
        for i in 0..8 {
            transcript.push_tool_output_chunk(
                "c1",
                "Bash(command=\"run\")",
                &format!("line {i}\n"),
            );
        }

        // No ToolResult event: the safety net in `finish_turn` must commit
        // the stream rather than silently dropping the call.
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text.iter()
                .filter(|l| *l == "◆ Bash(command=\"run\")")
                .count(),
            1,
            "{text:?}"
        );
        let title = text
            .iter()
            .position(|l| l == "◆ Bash(command=\"run\")")
            .unwrap();
        assert_eq!(text[title + 1], "    line 3");
        assert!(text.contains(&"    line 7".to_string()), "{text:?}");
        assert!(!text.contains(&"    line 0".to_string()), "{text:?}");
    }

    #[test]
    fn turn_complete_after_commit_does_not_duplicate_tool() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");

        // ToolResult arrives before the TurnComplete that seeds the list.
        transcript.commit_tool_result("g1", "Grep(pattern=\"fn main\")");
        let calls = vec!["Grep(pattern=\"fn main\")".to_string()];
        transcript.reconcile_tool_calls(&calls);
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text.iter()
                .filter(|l| *l == "◆ Grep(pattern=\"fn main\")")
                .count(),
            1,
            "{text:?}"
        );
    }

    #[test]
    fn later_turn_complete_merges_pending_calls() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");

        let first = vec![
            "Grep(pattern=\"fn main\")".to_string(),
            "Bash(command=\"one\")".to_string(),
        ];
        transcript.reconcile_tool_calls(&first);
        transcript.commit_tool_result("g1", "Grep(pattern=\"fn main\")");

        // A later agent-loop iteration must not wipe the still-uncommitted
        // Bash call from the first iteration.
        let second = vec!["Glob(pattern=\"*.rs\")".to_string()];
        transcript.reconcile_tool_calls(&second);
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text.iter()
                .filter(|l| *l == "◆ Grep(pattern=\"fn main\")")
                .count(),
            1,
            "{text:?}"
        );
        assert_eq!(
            text.iter()
                .filter(|l| *l == "◆ Bash(command=\"one\")")
                .count(),
            1,
            "{text:?}"
        );
        assert_eq!(
            text.iter()
                .filter(|l| *l == "◆ Glob(pattern=\"*.rs\")")
                .count(),
            1,
            "{text:?}"
        );
    }

    #[test]
    fn reasoning_tail_commits_between_prompt_and_response() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_reasoning_text("thinking\n");
        transcript.flush_reasoning();
        transcript.push_response("answer");
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text,
            vec![
                "▍» hi".to_string(),
                "    thinking".to_string(),
                "answer".to_string()
            ]
        );
    }

    #[test]
    fn finish_turn_orders_prompt_reasoning_tools_response() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_reasoning_text("thinking\n");
        transcript.push_tool_line("grep");
        transcript.push_response("answer");
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text,
            vec![
                "▍» hi".to_string(),
                "    thinking".to_string(),
                "◆ grep".to_string(),
                "answer".to_string(),
            ]
        );
    }

    #[test]
    fn reasoning_commit_uses_retained_five_row_tail_after_short_display() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");
        for i in 0..8 {
            transcript.push_reasoning_text(&format!("thinking {i}\n"));
        }

        // Simulate a contested frame where the reasoning band is transiently
        // painted into a two-row body viewport.
        let ctx = input_ctx("", 0);
        {
            let stream = &mut transcript.reasoning;
            let area = Rect::new(0, 0, 40, 2);
            let mut buf = Buffer::empty(area);
            LiveAreaWidget::render(stream, area, &mut buf, &ctx);
        }

        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text,
            vec![
                "▍» hi".to_string(),
                "    thinking 3".to_string(),
                "    thinking 4".to_string(),
                "    thinking 5".to_string(),
                "    thinking 6".to_string(),
                "    thinking 7".to_string(),
                "answer".to_string(),
            ]
        );
    }

    #[test]
    fn reasoning_stays_out_of_fifo_while_streaming() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_reasoning_text("thinking\n");
        transcript.push_response("answer");
        LiveAreaWidget::finalize(&mut transcript, 80);

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(!text.iter().any(|l| l.contains("thinking")), "{text:?}");

        let reasoning: Vec<String> = transcript
            .reasoning
            .rendered_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(reasoning, vec!["    thinking".to_string()]);
    }

    #[test]
    fn non_streaming_tool_commit_uses_diamond_line_and_filters_read_edit() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");
        let calls = vec![
            "Grep(pattern=\"fn main\")".to_string(),
            "Read(file_path=\"x\")".to_string(),
        ];
        transcript.reconcile_tool_calls(&calls);
        transcript.commit_tool_result("g1", "Grep(pattern=\"fn main\")");
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            text.iter()
                .filter(|l| *l == "◆ Grep(pattern=\"fn main\")")
                .count(),
            1,
            "{text:?}"
        );
        assert!(
            !text.iter().any(|l| l.contains("Read")),
            "Read/Edit must stay filtered: {text:?}"
        );
    }

    #[test]
    fn stream_commit_uses_retained_five_row_tail_after_short_display() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");
        let calls = vec!["Bash(command=\"run\")".to_string()];
        transcript.reconcile_tool_calls(&calls);
        for i in 0..8 {
            transcript.push_tool_output_chunk(
                "c1",
                "Bash(command=\"run\")",
                &format!("line {i}\n"),
            );
        }

        // Simulate a contested frame where the stream is transiently painted
        // into a title row plus a two-row body band.
        let ctx = input_ctx("", 0);
        {
            let stream = transcript.tools.get_mut("c1").unwrap();
            let area = Rect::new(0, 0, 40, 3);
            let mut buf = Buffer::empty(area);
            LiveAreaWidget::render(stream, area, &mut buf, &ctx);
        }

        transcript.commit_tool_result("c1", "Bash(command=\"run\")");
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.contains(&"    line 7".to_string()), "{text:?}");
        assert!(text.contains(&"    line 3".to_string()), "{text:?}");
        assert!(!text.contains(&"    line 2".to_string()), "{text:?}");
        assert!(!text.contains(&"    line 0".to_string()), "{text:?}");
    }

    #[test]
    fn concurrent_stream_commits_are_separate_blocks_in_result_order() {
        let mut transcript = TranscriptWidget::new();
        transcript.submit_prompt("hi".to_string());
        transcript.push_response("answer");
        let calls = vec![
            "Bash(command=\"one\")".to_string(),
            "Bash(command=\"two\")".to_string(),
        ];
        transcript.reconcile_tool_calls(&calls);

        transcript.push_tool_output_chunk("c2", "Bash(command=\"two\")", "two-body\n");
        transcript.commit_tool_result("c2", "Bash(command=\"two\")");
        transcript.push_tool_output_chunk("c1", "Bash(command=\"one\")", "one-body\n");
        transcript.commit_tool_result("c1", "Bash(command=\"one\")");
        transcript.finish_turn();

        let text: Vec<String> = transcript
            .scrollable
            .visible_lines()
            .iter()
            .map(|l| l.to_string())
            .collect();
        let two = text
            .iter()
            .position(|l| l == "◆ Bash(command=\"two\")")
            .unwrap();
        let one = text
            .iter()
            .position(|l| l == "◆ Bash(command=\"one\")")
            .unwrap();
        assert!(
            two < one,
            "commit order follows tool-result order: {text:?}"
        );
        assert_eq!(text[two + 1], "    two-body");
        assert_eq!(text[one + 1], "    one-body");
    }

    #[test]
    fn strip_ansi_drops_trailing_incomplete_escape() {
        assert_eq!(strip_ansi("ok\u{1b}[31mred\u{1b}["), "okred");
    }

    #[test]
    fn strip_ansi_drops_carriage_returns_and_other_controls_but_keeps_tab() {
        assert_eq!(strip_ansi("a\r b\u{1}c\td"), "a bc\td");
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
            active_runs: EMPTY.get_or_init(HashMap::new),
            watch_selection: None,
            watch_following: None,
            watch_follow_error: None,
            completion_preview: None,
        }
    }

    #[allow(dead_code)]
    fn rendered_rows(buf: &Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[allow(dead_code)]
    fn full_rendered_rows(lines: &[Line<'static>], width: u16) -> Vec<String> {
        let height = wrapped_height(lines, width) as u16;
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);
        rendered_rows(&buf, width, height)
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

    // ── multi-line input bar ────────────────────────────────────

    #[test]
    fn full_lines_splits_on_newline_with_chevron_on_first_line_only() {
        let lines = InputBarWidget::full_lines("one\ntwo\nthree");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].to_string(), "» one");
        assert_eq!(lines[1].to_string(), "  two");
        assert_eq!(lines[2].to_string(), "  three");
    }

    #[test]
    fn full_lines_empty_input_yields_chevron_only() {
        let lines = InputBarWidget::full_lines("");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "» ");
    }

    #[test]
    fn full_lines_trailing_newline_produces_empty_continuation() {
        let lines = InputBarWidget::full_lines("a\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "» a");
        assert_eq!(lines[1].to_string(), "  ");
    }

    #[test]
    fn wrapped_rows_counts_hard_breaks() {
        let bar = InputBarWidget::new();
        // Two logical lines at a wide width → 2 rows.
        assert_eq!(bar.wrapped_rows(80, "one\ntwo", None), 2);
        // Three logical lines → 3 rows.
        assert_eq!(bar.wrapped_rows(80, "one\ntwo\nthree", None), 3);
    }

    #[test]
    fn cursor_position_on_second_logical_line() {
        // "ab\ncd" at width 10: line 0 = "» ab" (4 cells), line 1 = "  cd"
        // (4 cells). Cursor at end of input (byte 5) → end of line 1.
        assert_eq!(InputBarWidget::cursor_position(5, "ab\ncd", 10), (1, 4));
        // Cursor before 'c' (byte 3) → line 1, col 2 (after the 2-cell indent).
        assert_eq!(InputBarWidget::cursor_position(3, "ab\ncd", 10), (1, 2));
    }

    #[test]
    fn cursor_position_on_newline_parks_at_end_of_prior_line() {
        // Cursor on the '\n' (byte 2 in "ab\ncd") sits at the end of line 0.
        assert_eq!(InputBarWidget::cursor_position(2, "ab\ncd", 10), (0, 4));
    }

    #[test]
    fn cursor_position_word_wraps_within_a_logical_line() {
        // "hello world\nnext" at width 7. Line 0 wraps: "» hello" / "world".
        // Line 1 = "  next". Cursor at end of input lands after "next".
        let input = "hello world\nnext";
        let end = input.len();
        // "  next" is 6 cells, so end of line 1 is col 6, on row 2.
        assert_eq!(InputBarWidget::cursor_position(end, input, 7), (2, 6));
    }

    #[test]
    fn ensure_cursor_visible_scrolls_to_later_logical_line() {
        let mut bar = InputBarWidget::new();
        // 10 logical lines at width 80 → 10 rows. Cursor at end (row 9) must
        // scroll within a 5-row window: v_scroll = 9 - 4 = 5.
        let input = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj";
        bar.ensure_cursor_visible(input.len(), input, 80, 5);
        assert_eq!(bar.v_scroll, 5);
    }

    #[test]
    fn render_multi_line_chevron_only_on_first_row() {
        let mut bar = InputBarWidget::new();
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        let ctx = input_ctx("one\ntwo", 7);
        bar.render(area, &mut buf, &ctx);

        // Row 0 starts with the chevron glyph; row 1 starts with two spaces.
        let row0: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        let row1: String = (0..area.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(row0.starts_with('»'), "row0 = {row0:?}");
        assert!(row1.starts_with("  "), "row1 = {row1:?}");
        assert!(row0.contains("one"), "row0 = {row0:?}");
        assert!(row1.contains("two"), "row1 = {row1:?}");
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
