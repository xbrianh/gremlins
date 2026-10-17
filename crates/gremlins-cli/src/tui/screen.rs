//! Main-screen double-buffered renderer.
//!
//! [`LiveArea`] owns the primary terminal screen (no alternate screen),
//! keeping two screen-sized [`Buffer`]s and flushing only the cells that
//! change between frames.
//!
//! ## Scrollback model
//!
//! [`commit_scrollback`](LiveArea::commit_scrollback) renders a paragraph
//! of overflow lines, writes them to the terminal, and calls
//! `scroll_region_up` to push them into terminal scrollback. The prev
//! buffer is shifted so the next diff stays in sync.
//!
//! [`draw`](LiveArea::draw) handles only the diff-driven paint of the
//! visible frame. The closure paints the visible tail into `curr` —
//! the overflow content was already committed separately.

use std::collections::VecDeque;
use std::io::{self, Stdout};

use crossterm::{
    cursor, execute,
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend},
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::tui::app::App;
use crate::tui::buffer::{line_wrapped_height, wrapped_height};
use crate::tui::motif;
use crate::tui::theme::DEFAULT_THEME;
use crate::tui::widgets::{
    layout_widgets, render_wrapped_tail, DebugWidget, InfoBarWidget, InputBarWidget,
    LiveAreaWidget, RenderContext, StreamingWidget, TranscriptWidget,
};
use crate::tui::REASONING_PLACEHOLDER_TITLE;

/// Approximate number of rows reserved at the bottom of the screen for the
/// input and info bars before the first layout pass runs. The real value is
/// tracked each frame from the live widget stack.
const INITIAL_RESERVED_ROWS: u16 = 2;

/// A fresh reasoning band: the shared reasoning stream configured with the
/// transcript's one-cell left inset so its title and body align with the
/// transcript's own inner margin. Constructed identically in
/// [`Scene::default`] and [`LiveArea::finish_turn`] so the two cannot drift.
fn new_reasoning_band() -> StreamingWidget {
    StreamingWidget::reasoning(TranscriptWidget::STREAM_MAX_ROWS).with_left_margin(1)
}

/// The typed widget owner.
///
/// Chat widgets live in concrete fields, and full-screen bodies live behind
/// [`ScreenView`]. [`LiveArea::compose`] takes the scene out temporarily,
/// erases each field to `&mut dyn LiveAreaWidget` for the recursive layout
/// engine, then puts it back. The renderer only calls trait methods.
struct Scene {
    transcript: TranscriptWidget,
    reasoning: StreamingWidget,
    input_bar: InputBarWidget,
    info_bar: InfoBarWidget,
    screen_view: Option<ScreenView>,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            transcript: TranscriptWidget::new(),
            reasoning: new_reasoning_band(),
            input_bar: InputBarWidget::new(),
            info_bar: InfoBarWidget,
            screen_view: None,
        }
    }
}

/// Full-screen view bodies, typed by the screen they belong to.
enum ScreenView {
    Runs(RunsView),
    Debug(DebugWidget),
}

/// Owns the main terminal screen with diff-driven double buffering.
pub struct LiveArea<B: Backend = CrosstermBackend<Stdout>> {
    backend: B,
    curr: Buffer,
    prev: Buffer,
    cols: u16,
    rows: u16,
    /// Lines committed to terminal scrollback since creation (captured
    /// for test inspection). Each entry is one terminal row, trailing
    /// whitespace trimmed.
    scrollback_log: Vec<String>,
    /// Whether [`Drop`] should restore the terminal (raw mode, cursor).
    needs_terminal_cleanup: bool,
    /// Typed widget owner; taken out per frame so chat widgets or the active
    /// full-screen body can be erased into an ordered slice of
    /// `&mut dyn LiveAreaWidget` for the shared recursive layout engine.
    scene: Scene,
    /// Whether the previous composed screen was chat. Used to clear the
    /// visible frame exactly once when leaving a full-screen view.
    last_screen_was_chat: bool,
    /// Rows occupied by the bottom bars (input + info) in the most recent
    /// frame. These rows are excluded from the scrollback region.
    reserved_rows: u16,
    /// Whether the reasoning title currently carries a `TurnComplete` stats
    /// line. Set by [`set_reasoning_title`], cleared by `reset_reasoning` and
    /// [`push_reasoning_text`] so a stale prior-turn stats header never sits
    /// over the next agent-loop turn's reasoning.
    reasoning_stats_shown: bool,
}

impl<B: Backend> LiveArea<B> {
    /// Create a `LiveArea` backed by an arbitrary `Backend`.
    ///
    /// Does not enter raw mode or touch the terminal. Buffers are
    /// allocated for the given dimensions.
    pub fn with_backend(backend: B, cols: u16, rows: u16) -> Self {
        let area = Rect::new(0, 0, cols, rows);
        Self {
            backend,
            curr: Buffer::empty(area),
            prev: Buffer::empty(area),
            cols,
            rows,
            scrollback_log: Vec::new(),
            needs_terminal_cleanup: false,
            scene: Scene::default(),
            last_screen_was_chat: true,
            reserved_rows: INITIAL_RESERVED_ROWS,
            reasoning_stats_shown: false,
        }
    }

