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
    layout::Rect,
};

#[cfg(test)]
pub use ratatui::backend::TestBackend;

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

    /// Mutable access to the current frame buffer (for render functions).
    pub fn curr_mut(&mut self) -> &mut Buffer {
        &mut self.curr
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
    use ratatui::{backend::TestBackend, buffer::Buffer, style::Style, text::Line as TuiLine};

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
}
