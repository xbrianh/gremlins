//! Main-screen double-buffered renderer.
//!
//! [`LiveArea`] owns the primary terminal screen (no alternate screen),
//! keeping two screen-sized [`Buffer`]s and flushing only the cells that
//! change between frames. Content that outgrows the scrollable region is
//! committed to the terminal scrollback so tmux scrollback keeps working.
//!
//! Phase 1 is purely additive: `LiveArea` is not yet wired into the event
//! loop, so its public API is intentionally unused for now.
#![allow(dead_code)]

use std::io::{self, Stdout};

use crossterm::{
    cursor, execute,
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend},
    buffer::{Buffer, Cell},
    layout::Rect,
};

#[cfg(test)]
pub use ratatui::backend::TestBackend;

/// Number of rows reserved at the bottom of the screen for future input and
/// info bars. These rows are excluded from the scrollback region.
const RESERVED_ROWS: u16 = 2;

/// Owns the main terminal screen with diff-driven double buffering.
pub struct LiveArea<B: Backend = CrosstermBackend<Stdout>> {
    backend: B,
    curr: Buffer,
    prev: Buffer,
    cols: u16,
    rows: u16,
    /// Number of content lines already committed to terminal scrollback.
    committed_lines: u16,
    /// Lines that have been pushed into terminal scrollback (captured for
    /// test inspection). Each entry is one terminal row, trailing whitespace
    /// trimmed.
    scrollback_log: Vec<String>,
    /// Whether [`Drop`] should restore the terminal (raw mode, cursor).
    /// Set only by the [`CrosstermBackend`] constructor; `TestBackend`
    /// callers use [`with_backend`] which leaves this `false`.
    needs_terminal_cleanup: bool,
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
            committed_lines: 0,
            scrollback_log: Vec::new(),
            needs_terminal_cleanup: false,
        }
    }

    /// Render one frame.
    ///
    /// The closure paints the visible frame and returns
    /// `(scrollable_lines, total_lines)`. `scrollable_lines` is the count
    /// of lines that are allowed to enter terminal scrollback (transcript +
    /// response). `total_lines` is the full rendered line count including
    /// ephemeral regions (reasoning, tools).
    ///
    /// Overflow is computed from `scrollable_lines` only, so ephemeral
    /// content never reaches terminal scrollback.
    pub fn draw(
        &mut self,
        f: impl FnOnce(&mut Buffer) -> (u16, u16),
    ) -> io::Result<()> {
        let (scrollable_count, total_count) = f(&mut self.curr);

        log::debug!(
            "LiveArea::draw scrollable={} total={} region_rows={} committed={}",
            scrollable_count, total_count, self.scrollable_rows(), self.committed_lines,
        );

        let region_rows = self.scrollable_rows();
        if region_rows > 0 {
            let overflow = scrollable_count.saturating_sub(region_rows);
            let new_overflow = overflow.saturating_sub(self.committed_lines);

            if new_overflow > 0 {
                log::debug!("LiveArea::draw scroll_region_up new_overflow={}", new_overflow);
                // Capture the rows being pushed to scrollback before
                // shifting the previous buffer.
                let width = self.prev.area.width as usize;
                for y in 0..new_overflow {
                    let row: String = (0..width)
                        .map(|x| self.prev[(x as u16, y)].symbol())
                        .collect();
                    self.scrollback_log.push(trim_trailing_whitespace(&row));
                }

                // Keep the previous buffer aligned with the terminal scroll,
                // then diff against the freshly painted current frame.
                shift_buffer_up(&mut self.prev, region_rows, new_overflow);
                self.backend
                    .scroll_region_up(0..region_rows, new_overflow)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                self.committed_lines = overflow;
            } else if overflow < self.committed_lines {
                // Content shrank. The terminal cannot pull lines back from
                // scrollback, but future growth should scroll from this new
                // baseline.
                self.committed_lines = overflow;
            }
        }

        // `diff_iter` yields the cells of its *argument* where they differ
        // from `self`, so pass `curr` to obtain the new frame's changed cells.
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
    ///
    /// This invalidates the previous-frame diff cache without clearing the
    /// terminal, so a Ctrl+L style refresh can repaint the screen without
    /// disturbing main-screen scrollback.
    pub fn invalidate(&mut self) {
        self.prev.reset();
    }

    /// Lines committed to terminal scrollback since creation.
    pub fn scrollback(&self) -> &[String] {
        &self.scrollback_log
    }

    /// Borrow the inner backend (e.g. to read a [`TestBackend`] buffer).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Mutably borrow the inner backend.
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Rows that participate in scrollback. The bottom [`RESERVED_ROWS`] are
    /// kept out of the scrolling region.
    fn scrollable_rows(&self) -> u16 {
        self.rows.saturating_sub(RESERVED_ROWS)
    }
}

