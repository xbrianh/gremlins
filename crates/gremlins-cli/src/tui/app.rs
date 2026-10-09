use std::collections::HashMap;

use ratatui::style::Style;

use crate::tui::widgets::SplitWidget;

/// Application state for the TUI.
///
/// Pure state, no I/O. Owned by the event loop and passed mutably to the
/// renderer on each frame.
///
/// ## Rendering model
///
/// The ratatui fullscreen viewport occupies the alternate screen buffer and
/// is split into four regions:
/// 1. Scrollback — all transcript content (daemon events, command output,
///    banner, chat history, frozen prompt + response lines). Rendered as
///    plain text with `Constraint::Min(0)` so it absorbs all space not used
///    by the other sections.
/// 2. Widget area — live streaming content (reasoning + tool results),
///    rendered by the active [`DynamicWidget`].
/// 3. Input bar.
/// 4. Info bar.
///
/// The alternate screen is entered on startup and left on exit. There is no
/// terminal scrollback promotion — the scrollback buffer grows unbounded
/// within the viewport, and scrolling through history is handled by the
/// terminal multiplexer (e.g. tmux copy-mode).
pub struct App {
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
    /// All transcript content: daemon events, command output, banner, chat
    /// history, frozen prompt + response lines. Grows unbounded within the
    /// alternate-screen viewport; scrolling through history is handled by
    /// the terminal multiplexer.
    pub scrollback_lines: Vec<(String, Style)>,
    /// The active streaming widget, if any. None when idle.
    pub widget: Option<SplitWidget>,
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
            conversation_history: Vec::new(),
            current_response: String::new(),
            active_request: false,
            pending_user_message: String::new(),
            scrollback_lines: Vec::new(),
            widget: None,
        }
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

    /// Push a line to scrollback with default style.
    pub fn push_scrollback(&mut self, line: String) {
        self.scrollback_lines.push((line, Style::default()));
    }

    /// Push a line to scrollback with a specific style.
    pub fn push_scrollback_styled(&mut self, line: String, style: Style) {
        self.scrollback_lines.push((line, style));
    }

    /// Extend scrollback with (String, Style) pairs (e.g. from widget freeze).
    pub fn extend_scrollback(&mut self, lines: Vec<(String, Style)>) {
        self.scrollback_lines.extend(lines);
    }

    /// Compute scrollback height accounting for line wrapping at the given width.
    pub fn scrollback_height(&self, width: u16) -> u16 {
        if self.scrollback_lines.is_empty() {
            return 0;
        }
        let wrap_width = (width as usize).max(1);
        let mut total: u16 = 0;
        for (line, _) in &self.scrollback_lines {
            let chars = line.chars().count();
            let rows = if chars == 0 {
                1
            } else {
                chars.div_ceil(wrap_width)
            };
            total += rows as u16;
        }
        total
    }
}
