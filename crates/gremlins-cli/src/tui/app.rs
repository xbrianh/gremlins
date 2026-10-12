use std::collections::HashMap;

/// Application state for the TUI.
///
/// Pure display state, no I/O and no widget ownership. Owned by the event
/// loop and passed immutably to [`LiveArea::compose`](crate::tui::screen::LiveArea::compose)
/// on each frame. The rendering widgets (transcript, reasoning, tools, input
/// bar, info bar) live in `LiveArea`; `App` only carries the read-only state
/// those widgets consume via their render context.
pub struct App {
    /// Current text in the input bar.
    pub input: String,
    /// Cursor byte index into `input` (always on a `char` boundary).
    pub input_cursor: usize,
    /// Live gremlin state: id → run details.
    pub active_runs: HashMap<String, ActiveRun>,
    /// Cached project name for the info bar.
    pub project_name: String,
    /// When Some, the TUI is following a gremlin's log and printing log lines
    /// to the transcript as they arrive.
    pub following_log: Option<String>,
    /// The persistent chat gremlin id, captured from the first `Done` event
    /// and sent with subsequent chat requests. Cleared on `/clear`/`/new`.
    pub chat_id: Option<String>,
    /// Whether a chat request is currently in-flight (guards against concurrent submissions).
    pub active_request: bool,
    /// Active ephemeral overlay, if any.
    pub overlay: Option<Overlay>,
    /// Submitted inputs in chronological order (in-memory only).
    pub input_history: Vec<String>,
    /// Cursor into `input_history`; `None` means editing a fresh line.
    pub history_cursor: Option<usize>,
    /// Terminal width cached for buffer trimming. Updated before each frame.
    pub width: u16,
}

/// One tracked gremlin run.
#[derive(Debug, Clone)]
pub struct ActiveRun {
    /// "running", "done", "failed", or "stopped".
    pub status: String,
    /// Most recent stage reported by the daemon.
    pub stage: String,
    /// Pipeline name carried by [`gremlins::executor::DaemonEvent::RunStarted`].
    pub definition: String,
    /// Project associated with the run.
    pub project: String,
}

impl ActiveRun {
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            stage: String::new(),
            definition: String::new(),
            project: String::new(),
        }
    }
}

/// Ephemeral overlay mode. A pure marker used only for input routing and
/// dismissal. Overlay-local state (log lines, debug prompt/history,
/// connection flags) lives in the corresponding overlay widgets in
/// `LiveArea`.
#[derive(Debug, Clone)]
pub enum Overlay {
    /// Gremlins watch table. Non-blocking; main input bar still works.
    Watch,
    /// Single-gremlin log viewer. Blocking overlay.
    WatchSingle,
    /// Interactive debug session. Blocking overlay with its own prompt.
    Debug,
}

impl Overlay {
    /// Blocking overlays occlude the transcript and disable the main input
    /// bar (slash commands are still accepted for `WatchSingle`; `Debug` has
    /// its own prompt).
    pub fn is_blocking(&self) -> bool {
        !matches!(self, Overlay::Watch)
    }

    /// Whether this overlay owns a prompt that should receive keystrokes.
    pub fn has_prompt(&self) -> bool {
        matches!(self, Overlay::Debug)
    }
}

impl App {
    pub fn new() -> Self {
        let project_name = gremlins::config::project_root()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "?".to_string());