impl LiveArea<CrosstermBackend<Stdout>> {
    /// Enter raw mode, hide the cursor, clear the main screen, and allocate
    /// buffers for the current terminal size.
    ///
    /// If any step after raw mode is enabled fails, raw mode and the cursor
    /// are restored before the error is returned.
    pub fn new() -> io::Result<Self> {
        let mut guard = RawModeGuard::enter()?;
        execute!(io::stdout(), cursor::Hide)?;

        let (cols, rows) = size()?;
        let mut live = Self::with_backend(CrosstermBackend::new(io::stdout()), cols, rows);
        live.needs_terminal_cleanup = true;

        // Establish a clean main-screen baseline so the first diff does not
        // leave pre-existing terminal content behind.
        live.backend.clear_region(ClearType::All)?;
        guard.disarm();
        Ok(live)
    }

    /// Reallocate both buffers for a new terminal size and clear the screen.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.cols = cols;
        self.rows = rows;
        self.committed_lines = 0;
        self.scrollback_log.clear();

        let area = Rect::new(0, 0, cols, rows);
        self.curr = Buffer::empty(area);
        self.prev = Buffer::empty(area);
        self.backend = CrosstermBackend::new(io::stdout());

        // A resize invalidates the previous screen contents, so clear the
        // display rather than letting the next diff leave stale glyphs where
        // the new frame is blank.
        self.backend.clear_region(ClearType::All)
    }
}

/// Trim trailing whitespace from a single row captured for scrollback.
fn trim_trailing_whitespace(s: &str) -> String {
    s.trim_end().to_string()
}

