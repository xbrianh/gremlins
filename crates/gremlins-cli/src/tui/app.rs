use std::collections::HashMap;

use crate::tui::widgets::StreamWidget;

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
/// The ratatui inline viewport is split into two regions when a turn is
/// active:
/// 1. Prompt — the user's message, static at the top.
/// 2. Widget area — live streaming content (reasoning + tool results),
///    rendered by the active [`DynamicWidget`].
///
/// When idle only the input bar and info bar are rendered.
///
/// No `EnterAlternateScreen` — raw mode only, in the main terminal buffer.
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
    /// Ordered conversation history: each entry is {"role": "user"|"assistant", "content": "..."}
    pub conversation_history: Vec<serde_json::Value>,
    /// Accumulated assistant response text for the current turn (committed to history on Done).
    pub current_response: String,
    /// Whether a chat request is currently in-flight (guards against concurrent submissions).
    pub active_request: bool,
    /// The user message for the current in-flight request (committed to history on Done).
    pub pending_user_message: String,
    /// The user's prompt for display in the viewport during streaming.
    pub prompt: String,
    /// The active streaming widget, if any. None when idle.
    pub widget: Option<StreamWidget>,
    /// Accumulated model response text, flushed to scrollback on newline
    /// boundaries during the turn and fully on Done.
    pub response_stream: String,
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
            conversation_history: Vec::new(),
            current_response: String::new(),
            active_request: false,
            pending_user_message: String::new(),
            prompt: String::new(),
            widget: None,
            response_stream: String::new(),
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

    /// Compute prompt line count (wrapping at ~80 cols).
    pub fn prompt_lines(&self) -> u16 {
        if self.prompt.is_empty() {
            return 0;
        }
        let mut total: u16 = 0;
        for line in self.prompt.lines() {
            let chars = line.chars().count();
            // Each line gets a "> " prefix on its first wrapped row;
            // subsequent wrapped rows use a 2-char indent.
            let first_row_width = 78usize; // 80 - "> "
            let cont_row_width = 78usize; // 80 - "  "
            if chars == 0 || chars <= first_row_width {
                total += 1;
            } else {
                let remaining = chars - first_row_width;
                total += 1 + (remaining.div_ceil(cont_row_width) as u16);
            }
        }
        total.max(1)
    }
}