    /// Commit overflow content to terminal scrollback.
    ///
    /// Renders `lines` into a paragraph at `width`, writes the rendered
    /// cells to the terminal at the top of the scroll region, then calls
    /// `scroll_region_up` to push them into scrollback. The prev buffer
    /// is shifted so the next diff stays in sync.
    ///
    /// Returns the number of rows committed (the wrapped height).
    pub fn commit_scrollback(
        &mut self,
        lines: &[ratatui::text::Line<'static>],
        width: u16,
    ) -> io::Result<u16> {
        use ratatui::widgets::{Paragraph, Widget, Wrap};

        let height = {
            let h = Paragraph::new(lines.to_vec())
                .wrap(Wrap { trim: false })
                .line_count(width);
            if h == 0 {
                return Ok(0);
            }
            h.min(u16::MAX as usize) as u16
        };

        log::debug!(
            "LiveArea::commit_scrollback rows={height} lines={}",
            lines.len()
        );

        // Render the overflow content into a temporary buffer.
        let area = Rect::new(0, 0, width, height);
        let mut tmp = Buffer::empty(area);
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .render(area, &mut tmp);

        #[cfg(feature = "telemetry")]
        {
            let fingerprints: Vec<String> = (0..height)
                .map(|y| {
                    let row: String = (0..width).map(|x| tmp[(x, y)].symbol()).collect();
                    let trimmed = row.trim_end();
                    let preview: String = trimmed.chars().take(40).collect();
                    if trimmed.len() > 40 {
                        format!("{}|{}c", preview, trimmed.len())
                    } else {
                        format!("{}|{}c", preview, trimmed.len())
                    }
                })
                .collect();
            log::info!(
                "SCROLLBACK_COMMIT rows={height} cells=[{}]",
                fingerprints.join(" ; "),
            );
        }

        // The terminal scroll region is only `region_rows` tall, so a
        // single draw+scroll can commit at most that many rows. Commit the
        // overflow in chunks so a response taller than the viewport is
        // preserved in full instead of being clipped.
        let region_rows = self.scrollable_rows();
        let mut committed = 0u16;
        while committed < height {
            let chunk = (height - committed).min(region_rows);
            if chunk == 0 {
                break;
            }

            // Write this chunk at the top of the scroll region.
            let mut cells: Vec<(u16, u16, &Cell)> = Vec::new();
            for y in 0..chunk {
                for x in 0..width {
                    cells.push((x, y, &tmp[(x, committed + y)]));
                }
            }
            self.backend
                .draw(cells.into_iter())
                .map_err(|e| io::Error::other(e.to_string()))?;

            // Push the chunk into terminal scrollback.
            self.backend
                .scroll_region_up(0..region_rows, chunk)
                .map_err(|e| io::Error::other(e.to_string()))?;

            // Keep prev buffer in lockstep with the terminal.
            shift_buffer_up(&mut self.prev, region_rows, chunk);

            // Capture for tests.
            for y in 0..chunk {
                let row: String = (0..width as usize)
                    .map(|x| tmp[(x as u16, committed + y)].symbol())
                    .collect();
                self.scrollback_log.push(trim_trailing_whitespace(&row));
            }

            committed += chunk;
        }

        self.backend
            .flush()
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(committed)
    }

    /// Render one frame.
    ///
    /// The closure paints the visible frame into the current buffer.
    /// Overflow must already have been committed via
    /// [`commit_scrollback`](Self::commit_scrollback) before this call.
    pub fn draw(&mut self, f: impl FnOnce(&mut Buffer)) -> io::Result<()> {
        f(&mut self.curr);

        // Diff-driven paint: only cells that changed from the previous
        // frame (post-scrollback shift) are sent to the terminal.
        let diff: Vec<(u16, u16, &Cell)> = self.prev.diff_iter(&self.curr).collect();
        self.backend
            .draw(diff.into_iter())
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.backend
            .flush()
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Swap buffers and clear the new "current" for the next frame.
        std::mem::swap(&mut self.curr, &mut self.prev);
        self.curr.reset();
        Ok(())
    }

    /// Current terminal dimensions.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Force the next diff to repaint every cell of the current frame.
    pub fn invalidate(&mut self) {
        self.prev.reset();
    }

    /// Clear the visible terminal frame immediately.
    ///
    /// Used when leaving a full-screen view: chat widgets paint only their
    /// own sub-rects, so the old full-screen cells in the gaps must be wiped
    /// before the next chat frame is composed.
    fn clear_visible(&mut self) -> io::Result<()> {
        self.curr.reset();
        self.draw(|_| {})
    }

    /// Lines committed to terminal scrollback since creation.
    #[cfg(test)]
    pub fn scrollback(&self) -> &[String] {
        &self.scrollback_log
    }

    /// Borrow the inner backend (e.g. to read a [`TestBackend`] buffer).
    #[cfg(test)]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Rows that participate in scrollback. The bottom bars (input + info)
    /// are kept out of the scrolling region; their combined height is
    /// tracked each frame in [`Self::reserved_rows`].
    pub fn scrollable_rows(&self) -> u16 {
        self.rows.saturating_sub(self.reserved_rows)
    }

    /// Compose and paint the active screen.
    ///
    /// Chat composes the three chat widgets (transcript, input, info) with
    /// terminal-scrollback overflow. Any other [`Screen`] composes just the
    /// full-screen view body; chat widgets are occluded and no overflow is
    /// committed.
    pub fn compose(&mut self, app: &App) -> io::Result<()> {
        let chat = app.screen.is_chat();

        // When returning from a full-screen view, clear the visible frame
        // first. Chat widgets paint only their own sub-rects, so any gaps
        // left unpainted would otherwise retain runs/log/debug text.
        if chat && !self.last_screen_was_chat {
            self.clear_visible()?;
        }
        self.last_screen_was_chat = chat;

        // Take the typed scene out temporarily so the concrete fields can be
        // erased into `&mut dyn LiveAreaWidget` without borrowing through
        // `self`, then put it back once the frame is finished.
        let mut scene = std::mem::take(&mut self.scene);
        let result = if chat {
            self.compose_chat(&mut scene, app)
        } else {
            self.compose_screen_view(&mut scene, app)
        };
        self.scene = scene;
        result
    }

    fn render_context<'a>(&self, app: &'a App, gremlin_count: &'a str) -> RenderContext<'a> {
        RenderContext {
            input: &app.input,
            cursor: app.input_cursor,
            following_log: app.following_log.as_deref(),
            gremlin_count,
            project_name: app.project_name_str(),
            active_runs: &app.active_runs,
            watch_selection: app.watch_selection.as_deref(),
            watch_following: app.watch_following.as_deref(),
            watch_follow_error: app.watch_follow_error.as_deref(),
            completion_preview: app.completion_preview_suffix(),
        }
    }

    /// Compose the ordered chat stack: transcript, reasoning, input bar,
    /// info bar. The transcript absorbs leftover rows; the reasoning band is
    /// fixed-height and collapses to zero when idle, pinning it directly
    /// above the input and info bars.
    fn compose_chat(&mut self, scene: &mut Scene, app: &App) -> io::Result<()> {
        let gremlin_count = app.gremlin_count_str();
        let ctx = self.render_context(app, &gremlin_count);
        let width = self.cols;
        let mut widgets: Vec<&mut dyn LiveAreaWidget> = vec![
            &mut scene.transcript,
            &mut scene.reasoning,
            &mut scene.input_bar,
            &mut scene.info_bar,
        ];
        let layout = layout_widgets(widgets.as_mut_slice(), width, self.rows, &ctx);
        self.reserved_rows = layout
            .heights
            .get(2)
            .copied()
            .unwrap_or(0)
            .saturating_add(layout.heights.get(3).copied().unwrap_or(0));
        self.finish_compose(
            widgets.as_mut_slice(),
            &layout.heights,
            layout.overflow,
            &ctx,
            true,
        )
    }

    /// Compose the active full-screen view body. The chat scene is occluded
    /// and no overflow is committed.
    fn compose_screen_view(&mut self, scene: &mut Scene, app: &App) -> io::Result<()> {
        let gremlin_count = app.gremlin_count_str();
        let ctx = self.render_context(app, &gremlin_count);
        let width = self.cols;
        // The `ScreenView` invariant is maintained by the paired
        // `app.show_*` / `live_area.show_*` calls in `mod.rs`. If those ever
        // desync, fall back to composing chat instead of panicking in raw
        // mode (a missed pairing in a future edit must not take the
        // terminal down).
        let mut widget: &mut dyn LiveAreaWidget = match scene.screen_view.as_mut() {
            Some(view) => match view {
                ScreenView::Runs(w) => w,
                ScreenView::Debug(w) => w,
            },
            None => {
                log::warn!("compose_screen_view without an active screen view; composing chat");
                return self.compose_chat(scene, app);
            }
        };
        let widgets = std::slice::from_mut(&mut widget);
        let layout = layout_widgets(&mut *widgets, width, self.rows, &ctx);
        self.reserved_rows = 0;
        self.finish_compose(&mut *widgets, &layout.heights, layout.overflow, &ctx, false)
    }

    /// Paint the widgets whose layout has already been resolved and commit
    /// terminal scrollback overflow when `chat_layout` is true.
    fn finish_compose(
        &mut self,
        widgets: &mut [&mut dyn LiveAreaWidget],
        heights: &[u16],
        overflow: Vec<(i32, Line<'static>)>,
        ctx: &RenderContext,
        chat_layout: bool,
    ) -> io::Result<()> {
        if chat_layout && !overflow.is_empty() {
            let lines: Vec<Line<'static>> = overflow.into_iter().map(|(_, line)| line).collect();
            self.commit_scrollback(&lines, self.cols)?;
        }

        let width = self.cols;
        let mut y = 0u16;
        for (i, widget) in widgets.iter_mut().enumerate() {
            let h = heights[i];
            if h == 0 {
                continue;
            }
            let chunk = Rect::new(0, y, width, h);
            y += h;
            widget.render(chunk, &mut self.curr, ctx);
        }

        self.draw(|_| {})
    }

    // ── Full-screen view switch ──────────────────────────────

    /// Show the full-screen runs table.
    pub fn show_runs(&mut self) {
        self.scene.screen_view = Some(ScreenView::Runs(RunsView::new()));
    }

    /// Show the full-screen interactive debug session.
    pub fn show_debug(&mut self, id: String) {
        self.scene.screen_view = Some(ScreenView::Debug(DebugWidget::new(id)));
    }

    /// Drop the active full-screen view and return the renderer to chat.
    pub fn exit_screen_view(&mut self) {
        self.scene.screen_view = None;
    }

    // ── Widget accessors ─────────────────────────────────────────

    fn transcript_mut(&mut self) -> &mut TranscriptWidget {
        &mut self.scene.transcript
    }

    fn reasoning_mut(&mut self) -> &mut StreamingWidget {
        &mut self.scene.reasoning
    }

    /// Clear the live reasoning band's title and body for a new turn, and
    /// drop the "stats shown" marker so the next reasoning chunk installs the
    /// `clanking` placeholder.
    fn reset_reasoning(&mut self) {
        self.reasoning_mut().set_title("");
        self.reasoning_mut().clear();
        self.reasoning_stats_shown = false;
    }

    fn input_bar_mut(&mut self) -> &mut InputBarWidget {
        &mut self.scene.input_bar
    }

    fn runs_view_mut(&mut self) -> Option<&mut RunsView> {
        match self.scene.screen_view.as_mut() {
            Some(ScreenView::Runs(w)) => Some(w),
            _ => None,
        }
    }

    fn debug_mut(&mut self) -> Option<&mut DebugWidget> {
        match self.scene.screen_view.as_mut() {
            Some(ScreenView::Debug(w)) => Some(w),
            _ => None,
        }
    }

    // ── Widget mutation (moved from `App`) ───────────────────────

    /// Start a new turn with the given prompt: push a separator and the
    /// markdown-rendered prompt, then pin the turn start. The live reasoning
    /// band and the transcript's tool slots are cleared with the new prompt.
    pub fn submit_prompt(&mut self, prompt: String) {
        self.reset_reasoning();
        self.transcript_mut().submit_prompt(prompt);
    }

    /// Reconcile the input bar's scroll offsets so the cursor stays visible
    /// after a cursor move or text edit.
    pub fn ensure_input_cursor_visible(&mut self, cursor: usize, input: &str, width: u16) {
        let rows = self.input_bar_mut().wrapped_rows(width, input, None).min(5);
        self.input_bar_mut()
            .ensure_cursor_visible(cursor, input, width, rows);
    }

    /// Reset the input bar's scroll offsets (e.g. after history recall).
    pub fn reset_input_scroll(&mut self) {
        self.input_bar_mut().reset_scroll();
    }

    /// Flush the reasoning partial buffer (if any) into a completed
    /// reasoning line.
    #[allow(dead_code)]
    pub fn flush_reasoning(&mut self) {
        self.reasoning_mut().flush();
    }

    /// Append reasoning text (may be a partial chunk). The first chunk of a
    /// turn establishes the `clanking` placeholder title so the live band
    /// shows its header immediately instead of waiting for `TurnComplete`.
    pub fn push_reasoning_text(&mut self, text: &str) {
        // A reasoning chunk after a `TurnComplete` belongs to the next
        // agent-loop turn: reset the stats header back to the placeholder so
        // the prior turn's numbers never sit over the new turn's reasoning.
        let stats_shown = self.reasoning_stats_shown;
        self.reasoning_stats_shown = false;
        let reasoning = self.reasoning_mut();
        if stats_shown && !reasoning.title_is_empty() {
            reasoning.set_title(REASONING_PLACEHOLDER_TITLE);
        }
        if reasoning.title_is_empty() {
            reasoning.set_title(REASONING_PLACEHOLDER_TITLE);
        }
        reasoning.push_text(text);
    }

    /// Replace the reasoning stream's title row with the live
    /// token/cache stats string.
    pub fn set_reasoning_title(&mut self, title: &str) {
        self.reasoning_mut().set_title(title);
        self.reasoning_stats_shown = true;
    }

    /// Append a live tool-output chunk to the transcript's tool stream keyed
    /// by tool-call id.
    pub fn push_tool_output_chunk(&mut self, id: &str, call: &str, text: &str) {
        self.transcript_mut().push_tool_output_chunk(id, call, text);
    }

    /// Commit the finished tool stream (or a single `◆` event line for a
    /// non-streaming tool) into the transcript's permanent tool segment.
    pub fn commit_tool_result(&mut self, id: &str, call: &str) {
        self.transcript_mut().commit_tool_result(id, call);
    }

    /// Seed the current turn's tool-call list for exactly-once completion
    /// reconciliation.
    pub fn reconcile_tool_calls(&mut self, calls: &[String]) {
        self.transcript_mut().reconcile_tool_calls(calls);
    }

    /// Clear the embedded tool streams at a turn boundary.
    pub fn clear_tool_output(&mut self) {
        self.transcript_mut().clear_tool_output();
    }

    /// Record a tool call with a bare `◆` name line. Production routing
    /// uses `commit_tool_result`; this direct insertion path exists for
    /// tests and future non-streaming callers.
    #[cfg(test)]
    pub fn push_tool(&mut self, name: &str) {
        self.transcript_mut().push_tool_line(name);
    }

    /// Append streaming response text.
    pub fn push_response(&mut self, text: &str) {
        self.transcript_mut().push_response(text);
    }

    /// Replace the entire response text (used for Done reconciliation).
    pub fn replace_response(&mut self, text: &str) {
        self.transcript_mut().replace_response(text);
    }

    /// Reconcile the current turn's streamed response against the
    /// authoritative full text from `TurnComplete`.
    pub fn reconcile_response(&mut self, text: &str) {
        self.transcript_mut().reconcile_response(text);
    }

    /// Unpin the current turn and clear its draft state. The live reasoning
    /// band is replaced with a fresh title-less stream so it collapses and its
    /// body is discarded; after the transcript has rebuilt the response and
    /// reconciled pending tool results, the band's title line is committed as
    /// the final FIFO line of the finished turn.
    pub fn finish_turn(&mut self) {
        let stream = std::mem::replace(&mut self.scene.reasoning, new_reasoning_band());
        self.reasoning_stats_shown = false;
        let had_turn = self.transcript_mut().has_current_turn();
        self.transcript_mut().finish_turn();
        if had_turn {
            self.transcript_mut().commit_reasoning_title(stream);
        }
    }

    /// Whether the current turn has any content.
    pub fn has_current_turn(&self) -> bool {
        self.scene.transcript.has_current_turn()
    }

    /// Whether the current turn's streaming response is empty.
    pub fn response_is_empty(&self) -> bool {
        self.scene.transcript.response_is_empty()
    }

    /// Borrow the current turn's streaming response text.
    #[cfg(test)]
    pub fn response_text(&self) -> &str {
        self.scene.transcript.response_accumulator()
    }

    /// Finish the current turn's response at `Done` time.
    ///
    /// `text` is the authoritative final model text (the `Done` tool's
    /// summary when nothing was streamed, otherwise the streamed response).
    /// `summary` is the `Done` tool's `summary` argument, appended to the
    /// response so it is displayed as part of the finished turn.
    ///
    /// The summary is skipped when it is empty or when it already equals the
    /// text now on screen (e.g. the fallback above used it as the response
    /// body), so it is never duplicated.
    pub fn finish_response(&mut self, text: &str, summary: &str) {
        if self.response_is_empty() && !text.is_empty() {
            self.replace_response(text);
        }
        if !summary.is_empty() && summary != text {
            self.push_response("\n\n");
            self.push_response(summary);
        }
    }

    /// Discard the current turn without committing it to the transcript.
    pub fn clear_current_turn(&mut self) {
        self.reset_reasoning();
        self.transcript_mut().clear_current_turn();
    }

    /// Clear the transcript/scrollback, including the live reasoning band and
    /// tool slots.
    pub fn clear_transcript(&mut self) {
        self.reset_reasoning();
        self.transcript_mut().clear_scrollable();
    }

    /// Append a system message to the transcript with default style and
    /// blank-line padding from the previous entry.
    pub fn push_transcript_line(&mut self, line: String) {
        self.transcript_mut().push_transcript_line(line);
    }

    /// Append a block of system messages to the transcript with default
    /// style, applying blank-line padding once before the block.
    pub fn push_transcript_block(&mut self, block: Vec<String>) {
        self.transcript_mut().push_transcript_block(block);
    }

    /// Append a pre-styled line to the transcript (e.g. the startup spire).
    pub fn push_styled_line(&mut self, line: ratatui::text::Line<'static>) {
        self.transcript_mut().push_styled_line(line);
    }

    // ── Full-screen view mutation ───────────────────────────────

    /// Push a log line into the watch stream pane, if present.
    pub fn push_watch_line(&mut self, line: String) {
        if let Some(w) = self.runs_view_mut() {
            w.push_stream_line(line);
        }
    }

    /// Clear the watch stream pane's log buffer.
    pub fn clear_watch_stream(&mut self) {
        if let Some(w) = self.runs_view_mut() {
            w.clear_stream();
        }
    }

    /// Append output to the full-screen debug view, if present.
    pub fn push_debug_output(&mut self, line: String) {
        if let Some(w) = self.debug_mut() {
            w.push_output(line);
        }
    }

    /// Borrow the debug prompt, if the debug view is present.
    pub fn debug_prompt_mut(&mut self) -> Option<&mut String> {
        self.debug_mut().map(|w| w.prompt_mut())
    }

    /// Take the debug prompt for submission, if the debug view is present.
    pub fn take_debug_prompt(&mut self) -> Option<String> {
        self.debug_mut().map(|w| w.take_prompt())
    }

    /// Whether the debug view is still connecting.
    pub fn debug_connecting(&self) -> bool {
        match self.scene.screen_view.as_ref() {
            Some(ScreenView::Debug(w)) => w.connecting(),
            _ => false,
        }
    }

    /// Whether the debug view is still connected.
    pub fn debug_connected(&self) -> bool {
        match self.scene.screen_view.as_ref() {
            Some(ScreenView::Debug(w)) => w.connected(),
            _ => false,
        }
    }

    /// Set the connected flag on the debug view.
    pub fn set_debug_connected(&mut self, connected: bool) {
        if let Some(w) = self.debug_mut() {
            w.set_connected(connected);
        }
    }

    /// Mark the debug view as connected.
    pub fn mark_debug_ready(&mut self) {
        if let Some(w) = self.debug_mut() {
            w.mark_ready();
        }
    }
}

impl LiveArea<CrosstermBackend<Stdout>> {
    /// Enter raw mode, hide the cursor, clear the main screen, and allocate
    /// buffers for the current terminal size.
    pub fn new() -> io::Result<Self> {
        let mut guard = RawModeGuard::enter()?;
        execute!(io::stdout(), cursor::Hide)?;

        let (cols, rows) = size()?;
        let mut live = Self::with_backend(CrosstermBackend::new(io::stdout()), cols, rows);
        live.needs_terminal_cleanup = true;

        live.backend.clear_region(ClearType::All)?;
        guard.disarm();
        Ok(live)
    }

    /// Reallocate both buffers for a new terminal size and clear the screen.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.cols = cols;
        self.rows = rows;
        self.scrollback_log.clear();

        let area = Rect::new(0, 0, cols, rows);
        self.curr = Buffer::empty(area);
        self.prev = Buffer::empty(area);
        self.backend = CrosstermBackend::new(io::stdout());
        self.backend.clear_region(ClearType::All)
    }
}

/// Trim trailing whitespace from a single row captured for scrollback.
fn trim_trailing_whitespace(s: &str) -> String {
    s.trim_end().to_string()
}

/// Scroll the top `region_rows` rows of `buf` up by `amount`, clearing the
/// bottom `amount` rows of the region.
fn shift_buffer_up(buf: &mut Buffer, region_rows: u16, amount: u16) {
    let width = buf.area.width as usize;
    for y in 0..region_rows {
        let src = y + amount;
        for x in 0..width {
            let x = x as u16;
            if src < region_rows {
                let cell = buf[(x, src)].clone();
                buf[(x, y)] = cell;
            } else {
                buf[(x, y)].reset();
            }
        }
    }
}

/// Restores raw mode and cursor visibility if initialization fails partway.
struct RawModeGuard(bool);

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self(true))
    }

    fn disarm(&mut self) {
        self.0 = false;
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.0 {
            execute!(io::stdout(), cursor::Show).ok();
            disable_raw_mode().ok();
        }
    }
}

