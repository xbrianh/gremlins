use std::collections::HashMap;

use crate::tui::completion::CompletionState;

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
    /// Display-only cache of the active chat gremlin's model, populated by
    /// the daemon's `chat_model` op. The daemon owns the ground truth.
    pub chat_model: Option<String>,
    /// When Some, the TUI is following a gremlin's log and printing log lines
    /// to the transcript as they arrive.
    pub following_log: Option<String>,
    /// Whether a chat request is currently in-flight (guards against concurrent submissions).
    pub active_request: bool,
    /// Whether `/clear` is currently recreating the session. Guards against
    /// submitting a prompt (or starting a second reset) before the
    /// replacement session socket is installed.
    pub session_recreating: bool,
    /// Active screen. Chat is the default; the other variants are full-screen
    /// views entered from slash commands and dismissed with `Esc`.
    pub screen: Screen,
    /// Highlighted gremlin id on the `/watch` screen.
    pub watch_selection: Option<String>,
    /// Gremlin whose inline watch stream pane is open, if any.
    pub watch_following: Option<String>,
    /// Error from the current watch follow, if the dedicated socket died
    /// while the run is still reported as running.
    pub watch_follow_error: Option<String>,
    /// Submitted inputs in chronological order (in-memory only).
    pub input_history: Vec<String>,
    /// Cursor into `input_history`; `None` means editing a fresh line.
    pub history_cursor: Option<usize>,
    /// Terminal width cached for buffer trimming. Updated before each frame.
    pub width: u16,
    /// Active filepath-completion state for the main input bar. The state is
    /// inert (no filesystem access); the completion module owns all I/O.
    pub completion: Option<CompletionState>,
    /// Active skill names for the current session. Populated by `/skill`;
    /// cleared by `/clear`. Sent to the daemon on each chat request.
    pub active_skills: Vec<String>,
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
    /// Token usage fields from the daemon's ls response.
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub turns: Option<usize>,
    /// Total cost in USD from the daemon's ls response.
    pub cost_usd: Option<f64>,
}

impl ActiveRun {
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            stage: String::new(),
            definition: String::new(),
            project: String::new(),
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            turns: None,
            cost_usd: None,
        }
    }
}

/// Which full screen is currently active.
///
/// Chat is the default. The other variants are entered from slash commands
/// and dismissed by `Esc` (or the first `Ctrl+C` press), which always
/// returns to [`Screen::Chat`]. Screen-local widget state (log lines, debug
/// prompt/history, connection flags) lives in `LiveArea`'s screen view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// The normal chat transcript with the main input bar.
    Chat,
    /// Full-screen gremlins watch table.
    Runs,
    /// Full-screen interactive debug session with its own prompt.
    Debug { id: String },
}

impl Screen {
    /// Whether the active screen owns a prompt that should receive keystrokes.
    pub fn has_prompt(&self) -> bool {
        matches!(self, Screen::Debug { .. })
    }