/// Scroll the top `region_rows` rows of `buf` up by `amount`, clearing the
/// bottom `amount` rows of the region.
///
/// This mirrors [`Backend::scroll_region_up`] so the double buffers stay in
/// lockstep with the terminal.
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

    // ── terminal capture tests ─────────────────────────────────────
    //
    // These tests render frames through a LiveArea<TestBackend> and
    // verify both LiveArea's internal scrollback_log AND the
    // TestBackend's own scrollback buffer. This dual check catches
    // cases where LiveArea thinks it scrolled one set of lines but
    // the backend actually scrolled something different.

    mod terminal_tests {
        use super::*;
        use ratatui::{
            backend::TestBackend,
            buffer::Buffer,
            style::Style,
        };

        /// Helper: write a single text line at a given row.
        fn put_line(buf: &mut Buffer, y: u16, text: &str) {
            buf.set_string(0, y, text, Style::default());
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
                    let row: String = (0..w)
                        .map(|x| sb[(x as u16, y)].symbol())
                        .collect();
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

        /// Create a LiveArea<TestBackend> with the given dimensions.
        fn new_live(cols: u16, rows: u16) -> LiveArea<TestBackend> {
            LiveArea::with_backend(TestBackend::new(cols, rows), cols, rows)
        }

        #[test]
        fn scrollback_log_matches_backend_scrollback() {
            // 5 rows, 2 reserved = 3 scrollable rows.
            let mut live = new_live(40, 5);

            // Frame 1: 2 scrollable lines (fits).
            live.draw(|buf| {
                put_line(buf, 0, "alpha");
                put_line(buf, 1, "beta");
                (2, 2)
            })
            .unwrap();

            // Frame 2: 4 scrollable lines — overflows by 1.
            live.draw(|buf| {
                put_line(buf, 0, "beta");
                put_line(buf, 1, "gamma");
                put_line(buf, 2, "delta");
                put_line(buf, 3, "epsilon");
                (4, 4)
            })
            .unwrap();

            // Frame 3: 5 scrollable lines — overflows by 2 total (1 new).
            live.draw(|buf| {
                put_line(buf, 0, "gamma");
                put_line(buf, 1, "delta");
                put_line(buf, 2, "epsilon");
                put_line(buf, 3, "zeta");
                put_line(buf, 4, "eta");
                (5, 5)
            })
            .unwrap();

            let la_sb = live.scrollback();
            let be_sb = backend_scrollback(live.backend());

            // LiveArea scrollback_log should contain "alpha" and "beta".
            assert!(
                la_sb.iter().any(|r| r.contains("alpha")),
                "LiveArea scrollback should contain alpha: {la_sb:?}"
            );
            assert!(
                la_sb.iter().any(|r| r.contains("beta")),
                "LiveArea scrollback should contain beta: {la_sb:?}"
            );

            // Backend scrollback must match.
            assert_eq!(
                la_sb, be_sb,
                "LiveArea and TestBackend scrollback must agree"
            );
        }

        /// This is the APPROVAL TEST for the design doc invariants.
        ///
        /// Ephemeral regions (reasoning, tools) must NEVER appear in
        /// scrollback — only transcript lines and response text may
        /// be committed. This test currently FAILS because the old
        /// widget model doesn't separate ephemeral from persistent
        /// content.
        #[test]
        fn ephemeral_content_never_reaches_scrollback() {
            // 6 rows, 2 reserved = 4 scrollable rows.
            let mut live = new_live(60, 6);

            // Frame 1: transcript + prompt + reasoning = 3 lines (2 scrollable).
            live.draw(|buf| {
                put_line(buf, 0, "[transcript] turn 1 response");
                put_line(buf, 1, "> turn 2 prompt");
                put_line(buf, 2, "  reasoning: analyzing...");
                (2, 3)
            })
            .unwrap();

            // Frame 2: +tool = 4 lines (2 scrollable, 2 ephemeral).
            live.draw(|buf| {
                put_line(buf, 0, "[transcript] turn 1 response");
                put_line(buf, 1, "> turn 2 prompt");
                put_line(buf, 2, "  reasoning: analyzing...");
                put_line(buf, 3, "  bash(cargo build)");
                (2, 4)
            })
            .unwrap();

            // Frame 3: +2 response lines = 6 total (5 scrollable).
            // Scrollable rows = 4, so overflow by 1 scrollable line.
            live.draw(|buf| {
                put_line(buf, 0, "> turn 2 prompt");
                put_line(buf, 1, "  reasoning: analyzing...");
                put_line(buf, 2, "  bash(cargo build)");
                put_line(buf, 3, "response line one");
                put_line(buf, 4, "response line two");
                put_line(buf, 5, "response line three");
                (5, 6)
            })
            .unwrap();

            let la_sb = live.scrollback();
            let be_sb = backend_scrollback(live.backend());

            // Assert scrollback consistency.
            assert_eq!(la_sb, be_sb, "scrollback buffers must match");

            // DESIGN INVARIANT: ephemeral lines must never reach
            // scrollback.
            for line in la_sb {
                assert!(
                    !line.contains("reasoning"),
                    "REASONING LEAKED TO SCROLLBACK: {line:?}"
                );
                assert!(
                    !line.contains("bash("),
                    "TOOLS LEAKED TO SCROLLBACK: {line:?}"
                );
            }

            // The transcript line SHOULD be in scrollback.
            assert!(
                la_sb.iter().any(|r| r.contains("turn 1")),
                "Transcript line should be in scrollback"
            );

            // Reasoning/tools should still be visible on screen.
            let buf = live.backend().buffer();
            let visible: String = (0..buf.area.height)
                .map(|y| {
                    let row: String = (0..buf.area.width)
                        .map(|x| buf[(x as u16, y)].symbol())
                        .collect();
                    row
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                visible.contains("reasoning"),
                "Reasoning should still be visible after overflow"
            );
            assert!(
                visible.contains("bash("),
                "Tools should still be visible after overflow"
            );
        }

        #[test]
        fn reserved_rows_excluded_from_scrollback() {
            // 6 rows, 2 reserved = 4 scrollable.
            //
            // The bottom 2 rows are the input bar and info bar.
            // They are OUTSIDE the scroll region (0..rows-2).
            // As content overflows, only rows 0..3 should go to
            // scrollback — never rows 4 or 5.
            let mut live = new_live(50, 6);

            let mut logical_total: u16 = 0;

            for _i in 0..5 {
                logical_total += 1;
                let total = logical_total + 2; // +2 for reserved bars
                live.draw(move |buf| {
                    // Paint only the last `scrollable_rows()` content
                    // lines into the buffer, simulating the real
                    // render which shows the tail of a longer
                    // transcript.
                    let scrollable = 4; // rows - RESERVED_ROWS
                    let start = logical_total.saturating_sub(scrollable);
                    for y in 0..scrollable {
                        let idx = start + y;
                        if idx < logical_total {
                            put_line(buf, y, &format!("content {idx}"));
                        }
                    }
                    // Reserved bars at the bottom (rows 4,5).
                    put_line(buf, 4, "> user input here");
                    put_line(buf, 5, "gremlins tui | 1 gremlin");
                    (logical_total, total)
                })
                .unwrap();
            }

            // Neither the input bar nor the info bar should appear
            // in scrollback.
            let la_sb = live.scrollback();
            for line in la_sb {
                assert!(
                    !line.contains("user input"),
                    "INPUT BAR LEAKED TO SCROLLBACK: {line:?}"
                );
                assert!(
                    !line.contains("gremlins tui"),
                    "INFO BAR LEAKED TO SCROLLBACK: {line:?}"
                );
            }

            let be_sb = backend_scrollback(live.backend());
            for line in &be_sb {
                assert!(
                    !line.contains("user input"),
                    "INPUT BAR IN BACKEND SCROLLBACK: {line:?}"
                );
            }
            assert_eq!(la_sb, be_sb, "scrollback must agree");
        }

        #[test]
        fn overflow_lines_captured_in_scrollback_log() {
            // 4 rows, 2 reserved = 2 scrollable rows.
            // Overflow progressively pushes old content to scrollback.
            let mut live = new_live(30, 4);
            let scrollable: u16 = 2;

            let mut logical_total: u16 = 0;

            for _ in 0..6 {
                logical_total += 1;
                live.draw(move |buf| {
                    let start = logical_total.saturating_sub(scrollable);
                    for y in 0..scrollable {
                        let idx = start + y;
                        if idx < logical_total {
                            put_line(
                                buf,
                                y,
                                &format!("L{idx:02}: {}", "X".repeat(20)),
                            );
                        }
                    }
                    (logical_total, logical_total)
                })
                .unwrap();
            }

            // Scrollback should contain the oldest content lines.
            let la_sb = live.scrollback();
            let be_sb = backend_scrollback(live.backend());

            // After 6 frames, logical_total = 6, scrollable = 2
            // committed should be 4, so scrollback should have 4 lines.
            assert_eq!(
                la_sb.len(),
                be_sb.len(),
                "scrollback lengths must match"
            );
            assert!(
                !la_sb.is_empty(),
                "scrollback should contain overflowed lines"
            );
            assert!(
                la_sb.iter().any(|r| r.contains("L00")),
                "oldest line should be in scrollback: {la_sb:?}"
            );
            assert!(
                la_sb.iter().any(|r| r.contains("L01")),
                "second line should be in scrollback: {la_sb:?}"
            );
        }
    }

    #[test]
    fn shift_buffer_up_moves_rows_and_clears_bottom() {
        let area = Rect::new(0, 0, 3, 4);
        let mut buf = Buffer::empty(area);
        for (y, row) in ["abc", "def", "ghi", "jkl"].iter().enumerate() {
            buf.set_string(0, y as u16, *row, ratatui::style::Style::default());
        }

        shift_buffer_up(&mut buf, 3, 1);

        let mut expected = Buffer::empty(area);
        for (y, row) in ["def", "ghi", "   "].iter().enumerate() {
            expected.set_string(0, y as u16, *row, ratatui::style::Style::default());
        }
        // Row 3 is outside the scrolled region and must be untouched.
        expected.set_string(0, 3, "jkl", ratatui::style::Style::default());
        assert_eq!(buf, expected);
    }

    #[test]
    fn shift_buffer_up_clears_whole_region_when_amount_exceeds_it() {
        let area = Rect::new(0, 0, 2, 3);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "ab", ratatui::style::Style::default());
        buf.set_string(0, 1, "cd", ratatui::style::Style::default());
        buf.set_string(0, 2, "ef", ratatui::style::Style::default());

        shift_buffer_up(&mut buf, 2, 5);

        let mut expected = Buffer::empty(area);
        expected.set_string(0, 2, "ef", ratatui::style::Style::default());
        assert_eq!(buf, expected);
    }
}