impl<B: Backend> Drop for LiveArea<B> {
    fn drop(&mut self) {
        if self.needs_terminal_cleanup {
            execute!(io::stdout(), cursor::Show).ok();
            disable_raw_mode().ok();
        }
    }
}

/// Minimum rows kept for the inline stream body so the wrapped header cannot
/// crowd the live tail out of a short or narrow terminal.
const WATCH_STREAM_MIN_BODY_ROWS: u16 = 3;

/// Maximum number of raw log lines retained in the inline stream pane. The
/// renderer still tail-slices by wrapped rows, but this bound keeps the
/// per-frame mapping cost independent of how long the pane has been open.
const WATCH_STREAM_MAX_LINES: usize = 2_000;

/// Full-screen gremlins watch table with an optional inline stream pane.
///
/// The table shows all tracked gremlins — running first, then done/failed/
/// stopped — and sorts each group by id so the selection is stable regardless
/// of `HashMap` iteration order. When `watch_following` is set, the view owns
/// an internal vertical split and paints a tail-anchored live-log pane for
/// the followed id.
pub struct RunsView {
    stream_lines: VecDeque<String>,
}

impl RunsView {
    pub fn new() -> Self {
        Self {
            stream_lines: VecDeque::new(),
        }
    }

    /// Append a line to the inline stream pane, dropping the oldest raw lines
    /// once the bounded buffer is full.
    pub fn push_stream_line(&mut self, line: String) {
        self.stream_lines.push_back(line);
        while self.stream_lines.len() > WATCH_STREAM_MAX_LINES {
            self.stream_lines.pop_front();
        }
    }

