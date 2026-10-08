use std::collections::HashMap;

use crate::tui::chat::ChatGremlin;

/// Number of lines reserved in the ratatui viewport for live streaming content
/// (reasoning + visible text deltas).
pub const STREAMING_HEIGHT: u16 = 10;

/// Application state for the TUI.
///
/// Pure state, no I/O. Owned by the event loop and passed mutably to the
/// renderer on each frame.
///
/// ## Rendering model
///
/// The transcript (command output, help text, subprocess results) is printed
/// directly to stdout as ordinary terminal lines — it becomes terminal
/// scrollback and tmux copy-mode history. Ratatui is **not** used for the
/// transcript region.
///
/// Only the bottom two lines (input bar + info bar) are managed by the TUI
/// and redrawn in place on each frame. No `EnterAlternateScreen` — raw mode
/// only, in the main terminal buffer.
///
/// The `output` buffer is a write-only in-memory log used for:
/// - Reprinting the transcript after terminal resize (`Ctrl+L`).
/// - `/clear` (resets the in-memory buffer; cannot retroactively clear stdout).
pub struct App {
    /// Lines accumulated in the output log (write-only; never ratatui-painted).
    pub output: Vec<String>,
    /// Current text in the input bar.
    pub input: String,
    /// Live gremlin state: id → status ("running", "done", "stopped").
    pub active_runs: HashMap<String, String>,
    /// Cached project name for the info bar.
    pub project_name: String,
    /// When Some, the TUI is following a gremlin's log and printing log lines
    /// to the transcript as they arrive.
    pub following_log: Option<String>,
    /// Active chat agent session. None when no default client is configured.
    pub chat: Option<ChatGremlin>,
    /// Streaming block state for incremental response rendering.
    /// Accumulated text for the current live streaming block.
    pub stream_text: String,
    /// Whether any visible StreamChunk has been received this turn.
    pub streamed_visible_text: bool,
    /// Whether the turn's text has already been committed to scrollback
    /// (guards against double-commit when Done arrives after TurnComplete).
    pub turn_committed: bool,
    /// Whether the next reasoning character starts a new line (needs "  " prefix).
    pub reasoning_line_start: bool,
}

impl App {
    pub fn new() -> Self {
        let project_name = gremlins::config::project_root()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "?".to_string());

        Self {
            output: Vec::new(),
            input: String::new(),
            active_runs: HashMap::new(),
            project_name,
            following_log: None,
            chat: None,
            stream_text: String::new(),
            streamed_visible_text: false,
            turn_committed: false,
            reasoning_line_start: true,
        }
    }

    /// Append a line to the output buffer.
    pub fn push_line(&mut self, line: &str) {
        self.output.push(line.to_string());
    }

    /// Clear the output buffer.
    pub fn clear_output(&mut self) {
        self.output.clear();
    }

    /// Number of active (running) gremlins.
    pub fn gremlin_count_str(&self) -> String {
        let count = self
            .active_runs
            .values()
            .filter(|s| *s == "running")
            .count();
        count.to_string()
    }

    /// Cached project name.
    pub fn project_name_str(&self) -> &str {
        &self.project_name
    }

    /// Update state when a run starts.
    pub fn on_run_started(&mut self, id: String, _definition: String, _stage: String) {
        self.active_runs.insert(id, "running".to_string());
    }

    /// Update state when a run completes.
    pub fn on_run_completed(&mut self, id: String) {
        self.active_runs.insert(id, "done".to_string());
    }

    /// Update state when a run fails.
    pub fn on_run_failed(&mut self, id: String) {
        self.active_runs.insert(id, "failed".to_string());
    }

    /// Update state when a run is stopped.
    pub fn on_run_stopped(&mut self, id: String) {
        self.active_runs.insert(id, "stopped".to_string());
    }
}
