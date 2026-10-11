use std::collections::HashMap;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::tui::buffer::LineOrigin;
use crate::tui::widgets::{
    InfoBarWidget, InputBarWidget, ReasoningWidget, ToolsWidget, TranscriptWidget,
};

/// Application state for the TUI.
///
/// Pure state, no I/O. Owned by the event loop and passed mutably to the
/// renderer on each frame.
///
/// ## Transcript model
///
/// Completed turns are flattened into [`scrollable`](Self::scrollable). The
/// current turn lives in separate fields ([`current_prompt`](Self::current_prompt),
/// [`reasoning`](Self::reasoning), [`tools`](Self::tools),
/// [`response_text`](Self::response_text)) and is rendered below the transcript.
///
/// Ephemeral regions (reasoning, tools) are bounded (max 5 visible rows each)
/// and are never committed to terminal scrollback. Only
/// [`scrollable`](Self::scrollable) and [`response_text`](Self::response_text)
/// can overflow.
pub struct App {
    /// Current text in the input bar.
    pub input: String,
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

    // ── LiveArea widgets ──────────────────────────────────────────
    //
    /// Scrollable transcript (completed turns + current prompt + response).
    pub(crate) transcript: TranscriptWidget,
    /// Current-turn reasoning region (bounded to 5 visible rows).
    pub(crate) reasoning: ReasoningWidget,
    /// Current-turn active-tools region (bounded to 5 visible rows).
    pub(crate) tools: ToolsWidget,
    /// Input bar (stateless).
    pub(crate) input_bar: InputBarWidget,
    /// Info bar (stateless).
    pub(crate) info_bar: InfoBarWidget,
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

/// Ephemeral overlay widgets rendered above the transcript.
#[derive(Debug, Clone)]
pub enum Overlay {
    /// Gremlins watch table. Non-blocking; main input bar still works.
    Watch,
    /// Single-gremlin log viewer. Blocking overlay with its own log buffer.
    WatchSingle { log_lines: Vec<String> },
    /// Interactive debug session. Blocking overlay with its own prompt.
    Debug {
        input: String,
        history: Vec<String>,
        /// Whether the debug socket is open but `ready` has not arrived yet.
        connecting: bool,
        /// Whether the debug socket session is still connected.
        connected: bool,
    },
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
        matches!(self, Overlay::Debug { .. })
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
            active_runs: HashMap::new(),
            project_name,
            following_log: None,
            chat_id: None,
            active_request: false,
            overlay: None,
            input_history: Vec::new(),
            history_cursor: None,
            transcript: TranscriptWidget::new(),
            reasoning: ReasoningWidget::new(),
            tools: ToolsWidget::new(),
            input_bar: InputBarWidget,
            info_bar: InfoBarWidget,
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

    pub fn open_watch_single(&mut self, _id: String) {
        self.overlay = Some(Overlay::WatchSingle {
            log_lines: Vec::new(),
        });
    }

    pub fn open_debug(&mut self, _id: String) {
        self.overlay = Some(Overlay::Debug {
            input: String::new(),
            history: Vec::new(),
            connecting: true,
            connected: false,
        });
    }

    /// Mutable debug-prompt input, when the debug overlay is active.
    pub fn overlay_prompt_mut(&mut self) -> Option<&mut String> {
        match self.overlay {
            Some(Overlay::Debug { ref mut input, .. }) => Some(input),
            _ => None,
        }
    }

    /// Take the debug prompt for submission.
    pub fn take_overlay_prompt(&mut self) -> Option<String> {
        match &mut self.overlay {
            Some(Overlay::Debug { input, .. }) => Some(std::mem::take(input)),
            _ => None,
        }
    }

    /// Append a line to the active single-gremlin log overlay, if visible.
    pub fn push_overlay_log_line(&mut self, line: String) {
        if let Some(Overlay::WatchSingle { log_lines, .. }) = &mut self.overlay {
            log_lines.push(line);
        }
    }

    /// Append a line to the debug overlay scrollback, if visible.
    pub fn push_overlay_debug_output(&mut self, line: String) {
        if let Some(Overlay::Debug { history, .. }) = &mut self.overlay {
            history.push(line);
        }
    }

    /// Whether the active debug overlay is still connecting (socket open,
    /// `ready` not yet received).
    pub fn debug_connecting(&self) -> bool {
        matches!(
            self.overlay,
            Some(Overlay::Debug {
                connecting: true,
                ..
            })
        )
    }

    /// Whether the active debug overlay is still connected to the daemon.
    pub fn debug_connected(&self) -> bool {
        matches!(
            self.overlay,
            Some(Overlay::Debug {
                connected: true,
                ..
            })
        )
    }

    /// Set the connected flag on the active debug overlay. Setting `false`
    /// also clears the connecting flag so a session that never became ready
    /// is reported as ended rather than stuck connecting.
    pub fn set_debug_connected(&mut self, connected: bool) {
        if let Some(Overlay::Debug {
            connecting,
            connected: c,
            ..
        }) = &mut self.overlay
        {
            *c = connected;
            if !connected {
                *connecting = false;
            }
        }
    }