    /// Clear the inline stream pane's log buffer.
    pub fn clear_stream(&mut self) {
        self.stream_lines.clear();
    }

    fn listed_runs<'a>(
        ctx: &'a RenderContext,
    ) -> Vec<(&'a String, &'a crate::tui::app::ActiveRun)> {
        let mut runs: Vec<_> = ctx.active_runs.iter().collect();
        runs.sort_by(|a, b| {
            let running = |r: &(&String, &crate::tui::app::ActiveRun)| r.1.status != "running";
            running(a).cmp(&running(b)).then_with(|| a.0.cmp(b.0))
        });
        runs
    }

    fn run_row(id: &str, run: &crate::tui::app::ActiveRun, selected: bool) -> Line<'static> {
        let glyph = match run.status.as_str() {
            "running" => motif::glyph::RUNNING,
            "done" => motif::glyph::DONE,
            "failed" => motif::glyph::FAILED,
            "stopped" => motif::glyph::STOPPED,
            _ => motif::glyph::DONE,
        };
        let text = if selected {
            format!(
                "▶ {id}  {glyph} {}  {}  {}",
                run.status, run.stage, run.definition
            )
        } else {
            format!(
                "  {id}  {glyph} {}  {}  {}",
                run.status, run.stage, run.definition
            )
        };
        let style = if selected {
            Style::default()
                .fg(DEFAULT_THEME.midnight)
                .bg(DEFAULT_THEME.cursor)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DEFAULT_THEME.text)
        };
        Line::from(Span::styled(text, style))
    }

    fn render_table(&self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)].reset();
            }
        }
        if area.width == 0 || area.height == 0 {
            return;
        }

        let header_style = Style::default()
            .fg(DEFAULT_THEME.heading_primary)
            .add_modifier(Modifier::BOLD);
        let header_lines = vec![
            Line::from(Span::styled("  g r e m l i n s", header_style)),
            motif::streamline_rule_brass(area.width),
        ];
        let header_wrapped = wrapped_height(&header_lines, area.width) as u16;

        let runs = Self::listed_runs(ctx);
        let running = runs.iter().filter(|(_, r)| r.status == "running").count();
        let total = ctx.active_runs.len();
        let footer_line = Line::from(Span::styled(
            format!("  {running} running · {total} total"),
            Style::default()
                .fg(DEFAULT_THEME.accent_secondary)
                .add_modifier(Modifier::BOLD),
        ));
        let footer_wrapped = line_wrapped_height(&footer_line, area.width) as u16;

        // Pin the header and footer, and give the run list the space between
        // them. On a one-row viewport the footer wins and the list disappears.
        let footer_rows = footer_wrapped.min(area.height);
        let header_rows = header_wrapped.min(area.height.saturating_sub(footer_rows));
        let body_rows = area
            .height
            .saturating_sub(header_rows)
            .saturating_sub(footer_rows);

        Paragraph::new(header_lines)
            .wrap(Wrap { trim: false })
            .render(Rect::new(area.x, area.y, area.width, header_rows), buf);

        let body_area = Rect::new(
            area.x,
            area.y.saturating_add(header_rows),
            area.width,
            body_rows,
        );
        if runs.is_empty() {
            Paragraph::new(vec![Line::from(Span::styled(
                "  no gremlins",
                Style::default().fg(DEFAULT_THEME.subtle),
            ))])
            .wrap(Wrap { trim: false })
            .render(body_area, buf);
        } else {
            // Scroll a contiguous window of running rows so the highlighted
            // id stays visible even when the sorted list is taller than the
            // table body.
            let selected_idx = ctx
                .watch_selection
                .and_then(|selected| runs.iter().position(|(id, _)| (*id).as_str() == selected))
                .unwrap_or(0);
            let run_lines: Vec<Line<'static>> = runs
                .iter()
                .map(|(id, run)| {
                    Self::run_row(
                        (*id).as_str(),
                        run,
                        ctx.watch_selection == Some((*id).as_str()),
                    )
                })
                .collect();

            let mut prefix = Vec::with_capacity(run_lines.len() + 1);
            prefix.push(0usize);
            for line in &run_lines {
                let next =
                    prefix.last().copied().unwrap_or(0) + line_wrapped_height(line, area.width);
                prefix.push(next);
            }

            let visible_rows = body_rows as usize;
            let mut start = 0usize;
            while start < selected_idx && prefix[selected_idx + 1] - prefix[start] > visible_rows {
                start += 1;
            }
            let mut end = start;
            while end < run_lines.len() && prefix[end + 1] - prefix[start] <= visible_rows {
                end += 1;
            }
            // Always include at least the selected row, even if it wraps
            // beyond the available body height.
            if end == start && start < run_lines.len() {
                end = start + 1;
            }

            Paragraph::new(run_lines[start..end].to_vec())
                .wrap(Wrap { trim: false })
                .render(body_area, buf);
        }

        if footer_rows > 0 {
            let footer_area = Rect::new(
                area.x,
                area.y.saturating_add(header_rows).saturating_add(body_rows),
                area.width,
                footer_rows,
            );
            Paragraph::new(vec![footer_line])
                .wrap(Wrap { trim: false })
                .render(footer_area, buf);
        }
    }

    fn render_stream(&self, area: Rect, buf: &mut Buffer, ctx: &RenderContext, id: &str) {
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)].reset();
            }
        }
        if area.width == 0 || area.height == 0 {
            return;
        }

        let status = ctx
            .active_runs
            .get(id)
            .map(|run| (run.status.as_str(), run.stage.as_str()))
            .unwrap_or(("unknown", ""));
        let finished = status.0 != "running";
        let stream_error = ctx.watch_follow_error;

        let header_label = if stream_error.is_some() {
            "◆ stream error: "
        } else {
            "◆ following: "
        };
        let mut header_spans = vec![
            Span::styled(
                header_label,
                Style::default()
                    .fg(DEFAULT_THEME.heading_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                id.to_string(),
                Style::default()
                    .fg(DEFAULT_THEME.danger)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(selected) = ctx.watch_selection {
            header_spans.push(Span::styled(
                "  selected: ",
                Style::default().fg(DEFAULT_THEME.subtle),
            ));
            header_spans.push(Span::styled(
                selected.to_string(),
                Style::default().fg(DEFAULT_THEME.chrome),
            ));
        }

        let status_line = Line::from(vec![
            Span::styled("  status: ", Style::default().fg(DEFAULT_THEME.subtle)),
            Span::styled(
                status.0.to_string(),
                match status.0 {
                    "running" => Style::default().fg(DEFAULT_THEME.success),
                    "failed" => Style::default().fg(DEFAULT_THEME.danger),
                    _ => Style::default().fg(DEFAULT_THEME.text),
                },
            ),
            Span::styled("  stage: ", Style::default().fg(DEFAULT_THEME.subtle)),
            Span::styled(
                status.1.to_string(),
                Style::default().fg(DEFAULT_THEME.subtle),
            ),
        ]);

        let header_lines = vec![Line::from(header_spans), status_line];
        let header_wrapped = wrapped_height(&header_lines, area.width) as u16;

        let mut footer_lines: Vec<Line<'static>> = Vec::new();
        if let Some(message) = stream_error {
            footer_lines.push(Line::from(Span::styled(
                format!("  ◆ disconnected: {message}"),
                Style::default()
                    .fg(DEFAULT_THEME.danger)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if finished {
            footer_lines.push(Line::from(Span::styled(
                "  ◆ stream finished",
                Style::default()
                    .fg(DEFAULT_THEME.accent_secondary)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        let footer_wrapped = if footer_lines.is_empty() {
            0
        } else {
            wrapped_height(&footer_lines, area.width) as u16
        };

        // Reserve a minimum body band for the log tail, then pin the footer
        // and let the wrapped header take whatever remains.
        let footer_rows = footer_wrapped.min(area.height);
        let max_header_rows = area
            .height
            .saturating_sub(footer_rows)
            .saturating_sub(WATCH_STREAM_MIN_BODY_ROWS);
        let header_rows = header_wrapped.min(max_header_rows);
        let body_rows = area
            .height
            .saturating_sub(header_rows)
            .saturating_sub(footer_rows);

        if header_rows > 0 {
            Paragraph::new(header_lines)
                .wrap(Wrap { trim: false })
                .render(Rect::new(area.x, area.y, area.width, header_rows), buf);
        }

        let body_area = Rect::new(
            area.x,
            area.y.saturating_add(header_rows),
            area.width,
            body_rows,
        );
        let log_lines: Vec<Line<'static>> = self
            .stream_lines
            .iter()
            .map(|line| {
                Line::from(Span::styled(
                    line.clone(),
                    Style::default().fg(DEFAULT_THEME.text),
                ))
            })
            .collect();

        // Tail-anchored by wrapped terminal rows, so a long wrapping entry
        // never pushes the newest output below the visible body.
        render_wrapped_tail(&log_lines, body_area, buf, body_rows as usize);

        if footer_rows > 0 {
            let footer_area = Rect::new(
                area.x,
                area.y.saturating_add(header_rows).saturating_add(body_rows),
                area.width,
                footer_rows,
            );
            Paragraph::new(footer_lines)
                .wrap(Wrap { trim: false })
                .render(footer_area, buf);
        }
    }
}

impl Default for RunsView {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAreaWidget for RunsView {
    #[cfg(feature = "telemetry")]
    fn name(&self) -> &'static str {
        "runs_view"
    }

    fn size_priority(&self) -> i32 {
        50
    }

    fn min_height(&self, _width: u16, _ctx: &RenderContext) -> u16 {
        0
    }

    fn flexible(&self, _ctx: &RenderContext) -> bool {
        true
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &RenderContext) {
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)].reset();
            }
        }
        if area.width == 0 || area.height == 0 {
            return;
        }

        match ctx.watch_following {
            Some(following) if area.width >= 2 => {
                let left_width = area.width / 2;
                let right_width = area.width - left_width;
                let left_area = Rect::new(area.x, area.y, left_width, area.height);
                let right_area = Rect::new(area.x + left_width, area.y, right_width, area.height);
                self.render_table(left_area, buf, ctx);
                self.render_stream(right_area, buf, ctx, following);
            }
            _ => self.render_table(area, buf, ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{ActiveRun, App, Screen};
    use crate::tui::theme::DEFAULT_THEME;
    use ratatui::{backend::TestBackend, buffer::Buffer, style::Style, text::Line as TuiLine};

    /// Helper: write a single text line at a given row.
    fn put_line(buf: &mut Buffer, y: u16, text: &str) {
        buf.set_string(0, y, text, Style::default());
    }

    /// Dump the backend's visible buffer as trimmed rows.
    fn dump_buffer(buf: &Buffer) -> String {
        let area = buf.area;
        let mut rows: Vec<String> = Vec::new();
        for y in 0..area.height {
            let row: String = (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>();
            rows.push(row.trim_end().to_string());
        }
        while rows.last().is_some_and(|r| r.is_empty()) {
            rows.pop();
        }
        rows.join("\n")
    }

    /// Returns the backend's internal scrollback buffer as trimmed
    /// strings, oldest first.
    fn backend_scrollback(backend: &TestBackend) -> Vec<String> {
        let sb = backend.scrollback();
        if sb.area.height == 0 {
            return Vec::new();
        }
        let w = sb.area.width as usize;
        (0..sb.area.height)
            .map(|y| {
                let row: String = (0..w).map(|x| sb[(x as u16, y)].symbol()).collect();
                trim_end_in_place(row)
            })
            .collect()
    }

    fn trim_end_in_place(mut s: String) -> String {
        let trimmed = s.trim_end();
        if trimmed.len() < s.len() {
            s.truncate(trimmed.len());
        }
        s
    }

    fn plain(s: &str) -> TuiLine<'static> {
        TuiLine::from(ratatui::text::Span::styled(
            s.to_string(),
            Style::default().fg(DEFAULT_THEME.text),
        ))
    }

    /// Create a LiveArea<TestBackend> with the given dimensions.
    fn new_live(cols: u16, rows: u16) -> LiveArea<TestBackend> {
        LiveArea::with_backend(TestBackend::new(cols, rows), cols, rows)
    }

    #[test]
    fn commit_scrollback_pushes_to_backend_scrollback() {
        let mut live = new_live(40, 5); // 3 scrollable rows

        let lines: Vec<TuiLine> = vec![plain("alpha"), plain("beta")];
        let committed = live.commit_scrollback(&lines, 40).unwrap();
        assert_eq!(committed, 2);

        let la_sb = live.scrollback();
        let be_sb = backend_scrollback(live.backend());
        assert_eq!(la_sb.len(), 2);
        assert_eq!(la_sb, be_sb);
        assert!(la_sb[0].contains("alpha"));
        assert!(la_sb[1].contains("beta"));
    }

    #[test]
    fn commit_scrollback_shifts_prev_buffer() {
        let mut live = new_live(40, 5); // 3 scrollable rows

        // Paint a frame so prev has content.
        live.draw(|buf| {
            put_line(buf, 0, "visible");
            put_line(buf, 1, "content");
            put_line(buf, 2, "here");
        })
        .unwrap();

        // Commit scrollback — prev should shift.
        let lines: Vec<TuiLine> = vec![plain("header")];
        live.commit_scrollback(&lines, 40).unwrap();

        // The prev buffer was shifted: "visible" -> "content" -> "" (blank)
        // at rows 0, 1, 2 of the scrollable region.
        assert_eq!(live.prev[(0, 0)].symbol(), "c"); // "content" shifted up
    }

    #[test]
    fn commit_scrollback_then_draw_does_not_duplicate() {
        let mut live = new_live(40, 6); // 4 scrollable rows

        // Frame 1: paint initial content.
        live.draw(|buf| {
            put_line(buf, 0, "line 0");
            put_line(buf, 1, "line 1");
            put_line(buf, 2, "line 2");
            put_line(buf, 3, "line 3");
        })
        .unwrap();

        // Commit "line 0" to scrollback.
        let lines: Vec<TuiLine> = vec![plain("line 0")];
        live.commit_scrollback(&lines, 40).unwrap();

        // Frame 2: paint tail (line 1-3 + new).
        live.draw(|buf| {
            put_line(buf, 0, "line 1");
            put_line(buf, 1, "line 2");
            put_line(buf, 2, "line 3");
            put_line(buf, 3, "line 4");
        })
        .unwrap();

        let la_sb = live.scrollback();
        let be_sb = backend_scrollback(live.backend());

        // Scrollback should contain "line 0" exactly once.
        let count = la_sb.iter().filter(|r| r.contains("line 0")).count();
        assert_eq!(
            count, 1,
            "line 0 should appear once in scrollback: {la_sb:?}"
        );

        // Visible frame should show lines 1-4.
        let buf = live.backend().buffer();
        let visible: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert!(visible.iter().any(|r| r.contains("line 4")));
        assert!(
            !visible.iter().any(|r| r.contains("line 0")),
            "line 0 should not be visible"
        );
        assert_eq!(la_sb, be_sb, "scrollback must agree");
    }

    #[test]
    fn reserved_rows_excluded_from_scrollback() {
        let mut live = new_live(50, 6); // 4 scrollable rows

        // Frame: put content in scrollable + reserved.
        live.draw(|buf| {
            put_line(buf, 0, "content 0");
            put_line(buf, 1, "content 1");
            put_line(buf, 2, "content 2");
            put_line(buf, 3, "content 3");
            put_line(buf, 4, "> input bar");
            put_line(buf, 5, "info bar");
        })
        .unwrap();

        // Commit overflow.
        let lines: Vec<TuiLine> = vec![plain("content 0")];
        live.commit_scrollback(&lines, 50).unwrap();

        let la_sb = live.scrollback();
        for line in la_sb {
            assert!(
                !line.contains("input bar"),
                "INPUT BAR LEAKED TO SCROLLBACK: {line:?}"
            );
            assert!(
                !line.contains("info bar"),
                "INFO BAR LEAKED TO SCROLLBACK: {line:?}"
            );
        }
    }

    #[test]
    fn live_tool_output_never_reaches_scrollback() {
        let mut live = new_live(40, 8);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        for i in 0..10 {
            live.push_transcript_line(format!("transcript line {i}"));
        }
        for i in 0..20 {
            live.push_tool_output_chunk(
                "t1",
                "Bash(command=\"live\")",
                &format!("tool live {i}\n"),
            );
        }

        live.compose(&app).unwrap();

        let scrollback = live.scrollback().join("\n");
        assert!(
            !scrollback.contains("tool live"),
            "live tool output must never enter terminal scrollback.\nScrollback:\n{scrollback}"
        );

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("tool live"),
            "live tool tail should remain visible.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn shift_buffer_up_moves_rows_and_clears_bottom() {
        let area = Rect::new(0, 0, 3, 4);
        let mut buf = Buffer::empty(area);
        for (y, row) in ["abc", "def", "ghi", "jkl"].iter().enumerate() {
            buf.set_string(0, y as u16, *row, Style::default());
        }

        shift_buffer_up(&mut buf, 3, 1);

        let mut expected = Buffer::empty(area);
        for (y, row) in ["def", "ghi", "   "].iter().enumerate() {
            expected.set_string(0, y as u16, *row, Style::default());
        }
        expected.set_string(0, 3, "jkl", Style::default());
        assert_eq!(buf, expected);
    }

    #[test]
    fn shift_buffer_up_clears_whole_region_when_amount_exceeds_it() {
        let area = Rect::new(0, 0, 2, 3);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "ab", Style::default());
        buf.set_string(0, 1, "cd", Style::default());
        buf.set_string(0, 2, "ef", Style::default());

        shift_buffer_up(&mut buf, 2, 5);

        let mut expected = Buffer::empty(area);
        expected.set_string(0, 2, "ef", Style::default());
        assert_eq!(buf, expected);
    }

    // ── compose() frame tests ────────────────────────────────────

    #[test]
    fn compose_in_progress_tall_response_overflows_and_tail_visible() {
        let cols: u16 = 40;
        let rows: u16 = 8;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        // Completed content before the turn.
        live_area.push_transcript_line("banner".to_string());
        live_area.submit_prompt("hi".to_string());

        // A response far taller than the viewport, still in-progress (no
        // `finish_turn`).
        let body: String = (0..40)
            .map(|i| format!("response line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        live_area.replace_response(&body);

        live_area.compose(&app).unwrap();

        // In-progress overflow now commits to terminal scrollback.
        assert!(
            !live_area.scrollback().is_empty(),
            "in-progress response taller than viewport must overflow"
        );

        // The tail (most recent content) is visible in the rendered frame;
        // the oldest content has overflowed into scrollback. The response is
        // a single wrapped line, so its head is above the visible window
        // rather than in scrollback.
        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("response line 39"),
            "the most recent response line must be visible: {visible}"
        );
        assert!(
            !visible.contains("response line 0"),
            "the oldest response line must not be visible: {visible}"
        );
        assert!(
            live_area.scrollback().iter().any(|l| l.contains("▍» hi")),
            "the prompt must overflow into scrollback: {:?}",
            live_area.scrollback()
        );
    }

    #[test]
    fn compose_short_viewport_shows_fresh_prompt_not_oldest_line() {
        let cols: u16 = 40;
        let rows: u16 = 6;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        // A completed turn that pushes content beyond the viewport.
        for i in 0..12 {
            live_area.push_transcript_line(format!("completed line {i}"));
        }
        // Fresh prompt: the tail the user just submitted.
        live_area.submit_prompt("brand new prompt".to_string());

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        let combined = format!("{}\n\n{}", live_area.scrollback().join("\n"), visible);
        assert!(
            visible.contains("brand new prompt"),
            "the freshly submitted prompt (tail) must be visible: {visible}"
        );
        assert!(
            !visible.contains("completed line 0"),
            "the oldest completed line (head) must not be visible: {visible}"
        );
        assert!(
            combined.contains("completed line 0"),
            "the oldest completed line belongs in scrollback, not dropped: {combined}"
        );
    }

    #[test]
    fn compose_tail_anchor_is_wrapping_aware() {
        let cols: u16 = 10;
        let rows: u16 = 6;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();
        // A non-empty input keeps the empty-state hint out of the input bar
        // so this test exercises transcript tail anchoring in isolation.
        app.input = "x".to_string();
        app.input_cursor = 1;

        // Two long raw lines, each longer than the 10-col width so each wraps
        // to multiple rows. The transcript gets fewer rows than the combined
        // wrapped height, so tail anchoring must show the wrapped tail of the
        // newest line rather than clipping at a raw line index.
        live_area.push_transcript_line("abcdefghijklmnopqrstuvwxyz".to_string());
        live_area.push_transcript_line("ABCDEFGHIJKLMNOPQRSTUVWXYZ".to_string());

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        // The wrapped tail of the newest line must be visible.
        assert!(
            visible.contains("WXYZ") || visible.contains("UVWXYZ"),
            "the wrapped tail of the newest line must be visible: {visible}"
        );
        // The full oldest line must not be visible: its head overflowed.
        assert!(
            !visible.contains("abcdefghijklmnopqrstuvwxyz"),
            "the oldest line's full text must not be visible: {visible}"
        );
    }

    #[test]
    fn compose_chat_response_visible_after_done() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.push_transcript_line(
            "gremlins tui — type /help for commands — Ctrl+C clears/cancels (repeat within 1s to exit)"
                .to_string(),
        );
        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking...");
        live_area.flush_reasoning();
        live_area.push_reasoning_text("The user just said hi.");
        live_area.flush_reasoning();
        live_area.push_tool("Done");
        live_area.replace_response("Greeted the user; no task was requested yet.");

        live_area.compose(&app).unwrap();

        // In a 24-row terminal, this content should not overflow.
        assert!(live_area.scrollback().is_empty());

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("Greeted the user"),
            "Response text should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("▍» hi"),
            "Prompt should be visible.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn compose_multi_frame_streaming_response_visible_after_done() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.push_transcript_line(
            "gremlins tui — type /help for commands — Ctrl+C clears/cancels (repeat within 1s to exit)"
                .to_string(),
        );

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking...");
        live_area.flush_reasoning();
        live_area.compose(&app).unwrap();

        live_area.push_reasoning_text("The user just said hi.");
        live_area.flush_reasoning();
        live_area.compose(&app).unwrap();

        live_area.push_reasoning_text("I should respond briefly.");
        live_area.flush_reasoning();
        live_area.push_tool("Done");
        live_area.compose(&app).unwrap();

        live_area.replace_response("Greeted the user; no task was requested yet.");
        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("Greeted the user"),
            "Response should be visible after Done.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("▍» hi"),
            "Prompt should be visible.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn compose_renders_prompt_reasoning_tool_response_in_order() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking\n");
        live_area.flush_reasoning();
        live_area.push_tool_output_chunk("t1", "Bash(command=\"run\")", "running a tool\n");
        live_area.replace_response("answer");

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        let lines: Vec<&str> = visible.lines().collect();
        let prompt = lines.iter().position(|l| l.contains("▍» hi"));
        let reasoning = lines.iter().position(|l| l.contains("thinking"));
        let tool = lines.iter().position(|l| l.contains("    running a tool"));
        let response = lines.iter().position(|l| l.contains("answer"));
        assert!(
            prompt.is_some() && reasoning.is_some() && tool.is_some() && response.is_some(),
            "prompt, reasoning, tool, and response should all be visible.\nBuffer:\n{visible}"
        );
        let (Some(prompt), Some(reasoning), Some(tool), Some(response)) =
            (prompt, reasoning, tool, response)
        else {
            unreachable!("checked above");
        };
        let info = lines.iter().position(|l| l.contains("◆ GREMLINS")).unwrap();
        let input = lines.iter().rposition(|l| l.contains("»")).unwrap();
        assert!(
            prompt < tool
                && tool < response
                && response < reasoning
                && reasoning + 1 == input
                && input + 1 == info,
            "a turn must render transcript (prompt → tool → response), then the bottom reasoning band directly above the input bar, then the info bar.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn compose_renders_prompt_reasoning_response_in_order() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking\n");
        live_area.flush_reasoning();
        live_area.replace_response("answer");

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        let lines: Vec<&str> = visible.lines().collect();
        let prompt = lines.iter().position(|l| l.contains("▍» hi"));
        let reasoning = lines.iter().position(|l| l.contains("thinking"));
        let response = lines.iter().position(|l| l.contains("answer"));
        assert!(
            prompt.is_some() && reasoning.is_some() && response.is_some(),
            "prompt, reasoning, and response should all be visible.\nBuffer:\n{visible}"
        );
        let (Some(prompt), Some(reasoning), Some(response)) = (prompt, reasoning, response) else {
            unreachable!("checked above");
        };
        let info = lines.iter().position(|l| l.contains("◆ GREMLINS")).unwrap();
        let input = lines.iter().rposition(|l| l.contains("»")).unwrap();
        assert!(
            prompt < response
                && response < reasoning
                && reasoning + 1 == input
                && input + 1 == info,
            "a turn must render prompt → response, then the bottom reasoning band directly above the input bar, then the info bar.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn finish_turn_commits_reasoning_title_only() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking\n");
        live_area.flush_reasoning();
        live_area.replace_response("answer");
        live_area.finish_turn();

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("answer"),
            "finished response should remain in the FIFO.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("◈ clanking"),
            "finished reasoning title should be committed to the transcript.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("    thinking"),
            "finished reasoning body must be cleared, not committed.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn reasoning_placeholder_title_gives_way_to_turn_complete_stats() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking\n");
        live_area.flush_reasoning();

        live_area.compose(&app).unwrap();
        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("◈ clanking"),
            "first reasoning chunk should surface the clanking placeholder header.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("thinking"),
            "first reasoning body should paint beside the placeholder header.\nBuffer:\n{visible}"
        );
        assert_eq!(
            visible.matches('◈').count(),
            1,
            "the placeholder header should render the reasoning glyph exactly once.\nBuffer:\n{visible}"
        );

        live_area.set_reasoning_title(
            "clanking · turn 3 · in 100 (cache 40%, +5 new) · out 50 · reason 30 · Σ out 200 · reason 120",
        );
        live_area.compose(&app).unwrap();
        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("◈ clanking · turn 3"),
            "TurnComplete stats should replace the placeholder without losing the clanking prefix.\nBuffer:\n{visible}"
        );
        assert_eq!(
            visible.matches('◈').count(),
            1,
            "the stats header should render the reasoning glyph exactly once.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn fresh_reasoning_after_stats_resets_header_to_placeholder() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("first turn thinking\n");
        live_area.flush_reasoning();
        live_area.set_reasoning_title(
            "clanking · turn 1 · in 100 (cache 40%, +5 new) · out 50 · reason 30 · Σ out 200 · reason 120",
        );
        live_area.compose(&app).unwrap();
        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("clanking · turn 1"),
            "TurnComplete stats should carry the turn-1 header.\nBuffer:\n{visible}"
        );

        // A new agent-loop iteration streams reasoning under the previous
        // turn's stats header. The header must reset to the placeholder so
        // stale turn-1 numbers never sit over the new turn's reasoning.
        live_area.push_reasoning_text("second turn thinking\n");
        live_area.flush_reasoning();
        live_area.compose(&app).unwrap();
        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            !visible.contains("turn 1"),
            "the prior turn's stats must not persist over the next turn's reasoning.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("◈ clanking"),
            "the placeholder header should return for the next turn's reasoning.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("second turn thinking"),
            "the next turn's reasoning body should be visible.\nBuffer:\n{visible}"
        );
        assert_eq!(
            visible.matches('◈').count(),
            1,
            "the placeholder header should render the reasoning glyph exactly once.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn finish_turn_commits_clanking_header_after_response() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking\n");
        live_area.flush_reasoning();
        live_area.replace_response("answer");
        live_area.finish_turn();

        live_area.compose(&app).unwrap();
        let visible = dump_buffer(live_area.backend().buffer());
        let lines: Vec<&str> = visible.lines().collect();
        let prompt = lines.iter().position(|l| l.contains("▍» hi"));
        let header = lines.iter().position(|l| l.contains("◈ clanking"));
        let reasoning = lines.iter().position(|l| l.contains("    thinking"));
        let response = lines.iter().position(|l| l.contains("answer"));
        assert_eq!(
            visible.matches("◈ clanking").count(),
            1,
            "exactly one committed reasoning header should carry clanking.\nBuffer:\n{visible}"
        );
        assert!(
            reasoning.is_none(),
            "reasoning body must not be committed.\nBuffer:\n{visible}"
        );
        let (Some(prompt), Some(header), Some(response)) = (prompt, header, response) else {
            panic!(
                "prompt, clanking header, and response should all be visible.\nBuffer:\n{visible}"
            );
        };
        assert!(
            prompt < response && response < header,
            "committed order must be prompt → response → clanking header.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn turn_without_reasoning_commits_clanking_stats_header_once() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.set_reasoning_title(
            "clanking · turn 2 · in 10 (cache 40%, +1 new) · out 5 · reason 3 · Σ out 10 · reason 6",
        );
        live_area.replace_response("answer");
        live_area.finish_turn();

        live_area.compose(&app).unwrap();
        let visible = dump_buffer(live_area.backend().buffer());
        let lines: Vec<&str> = visible.lines().collect();
        let header = lines.iter().position(|l| l.contains("◈ clanking"));
        let response = lines.iter().position(|l| l.contains("answer"));
        assert_eq!(
            visible.matches("◈ clanking").count(),
            1,
            "no-body turns should commit exactly one clanking stats header.\nBuffer:\n{visible}"
        );
        let (Some(header), Some(response)) = (header, response) else {
            panic!(
                "clanking stats header and response should both be visible.\nBuffer:\n{visible}"
            );
        };
        assert!(
            response < header,
            "committed stats header must be the final line of the turn.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn clear_transcript_clears_embedded_reasoning() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking\n");
        live_area.flush_reasoning();
        live_area.clear_transcript();

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            !visible.contains("thinking"),
            "reasoning must vanish when the transcript is cleared.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn clear_current_turn_clears_embedded_reasoning_and_truncates_fifo() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking\n");
        live_area.flush_reasoning();
        live_area.replace_response("partial answer");
        live_area.clear_current_turn();

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            !visible.contains("thinking"),
            "reasoning must vanish when the current turn is cancelled.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("▍» hi") && !visible.contains("partial answer"),
            "clearing the current turn must truncate the prompt and draft response.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn overflow_commits_fifo_lines_only_never_reasoning() {
        let cols: u16 = 80;
        let rows: u16 = 8;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        // Enough FIFO content to force overflow while reasoning streams.
        for i in 0..6 {
            live_area.push_transcript_line(format!("completed line {i}"));
        }
        live_area.submit_prompt("hi".to_string());
        for i in 0..6 {
            live_area.push_reasoning_text(&format!("reasoning line {i}\n"));
        }
        live_area.replace_response("answer");

        live_area.compose(&app).unwrap();

        let scrollback = live_area.scrollback().join("\n");
        assert!(
            scrollback.contains("completed line") || scrollback.contains("▍» hi"),
            "transcript FIFO head lines should be committed to scrollback.\nScrollback:\n{scrollback}"
        );
        assert!(
            !scrollback.contains("reasoning"),
            "reasoning is display-only and must never enter scrollback.\nScrollback:\n{scrollback}"
        );

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("reasoning line 5"),
            "the reasoning tail should remain visible.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn short_reasoning_band_keeps_newest_lines() {
        let cols: u16 = 80;
        let rows: u16 = 6;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        // A title header plus more reasoning than the short band can show.
        // The first chunk after a stats title resets it back to the `clanking`
        // placeholder, which still occupies the header row the layout math
        // depends on.
        live_area.set_reasoning_title("turn 1 stats");
        for i in 0..6 {
            live_area.push_reasoning_text(&format!("reasoning line {i}\n"));
        }

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        // The reasoning band carries the live header, so with 4
        // transcript rows (1 reserved for the prompt + 3 for the reasoning
        // band) only two body rows remain after the header. The band must
        // still keep the newest reasoning lines, not the head, and later
        // taller frames must not resurrect lines trimmed from this shorter
        // band.
        assert!(
            visible.contains("◈ clanking"),
            "reasoning header should stay visible in the short band.\nBuffer:\n{visible}"
        );
        for i in 4..6 {
            assert!(
                visible.contains(&format!("reasoning line {i}")),
                "reasoning line {i} should be visible in the short-band tail.\nBuffer:\n{visible}"
            );
        }
        for i in 0..4 {
            assert!(
                !visible.contains(&format!("reasoning line {i}")),
                "reasoning line {i} should be trimmed by the short band.\nBuffer:\n{visible}"
            );
        }
    }

    #[test]
    fn compose_short_viewport_reserves_bars_and_skips_overflow() {
        let cols: u16 = 80;
        let rows: u16 = 2;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();
        live_area.push_transcript_line("some transcript line".to_string());
        live_area.submit_prompt("hi".to_string());
        live_area.replace_response("a response");

        live_area.compose(&app).unwrap();

        // With zero content rows, nothing can scroll into the terminal.
        assert!(live_area.scrollback().is_empty());

        let visible = dump_buffer(live_area.backend().buffer());
        // The two fixed bars must still be rendered even when the content
        // area has zero height.
        assert!(
            visible.contains("GREMLINS"),
            "Info bar should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("»"),
            "Input bar prompt should be visible.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn submit_prompt_clears_live_reasoning_widget() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        // First turn: reasoning ends WITHOUT a trailing newline, so the
        // final fragment sits in the reasoning widget as a live partial.
        live_area.submit_prompt("first".to_string());
        live_area.push_reasoning_text("thinking without a newline");

        // Submitting the next prompt resets the live region so the prior
        // turn's reasoning never becomes part of the next turn's transcript.
        live_area.submit_prompt("second".to_string());

        let mut app = App::new();
        app.project_name = "gremlins".to_string();
        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            !visible.contains("thinking without a newline"),
            "Previous turn reasoning should be cleared on the next prompt.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn submit_prompt_clears_newline_terminated_reasoning() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        live_area.submit_prompt("first".to_string());
        live_area.push_reasoning_text("thinking with a newline\n");
        live_area.submit_prompt("second".to_string());

        let mut app = App::new();
        app.project_name = "gremlins".to_string();
        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            !visible.contains("thinking with a newline"),
            "Previous newline-terminated reasoning should be cleared.\nBuffer:\n{visible}"
        );
    }

    // ── Full-screen view switch tests ────────────────────────────

    #[test]
    fn runs_screen_occludes_chat_and_switches_back() {
        let mut live = new_live(80, 24);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.active_runs.insert(
            "g1".to_string(),
            ActiveRun {
                status: "running".to_string(),
                stage: "build".to_string(),
                definition: "def".to_string(),
                project: "gremlins".to_string(),
            },
        );

        live.push_transcript_line("some transcript line".to_string());
        live.submit_prompt("hi".to_string());
        live.replace_response("a response");

        app.screen = Screen::Runs;
        live.show_runs();
        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("g1") && visible.contains("running"),
            "Runs view should show the run table.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("a response"),
            "Chat transcript must be occluded by the runs screen.\nBuffer:\n{visible}"
        );

        // Esc returns to chat.
        assert!(app.exit_screen());
        live.exit_screen_view();
        live.compose(&app).unwrap();
        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("a response"),
            "Transcript should be restored after switching back to chat.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("g r e m l i n s") && !visible.contains("g1"),
            "Runs view text must not survive the switch back to chat.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn watch_pane_shows_followed_stream_and_status() {
        let mut live = new_live(80, 20);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.active_runs.insert(
            "g1".to_string(),
            ActiveRun {
                status: "running".to_string(),
                stage: "build".to_string(),
                definition: "def".to_string(),
                project: "gremlins".to_string(),
            },
        );

        live.push_transcript_line("chat seed line".to_string());

        app.screen = Screen::Runs;
        app.watch_selection = Some("g1".to_string());
        app.watch_following = Some("g1".to_string());
        live.show_runs();
        live.push_watch_line("log one".to_string());
        live.push_watch_line("log two".to_string());

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("following: g1"),
            "Watch pane should render its header.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("running") && visible.contains("build"),
            "Watch pane should render live status and stage.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("log two"),
            "Watch pane should show the newest log line.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("chat seed line"),
            "Chat transcript must be occluded by the watch screen.\nBuffer:\n{visible}"
        );

        // Esc returns to chat.
        assert!(app.exit_screen());
        live.exit_screen_view();
        live.compose(&app).unwrap();
        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("chat seed line"),
            "Transcript should be restored after switching back to chat.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("following: g1"),
            "Watch pane text must not survive the switch back to chat.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn watch_pane_keeps_tail_and_marks_finished_run() {
        let mut live = new_live(80, 20);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.active_runs.insert(
            "g1".to_string(),
            ActiveRun {
                status: "failed".to_string(),
                stage: "build".to_string(),
                definition: "def".to_string(),
                project: "gremlins".to_string(),
            },
        );

        app.screen = Screen::Runs;
        app.watch_selection = Some("g1".to_string());
        app.watch_following = Some("g1".to_string());
        live.show_runs();
        live.push_watch_line("final terminal line".to_string());

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("final terminal line"),
            "A non-running followed id must keep its rendered log tail.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("stream finished"),
            "A non-running followed id must be marked finished.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn watch_pane_tail_accounts_for_wrapped_lines() {
        let mut live = new_live(40, 10);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.active_runs.insert(
            "g1".to_string(),
            ActiveRun {
                status: "running".to_string(),
                stage: "build".to_string(),
                definition: "def".to_string(),
                project: "gremlins".to_string(),
            },
        );

        app.screen = Screen::Runs;
        app.watch_selection = Some("g1".to_string());
        app.watch_following = Some("g1".to_string());
        live.show_runs();

        for i in 0..4 {
            live.push_watch_line(format!("prior {i} long line wraps!"));
        }
        live.push_watch_line("NEWEST_TAIL_END".to_string());

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("NEWEST"),
            "The newest log line should remain visible after wrapping.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn empty_watch_table_renders_empty_state() {
        let mut live = new_live(40, 8);
        let mut app = App::new();

        app.screen = Screen::Runs;
        live.show_runs();
        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("no gremlins"),
            "A watch table with no runs at all should render an empty state.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn watch_table_shows_finished_runs() {
        let mut live = new_live(40, 8);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.active_runs.insert(
            "g1".to_string(),
            ActiveRun {
                status: "done".to_string(),
                stage: "done".to_string(),
                definition: "def".to_string(),
                project: "gremlins".to_string(),
            },
        );

        app.screen = Screen::Runs;
        live.show_runs();
        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("g1"),
            "Finished runs must stay in the watch table.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains(motif::glyph::DONE),
            "A finished run should render its own status glyph.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn debug_screen_occludes_chat_and_keeps_prompt() {
        let mut live = new_live(80, 24);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live.push_transcript_line("some transcript line".to_string());
        live.submit_prompt("hi".to_string());
        live.replace_response("a response");

        app.screen = Screen::Debug {
            id: "g1".to_string(),
        };
        live.show_debug("g1".to_string());
        live.push_debug_output("connected to gremlin g1".to_string());
        live.debug_prompt_mut().unwrap().push_str("hello");

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("debug: g1"),
            "Debug header should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("connected to gremlin g1"),
            "Debug output should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("hello"),
            "Debug prompt should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("a response"),
            "Chat transcript must be occluded by the debug screen.\nBuffer:\n{visible}"
        );

        // Esc returns to chat and drops the view-local prompt.
        assert!(app.exit_screen());
        live.exit_screen_view();
        live.compose(&app).unwrap();
        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("a response"),
            "Transcript should be restored after leaving debug.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("debug: g1"),
            "Debug view text must not survive the switch back to chat.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn screen_mutation_methods_feed_views() {
        let mut live = new_live(80, 24);

        // Inline watch stream pane.
        live.show_runs();
        live.push_watch_line("watch line one".to_string());
        live.push_watch_line("watch line two".to_string());
        live.clear_watch_stream();

        // Full-screen debug view.
        live.show_debug("g1".to_string());
        assert!(live.debug_connecting());
        assert!(!live.debug_connected());

        live.push_debug_output("hello".to_string());
        live.mark_debug_ready();
        assert!(!live.debug_connecting());
        assert!(live.debug_connected());

        live.debug_prompt_mut().unwrap().push_str("abc");
        assert_eq!(live.take_debug_prompt().as_deref(), Some("abc"));
        assert_eq!(live.take_debug_prompt().as_deref(), Some(""));

        live.set_debug_connected(false);
        assert!(!live.debug_connected());
        assert!(!live.debug_connecting());
    }

    #[test]
    fn short_viewport_with_screen_does_not_panic() {
        let mut live = new_live(80, 2);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.screen = Screen::Runs;
        live.show_runs();

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            !visible.is_empty(),
            "A full-screen view should render even on a 2-row viewport.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn full_screen_views_never_commit_to_scrollback() {
        let mut live = new_live(80, 8);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live.push_transcript_line("some transcript line".to_string());
        live.submit_prompt("hi".to_string());
        live.replace_response("a response");

        app.screen = Screen::Runs;
        live.show_runs();
        live.compose(&app).unwrap();

        assert!(
            live.scrollback().is_empty(),
            "Full-screen views must never commit chat scrollback.\nScrollback:\n{:?}",
            live.scrollback()
        );
    }

    #[test]
    fn finish_response_appends_distinct_summary_once() {
        let mut live = new_live(80, 24);
        live.push_response("streamed response");

        live.finish_response("streamed response", "answered the question");

        assert_eq!(
            live.response_text(),
            "streamed response\n\nanswered the question"
        );
    }

    #[test]
    fn finish_response_fallback_does_not_duplicate_summary() {
        let mut live = new_live(80, 24);

        // No streamed text: the summary becomes the response body, and must
        // not be appended a second time.
        live.finish_response("did the thing", "did the thing");

        assert_eq!(live.response_text(), "did the thing");
    }

    #[test]
    fn finish_response_ignores_empty_summary() {
        let mut live = new_live(80, 24);
        live.push_response("streamed response");

        live.finish_response("streamed response", "");

        assert_eq!(live.response_text(), "streamed response");
    }

    #[test]
    fn reasoning_band_trims_to_five_rows() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        // The bottom reasoning band caps at 5 rows; only the newest 5
        // lines stay visible even when no other widget contests the space.
        for i in 0..8 {
            live_area.push_reasoning_text(&format!("reasoning line {i}\n"));
        }

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        for i in 3..8 {
            assert!(
                visible.contains(&format!("reasoning line {i}")),
                "reasoning line {i} should be visible (tail of the 5-row band).\nBuffer:\n{visible}"
            );
        }
        for i in 0..3 {
            assert!(
                !visible.contains(&format!("reasoning line {i}")),
                "reasoning line {i} should be trimmed from the band head.\nBuffer:\n{visible}"
            );
        }
    }

    #[test]
    fn response_wins_and_shrinks_reasoning_while_tools_stay_in_transcript() {
        let cols: u16 = 80;
        let rows: u16 = 12;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        // Plenty of reasoning content and tool lines, then a tall response.
        for i in 0..10 {
            live_area.push_reasoning_text(&format!("reasoning line {i}\n"));
        }
        for i in 0..10 {
            live_area.push_tool(&format!("tool-{i}"));
        }
        let body: String = (0..20)
            .map(|i| format!("response line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        live_area.replace_response(&body);

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        // The response must be present (it wins the contested space). The
        // transcript is tail-anchored, so the newest response line is what
        // stays visible.
        assert!(
            visible.contains("response line 19"),
            "response should be visible.\nBuffer:\n{visible}"
        );
        // Reasoning (priority 5) shrinks before the response (priority 10),
        // keeping its tail and trimming its head.
        assert!(
            visible.contains("reasoning line 9"),
            "reasoning should keep its tail.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("reasoning line 0"),
            "reasoning head should be trimmed.\nBuffer:\n{visible}"
        );
        // Tool lines are now ordinary transcript content: they overflow into
        // scrollback ahead of the tall response rather than being trimmed.
        let scrollback = live_area.scrollback().join("\n");
        assert!(
            scrollback.contains("tool-0") && scrollback.contains("tool-9"),
            "tool lines should overflow into scrollback.\nScrollback:\n{scrollback}"
        );
        // Reasoning content is never committed to scrollback.
        assert!(
            !scrollback.contains("reasoning"),
            "reasoning content must not reach scrollback: {scrollback}"
        );
    }

    #[test]
    fn tool_lines_survive_turn_and_overflow_with_transcript() {
        let cols: u16 = 80;
        let rows: u16 = 8;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        for i in 0..12 {
            live_area.push_tool(&format!("tool-{i}"));
        }
        live_area.replace_response("answer");
        live_area.finish_turn();

        live_area.compose(&app).unwrap();

        // Tool lines persist as transcript content and are visible or in
        // scrollback, between the prompt and the response.
        let visible = dump_buffer(live_area.backend().buffer());
        let sb = live_area.scrollback().join("\n");
        let combined = format!("{sb}\n{visible}");
        for i in 0..12 {
            assert!(
                combined.contains(&format!("tool-{i}")),
                "tool line {i} must survive as transcript content.\nCombined:\n{combined}"
            );
        }
        assert!(
            combined.contains("answer"),
            "response must survive.\nCombined:\n{combined}"
        );
    }

    #[test]
    fn empty_reasoning_leaves_no_blank_gap() {
        let cols: u16 = 80;
        let rows: u16 = 6;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.submit_prompt("hi".to_string());
        live_area.replace_response("answer");

        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        // The transcript content sits directly above the input/info bars with
        // no rows consumed by the empty reasoning widget.
        assert!(visible.contains("▍» hi"));
        assert!(visible.contains("answer"));
        let lines: Vec<&str> = visible.lines().collect();
        let prompt_idx = lines
            .iter()
            .position(|l| l.contains("▍» hi"))
            .expect("prompt row present");
        assert!(
            lines[prompt_idx + 1].contains("answer"),
            "answer should sit directly below the prompt.\nBuffer:\n{visible}"
        );
        assert!(
            !visible.contains("reasoning"),
            "empty reasoning widget should render nothing.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn finish_turn_commits_tall_response_to_scrollback() {
        let cols: u16 = 40;
        let rows: u16 = 8;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.push_transcript_line("banner".to_string());
        live_area.submit_prompt("hi".to_string());
        live_area.push_reasoning_text("thinking...");
        live_area.flush_reasoning();
        live_area.push_tool("Done");
        live_area.push_tool("grep");

        // A response far taller than the viewport.
        let body: String = (0..40)
            .map(|i| format!("response line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        live_area.replace_response(&body);
        live_area.finish_response(&body, "final summary line");

        // Turn end: commit the transcript response. Tool lines are now part
        // of the transcript FIFO and overflow with it.
        live_area.finish_turn();
        live_area.push_transcript_line("42 tokens".to_string());

        live_area.compose(&app).unwrap();

        // The tail (including the summary) must be present in the rendered
        // frame or the scrollback log, not dropped.
        let visible = dump_buffer(live_area.backend().buffer());
        let sb = live_area.scrollback().join("\n");
        let combined = format!("{sb}\n{visible}");
        assert!(
            combined.contains("final summary line"),
            "summary should not be dropped.\nCombined:\n{combined}"
        );

        // Tool lines persist as transcript content in scrollback.
        assert!(
            combined.contains("Done") && combined.contains("grep"),
            "tool lines should survive as transcript content.\nCombined:\n{combined}"
        );
        // The reasoning title commits at turn end after the response; the
        // body is display-only and cleared.
        assert!(
            !combined.contains("    thinking..."),
            "reasoning body must be cleared, not committed.\nCombined:\n{combined}"
        );
        let prompt = combined.find("▍» hi").expect("prompt present");
        let tool = combined.find("◆ Done").expect("tool present");
        let summary = combined
            .find("final summary line")
            .expect("summary present");
        let title = combined
            .find("◈ clanking")
            .expect("reasoning title present");
        let tokens = combined
            .find("42 tokens")
            .expect("post-turn system line present");
        assert!(
            prompt < tool && tool < summary && summary < title && title < tokens,
            "committed order must be prompt → tools → response → reasoning title.\nCombined:\n{combined}"
        );
    }
}
