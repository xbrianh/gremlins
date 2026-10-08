use std::collections::HashMap;

/// Max rows the streaming area grows to before scrolling internally.
pub const STREAMING_HEIGHT: u16 = 8;

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
/// The ratatui inline viewport is split into three content sections when a
/// turn is active:
/// 1. Prompt — the user's message, static at the top.
/// 2. Streaming area — live reasoning + tool results, fixed height, scrolls.
/// 3. Response area — accumulated model response, grows downward.
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
    /// Accumulated reasoning text for the current live streaming block.
    pub reasoning_stream: String,
    /// Accumulated response text for the current live streaming block.
    pub response_stream: String,
    /// Whether any visible StreamChunk has been received this turn.
    pub streamed_visible_text: bool,
    /// Whether the response area has been expanded (first StreamChunk arrived).
    /// Avoids a blank gap while the agent is still thinking / running tools.
    pub response_area_open: bool,
    /// Whether the turn's text has already been committed to scrollback
    /// (guards against double-commit when Done arrives after TurnComplete).
    pub turn_committed: bool,
    /// Whether the "thinking..." placeholder is currently shown.
    pub showing_thinking: bool,
    /// Whether there is an active streaming session (used for dynamic
    /// viewport sizing — collapses the streaming area when idle).
    pub streaming_active: bool,
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
    /// Snapshotted prompt line count so viewport height stays stable.
    pub prompt_lines: u16,
    /// Current streaming-area row count (grows up to STREAMING_HEIGHT).
    pub streaming_rows: u16,
    /// Current response-area row count.
    pub response_rows: u16,
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
            reasoning_stream: String::new(),
            response_stream: String::new(),
            streamed_visible_text: false,
            response_area_open: false,
            turn_committed: false,
            showing_thinking: false,
            streaming_active: false,
            conversation_history: Vec::new(),
            current_response: String::new(),
            active_request: false,
            pending_user_message: String::new(),
            prompt: String::new(),
            prompt_lines: 0,
            streaming_rows: 0,
            response_rows: 0,
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