    /// Mark the active debug overlay as connected (clears the connecting flag).
    pub fn mark_debug_ready(&mut self) {
        if let Some(Overlay::Debug {
            connecting,
            connected,
            ..
        }) = &mut self.overlay
        {
            *connecting = false;
            *connected = true;
        }
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

    // ── LiveArea region helpers ───────────────────────────────────

    /// Flatten the current turn into the transcript and start a new turn
    /// with the given prompt.
    ///
    /// The previous turn's prompt, reasoning, tools, and response are
    /// flattened with blank-line padding and appended.  Only the tail of
    /// each ephemeral buffer (the portion actually displayed) is preserved;
    /// the head is permanently dropped.  All current-turn fields are then
    /// cleared and the new prompt is set.
    pub fn submit_prompt(&mut self, prompt: String) {
        // Flatten the previous turn if there was one.
        if let Some(prev_prompt) = self.transcript.take_prompt() {
            let prompt_style = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            let marker_style = Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD);

            let width = self.width;

            if !self.transcript.is_empty() {
                self.transcript.push_separator();
                self.transcript.push_separator();
            }

            // Prompt.
            self.transcript.push_line(
                Line::from(vec![
                    Span::styled("> ", marker_style),
                    Span::styled(prev_prompt, prompt_style),
                ]),
                LineOrigin::FlattenedPrompt,
            );

            // Reasoning — only the visible tail.
            let reasoning_tail = self.reasoning.take_tail(width);
            if !reasoning_tail.is_empty() {
                self.transcript.push_separator();
                for line in reasoning_tail {
                    self.transcript
                        .push_line(line, LineOrigin::FlattenedReasoning);
                }
            }

            // Tools — only the visible tail.
            let tools_tail = self.tools.take_tail(width);
            if !tools_tail.is_empty() {
                self.transcript.push_separator();
                for line in tools_tail {
                    self.transcript.push_line(line, LineOrigin::FlattenedTools);
                }
            }

            // Response.
            if !self.transcript.response_is_empty() {
                self.transcript.push_separator();
                let response_lines: Vec<String> = self
                    .transcript
                    .response_text()
                    .lines()
                    .map(|s| s.to_string())
                    .collect();
                for line_str in response_lines {
                    self.transcript.push_line(
                        Line::from(Span::styled(line_str, Style::default())),
                        LineOrigin::FlattenedResponse,
                    );
                }
            }
        }

        // Clear current-turn fields.
        self.transcript.set_prompt(prompt);
        self.reasoning.clear();
        self.tools.clear();
        self.transcript.clear_response();
    }

    /// Flush the reasoning partial buffer (if any) into a completed
    /// reasoning line.
    pub fn flush_reasoning(&mut self) {
        self.reasoning.flush();
    }

    /// Append reasoning text (may be a partial chunk). Text is accumulated
    /// and flushed to a reasoning line only when a newline is encountered.
    pub fn push_reasoning_text(&mut self, text: &str) {
        self.reasoning.push_text(text);
    }

    /// Record a tool call. Oldest entries are discarded once the region
    /// exceeds its visible-row bound.
    pub fn push_tool(&mut self, name: &str) {
        self.tools.push(name);
    }

    /// Append streaming response text.
    pub fn push_response(&mut self, text: &str) {
        self.transcript.push_response(text);
    }

    /// Replace the entire response text (used for Done reconciliation).
    pub fn replace_response(&mut self, text: &str) {
        self.transcript.replace_response(text);
    }

    /// Reconcile the current turn's streamed response against the
    /// authoritative full text from `TurnComplete`.
    ///
    /// `response_text` accumulates across the agent loop until the next
    /// user prompt, so the authoritative text must only repair the current
    /// turn's trailing slice. If the buffer already ends with the full text,
    /// it is left untouched; if it ends with a prefix of the full text, the
    /// truncated suffix is replaced; otherwise the buffer is empty and the
    /// full text is appended.
    pub fn reconcile_response(&mut self, text: &str) {
        self.transcript.reconcile_response(text);
    }

    /// Whether the current turn has any content.
    pub fn has_current_turn(&self) -> bool {
        self.transcript.has_prompt()
    }

    /// Whether the current turn's streaming response is empty.
    pub fn response_is_empty(&self) -> bool {
        self.transcript.response_is_empty()
    }

    /// Discard the current turn without committing it to the transcript.
    pub fn clear_current_turn(&mut self) {
        self.transcript.clear_prompt();
        self.reasoning.clear();
        self.tools.clear();
        self.transcript.clear_response();
    }

    /// Clear the transcript/scrollback.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear_scrollable();
    }

    /// Append a system message to the transcript with default style and
    /// blank-line padding from the previous entry.
    pub fn push_transcript_line(&mut self, line: String) {
        self.transcript.push_transcript_line(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_response_fills_dropped_suffix_without_touching_prior_turns() {
        let mut app = App::new();
        app.push_response("first turn\n");
        app.push_response("second tur");

        app.reconcile_response("second turn complete");

        assert_eq!(
            app.transcript.response_text(),
            "first turn\nsecond turn complete"
        );
    }

    #[test]
    fn reconcile_response_keeps_complete_streamed_text() {
        let mut app = App::new();
        app.push_response("first turn\nsecond turn complete");

        app.reconcile_response("second turn complete");

        assert_eq!(
            app.transcript.response_text(),
            "first turn\nsecond turn complete"
        );
    }

    #[test]
    fn reconcile_response_appends_when_buffer_is_empty() {
        let mut app = App::new();

        app.reconcile_response("complete turn");

        assert_eq!(app.transcript.response_text(), "complete turn");
    }

    #[test]
    fn reconcile_response_ignores_empty_authoritative_text() {
        let mut app = App::new();
        app.push_response("streamed");

        app.reconcile_response("");

        assert_eq!(app.transcript.response_text(), "streamed");
    }
}