    /// Each non-chat screen is a full-screen view.
    pub fn is_chat(&self) -> bool {
        matches!(self, Screen::Chat)
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
            chat_model: None,
            following_log: None,
            active_request: false,
            session_recreating: false,
            screen: Screen::Chat,
            watch_selection: None,
            watch_following: None,
            watch_follow_error: None,
            input_history: Vec::new(),
            history_cursor: None,
            width: 80,
            completion: None,
            active_skills: Vec::new(),
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
        let prompt_tokens = entry
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| existing.as_ref().and_then(|r| r.prompt_tokens));
        let completion_tokens = entry
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| existing.as_ref().and_then(|r| r.completion_tokens));
        let reasoning_tokens = entry
            .get("reasoning_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| existing.as_ref().and_then(|r| r.reasoning_tokens));
        let cached_input_tokens = entry
            .get("cached_input_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| existing.as_ref().and_then(|r| r.cached_input_tokens));
        let cache_creation_input_tokens = entry
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|r| r.cache_creation_input_tokens)
            });
        let turns = entry
            .get("turns")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or_else(|| existing.as_ref().and_then(|r| r.turns));
        let cost_usd = entry
            .get("cost_usd")
            .and_then(|v| v.as_f64())
            .or_else(|| existing.as_ref().and_then(|r| r.cost_usd));

        self.active_runs.insert(
            id,
            ActiveRun {
                status,
                stage,
                definition,
                project,
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
                turns,
                cost_usd,
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
                prompt_tokens: None,
                completion_tokens: None,
                reasoning_tokens: None,
                cached_input_tokens: None,
                cache_creation_input_tokens: None,
                turns: None,
                cost_usd: None,
            },
        );
        self.ensure_watch_selection();
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
        self.ensure_watch_selection();
    }

    // ── Screen helpers ──────────────────────────────────────────────

    /// Enter the full-screen runs table.
    pub fn show_runs(&mut self) {
        self.screen = Screen::Runs;
        self.watch_following = None;
        self.watch_follow_error = None;
        self.ensure_watch_selection();
    }

    /// Stable-sorted ids of every tracked gremlin, running first.
    ///
    /// Done/failed/stopped runs stay in the table so a run that just failed
    /// doesn't vanish at the moment it's most interesting to inspect. The
    /// selection falls back to the first non-running id when nothing is
    /// running, so the table never points at nothing.
    pub fn watch_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.active_runs.keys().cloned().collect();
        ids.sort_by(|a, b| {
            let ra = self
                .active_runs
                .get(a)
                .map(|r| r.status.as_str())
                .unwrap_or_default();
            let rb = self
                .active_runs
                .get(b)
                .map(|r| r.status.as_str())
                .unwrap_or_default();
            let running = |s: &str| s != "running";
            running(ra).cmp(&running(rb)).then_with(|| a.cmp(b))
        });
        ids
    }

    /// Ensure the watch selection points at a listed gremlin, choosing the
    /// first sorted id (running first) when the selection is absent or stale.
    pub fn ensure_watch_selection(&mut self) {
        let ids = self.watch_ids();
        if ids.is_empty() {
            self.watch_selection = None;
            return;
        }
        let stale = self
            .watch_selection
            .as_ref()
            .map(|selected| !ids.contains(selected))
            .unwrap_or(true);
        if stale {
            self.watch_selection = Some(ids[0].clone());
        }
    }

    /// Move the watch selection through the stable-sorted id list.
    pub fn watch_move(&mut self, delta: i32) {
        self.ensure_watch_selection();
        let Some(selected) = self.watch_selection.as_ref() else {
            return;
        };
        let ids = self.watch_ids();
        let Some(current) = ids.iter().position(|id| id == selected) else {
            return;
        };
        let next = (current as i64 + delta as i64).clamp(0, ids.len().saturating_sub(1) as i64);
        self.watch_selection = Some(ids[next as usize].clone());
    }

    /// Enter the full-screen interactive debug session.
    pub fn show_debug(&mut self, id: String) {
        self.screen = Screen::Debug { id };
    }

    /// Exit any non-chat screen and return to chat. Returns `true` when a
    /// full-screen view was actually dismissed.
    pub fn exit_screen(&mut self) -> bool {
        if self.screen.is_chat() {
            false
        } else {
            self.screen = Screen::Chat;
            true
        }
    }

    // ── Completion helpers ──────────────────────────────────────

    /// Store a fresh completion state.
    pub fn set_completion(&mut self, state: CompletionState) {
        self.completion = Some(state);
    }

    /// Advance to the next completion candidate.
    pub fn completion_advance(&mut self) {
        if let Some(state) = &mut self.completion {
            state.advance();
        }
    }

    /// Rewind to the previous completion candidate.
    pub fn completion_rewind(&mut self) {
        if let Some(state) = &mut self.completion {
            state.rewind();
        }
    }

    /// Drop any active completion state.
    pub fn clear_completion(&mut self) {
        self.completion = None;
    }

    /// Update the active completion's replaced span to the given range.
    /// Used after each candidate/prefix insert so the next Tab/BackTab
    /// replaces exactly the text that was inserted.
    pub fn update_completion_span(&mut self, span: std::ops::Range<usize>) {
        if let Some(state) = &mut self.completion {
            state.span = span;
        }
    }

    /// Derive the paint-only suffix for the currently selected completion
    /// candidate: the untyped remainder that appears dimmed after the real
    /// input. Returns `None` when completion is inactive or the current
    /// candidate no longer extends the replaced path word.
    pub fn completion_preview_suffix(&self) -> Option<&str> {
        let state = self.completion.as_ref()?;
        let candidate = state.current();
        // The candidate replaces the path word at `state.span`, which may be
        // preceded by command/argument text. Compare against that slice (the
        // currently inserted text) rather than the whole input buffer.
        let inserted = self.input.get(state.span.start..state.span.end)?;
        let suffix = candidate.strip_prefix(inserted)?;
        (!suffix.is_empty()).then_some(suffix)
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

    /// Replace the completion span with `replacement` and move the cursor to
    /// the end of the replacement. The span must lie on `char` boundaries.
    pub fn replace_input_span(&mut self, span: std::ops::Range<usize>, replacement: String) {
        debug_assert!(self.input.is_char_boundary(span.start));
        debug_assert!(self.input.is_char_boundary(span.end));
        let end = span.end.min(self.input.len());
        self.input.replace_range(span.start..end, &replacement);
        self.input_cursor = span.start + replacement.len();
    }

    /// Reset the cursor to the end of the current input (e.g. after history
    /// recall or editor write-back).
    pub fn reset_cursor_to_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    // ── Input history helpers ───────────────────────────────────────

    /// Maximum number of entries retained in `input_history` (and thus on
    /// disk); older entries are dropped when the cap is exceeded.
    pub const HISTORY_CAP: usize = 500;

    /// Append a submitted input to history and reset the cursor.
    ///
    /// Consecutive duplicates are suppressed (re-submitting the same prompt
    /// does not spam history), and the buffer is capped at
    /// [`Self::HISTORY_CAP`] entries, dropping the oldest when exceeded.
    ///
    /// Returns `true` when the history actually changed (an entry was
    /// appended or dropped), so callers can skip redundant persistence.
    pub fn push_history(&mut self, line: String) -> bool {
        if self.input_history.last() == Some(&line) {
            self.history_cursor = None;
            return false;
        }
        self.input_history.push(line);
        if self.input_history.len() > Self::HISTORY_CAP {
            let excess = self.input_history.len() - Self::HISTORY_CAP;
            self.input_history.drain(..excess);
        }
        self.history_cursor = None;
        true
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

    #[test]
    fn watch_ids_sorts_running_first_then_id() {
        let mut app = App::new();
        app.on_run_started("b".into(), "def".into(), "run".into());
        app.on_run_started("a".into(), "def".into(), "run".into());
        app.on_run_started("c".into(), "def".into(), "run".into());
        app.on_run_completed("a".into());
        app.on_run_failed("c".into());

        // `b` is the only running gremlin; done/failed `a`/`c` follow, sorted
        // by id within the non-running group.
        assert_eq!(
            app.watch_ids(),
            vec!["b".to_string(), "a".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn ensure_watch_selection_picks_first_and_falls_back_to_finished() {
        let mut app = App::new();
        assert_eq!(app.watch_selection, None);

        // An unknown-id failure inserts a finished run; with nothing running
        // the selection must still point at it rather than nothing.
        app.on_run_failed("f".into());
        assert_eq!(app.watch_selection.as_deref(), Some("f"));

        // A stable selection is not yanked away when a new run starts.
        app.on_run_started("r".into(), "def".into(), "run".into());
        assert_eq!(app.watch_selection.as_deref(), Some("f"));

        // Once the selected run leaves the list, selection falls back to the
        // first listed id (running runs sort first → "r").
        app.active_runs.remove("f");
        app.ensure_watch_selection();
        assert_eq!(app.watch_selection.as_deref(), Some("r"));
    }

    #[test]
    fn watch_move_steps_and_clamps() {
        let mut app = App::new();
        app.on_run_started("b".into(), "def".into(), "run".into());
        app.on_run_started("a".into(), "def".into(), "run".into());
        app.on_run_started("c".into(), "def".into(), "run".into());
        // First insert pins the selection; later inserts don't move it.
        assert_eq!(app.watch_selection.as_deref(), Some("b"));

        app.watch_move(1);
        assert_eq!(app.watch_selection.as_deref(), Some("c"));
        app.watch_move(1); // clamped at the end
        assert_eq!(app.watch_selection.as_deref(), Some("c"));

        app.watch_move(-1);
        assert_eq!(app.watch_selection.as_deref(), Some("b"));
        app.watch_move(-1);
        assert_eq!(app.watch_selection.as_deref(), Some("a"));
        app.watch_move(-1); // clamped at the start
        assert_eq!(app.watch_selection.as_deref(), Some("a"));
    }
}
