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

use std::io::{self, Stdout};

use crossterm::{
    cursor, execute,
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend},
    buffer::{Buffer, Cell},
    layout::{Constraint, Rect},
    text::Line,
};

use crate::tui::app::App;
use crate::tui::widgets::{
    DebugWidget, InfoBarWidget, InputBarWidget, LiveAreaWidget, ReasoningWidget, RenderContext,
    ToolEventsWidget, TranscriptWidget, WatchSingleWidget,
};

/// Approximate number of rows reserved at the bottom of the screen for the
/// input and info bars before the first layout pass runs. The real value is
/// tracked each frame from the live widget stack.
const INITIAL_RESERVED_ROWS: u16 = 2;

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
    /// Ordered widget stack, top-to-bottom. The initial stack is transcript,
    /// input bar, info bar. Overlays are pushed/popped at the top of the
    /// stack.
    widgets: Vec<Box<dyn LiveAreaWidget>>,
    /// Rows occupied by the bottom bars (input + info) in the most recent
    /// frame. These rows are excluded from the scrollback region.
    reserved_rows: u16,
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
            widgets: vec![
                Box::new(TranscriptWidget::new()),
                Box::new(ReasoningWidget::new()),
                Box::new(ToolEventsWidget::new()),
                Box::new(InputBarWidget::new()),
                Box::new(InfoBarWidget),
            ],
            reserved_rows: INITIAL_RESERVED_ROWS,
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

    /// Compose and paint a full frame.
    ///
    /// Lays all widgets out top-to-bottom in a single priority-aware pass.
    /// Each widget reports a requested height via `height_constraint`; when
    /// the sum exceeds the viewport, fixed-height widgets are shrunk in
    /// ascending `size_priority` order (lowest first) until the layout fits.
    /// Flexible (`Min`) widgets absorb the remaining rows. Only the
    /// transcript widget may return overflow; overlay widgets always return
    /// an empty overflow `Vec`. Collapsed (zero-height) chunks are skipped
    /// so a blocking overlay or a short viewport never commits transcript
    /// overflow.
    pub fn compose(&mut self, app: &App) -> io::Result<()> {
        let gremlin_count = app.gremlin_count_str();
        let ctx = RenderContext {
            input: &app.input,
            cursor: app.input_cursor,
            following_log: app.following_log.as_deref(),
            gremlin_count: &gremlin_count,
            project_name: app.project_name_str(),
            overlay: app.overlay.as_ref(),
            active_runs: &app.active_runs,
            content_height: self.scrollable_rows(),
        };

        let width = self.cols;

        // ── Priority-aware layout pass ────────────────────────────
        //
        // For each widget, resolve its constraint into a minimum height and
        // a flexibility flag. Fixed widgets (`Length`/`Max`/`Ratio`/
        // `Percentage`) request an exact height; flexible widgets (`Min`/
        // `Fill`) request a minimum and absorb slack. When the sum of
        // minimums exceeds the viewport, fixed widgets are shrunk in
        // ascending `size_priority` order (lowest first) until it fits.
        let n = self.widgets.len();
        let mut min_heights = vec![0u16; n];
        let mut flexible = vec![false; n];
        let mut fixed_total = 0u16;

        for (i, w) in self.widgets.iter().enumerate() {
            let c = w.height_constraint(width, &ctx);
            let (h, flex) = constraint_height(c, self.rows);
            min_heights[i] = h;
            flexible[i] = flex;
            fixed_total += h;
        }

        // Shrink fixed widgets (ascending size priority) when over budget.
        if fixed_total > self.rows {
            let mut order: Vec<usize> = (0..n).filter(|&i| !flexible[i]).collect();
            order.sort_by_key(|&i| self.widgets[i].size_priority());
            for &i in &order {
                if fixed_total <= self.rows {
                    break;
                }
                let excess = fixed_total - self.rows;
                let shrink = min_heights[i].min(excess);
                min_heights[i] -= shrink;
                fixed_total -= shrink;
            }
        }

        // Distribute remaining rows to flexible widgets (on top of their
        // minimums).
        let mut heights = min_heights.clone();
        let remaining = self.rows.saturating_sub(fixed_total);
        let flex_count = flexible.iter().filter(|&&f| f).count();
        if flex_count > 0 {
            let per = remaining / flex_count as u16;
            let mut extra = remaining % flex_count as u16;
            for i in 0..n {
                if flexible[i] {
                    heights[i] += per;
                    if extra > 0 {
                        heights[i] += 1;
                        extra -= 1;
                    }
                }
            }
        }

        // The bottom bars (input + info) are always the last two widgets.
        // Track their combined height so the scroll region excludes them.
        self.reserved_rows = if n >= 2 {
            heights[n - 2] + heights[n - 1]
        } else {
            0
        };

        // ── Drain overflow, then paint each widget ──────────────
        //
        // `take_overflow` is the single place where a widget decides what
        // to do when its content exceeds its allocated rows. The transcript
        // returns head lines to commit to scrollback; live-tail widgets trim
        // themselves and return nothing. Every other widget returns nothing.
        let mut overflow: Vec<(i32, Line<'static>)> = Vec::new();
        for (i, widget) in self.widgets.iter_mut().enumerate() {
            let h = heights[i];
            let prio = widget.overflow_priority();
            for line in widget.take_overflow(h, width) {
                overflow.push((prio, line));
            }
        }

        if !overflow.is_empty() {
            overflow.sort_by_key(|(prio, _)| *prio);
            let lines: Vec<Line<'static>> = overflow.into_iter().map(|(_, l)| l).collect();
            self.commit_scrollback(&lines, self.cols)?;
        }

        let mut y = 0u16;
        for (i, widget) in self.widgets.iter_mut().enumerate() {
            let h = heights[i];
            if h == 0 {
                continue;
            }
            let chunk = Rect::new(0, y, width, h);
            y += h;
            widget.render(chunk, &mut self.curr, &ctx);
        }

        self.draw(|_| {})
    }

    // ── Overlay push/pop ─────────────────────────────────────────

    /// Push an overlay widget at the top of the stack (above the content
    /// widgets, below nothing — it becomes the first widget).
    pub fn push_overlay(&mut self, widget: Box<dyn LiveAreaWidget>) {
        self.widgets.insert(0, widget);
    }

    /// Remove the most recently added overlay widget, if any. The initial
    /// content widgets are never popped.
    pub fn pop_overlay(&mut self) -> Option<Box<dyn LiveAreaWidget>> {
        // The first five widgets are the fixed content stack; only pop
        // widgets pushed on top of them.
        if self.widgets.len() > 5 {
            Some(self.widgets.remove(0))
        } else {
            None
        }
    }

    // ── Widget accessors ─────────────────────────────────────────

    fn transcript_mut(&mut self) -> &mut TranscriptWidget {
        self.widgets
            .iter_mut()
            .find_map(|w| w.as_any_mut().downcast_mut::<TranscriptWidget>())
            .expect("transcript widget present")
    }

    fn reasoning_mut(&mut self) -> &mut ReasoningWidget {
        self.widgets
            .iter_mut()
            .find_map(|w| w.as_any_mut().downcast_mut::<ReasoningWidget>())
            .expect("reasoning widget present")
    }

    fn tool_events_mut(&mut self) -> &mut ToolEventsWidget {
        self.widgets
            .iter_mut()
            .find_map(|w| w.as_any_mut().downcast_mut::<ToolEventsWidget>())
            .expect("tool events widget present")
    }

    fn input_bar_mut(&mut self) -> &mut InputBarWidget {
        self.widgets
            .iter_mut()
            .find_map(|w| w.as_any_mut().downcast_mut::<InputBarWidget>())
            .expect("input bar widget present")
    }

    fn watch_single_mut(&mut self) -> Option<&mut WatchSingleWidget> {
        self.widgets
            .iter_mut()
            .find_map(|w| w.as_any_mut().downcast_mut::<WatchSingleWidget>())
    }

    fn debug_mut(&mut self) -> Option<&mut DebugWidget> {
        self.widgets
            .iter_mut()
            .find_map(|w| w.as_any_mut().downcast_mut::<DebugWidget>())
    }

    // ── Widget mutation (moved from `App`) ───────────────────────

    /// Start a new turn with the given prompt: push a separator and the
    /// markdown-rendered prompt, then pin the turn start.
    pub fn submit_prompt(&mut self, prompt: String) {
        self.reasoning_mut().clear();
        self.tool_events_mut().clear();
        self.transcript_mut().submit_prompt(prompt);
    }

    /// Reconcile the input bar's scroll offsets so the cursor stays visible
    /// after a cursor move or text edit.
    pub fn ensure_input_cursor_visible(&mut self, cursor: usize, input: &str, width: u16) {
        let rows = self.input_bar_mut().wrapped_rows(width, input);
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
        self.reasoning_mut().flush_reasoning();
    }

    /// Append reasoning text (may be a partial chunk).
    pub fn push_reasoning_text(&mut self, text: &str) {
        self.reasoning_mut().push_reasoning_text(text);
    }

    /// Record a tool call.
    pub fn push_tool(&mut self, name: &str) {
        self.tool_events_mut().push_tool(name);
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

    /// Unpin the current turn and clear its draft state. The content is
    /// already in the FIFO.
    pub fn finish_turn(&mut self) {
        self.transcript_mut().finish_turn();
    }

    /// Whether the current turn has any content.
    pub fn has_current_turn(&self) -> bool {
        self.widgets
            .iter()
            .find_map(|w| w.as_any().downcast_ref::<TranscriptWidget>())
            .expect("transcript widget present")
            .has_current_turn()
    }

    /// Whether the current turn's streaming response is empty.
    pub fn response_is_empty(&self) -> bool {
        self.widgets
            .iter()
            .find_map(|w| w.as_any().downcast_ref::<TranscriptWidget>())
            .expect("transcript widget present")
            .response_is_empty()
    }

    /// Borrow the current turn's streaming response text.
    #[cfg(test)]
    pub fn response_text(&self) -> &str {
        self.widgets
            .iter()
            .find_map(|w| w.as_any().downcast_ref::<TranscriptWidget>())
            .expect("transcript widget present")
            .response_accumulator()
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
        self.transcript_mut().clear_current_turn();
    }

    /// Clear the transcript/scrollback.
    pub fn clear_transcript(&mut self) {
        self.transcript_mut().clear_scrollable();
    }

    /// Append a system message to the transcript with default style and
    /// blank-line padding from the previous entry.
    pub fn push_transcript_line(&mut self, line: String) {
        self.transcript_mut().push_transcript_line(line);
    }

    // ── Overlay mutation ─────────────────────────────────────────

    /// Push a log line into the watch-single widget, if present.
    pub fn push_watch_single_line(&mut self, line: String) {
        if let Some(w) = self.watch_single_mut() {
            w.push_line(line);
        }
    }

    /// Append output to the debug widget, if present.
    pub fn push_debug_output(&mut self, line: String) {
        if let Some(w) = self.debug_mut() {
            w.push_output(line);
        }
    }

    /// Borrow the debug prompt, if the debug widget is present.
    pub fn debug_prompt_mut(&mut self) -> Option<&mut String> {
        self.debug_mut().map(|w| w.prompt_mut())
    }

    /// Take the debug prompt for submission, if the debug widget is present.
    pub fn take_debug_prompt(&mut self) -> Option<String> {
        self.debug_mut().map(|w| w.take_prompt())
    }

    /// Whether the debug widget is still connecting.
    pub fn debug_connecting(&self) -> bool {
        self.widgets
            .iter()
            .find_map(|w| w.as_any().downcast_ref::<DebugWidget>())
            .is_some_and(|w| w.connecting())
    }

    /// Whether the debug widget is still connected.
    pub fn debug_connected(&self) -> bool {
        self.widgets
            .iter()
            .find_map(|w| w.as_any().downcast_ref::<DebugWidget>())
            .is_some_and(|w| w.connected())
    }

    /// Set the connected flag on the debug widget.
    pub fn set_debug_connected(&mut self, connected: bool) {
        if let Some(w) = self.debug_mut() {
            w.set_connected(connected);
        }
    }

    /// Mark the debug widget as connected.
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

/// Resolve a ratatui [`Constraint`] into a minimum height and a flexibility
/// flag. Fixed constraints (`Length`/`Max`/`Ratio`/`Percentage`) request an
/// exact height; flexible constraints (`Min`/`Fill`) request a minimum and
/// absorb slack.
fn constraint_height(c: Constraint, rows: u16) -> (u16, bool) {
    match c {
        Constraint::Length(n) => (n, false),
        Constraint::Max(n) => (n, false),
        Constraint::Ratio(num, den) => {
            let r = if den == 0 {
                0.0
            } else {
                num as f32 / den as f32
            };
            ((rows as f32 * r) as u16, false)
        }
        Constraint::Percentage(p) => (rows.saturating_mul(p) / 100, false),
        Constraint::Min(n) => (n, true),
        Constraint::Fill(_) => (0, true),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{ActiveRun, App, Overlay};
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
            Style::default().fg(ratatui::style::Color::White),
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
    fn compose_chat_response_visible_after_done() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live_area.push_transcript_line(
            "gremlins tui — type /help for commands, Ctrl+C to exit".to_string(),
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
            visible.contains("> hi"),
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
            "gremlins tui — type /help for commands, Ctrl+C to exit".to_string(),
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
            visible.contains("> hi"),
            "Prompt should be visible.\nBuffer:\n{visible}"
        );
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
            visible.contains("gremlins tui"),
            "Info bar should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains(">"),
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

    // ── Dynamic widget stack / overlay tests ────────────────────

    #[test]
    fn push_and_pop_overlay_widgets() {
        use crate::tui::widgets::{DebugWidget, WatchTableWidget};

        let mut live = new_live(80, 24);
        assert_eq!(live.widgets.len(), 5);

        // Popping with no overlay is a no-op.
        assert!(live.pop_overlay().is_none());

        live.push_overlay(Box::new(WatchTableWidget::new()));
        assert_eq!(live.widgets.len(), 6);

        live.push_overlay(Box::new(DebugWidget::new("g1".to_string())));
        assert_eq!(live.widgets.len(), 7);

        // Pop removes the most recently added (debug) first.
        let popped = live.pop_overlay().unwrap();
        assert!(popped.as_any().downcast_ref::<DebugWidget>().is_some());
        assert_eq!(live.widgets.len(), 6);

        let popped = live.pop_overlay().unwrap();
        assert!(popped.as_any().downcast_ref::<WatchTableWidget>().is_some());
        assert_eq!(live.widgets.len(), 5);

        // Cannot pop the fixed content stack.
        assert!(live.pop_overlay().is_none());
        assert_eq!(live.widgets.len(), 5);
    }

    #[test]
    fn blocking_overlay_occludes_content_but_keeps_bars() {
        use crate::tui::widgets::DebugWidget;

        let mut live = new_live(80, 24);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        live.push_transcript_line("some transcript line".to_string());
        live.submit_prompt("hi".to_string());
        live.replace_response("a response");

        // Push a blocking debug overlay.
        app.overlay = Some(Overlay::Debug);
        live.push_overlay(Box::new(DebugWidget::new("g1".to_string())));

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        // The debug header is visible.
        assert!(
            visible.contains("debug: g1"),
            "Debug header should be visible.\nBuffer:\n{visible}"
        );
        // The transcript content is occluded.
        assert!(
            !visible.contains("a response"),
            "Transcript response should be occluded.\nBuffer:\n{visible}"
        );
        // The fixed bars remain visible.
        assert!(
            visible.contains("gremlins tui"),
            "Info bar should remain visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains(">"),
            "Input bar should remain visible.\nBuffer:\n{visible}"
        );

        // Popping restores the transcript.
        app.overlay = None;
        live.pop_overlay();
        live.compose(&app).unwrap();
        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("a response"),
            "Transcript should be restored after pop.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn watch_table_is_non_blocking_and_leaves_content_visible() {
        use crate::tui::widgets::WatchTableWidget;

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

        app.overlay = Some(Overlay::Watch);
        live.push_overlay(Box::new(WatchTableWidget::new()));

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        // Watch table shows the run.
        assert!(
            visible.contains("g1"),
            "Watch table should show the run id.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("running"),
            "Watch table should show the status.\nBuffer:\n{visible}"
        );
        // Transcript content stays visible below.
        assert!(
            visible.contains("a response"),
            "Transcript should stay visible under the watch table.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn overlay_mutation_methods_feed_widgets() {
        use crate::tui::widgets::{DebugWidget, WatchSingleWidget};

        let mut live = new_live(80, 24);

        // Watch-single log lines.
        live.push_overlay(Box::new(WatchSingleWidget::new("g1".to_string())));
        live.push_watch_single_line("log line one".to_string());
        live.push_watch_single_line("log line two".to_string());

        // Debug widget.
        live.push_overlay(Box::new(DebugWidget::new("g1".to_string())));
        assert!(live.debug_connecting());
        assert!(!live.debug_connected());

        live.push_debug_output("hello".to_string());
        live.mark_debug_ready();
        assert!(!live.debug_connecting());
        assert!(live.debug_connected());

        // Prompt mutation.
        live.debug_prompt_mut().unwrap().push_str("abc");
        assert_eq!(live.take_debug_prompt().as_deref(), Some("abc"));
        assert_eq!(live.take_debug_prompt().as_deref(), Some(""));

        // Connection state transitions.
        live.set_debug_connected(false);
        assert!(!live.debug_connected());
        assert!(!live.debug_connecting());
    }

    #[test]
    fn short_viewport_with_overlay_does_not_panic() {
        use crate::tui::widgets::WatchTableWidget;

        let mut live = new_live(80, 2);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.overlay = Some(Overlay::Watch);
        live.push_overlay(Box::new(WatchTableWidget::new()));

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("gremlins tui"),
            "Info bar should render even with an overlay on a 2-row viewport.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn content_mutation_targets_transcript_after_overlay_push() {
        use crate::tui::widgets::WatchTableWidget;

        let mut live = new_live(80, 24);
        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        // Push a non-blocking overlay first, then mutate the transcript.
        app.overlay = Some(Overlay::Watch);
        live.push_overlay(Box::new(WatchTableWidget::new()));

        live.push_transcript_line("after overlay".to_string());
        live.submit_prompt("hi".to_string());
        live.replace_response("response after overlay");

        live.compose(&app).unwrap();

        let visible = dump_buffer(live.backend().buffer());
        assert!(
            visible.contains("response after overlay"),
            "Transcript mutation should target the transcript even with an overlay pushed.\nBuffer:\n{visible}"
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

        // Turn end: commit the transcript response. Reasoning and tools stay
        // in their own live-tail widgets and never reach transcript scrollback.
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

        // Transcript scrollback must contain the prompt and response, and
        // must never contain reasoning or tool content.
        let combined_no_frame = sb.clone();
        assert!(!combined_no_frame.contains("thinking..."));
        assert!(!combined_no_frame.contains("Done") && !combined_no_frame.contains("grep"));

        // Reasoning and tools remain live in the visible frame (or were
        // already previously capped away from it), and are never scrollback.
        assert!(!sb.contains("thinking..."));
        assert!(!sb.contains("grep"));
    }
}
