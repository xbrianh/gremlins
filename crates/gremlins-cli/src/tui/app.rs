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
    /// Live gremlin state: id → run details.
    pub active_runs: HashMap<String, ActiveRun>,
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
    /// Active ephemeral overlay, if any.
    pub overlay: Option<Overlay>,
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
    WatchSingle { id: String, log_lines: Vec<String> },
    /// Interactive debug session. Blocking overlay with its own prompt.
    Debug {
        id: String,
        input: String,
        history: Vec<String>,
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
            conversation_history: Vec::new(),
            current_response: String::new(),
            active_request: false,
            pending_user_message: String::new(),
            transcript: Vec::new(),
            overlay: None,
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

    /// Whether the main input bar should be disabled by the active overlay.
    #[allow(dead_code)]
    pub fn overlay_blocks_input(&self) -> bool {
        self.overlay.as_ref().is_some_and(Overlay::is_blocking)
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

    pub fn open_watch_single(&mut self, id: String) {
        self.overlay = Some(Overlay::WatchSingle {
            id,
            log_lines: Vec::new(),
        });
    }

    pub fn open_debug(&mut self, id: String) {
        self.overlay = Some(Overlay::Debug {
            id,
            input: String::new(),
            history: Vec::new(),
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
