use std::collections::HashMap;

use ratatui::style::Style;

use crate::tui::widgets::{ActivePromptWidget, SystemWidget, Widget, WidgetEvent};

/// Application state for the TUI.
///
/// Pure state, no I/O. Owned by the event loop and passed mutably to the
/// renderer on each frame.
///
/// ## Rendering model
///
/// The ratatui fullscreen viewport occupies the alternate screen buffer and
/// is split into three regions:
/// 1. Transcript — widget list rendered bottom-up. Widgets that don't fit
///    scroll off the top into the terminal's scrollback buffer.
/// 2. Input bar.
/// 3. Info bar.
///
/// The alternate screen is entered on startup and left on exit. There is no
/// terminal scrollback promotion — the transcript grows unbounded within
/// the viewport, and scrolling through history is handled by the terminal
/// multiplexer (e.g. tmux copy-mode).
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
    /// Transcript widget list. Rendered bottom-up; widgets that don't fit
    /// scroll off the top into the terminal's scrollback buffer.
    pub transcript: Vec<Box<dyn Widget>>,
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
            transcript: Vec::new(),
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

    // ── Transcript helpers ──────────────────────────────────────────

    /// Push a system message (banner, daemon event, command output, error).
    /// Auto-collapses all existing SystemWidgets above it.
    /// Inserts before the active widget when a request is in-flight so the
    /// ActivePromptWidget remains the bottom-most widget.
    pub fn push_system(&mut self, lines: Vec<(String, Style)>) {
        // Auto-collapse existing SystemWidgets.
        for w in &mut self.transcript {
            w.auto_collapse();
        }
        let widget: Box<dyn Widget> = Box::new(SystemWidget::new(lines, true));
        // Insert before the active widget if one is present, so the
        // ActivePromptWidget stays at the bottom.
        let insert_at = if self.transcript.last().is_some_and(|w| !w.is_expandable()) {
            self.transcript.len().saturating_sub(1)
        } else {
            self.transcript.len()
        };
        self.transcript.insert(insert_at, widget);
    }

    /// Push a single-line system message with default style.
    pub fn push_system_line(&mut self, line: String) {
        self.push_system(vec![(line, Style::default())]);
    }

    /// Push a single-line system message with a specific style.
    #[allow(dead_code)]
    pub fn push_system_line_styled(&mut self, line: String, style: Style) {
        self.push_system(vec![(line, style)]);
    }

    /// Mutable ref to the bottom-most widget (for streaming), if any.
    pub fn active_mut(&mut self) -> Option<&mut (dyn Widget + '_)> {
        match self.transcript.last_mut() {
            Some(w) => Some(w.as_mut()),
            None => None,
        }
    }

    /// Finish the active (bottom-most) widget, replacing it with the
    /// returned passive widget. Panics if there is no active widget.
    pub fn finish_active(&mut self, events: Vec<WidgetEvent>) {
        let mut active = self
            .transcript
            .pop()
            .expect("finish_active called with no active widget");
        let finished = active.finish(events);
        self.transcript.push(finished);
    }

    /// Push a new ActivePromptWidget for a chat turn.
    /// Auto-collapses all existing SystemWidgets first.
    pub fn push_active_prompt(&mut self, prompt: String) {
        for w in &mut self.transcript {
            w.auto_collapse();
        }
        self.transcript
            .push(Box::new(ActivePromptWidget::new(prompt)));
    }

    /// Toggle expand/collapse for all expandable widgets.
    /// If any expandable widget is collapsed → expand all.
    /// If all expandable widgets are already expanded → collapse all.
    pub fn toggle_expand_all(&mut self) {
        let any_collapsed = self
            .transcript
            .iter()
            .any(|w| w.is_expandable() && !w.is_expanded());

        for w in &mut self.transcript {
            if w.is_expandable() {
                if any_collapsed {
                    // Expand all: only toggle if currently collapsed.
                    if !w.is_expanded() {
                        w.toggle_expand();
                    }
                } else {
                    // Collapse all: only toggle if currently expanded.
                    if w.is_expanded() {
                        w.toggle_expand();
                    }
                }
            }
        }
    }
}
