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
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::tui::app::App;
use crate::tui::buffer::LineOrigin;
use crate::tui::widgets::{
    DebugWidget, InfoBarWidget, InputBarWidget, LiveAreaWidget, ReasoningWidget, RenderContext,
    ToolsWidget, TranscriptWidget, WatchSingleWidget,
};

/// Number of rows reserved at the bottom of the screen for input and
/// info bars. These rows are excluded from the scrollback region.
const RESERVED_ROWS: u16 = 2;

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
    /// reasoning, tools, input bar, info bar. Overlays are pushed/popped at
    /// the top of the stack.
    widgets: Vec<Box<dyn LiveAreaWidget>>,
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
                Box::new(ToolsWidget::new()),
                Box::new(InputBarWidget),
                Box::new(InfoBarWidget),
            ],
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
            h.min(self.scrollable_rows() as usize) as u16
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

        // Write cells directly to the terminal.
        let mut cells: Vec<(u16, u16, &Cell)> = Vec::new();
        for y in 0..height {
            for x in 0..width {
                cells.push((x, y, &tmp[(x, y)]));
            }
        }
        self.backend
            .draw(cells.into_iter())
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Push to terminal scrollback.
        let region_rows = self.scrollable_rows();
        self.backend
            .scroll_region_up(0..region_rows, height)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Keep prev buffer in lockstep with the terminal.
        shift_buffer_up(&mut self.prev, region_rows, height);

        // Capture for tests.
        for y in 0..height {
            let row: String = (0..width as usize)
                .map(|x| tmp[(x as u16, y)].symbol())
                .collect();
            self.scrollback_log.push(trim_trailing_whitespace(&row));
        }

        self.backend
            .flush()
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(height)
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

    /// Rows that participate in scrollback. The bottom [`RESERVED_ROWS`] are
    /// kept out of the scrolling region.
    pub fn scrollable_rows(&self) -> u16 {
        self.rows.saturating_sub(RESERVED_ROWS)
    }

    /// Compose and paint a full frame.
    ///
    /// Reserves the bottom [`RESERVED_ROWS`] for the input and info bars
    /// (the last two widgets in the stack), then lays the remaining content
    /// widgets out top-to-bottom in a single ratatui layout pass using each
    /// widget's `height_constraint`. Only the transcript widget may return
    /// overflow; overlay widgets always return an empty overflow `Vec`.
    /// Collapsed (zero-height) chunks are skipped so a blocking overlay or
    /// a short viewport never commits transcript overflow.
    pub fn compose(&mut self, app: &App) -> io::Result<()> {
        let gremlin_count = app.gremlin_count_str();
        let ctx = RenderContext {
            input: &app.input,
            following_log: app.following_log.as_deref(),
            gremlin_count: &gremlin_count,
            project_name: app.project_name_str(),
            overlay: app.overlay.as_ref(),
            active_runs: &app.active_runs,
            content_height: self.scrollable_rows(),
        };

        let width = self.cols;
        let area = Rect::new(0, 0, self.cols, self.rows);

        // Reserve the bottom two rows for the fixed input/info bars first,
        // so their `Length(1)` constraints can never be compressed by the
        // variable-height content widgets above them. `commit_scrollback`
        // scrolls everything above `RESERVED_ROWS`, so the bars must always
        // land in those unscrolled rows.
        let bar_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        let content_area = bar_chunks[0];

        // The input and info bars are the last two widgets in the stack.
        let split = self.widgets.len() - 2;
        let (content_widgets, bar_widgets) = self.widgets.split_at_mut(split);

        // Lay out the content widgets within the remaining rect.
        let constraints: Vec<Constraint> = content_widgets
            .iter()
            .map(|w| w.height_constraint(width, &ctx))
            .collect();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(content_area);

        let mut overflow = Vec::new();
        for (widget, chunk) in content_widgets.iter_mut().zip(chunks.iter()) {
            if chunk.height == 0 {
                continue;
            }
            overflow.extend(widget.render(*chunk, &mut self.curr, &ctx));
        }

        // Render the fixed bars unconditionally.
        for (widget, chunk) in bar_widgets.iter_mut().zip(bar_chunks[1..].iter()) {
            widget.render(*chunk, &mut self.curr, &ctx);
        }

        if !overflow.is_empty() {
            self.commit_scrollback(&overflow, self.cols)?;
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

    fn tools_mut(&mut self) -> &mut ToolsWidget {
        self.widgets
            .iter_mut()
            .find_map(|w| w.as_any_mut().downcast_mut::<ToolsWidget>())
            .expect("tools widget present")
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

    /// Flatten the current turn into the transcript and start a new turn
    /// with the given prompt.
    pub fn submit_prompt(&mut self, prompt: String) {
        let width = self.cols;

        // Gather the ephemeral tails first (they borrow `self` independently
        // of the transcript), so we can then mutate the transcript alone.
        let prev_prompt = self.transcript_mut().take_prompt();
        self.reasoning_mut().flush();
        let reasoning_tail = self.reasoning_mut().take_tail(width);
        let tools_tail = self.tools_mut().take_tail(width);

        let transcript = self.transcript_mut();
        if let Some(prev_prompt) = prev_prompt {
            let prompt_style = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            let marker_style = Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD);

            if !transcript.is_empty() {
                transcript.push_separator();
                transcript.push_separator();
            }

            transcript.push_line(
                Line::from(vec![
                    Span::styled("> ", marker_style),
                    Span::styled(prev_prompt, prompt_style),
                ]),
                LineOrigin::FlattenedPrompt,
            );

            if !reasoning_tail.is_empty() {
                transcript.push_separator();
                for line in reasoning_tail {
                    transcript.push_line(line, LineOrigin::FlattenedReasoning);
                }
            }

            if !tools_tail.is_empty() {
                transcript.push_separator();
                for line in tools_tail {
                    transcript.push_line(line, LineOrigin::FlattenedTools);
                }
            }

            if !transcript.response_is_empty() {
                transcript.push_separator();
                let response_lines: Vec<String> = transcript
                    .response_text()
                    .lines()
                    .map(|s| s.to_string())
                    .collect();
                for line_str in response_lines {
                    transcript.push_line(
                        Line::from(Span::styled(line_str, Style::default())),
                        LineOrigin::FlattenedResponse,
                    );
                }
            }
        }

        transcript.set_prompt(prompt);
        transcript.clear_response();
        self.reasoning_mut().clear();
        self.tools_mut().clear();
    }

    /// Flush the reasoning partial buffer (if any) into a completed
    /// reasoning line.
    pub fn flush_reasoning(&mut self) {
        self.reasoning_mut().flush();
    }

    /// Append reasoning text (may be a partial chunk).
    pub fn push_reasoning_text(&mut self, text: &str) {
        self.reasoning_mut().push_text(text);
    }

    /// Record a tool call.
    pub fn push_tool(&mut self, name: &str) {
        self.tools_mut().push(name);
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

    /// Whether the current turn has any content.
    pub fn has_current_turn(&self) -> bool {
        self.widgets
            .iter()
            .find_map(|w| w.as_any().downcast_ref::<TranscriptWidget>())
            .expect("transcript widget present")
            .has_prompt()
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
            .response_text()
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
        self.transcript_mut().clear_prompt();
        self.transcript_mut().clear_response();
        self.reasoning_mut().clear();
        self.tools_mut().clear();
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
    fn submit_prompt_flushes_trailing_reasoning_partial() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        // First turn: reasoning ends WITHOUT a trailing newline, so the
        // final fragment sits in `reasoning_partial`.
        live_area.submit_prompt("first".to_string());
        live_area.push_reasoning_text("thinking without a newline");

        // Flatten the turn by submitting the next prompt. The trailing
        // partial must be flushed into the transcript before `take_tail`.
        live_area.submit_prompt("second".to_string());

        // The flattened reasoning (with its two-space indent) must now be
        // part of the scrollable transcript, visible in the rendered frame.
        let mut app = App::new();
        app.project_name = "gremlins".to_string();
        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("thinking without a newline"),
            "Trailing reasoning fragment should be committed to the transcript.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn submit_prompt_newline_terminated_reasoning_unchanged() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut live_area = new_live(cols, rows);

        // Reasoning that already ends in a newline is fully flushed by
        // `push_text`; `flush` must be a no-op and add no extra blank line.
        live_area.submit_prompt("first".to_string());
        live_area.push_reasoning_text("thinking with a newline\n");
        live_area.submit_prompt("second".to_string());

        let mut app = App::new();
        app.project_name = "gremlins".to_string();
        live_area.compose(&app).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("thinking with a newline"),
            "Newline-terminated reasoning should be committed.\nBuffer:\n{visible}"
        );
        // `flush` must be a no-op: exactly one committed reasoning line,
        // with no duplicate and no extra blank row introduced.
        let occurrences = visible
            .lines()
            .filter(|l| l.contains("thinking with a newline"))
            .count();
        assert_eq!(
            occurrences, 1,
            "Reasoning should be committed exactly once, no duplicate.\nBuffer:\n{visible}"
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
}