        Self {
            input: String::new(),
            input_cursor: 0,
            active_runs: HashMap::new(),
            project_name,
            following_log: None,
            chat_id: None,
            active_request: false,
            overlay: None,
            input_history: Vec::new(),
            history_cursor: None,
            width: 80,
        }
    }

    /// Number of active (running) gremlins.
    pub fn gremlin_count_str(&self) -> String {
        let count = self
            .active_runs
            .values()
            .filter(|r| r.status == "running")
            .count();
        count.to_string()
    }

    /// Cached project name.
    pub fn project_name_str(&self) -> &str {
        &self.project_name
    }

    /// Insert or merge a run entry from an `ls` response.
    pub fn upsert_run_from_ls(&mut self, entry: &serde_json::Value) {
        let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        let id = id.to_string();
        let existing = self.active_runs.get(&id).cloned();

        let status = entry
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or(existing.as_ref().map(|r| r.status.as_str()).unwrap_or(""))
            .to_string();
        let stage = entry
            .get("stage")
            .and_then(|v| v.as_str())
            .unwrap_or(existing.as_ref().map(|r| r.stage.as_str()).unwrap_or(""))
            .to_string();
        let definition = entry
            .get("definition")
            .and_then(|v| v.as_str())
            .unwrap_or(
                existing
                    .as_ref()
                    .map(|r| r.definition.as_str())
                    .unwrap_or(""),
            )
            .to_string();
        let project = entry
            .get("project")
            .and_then(|v| v.as_str())
            .unwrap_or(existing.as_ref().map(|r| r.project.as_str()).unwrap_or(""))
            .to_string();
        let project = if project.is_empty() {
            self.project_name.clone()
        } else {
            project.to_string()
        };

        self.active_runs.insert(
            id,
            ActiveRun {
                status,
                stage,
                definition,
                project,
            },
        );
    }

    /// Update state when a run starts.
    pub fn on_run_started(&mut self, id: String, definition: String, stage: String) {
        self.active_runs.insert(
            id,
            ActiveRun {
                status: "running".to_string(),
                stage,
                definition,
                project: self.project_name.clone(),
            },
        );
    }

    /// Update state when a run completes.
    pub fn on_run_completed(&mut self, id: String) {
        self.update_run_status(&id, "done");
    }

    /// Update state when a run fails.
    pub fn on_run_failed(&mut self, id: String) {
        self.update_run_status(&id, "failed");
    }

    /// Update state when a run is stopped.
    pub fn on_run_stopped(&mut self, id: String) {
        self.update_run_status(&id, "stopped");
    }

    /// Update the stage of an existing run in-place.
    pub fn on_stage_transition(&mut self, id: String, stage: String) {
        if let Some(run) = self.active_runs.get_mut(&id) {
            run.stage = stage;
        }
    }

    fn update_run_status(&mut self, id: &str, status: &str) {
        match self.active_runs.get_mut(id) {
            Some(run) => run.status = status.to_string(),
            None => {
                self.active_runs
                    .insert(id.to_string(), ActiveRun::new(status));
            }
        }
    }

    // ── Overlay helpers ────────────────────────────────────────────

    pub fn overlay_is_watch(&self) -> bool {
        matches!(self.overlay, Some(Overlay::Watch))
    }

    /// Dismiss a blocking overlay. Returns true if one was dismissed.
    pub fn dismiss_blocking_overlay(&mut self) -> bool {
        if self.overlay.as_ref().is_some_and(Overlay::is_blocking) {
            self.overlay = None;
            true
        } else {
            false
        }
    }

    /// Dismiss the non-blocking watch table. Returns true if it was dismissed.
    pub fn dismiss_watch_overlay(&mut self) -> bool {
        if self.overlay_is_watch() {
            self.overlay = None;
            true
        } else {
            false
        }
    }

    pub fn open_watch_single(&mut self) {
        self.overlay = Some(Overlay::WatchSingle);
    }

    pub fn open_debug(&mut self) {
        self.overlay = Some(Overlay::Debug);
    }

    // ── Cursor editing helpers ────────────────────────────────────

    /// Move the cursor one grapheme/char left, clamped to a `char` boundary.
    pub fn cursor_left(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut idx = self.input_cursor;
        idx -= 1;
        while idx > 0 && !self.input.is_char_boundary(idx) {
            idx -= 1;
        }
        self.input_cursor = idx;
    }

    /// Move the cursor one grapheme/char right, clamped to a `char` boundary.
    pub fn cursor_right(&mut self) {
        if self.input_cursor >= self.input.len() {
            return;
        }
        let mut idx = self.input_cursor;
        idx += 1;
        while idx < self.input.len() && !self.input.is_char_boundary(idx) {
            idx += 1;
        }
        self.input_cursor = idx;
    }

    /// Jump the cursor to the start of the line.
    pub fn cursor_home(&mut self) {
        self.input_cursor = 0;
    }

    /// Jump the cursor to the end of the line.
    pub fn cursor_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    /// Insert `ch` at the cursor, keeping the cursor on a `char` boundary.
    pub fn insert_char(&mut self, ch: char) {
        let idx = self.input_cursor.min(self.input.len());
        self.input.insert(idx, ch);
        self.input_cursor = idx + ch.len_utf8();
    }

    /// Delete the character *before* the cursor (backspace).
    pub fn delete_before_cursor(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut start = self.input_cursor;
        start -= 1;
        while start > 0 && !self.input.is_char_boundary(start) {
            start -= 1;
        }
        self.input.drain(start..self.input_cursor);
        self.input_cursor = start;
    }

    /// Delete the character *at* the cursor (delete key).
    pub fn delete_at_cursor(&mut self) {
        if self.input_cursor >= self.input.len() {
            return;
        }
        let mut end = self.input_cursor;
        end += 1;
        while end < self.input.len() && !self.input.is_char_boundary(end) {
            end += 1;
        }
        self.input.drain(self.input_cursor..end);
    }

    /// Reset the cursor to the end of the current input (e.g. after history
    /// recall or editor write-back).
    pub fn reset_cursor_to_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    // ── Input history helpers ───────────────────────────────────────

    /// Append a submitted input to history and reset the cursor.
    pub fn push_history(&mut self, line: String) {
        self.input_history.push(line);
        self.history_cursor = None;
    }

    /// Move backward through history (older entries). Returns the selected
    /// entry, or `None` when there is no history.
    pub fn history_up(&mut self) -> Option<&str> {
        if self.input_history.is_empty() {
            return None;
        }
        let next = match self.history_cursor {
            None => self.input_history.len() - 1,
            Some(0) => 0,
            Some(n) => n - 1,
        };
        self.history_cursor = Some(next);
        self.input_history.get(next).map(|s| s.as_str())
    }

    /// Move forward through history (toward a fresh line). Returns the
    /// selected entry, or `None` when past the newest entry (fresh line).
    pub fn history_down(&mut self) -> Option<&str> {
        match self.history_cursor {
            None => None,
            Some(n) if n + 1 < self.input_history.len() => {
                self.history_cursor = Some(n + 1);
                self.input_history.get(n + 1).map(|s| s.as_str())
            }
            Some(_) => {
                self.history_cursor = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_moves_left_right_on_char_boundaries() {
        let mut app = App::new();
        app.input = "aé😀b".to_string();
        app.input_cursor = app.input.len();

        app.cursor_left(); // before 'b'
        assert_eq!(app.input_cursor, "aé😀".len());
        app.cursor_left(); // before 😀
        assert_eq!(app.input_cursor, "aé".len());
        app.cursor_left(); // before é
        assert_eq!(app.input_cursor, "a".len());
        app.cursor_left(); // before 'a'
        assert_eq!(app.input_cursor, 0);
        app.cursor_left(); // clamped
        assert_eq!(app.input_cursor, 0);

        app.cursor_right(); // after 'a'
        assert_eq!(app.input_cursor, "a".len());
        app.cursor_right(); // after é
        assert_eq!(app.input_cursor, "aé".len());
        app.cursor_right(); // after 😀
        assert_eq!(app.input_cursor, "aé😀".len());
        app.cursor_right(); // after 'b'
        assert_eq!(app.input_cursor, "aé😀b".len());
        app.cursor_right(); // clamped
        assert_eq!(app.input_cursor, "aé😀b".len());
    }

    #[test]
    fn insert_at_cursor_keeps_utf8_boundary() {
        let mut app = App::new();
        app.input = "ab".to_string();
        app.input_cursor = 1;

        app.insert_char('😀');
        assert_eq!(app.input, "a😀b");
        assert_eq!(app.input_cursor, "a😀".len());
        assert!(app.input.is_char_boundary(app.input_cursor));
    }

    #[test]
    fn backspace_deletes_char_before_cursor() {
        let mut app = App::new();
        app.input = "aé😀b".to_string();
        app.input_cursor = app.input.len();

        app.delete_before_cursor(); // delete 'b'
        assert_eq!(app.input, "aé😀");
        assert_eq!(app.input_cursor, "aé😀".len());

        app.delete_before_cursor(); // delete 😀
        assert_eq!(app.input, "aé");
        assert_eq!(app.input_cursor, "aé".len());

        app.delete_before_cursor(); // delete é
        assert_eq!(app.input, "a");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_before_cursor(); // delete 'a'
        assert_eq!(app.input, "");
        assert_eq!(app.input_cursor, 0);

        app.delete_before_cursor(); // no-op at start
        assert_eq!(app.input, "");
        assert_eq!(app.input_cursor, 0);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut app = App::new();
        app.input = "aé😀b".to_string();
        app.input_cursor = "a".len();

        app.delete_at_cursor(); // delete é
        assert_eq!(app.input, "a😀b");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_at_cursor(); // delete 😀
        assert_eq!(app.input, "ab");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_at_cursor(); // delete 'b'
        assert_eq!(app.input, "a");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_at_cursor(); // no-op at end
        assert_eq!(app.input, "a");
        assert_eq!(app.input_cursor, "a".len());
    }

    #[test]
    fn home_end_jump_to_ends() {
        let mut app = App::new();
        app.input = "hello".to_string();
        app.input_cursor = 2;

        app.cursor_home();
        assert_eq!(app.input_cursor, 0);

        app.cursor_end();
        assert_eq!(app.input_cursor, "hello".len());
    }
}
