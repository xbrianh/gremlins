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

/// Number of rows reserved at the bottom of the screen for future input and
/// info bars. These rows are excluded from the scrollback region.
const RESERVED_ROWS: u16 = 2;

/// Owns the main terminal screen with diff-driven double buffering.
pub struct LiveArea {
    backend: CrosstermBackend<Stdout>,
    curr: Buffer,
    prev: Buffer,
    cols: u16,
    rows: u16,
    /// Number of content lines already committed to terminal scrollback.
    committed_lines: u16,
}

impl LiveArea {
    /// Enter raw mode, hide the cursor, clear the main screen, and allocate
    /// buffers for the current terminal size.
    ///
    /// If any step after raw mode is enabled fails, raw mode and the cursor
    /// are restored before the error is returned.
    pub fn new() -> io::Result<Self> {
        let mut guard = RawModeGuard::enter()?;
        execute!(io::stdout(), cursor::Hide)?;

        let (cols, rows) = size()?;
        let area = Rect::new(0, 0, cols, rows);
        let mut live = Self {
            backend: CrosstermBackend::new(io::stdout()),
            curr: Buffer::empty(area),
            prev: Buffer::empty(area),
            cols,
            rows,
            committed_lines: 0,
        };

        // Establish a clean main-screen baseline so the first diff does not
        // leave pre-existing terminal content behind.
        live.backend.clear_region(ClearType::All)?;
        guard.disarm();
        Ok(live)
    }

    /// Render one frame.
    ///
    /// `line_count` is the total number of content lines in the logical
    /// transcript. The closure should paint the last `rows - RESERVED_ROWS`
    /// lines of that transcript into the scrollable region at the top of the
    /// buffer. When `line_count` grows, the newly overflowing lines are
    /// scrolled into the terminal scrollback and the visible region is
    /// repainted from the diff.
    pub fn draw(&mut self, line_count: u16, f: impl FnOnce(&mut Buffer)) -> io::Result<()> {
        f(&mut self.curr);

        let region_rows = self.scrollable_rows();
        if region_rows > 0 {
            let overflow = line_count.saturating_sub(region_rows);
            let new_overflow = overflow.saturating_sub(self.committed_lines);

            if new_overflow > 0 {
                // Keep the previous buffer aligned with the terminal scroll,
                // then diff against the freshly painted current frame.
                shift_buffer_up(&mut self.prev, region_rows, new_overflow);
                self.backend
                    .scroll_region_up(0..region_rows, new_overflow)?;
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
        self.backend.draw(diff.into_iter())?;
        self.backend.flush()?;

        // Swap buffers and clear the new "current" for the next frame.
        std::mem::swap(&mut self.curr, &mut self.prev);
        self.curr.reset();
        Ok(())
    }

    /// Reallocate both buffers for a new terminal size and clear the screen.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.cols = cols;
        self.rows = rows;
        self.committed_lines = 0;

        let area = Rect::new(0, 0, cols, rows);
        self.curr = Buffer::empty(area);
        self.prev = Buffer::empty(area);
        self.backend = CrosstermBackend::new(io::stdout());

        // A resize invalidates the previous screen contents, so clear the
        // display rather than letting the next diff leave stale glyphs where
        // the new frame is blank.
        self.backend.clear_region(ClearType::All)
    }

    /// Current terminal dimensions.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Rows that participate in scrollback. The bottom [`RESERVED_ROWS`] are
    /// kept out of the scrolling region.
    fn scrollable_rows(&self) -> u16 {
        self.rows.saturating_sub(RESERVED_ROWS)
    }
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

impl Drop for LiveArea {
    fn drop(&mut self) {
        execute!(io::stdout(), cursor::Show).ok();
        disable_raw_mode().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
