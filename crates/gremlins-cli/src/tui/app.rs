use std::collections::HashMap;

mod conversations;
pub(crate) use super::protocol::Execution;
pub(crate) use conversations::{Conversations, Receipt, Submission};

use crate::tui::completion::CompletionState;
use crate::tui::input_buffer::InputBuffer;

/// Application state for the TUI.
///
/// Pure display state, no I/O and no widget ownership. Owned by the event
/// loop and passed immutably to [`LiveArea::compose`](crate::tui::screen::LiveArea::compose)
/// on each frame. The rendering widgets (transcript, reasoning, tools, input
/// bar, info bar) live in `LiveArea`; `App` only carries the read-only state
/// those widgets consume via their render context.
pub struct App {
    /// Current text in the input bar.
    pub input: InputBuffer,
    /// Cursor byte index into `input` (always on a `char` boundary).
    pub input_cursor: usize,
    /// Inactive composers are moved here, never copied into render state.
    parked_inputs: HashMap<Target, (InputBuffer, usize)>,
    /// Live gremlin state: id → run details.
    pub active_runs: HashMap<String, ActiveRun>,
    pub live_executions: HashMap<String, LiveExecution>,
    replay_executions: HashMap<PipelineSource, ExecutionReplay>,
    lifecycle_sequences: HashMap<String, u64>,
    lifecycle_cuts: Vec<(ExecutionReplay, u64)>,
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
    pub home_run_id: Option<String>,
    /// Active screen. Chat is the default; the other variants are full-screen
    /// views entered from slash commands and dismissed with `Esc`.
    pub screen: Screen,
    /// The conversation target currently selected in the UI. The home chat is
    /// the default; stage-execution targets are attached from run events.
    pub selected_target: Target,
    conversation_statuses: HashMap<Target, ConversationStatus>,
    /// Routing metadata is independent of which target currently owns the input widget.
    pub conversations: Conversations,
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
    /// Active reverse history search state (Ctrl+R). When `Some`, the
    /// input bar renders the search prompt instead of the normal chevron.
    pub history_search: Option<HistorySearch>,
}

/// One lifecycle-confirmed stage invocation. Keys are opaque watch selections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveExecution {
    pub run_id: String,
    pub stage: String,
    pub stage_type: String,
    pub stage_run_id: String,
}

impl From<super::protocol::Invocation> for LiveExecution {
    fn from(invocation: super::protocol::Invocation) -> Self {
        Self {
            run_id: invocation.run_id,
            stage: invocation.stage,
            stage_type: invocation.stage_type,
            stage_run_id: invocation.stage_run_id,
        }
    }
}

impl LiveExecution {
    pub fn target(&self) -> Target {
        Target::Invocation {
            run_id: self.run_id.clone(),
            stage: self.stage.clone(),
            stage_run_id: self.stage_run_id.clone(),
        }
    }

    pub fn key(&self) -> String {
        format!(
            "invocation:{}",
            serde_json::json!([self.run_id, self.stage, self.stage_run_id])
        )
    }
}

/// Independent subscription streams may replay concurrently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PipelineSource {
    Passive,
    Inline,
}

#[derive(Default)]
struct ExecutionReplay {
    run_id: Option<String>,
    stage: Option<String>,
    stage_run_id: Option<String>,
    candidates: HashMap<String, (LiveExecution, bool, Option<u64>)>,
    live_updates: HashMap<String, Option<LiveExecution>>,
    incomplete: bool,
}

impl ExecutionReplay {
    fn new(frame: &serde_json::Value) -> Self {
        Self {
            run_id: frame["id"].as_str().map(str::to_owned),
            stage: frame["stage"].as_str().map(str::to_owned),
            stage_run_id: frame["stage_run_id"].as_str().map(str::to_owned),
            ..Self::default()
        }
    }

    fn matches(&self, execution: &LiveExecution) -> bool {
        !matches!(&self.run_id, Some(id) if id != &execution.run_id)
            && !matches!(&self.stage, Some(stage) if stage != &execution.stage)
            && !matches!(&self.stage_run_id, Some(id) if id != &execution.stage_run_id)
    }
}

/// One tracked gremlin run.
#[derive(Debug, Clone)]
pub struct ActiveRun {
    /// "running", "done", "failed", or "stopped".
    pub status: String,
    /// Most recent stage reported by the daemon.
    pub stage: String,
    /// Active leaf stage for blob lookup during nested execution.
    pub sub_stage: String,
    /// Pipeline name carried by [`gremlins_magic_bus::EventKind::RunStarted`].
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
    /// Per-turn prompt tokens from the most recently completed turn.
    pub last_prompt_tokens: Option<u64>,
    /// Per-turn cached input tokens from the most recently completed turn.
    pub last_cached_input_tokens: Option<u64>,
}

impl ActiveRun {
    pub fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            stage: String::new(),
            sub_stage: String::new(),
            definition: String::new(),
            project: String::new(),
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            turns: None,
            cost_usd: None,
            last_prompt_tokens: None,
            last_cached_input_tokens: None,
        }
    }
}

/// Interactive reverse history search state (Ctrl+R).
///
/// During search, `App::input` holds the incremental query string and
/// `App::input_cursor` tracks position within it. The pre-search input is
/// saved so it can be restored on cancel.
pub struct HistorySearch {
    /// Indices into `input_history` that match the query, newest first.
    matches: Vec<usize>,
    /// Current position within `matches` (0 = first / newest match).
    position: usize,
    /// Saved input buffer (text + substitutions) before search started,
    /// restored on cancel.
    saved_input: InputBuffer,
    /// Cursor position snapshotted before search started.
    saved_cursor: usize,
}

impl HistorySearch {
    /// Forget the underlying draft without disturbing the incremental search.
    pub(super) fn clear_saved_draft(&mut self) {
        self.saved_input.clear();
        self.saved_cursor = 0;
    }

    /// Whether no history entries match the current query.
    pub fn is_failed(&self) -> bool {
        self.matches.is_empty()
    }
}

/// Which full screen is currently active.
///
/// Chat is the default. The other variants are entered from slash commands
/// and dismissed by `Esc` (or the first `Ctrl+C` press), which always
/// returns to [`Screen::Chat`]. Screen-local widget state (watch log lines)
/// lives in `LiveArea`'s screen view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// The normal chat transcript with the main input bar.
    Chat,
    /// Full-screen gremlins watch table.
    Runs,
}

impl Screen {
    /// Each non-chat screen is a full-screen view.
    pub fn is_chat(&self) -> bool {
        matches!(self, Screen::Chat)
    }
}

/// Identifies a conversation target in the TUI.
///
/// The home chat has a stable identity across prompts and `/clear` cycles.
/// A stage execution is keyed by run id and stage name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// The session's home chat conversation.
    Home,
    /// A selected stage execution from a run.
    StageExecution { run_id: String, stage: String },
    /// A pinned invocation, never a replacement with the same stage name.
    Invocation {
        run_id: String,
        stage: String,
        stage_run_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationStatus {
    Idle,
    ReadOnly,
    Running,
    Paused,
    Ended,
}

impl ConversationStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::ReadOnly => "read-only",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Ended => "ended",
        }
    }
}

impl Target {
    /// Short label for display in the info bar.
    pub fn label(&self) -> String {
        match self {
            Target::Home => "chat".to_string(),
            Target::StageExecution { run_id, stage } => format!("{run_id}:{stage}"),
            Target::Invocation {
                run_id,
                stage,
                stage_run_id,
            } => format!("{run_id}:{stage}:{stage_run_id}"),
        }
    }

    /// Whether this target is the home chat.
    pub fn is_home(&self) -> bool {
        matches!(self, Target::Home)
    }
}

impl App {
    pub fn new() -> Self {
        let project_name = gremlins::config::project_root()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "?".to_string());

        Self {
            input: InputBuffer::new(),
            input_cursor: 0,
            parked_inputs: HashMap::new(),
            active_runs: HashMap::new(),
            live_executions: HashMap::new(),
            replay_executions: HashMap::new(),
            lifecycle_sequences: HashMap::new(),
            lifecycle_cuts: Vec::new(),
            project_name,
            chat_model: None,
            following_log: None,
            active_request: false,
            session_recreating: false,
            home_run_id: None,
            screen: Screen::Chat,
            selected_target: Target::Home,
            conversation_statuses: HashMap::new(),
            conversations: Conversations::default(),
            watch_selection: None,
            watch_following: None,
            watch_follow_error: None,
            input_history: Vec::new(),
            history_cursor: None,
            width: 80,
            completion: None,
            active_skills: Vec::new(),
            history_search: None,
        }
    }

    pub fn set_conversation_status(&mut self, target: Target, status: ConversationStatus) {
        self.conversation_statuses.insert(target, status);
    }

    pub fn conversation_status(&self, target: &Target) -> ConversationStatus {
        self.conversation_statuses
            .get(target)
            .copied()
            .unwrap_or_else(|| {
                if target.is_home() {
                    if self.selected_target.is_home() && self.active_request {
                        ConversationStatus::Running
                    } else {
                        ConversationStatus::Idle
                    }
                } else {
                    ConversationStatus::ReadOnly
                }
            })
    }

    pub fn replace_input(&mut self, text: &str) {
        self.input.replace_all(text);
    }

    pub fn add_input_substitution(&mut self, span: std::ops::Range<usize>, value: String) {
        self.input.add_substitution(span, value);
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }

    pub fn take_input(&mut self) -> InputBuffer {
        let input = std::mem::take(&mut self.input);
        self.input_cursor = 0;
        input
    }

    pub fn select_composer(&mut self, target: Target) {
        let next = self.parked_inputs.remove(&target).unwrap_or_default();
        let previous = std::mem::replace(&mut self.selected_target, target);
        let input = std::mem::replace(&mut self.input, next.0);
        let cursor = std::mem::replace(&mut self.input_cursor, next.1);
        if !input.is_empty() {
            self.parked_inputs.insert(previous, (input, cursor));
        }
    }

    pub fn clear_composer(&mut self, target: &Target) {
        if &self.selected_target == target {
            if let Some(search) = &mut self.history_search {
                search.clear_saved_draft();
            } else {
                self.clear_input();
                self.clear_completion();
            }
        } else {
            self.parked_inputs.remove(target);
        }
    }

    pub fn finish_delivery(&mut self, id: u64, result: Result<(), String>) -> Option<Receipt> {
        self.conversations.finish(id, result)
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
        let last_prompt_tokens = entry
            .get("last_prompt_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| existing.as_ref().and_then(|r| r.last_prompt_tokens));
        let last_cached_input_tokens = entry
            .get("last_cached_input_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| existing.as_ref().and_then(|r| r.last_cached_input_tokens));

        self.active_runs.insert(
            id,
            ActiveRun {
                status,
                stage,
                sub_stage: String::new(),
                definition,
                project,
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
                turns,
                cost_usd,
                last_prompt_tokens,
                last_cached_input_tokens,
            },
        );
    }

    /// Update state when a run starts. If a placeholder was already inserted
    /// by an early `TokenUsage` event, merge into it instead of overwriting.
    pub fn on_run_started(&mut self, id: String, definition: String, stage: String) {
        use std::collections::hash_map::Entry;
        match self.active_runs.entry(id) {
            Entry::Occupied(mut e) => {
                let run = e.get_mut();
                run.status = "running".to_string();
                run.stage = stage;
                run.definition = definition;
                if run.project.is_empty() {
                    run.project = self.project_name.clone();
                }
            }
            Entry::Vacant(e) => {
                e.insert(ActiveRun {
                    status: "running".to_string(),
                    stage,
                    sub_stage: String::new(),
                    definition,
                    project: self.project_name.clone(),
                    prompt_tokens: None,
                    completion_tokens: None,
                    reasoning_tokens: None,
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    turns: None,
                    cost_usd: None,
                    last_prompt_tokens: None,
                    last_cached_input_tokens: None,
                });
            }
        }
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

    /// Remove a run from the watch table entirely (the gremlin was rm'd).
    pub fn on_run_removed(&mut self, id: &str) {
        self.active_runs.remove(id);
        self.live_executions
            .retain(|_, execution| execution.run_id != id);
        if self.watch_selection.as_deref() == Some(id) {
            self.watch_selection = None;
        }
        if self.watch_following.as_deref() == Some(id) {
            self.watch_following = None;
            self.watch_follow_error = None;
        }
        self.ensure_watch_selection();
    }

    /// Update the stage of an existing run in-place.
    pub fn on_stage_transition(&mut self, id: String, stage: String, sub_stage: Option<String>) {
        if let Some(run) = self.active_runs.get_mut(&id) {
            run.stage = stage;
            run.sub_stage = sub_stage.unwrap_or_default();
        }
    }

    /// Update token usage fields from a live `TokenUsage` daemon event.
    /// If the run doesn't exist yet (event arrived before RunStarted),
    /// insert a placeholder so the /watch table never shows stale `-`.
    #[allow(clippy::too_many_arguments)]
    pub fn on_token_usage(
        &mut self,
        id: String,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
        turns: usize,
        cost_usd: Option<f64>,
        last_prompt_tokens: Option<u64>,
        last_cached_input_tokens: Option<u64>,
    ) {
        use std::collections::hash_map::Entry;
        match self.active_runs.entry(id) {
            Entry::Occupied(mut e) => {
                let run = e.get_mut();
                run.prompt_tokens = prompt_tokens;
                run.completion_tokens = completion_tokens;
                run.reasoning_tokens = reasoning_tokens;
                run.cached_input_tokens = cached_input_tokens;
                run.cache_creation_input_tokens = cache_creation_input_tokens;
                run.turns = Some(turns);
                run.cost_usd = cost_usd.or(run.cost_usd);
                // Completed-turn updates replace both fields atomically
                // so a turn with (prompt, None) doesn't combine with a
                // stale cached value from the previous turn.
                // Terminal-finalize / resync snapshots send None for
                // both fields and fall through to the .or() path below.
                if last_prompt_tokens.is_some() {
                    run.last_prompt_tokens = last_prompt_tokens;
                    run.last_cached_input_tokens = last_cached_input_tokens;
                } else {
                    run.last_prompt_tokens = last_prompt_tokens.or(run.last_prompt_tokens);
                    run.last_cached_input_tokens =
                        last_cached_input_tokens.or(run.last_cached_input_tokens);
                }
            }
            Entry::Vacant(e) => {
                e.insert(ActiveRun {
                    status: "running".to_string(),
                    stage: String::new(),
                    sub_stage: String::new(),
                    definition: String::new(),
                    project: self.project_name.clone(),
                    prompt_tokens,
                    completion_tokens,
                    reasoning_tokens,
                    cached_input_tokens,
                    cache_creation_input_tokens,
                    turns: Some(turns),
                    cost_usd,
                    last_prompt_tokens,
                    last_cached_input_tokens,
                });
                self.ensure_watch_selection();
            }
        }
    }

    pub(crate) fn update_run_status(&mut self, id: &str, status: &str) {
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

    /// Discard the passive subscription's replay without changing live executions.
    pub fn abandon_pipeline_replay(&mut self) {
        self.replay_executions.remove(&PipelineSource::Passive);
    }

    /// Discard only the inline subscription's pending replay.
    pub fn abandon_inline_pipeline_replay(&mut self) {
        self.replay_executions.remove(&PipelineSource::Inline);
    }

    /// Observe passive subscription frames. Candidates stay hidden until a complete cut.
    pub fn observe_pipeline_frame(&mut self, frame: &serde_json::Value) {
        self.observe_pipeline_frame_from(PipelineSource::Passive, frame);
    }

    /// Observe frames from the independent inline watch subscription.
    pub fn observe_inline_pipeline_frame(&mut self, frame: &serde_json::Value) {
        self.observe_pipeline_frame_from(PipelineSource::Inline, frame);
    }

    fn observe_pipeline_frame_from(&mut self, source: PipelineSource, frame: &serde_json::Value) {
        let kind = frame["type"].as_str().unwrap_or_default();
        if kind == "watched" && frame["replay"] == true {
            let incomplete = self
                .replay_executions
                .get(&source)
                .is_some_and(|replay| replay.incomplete);
            let mut replay = ExecutionReplay::new(frame);
            replay.incomplete = incomplete;
            self.replay_executions.insert(source, replay);
            return;
        }
        if kind == "history_gap" {
            let replay = self
                .replay_executions
                .entry(source)
                .or_insert_with(|| ExecutionReplay::new(frame));
            replay.incomplete = true;
            return;
        }
        if kind == "replay_checkpoint" {
            if let Some(mut replay) = self.replay_executions.remove(&source) {
                let complete = frame["complete"] == true && !replay.incomplete;
                let cut = frame["through_sequence"].as_u64();
                if complete && replay.run_id.is_some() {
                    let absent: Vec<_> = self
                        .live_executions
                        .iter()
                        .filter(|(key, execution)| {
                            replay.matches(execution)
                                && !replay.candidates.contains_key(*key)
                                && (cut.is_some() || !replay.live_updates.contains_key(*key))
                        })
                        .map(|(_, execution)| execution.clone())
                        .collect();
                    for execution in absent {
                        self.apply_lifecycle(execution, false, cut);
                    }
                }
                for (key, (execution, active, sequence)) in std::mem::take(&mut replay.candidates) {
                    let sequence = if complete { cut.or(sequence) } else { sequence };
                    if (complete || !active)
                        && (sequence.is_some() || !replay.live_updates.contains_key(&key))
                    {
                        self.apply_lifecycle(execution, active, sequence);
                    }
                }
                if complete {
                    if let Some(cut) = cut {
                        // Store only selector metadata, not per-replay live updates.
                        replay.live_updates.clear();
                        // Retain only the newest cut for an identical selector.
                        let previous = self.lifecycle_cuts.iter().position(|(scope, _)| {
                            scope.run_id == replay.run_id
                                && scope.stage == replay.stage
                                && scope.stage_run_id == replay.stage_run_id
                        });
                        if let Some(index) = previous {
                            if self.lifecycle_cuts[index].1 < cut {
                                self.lifecycle_cuts[index] = (replay, cut);
                            }
                        } else {
                            self.lifecycle_cuts.push((replay, cut));
                        }
                    }
                }
            }
            self.ensure_watch_selection();
            return;
        }
        if kind != "stage_boundary" {
            return;
        }
        let Some(execution) = super::protocol::invocation_from_frame(frame) else {
            return;
        };
        let Some(active) = frame["active"].as_bool() else {
            return;
        };
        let execution = LiveExecution::from(execution);
        let key = execution.key();
        let sequence = frame["sequence"].as_u64();
        match self.replay_executions.get_mut(&source) {
            Some(replay) if replay.matches(&execution) => {
                let stale = replay.candidates.get(&key).is_some_and(
                    |(_, _, old)| matches!(sequence.zip(*old), Some((new, old)) if new <= old),
                );
                if !stale {
                    replay.candidates.insert(key, (execution, active, sequence));
                }
            }
            _ => self.observe_live_pipeline_frame(frame),
        }
        self.ensure_watch_selection();
    }

    /// Observe a live notification independently of any watch replay in progress.
    pub fn observe_live_pipeline_frame(&mut self, frame: &serde_json::Value) {
        if frame["type"] != "stage_boundary" {
            return;
        }
        let Some(execution) = super::protocol::invocation_from_frame(frame) else {
            return;
        };
        let Some(active) = frame["active"].as_bool() else {
            return;
        };
        let execution = LiveExecution::from(execution);
        let key = execution.key();
        for replay in self.replay_executions.values_mut() {
            if replay.matches(&execution) {
                replay
                    .live_updates
                    .insert(key.clone(), active.then(|| execution.clone()));
            }
        }
        self.apply_lifecycle(execution, active, frame["sequence"].as_u64());
        self.ensure_watch_selection();
    }

    /// Sequence tombstones and selector cuts also protect executions absent from the table.
    fn apply_lifecycle(&mut self, execution: LiveExecution, active: bool, sequence: Option<u64>) {
        let key = execution.key();
        if let Some(sequence) = sequence {
            if self
                .lifecycle_sequences
                .get(&key)
                .is_some_and(|old| *old >= sequence)
                || self
                    .lifecycle_cuts
                    .iter()
                    .any(|(scope, cut)| scope.matches(&execution) && *cut >= sequence)
            {
                return;
            }
            self.lifecycle_sequences.insert(key.clone(), sequence);
        }
        if active {
            self.live_executions.insert(key, execution);
        } else {
            self.live_executions.remove(&key);
        }
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
        let mut grouped: HashMap<&str, Vec<String>> = HashMap::new();
        let mut orphans = Vec::new();
        for (key, execution) in &self.live_executions {
            if self.active_runs.contains_key(&execution.run_id) {
                grouped
                    .entry(&execution.run_id)
                    .or_default()
                    .push(key.clone());
            } else {
                orphans.push(key.clone());
            }
        }
        let mut rows = Vec::new();
        for id in ids {
            let mut children = grouped.remove(id.as_str()).unwrap_or_default();
            rows.push(id);
            children.sort();
            rows.extend(children);
        }
        orphans.sort();
        rows.extend(orphans);
        rows
    }

    pub fn watch_target(&self) -> Option<Target> {
        let key = self.watch_selection.as_deref()?;
        if let Some(execution) = self.live_executions.get(key) {
            return Some(execution.target());
        }
        self.active_runs.get(key)?;
        if self.home_run_id.as_deref() == Some(key) {
            return Some(Target::Home);
        }
        Some(Target::StageExecution {
            run_id: key.to_owned(),
            stage: String::new(),
        })
    }

    pub fn watch_run_id<'a>(&'a self, key: &'a str) -> Option<&'a str> {
        self.live_executions
            .get(key)
            .map(|execution| execution.run_id.as_str())
            .or_else(|| self.active_runs.contains_key(key).then_some(key))
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

    /// Drop the active completion preview while preserving path
    /// substitutions.
    ///
    /// Used by ordinary edit/cursor keys. The editing operations in
    /// [`InputBuffer`] already invalidate any substitution they cross, so
    /// dismissing the preview must not wipe unrelated resolved paths.
    pub fn dismiss_completion(&mut self) {
        self.completion = None;
    }

    /// Drop any active completion state and all path substitutions.
    ///
    /// Used on input-bar resets (hard cancel, Esc, `/clear`, search entry,
    /// submission) where the buffer is being replaced wholesale.
    pub fn clear_completion(&mut self) {
        self.completion = None;
        self.input.clear_substitutions();
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
        self.input.insert_char(idx, ch);
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
        self.input.delete_range(start..self.input_cursor);
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
        self.input.delete_range(self.input_cursor..end);
    }

    /// Replace the completion span with `replacement` and move the cursor to
    /// the end of the replacement. The span must lie on `char` boundaries.
    ///
    /// [`InputBuffer::replace_range`] drops any substitution overlapping the
    /// span and shifts later substitutions, so the caller only has to record
    /// the fresh substitution for `replacement` afterwards.
    pub fn replace_input_span(&mut self, span: std::ops::Range<usize>, replacement: String) {
        debug_assert!(self.input.is_char_boundary(span.start));
        debug_assert!(self.input.is_char_boundary(span.end));
        self.input.replace_range(span.clone(), &replacement);
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

    // ── Reverse history search (Ctrl+R) ─────────────────────────

    /// Enter reverse history search mode. Snapshots the current input and
    /// builds the initial match list (every history entry, newest first).
    /// The input bar becomes the incremental query editor; `input` holds
    /// the query string and `input_cursor` sits at its end.
    pub fn enter_search(&mut self) {
        self.clear_completion();
        let saved_input = std::mem::take(&mut self.input);
        self.input.clear();
        let saved_cursor = self.input_cursor;
        self.input_cursor = 0;
        let matches: Vec<usize> = (0..self.input_history.len()).rev().collect();
        self.history_search = Some(HistorySearch {
            matches,
            position: 0,
            saved_input,
            saved_cursor,
        });
    }

    /// Append `ch` to the search query and rebuild the match list.
    pub fn search_push_char(&mut self, ch: char) {
        let Some(_search) = &self.history_search else {
            return;
        };
        self.input.insert_char(self.input.len(), ch);
        self.input_cursor = self.input.len();
        self.rebuild_search_matches();
    }

    /// Remove the last character from the search query and rebuild matches.
    pub fn search_backspace(&mut self) {
        let Some(_search) = &self.history_search else {
            return;
        };
        if !self.input.is_empty() {
            let end = self.input.len();
            let mut start = end - 1;
            while start > 0 && !self.input.is_char_boundary(start) {
                start -= 1;
            }
            self.input.delete_range(start..end);
        }
        self.input_cursor = self.input.len();
        self.rebuild_search_matches();
    }

    /// Cycle to the next (older) match, wrapping around to the newest.
    pub fn search_next(&mut self) {
        let Some(search) = &mut self.history_search else {
            return;
        };
        if !search.matches.is_empty() {
            search.position = (search.position + 1) % search.matches.len();
        }
    }

    /// Cancel search: restore the saved input and clear search state.
    pub fn cancel_search(&mut self) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        self.input = search.saved_input;
        self.input_cursor = search.saved_cursor;
    }

    /// Accept the current match: fill the input bar with the matched entry
    /// but do **not** submit it. Returns to normal editing mode.
    pub fn accept_search(&mut self) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        if let Some(&idx) = search.matches.get(search.position) {
            if let Some(entry) = self.input_history.get(idx) {
                self.input.replace_all(entry);
                self.input_cursor = self.input.len();
                return;
            }
        }
        // No match selected — restore saved input.
        self.input = search.saved_input;
        self.input_cursor = search.saved_cursor;
    }

    /// Return the current search match text, if any.
    pub fn search_match_text(&self) -> Option<&str> {
        let search = self.history_search.as_ref()?;
        let idx = search.matches.get(search.position)?;
        self.input_history.get(*idx).map(|s| s.as_str())
    }

    /// Rebuild the match list from the current query (which is `self.input`).
    fn rebuild_search_matches(&mut self) {
        let Some(search) = &mut self.history_search else {
            return;
        };
        let query_lower = self.input.text().to_lowercase();
        search.matches = self
            .input_history
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| entry.to_lowercase().contains(&query_lower))
            .map(|(i, _)| i)
            .collect();
        search.position = 0;
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
    fn boundary(id: &str, active: bool) -> serde_json::Value {
        serde_json::json!({"type":"stage_boundary","run_id":"run","stage":"work",
            "stage_type":"exec","stage_run_id":id,"active":active})
    }

    #[test]
    fn interleaved_subscription_checkpoints_only_promote_their_own_candidates() {
        use super::PipelineSource::{Inline, Passive};
        for first in [Passive, Inline] {
            let mut app = super::App::new();
            let start = serde_json::json!({"type":"watched", "replay":true});
            app.observe_pipeline_frame(&start);
            app.observe_pipeline_frame(&boundary("passive", true));
            app.observe_inline_pipeline_frame(&start);
            app.observe_inline_pipeline_frame(&boundary("inline", true));
            assert!(app.live_executions.is_empty());
            let checkpoint = serde_json::json!({"type":"replay_checkpoint", "complete":true});
            app.observe_pipeline_frame_from(first, &checkpoint);
            let expected = if first == Passive {
                "passive"
            } else {
                "inline"
            };
            assert_eq!(app.live_executions.len(), 1);
            assert_eq!(
                app.live_executions.values().next().unwrap().stage_run_id,
                expected
            );
            assert_eq!(app.replay_executions.len(), 1);
            // A duplicate checkpoint cannot consume the other stream's replay.
            app.observe_pipeline_frame_from(first, &checkpoint);
            assert_eq!(app.replay_executions.len(), 1);
            let second = if first == Passive { Inline } else { Passive };
            app.observe_pipeline_frame_from(second, &checkpoint);
            assert_eq!(app.live_executions.len(), 2);
            assert!(app.replay_executions.is_empty());
        }
    }

    #[test]
    fn subscription_abandonment_is_isolated() {
        use super::PipelineSource::{Inline, Passive};
        for abandoned in [Passive, Inline] {
            let mut app = super::App::new();
            let start = serde_json::json!({"type":"watched", "replay":true, "id":"run"});
            app.observe_pipeline_frame(&start);
            app.observe_inline_pipeline_frame(&start);
            app.observe_pipeline_frame(&boundary("passive", true));
            app.observe_inline_pipeline_frame(&boundary("inline", true));
            app.observe_live_pipeline_frame(&boundary("live", true));
            if abandoned == Passive {
                app.abandon_pipeline_replay();
            } else {
                app.abandon_inline_pipeline_replay();
            }
            assert!(!app.replay_executions.contains_key(&abandoned));
            assert_eq!(app.replay_executions.len(), 1);
            let checkpoint = serde_json::json!({"type":"replay_checkpoint", "complete":true});
            app.observe_pipeline_frame_from(abandoned, &checkpoint);
            assert_eq!(app.live_executions.len(), 1);
            let remaining = if abandoned == Passive {
                Inline
            } else {
                Passive
            };
            app.observe_pipeline_frame_from(remaining, &checkpoint);
            let candidate = if remaining == Passive {
                "passive"
            } else {
                "inline"
            };
            assert_eq!(app.live_executions.len(), 2);
            assert!(app
                .live_executions
                .values()
                .any(|e| e.stage_run_id == candidate));
            assert!(app
                .live_executions
                .values()
                .any(|e| e.stage_run_id == "live"));
            assert!(app.replay_executions.is_empty());
        }
    }

    #[test]
    fn concurrent_replays_preserve_live_updates_and_isolate_incomplete_cuts() {
        use super::PipelineSource::{Inline, Passive};
        for incomplete in [Passive, Inline] {
            for gap in [false, true] {
                for first in [Passive, Inline] {
                    let mut app = super::App::new();
                    let start = serde_json::json!({"type":"watched", "replay":true, "id":"run"});
                    app.observe_live_pipeline_frame(&boundary("ended", true));
                    app.observe_pipeline_frame(&start);
                    app.observe_inline_pipeline_frame(&start);
                    app.observe_live_pipeline_frame(&boundary("started", true));
                    app.observe_live_pipeline_frame(&boundary("ended", false));
                    for source in [Passive, Inline] {
                        app.observe_pipeline_frame_from(source, &boundary("ended", true));
                        app.observe_pipeline_frame_from(source, &boundary("started", false));
                        let candidate = if source == incomplete {
                            "incomplete"
                        } else {
                            "complete"
                        };
                        app.observe_pipeline_frame_from(source, &boundary(candidate, true));
                    }
                    if gap {
                        app.observe_pipeline_frame_from(
                            incomplete,
                            &serde_json::json!({"type":"history_gap"}),
                        );
                    }
                    let second = if first == Passive { Inline } else { Passive };
                    for source in [first, second] {
                        app.observe_pipeline_frame_from(
                            source,
                            &serde_json::json!({
                                "type":"replay_checkpoint", "complete": gap || source != incomplete
                            }),
                        );
                        assert!(app
                            .live_executions
                            .values()
                            .any(|e| e.stage_run_id == "started"));
                        assert!(app
                            .live_executions
                            .values()
                            .all(|e| e.stage_run_id != "ended" && e.stage_run_id != "incomplete"));
                    }
                    assert_eq!(app.live_executions.len(), 2);
                    assert!(app
                        .live_executions
                        .values()
                        .any(|e| e.stage_run_id == "complete"));
                    assert!(app.replay_executions.is_empty());
                }
            }
        }
    }

    #[test]
    fn run_row_selection_does_not_resolve_current_stage() {
        let mut app = super::App::new();
        let mut run = super::ActiveRun::new("running");
        run.stage = "work".into();
        app.active_runs.insert("run".into(), run);
        app.watch_selection = Some("run".into());
        assert_eq!(
            app.watch_target(),
            Some(super::Target::StageExecution {
                run_id: "run".into(),
                stage: String::new(),
            })
        );
    }

    #[test]
    fn home_run_row_returns_home_but_invocation_stays_exact() {
        let mut app = super::App::new();
        app.home_run_id = Some("run".into());
        app.active_runs
            .insert("run".into(), super::ActiveRun::new("running"));
        app.watch_selection = Some("run".into());
        assert_eq!(app.watch_target(), Some(super::Target::Home));
        app.observe_live_pipeline_frame(&boundary("home-turn", true));
        let execution = app.live_executions.values().next().unwrap();
        let target = execution.target();
        app.watch_selection = Some(execution.key());
        assert_eq!(app.watch_target(), Some(target));
    }

    #[test]
    fn abandoning_pipeline_replay_discards_candidates_and_preserves_live_notifications() {
        let mut app = super::App::new();
        app.observe_live_pipeline_frame(&boundary("existing", true));
        app.observe_pipeline_frame(&serde_json::json!({
            "type":"watched", "replay":true, "id":"run"
        }));
        app.observe_pipeline_frame(&boundary("candidate", true));
        app.observe_live_pipeline_frame(&boundary("notified", true));
        let live_before = app.watch_ids();

        app.abandon_pipeline_replay();

        assert!(app.replay_executions.is_empty());
        assert_eq!(app.watch_ids(), live_before);
        assert_eq!(app.live_executions.len(), 2);
        assert!(app
            .live_executions
            .values()
            .all(|e| e.stage_run_id != "candidate"));
        app.observe_pipeline_frame(&serde_json::json!({
            "type":"replay_checkpoint", "complete":true
        }));
        assert_eq!(app.watch_ids(), live_before);

        app.observe_pipeline_frame(&boundary("future", true));
        assert!(app.replay_executions.is_empty());
        assert_eq!(app.live_executions.len(), 3);
        assert!(app
            .live_executions
            .values()
            .any(|e| e.stage_run_id == "future"));
        app.observe_pipeline_frame(&boundary("future", false));
        assert_eq!(app.watch_ids(), live_before);
    }

    #[test]
    fn live_notifications_survive_complete_and_incomplete_replay() {
        for complete in [false, true] {
            for gap in [false, true] {
                let mut app = super::App::new();
                app.observe_live_pipeline_frame(&boundary("ended", true));
                app.observe_pipeline_frame(&serde_json::json!({
                    "type":"watched", "replay":true, "id":"run"
                }));
                app.observe_pipeline_frame(&boundary("ended", true));
                app.observe_pipeline_frame(&boundary("started", false));
                app.observe_live_pipeline_frame(&boundary("started", true));
                app.observe_live_pipeline_frame(&boundary("ended", false));
                // Older replay records must not undo the live notifications.
                app.observe_pipeline_frame(&boundary("ended", true));
                app.observe_pipeline_frame(&boundary("started", false));
                assert_eq!(app.live_executions.len(), 1);
                assert!(app
                    .live_executions
                    .values()
                    .all(|e| e.stage_run_id == "started"));
                if gap {
                    app.observe_pipeline_frame(&serde_json::json!({"type":"history_gap"}));
                }
                app.observe_pipeline_frame(&serde_json::json!({
                    "type":"replay_checkpoint", "complete":complete
                }));
                assert_eq!(app.live_executions.len(), 1);
                assert!(app
                    .live_executions
                    .values()
                    .all(|e| e.stage_run_id == "started"));
            }
        }
    }

    #[test]
    fn repeated_agent_and_exec_starts_are_selectable_and_pins_survive_delayed_end() {
        for stage_type in ["agent", "exec"] {
            let mut app = super::App::new();
            app.active_runs
                .insert("run".into(), super::ActiveRun::new("running"));
            let mut start = boundary("old", true);
            start["stage_type"] = serde_json::json!(stage_type);
            app.observe_live_pipeline_frame(&start);
            let old = app.live_executions.values().next().unwrap().clone();
            app.watch_selection = Some(old.key());
            assert_eq!(app.watch_target(), Some(old.target()));
            app.select_composer(old.target());
            app.observe_live_pipeline_frame(&boundary("old", false));
            start["stage_run_id"] = serde_json::json!("replacement");
            app.observe_live_pipeline_frame(&start);
            let replacement = app.live_executions.values().next().unwrap().clone();
            app.watch_selection = Some(replacement.key());
            assert_eq!(app.watch_target(), Some(replacement.target()));
            assert_eq!(replacement.stage_type, stage_type);
            app.observe_live_pipeline_frame(&boundary("old", false));
            assert_eq!(app.live_executions.len(), 1);
            assert_eq!(app.watch_target(), Some(replacement.target()));
            assert_eq!(app.selected_target, old.target());
        }
    }

    #[test]
    fn concurrent_review_and_build_siblings_are_selectable_while_live() {
        for stage_type in ["agent", "exec"] {
            let mut app = super::App::new();
            app.active_runs
                .insert("run".into(), super::ActiveRun::new("running"));
            for (stage, id) in [("review", "review-1"), ("build", "build-1")] {
                let mut start = boundary(id, true);
                start["stage"] = serde_json::json!(stage);
                start["stage_type"] = serde_json::json!(stage_type);
                app.observe_live_pipeline_frame(&start);
            }
            assert_eq!(app.live_executions.len(), 2);
            let rows = app.watch_ids();
            assert_eq!(rows.len(), 3);
            let mut selections = Vec::new();
            for (stage, id) in [("review", "review-1"), ("build", "build-1")] {
                let execution = app
                    .live_executions
                    .values()
                    .find(|execution| execution.stage_run_id == id)
                    .unwrap();
                assert_eq!(execution.stage_type, stage_type);
                let key = execution.key();
                assert!(rows.contains(&key));
                assert_eq!(app.watch_run_id(&key), Some("run"));
                app.watch_selection = Some(key.clone());
                let target = super::Target::Invocation {
                    run_id: "run".into(),
                    stage: stage.into(),
                    stage_run_id: id.into(),
                };
                assert_eq!(app.watch_target(), Some(target.clone()));
                assert_eq!(app.live_executions.len(), 2);
                selections.push((key, target));
            }
            assert_ne!(selections[0].0, selections[1].0);
            assert_ne!(selections[0].1, selections[1].1);
        }
    }

    #[test]
    fn invocation_siblings_repeats_and_exact_removal() {
        let mut app = super::App::new();
        app.active_runs
            .insert("run".into(), super::ActiveRun::new("running"));
        for id in ["a", "b"] {
            app.observe_pipeline_frame(&boundary(id, true));
        }
        assert_eq!(app.live_executions.len(), 2);
        assert_eq!(app.watch_ids().len(), 3);
        let key = app
            .live_executions
            .iter()
            .find(|(_, e)| e.stage_run_id == "a")
            .unwrap()
            .0
            .clone();
        app.watch_selection = Some(key.clone());
        assert_eq!(app.watch_run_id(&key), Some("run"));
        assert!(
            matches!(app.watch_target(), Some(super::Target::Invocation { stage_run_id, .. }) if stage_run_id == "a")
        );
        app.observe_pipeline_frame(&boundary("b", false));
        assert_eq!(app.watch_selection.as_ref(), Some(&key));
        app.observe_pipeline_frame(&boundary("c", true));
        app.observe_pipeline_frame(&boundary("b", false));
        assert_eq!(app.live_executions.len(), 2);
        app.observe_pipeline_frame(&boundary("a", false));
        assert_eq!(app.live_executions.len(), 1);
        assert!(app.live_executions.values().all(|e| e.stage_run_id == "c"));
    }

    #[test]
    fn filtered_replay_replaces_only_its_selector() {
        for stage_run_id in [None, Some("a")] {
            let mut app = super::App::new();
            for id in ["a", "b"] {
                app.observe_pipeline_frame(&boundary(id, true));
            }
            let mut other_stage = boundary("a", true);
            other_stage["stage"] = serde_json::json!("other");
            app.observe_pipeline_frame(&other_stage);
            let mut other_run = boundary("a", true);
            other_run["run_id"] = serde_json::json!("other");
            app.observe_pipeline_frame(&other_run);
            app.observe_pipeline_frame(&serde_json::json!({
                "type":"watched", "replay":true, "id":"run",
                "stage":"work", "stage_run_id":stage_run_id
            }));
            app.observe_pipeline_frame(&boundary("a", true));
            app.observe_pipeline_frame(&boundary("a", false));
            // This sibling is outside an exact replay and must remain live.
            app.observe_pipeline_frame(&boundary("c", true));
            app.observe_pipeline_frame(&serde_json::json!({
                "type":"replay_checkpoint", "id":"run", "complete":true
            }));
            assert!(!app
                .live_executions
                .values()
                .any(|e| e.run_id == "run" && e.stage == "work" && e.stage_run_id == "a"));
            assert_eq!(
                app.live_executions.len(),
                if stage_run_id.is_some() { 4 } else { 3 }
            );
            assert!(app.live_executions.values().any(|e| e.stage_run_id == "c"));
        }
    }

    #[test]
    fn incomplete_scoped_replay_keeps_live_siblings_and_accepts_future_boundaries() {
        for gap in [false, true] {
            let mut app = super::App::new();
            app.observe_pipeline_frame(&boundary("known", true));
            app.observe_pipeline_frame(&serde_json::json!({
                "type":"watched", "replay":true, "id":"run",
                "stage":"work", "stage_run_id":"candidate"
            }));
            app.observe_pipeline_frame(&boundary("candidate", true));
            if gap {
                app.observe_pipeline_frame(&serde_json::json!({"type":"history_gap"}));
            }
            app.observe_pipeline_frame(&boundary("concurrent", true));
            app.observe_pipeline_frame(&boundary("known", false));
            assert_eq!(app.live_executions.len(), 1);
            app.observe_pipeline_frame(&serde_json::json!({
                "type":"replay_checkpoint", "id":"run", "complete":gap
            }));
            assert_eq!(app.live_executions.len(), 1);
            assert!(app
                .live_executions
                .values()
                .all(|e| e.stage_run_id == "concurrent"));
            app.observe_pipeline_frame(&boundary("candidate", true));
            assert_eq!(app.live_executions.len(), 2);
        }
    }

    fn sequenced_boundary(id: &str, active: bool, sequence: u64) -> serde_json::Value {
        let mut frame = boundary(id, active);
        frame["sequence"] = sequence.into();
        frame
    }

    #[test]
    fn pre_ack_gap_preserves_incompleteness_and_explicit_ends() {
        for source in [PipelineSource::Passive, PipelineSource::Inline] {
            let mut app = super::App::new();
            app.observe_live_pipeline_frame(&sequenced_boundary("ended", true, 1));
            app.observe_pipeline_frame_from(source, &serde_json::json!({"type":"history_gap"}));
            app.observe_pipeline_frame_from(
                source,
                &serde_json::json!({
                    "type":"watched", "replay":true, "id":"run"
                }),
            );
            app.observe_pipeline_frame_from(source, &sequenced_boundary("candidate", true, 2));
            app.observe_pipeline_frame_from(source, &sequenced_boundary("ended", false, 3));
            app.observe_pipeline_frame_from(source, &sequenced_boundary("newer", false, 4));
            app.observe_live_pipeline_frame(&sequenced_boundary("newer", true, 5));
            app.observe_pipeline_frame_from(
                source,
                &serde_json::json!({
                    "type":"replay_checkpoint", "complete":true, "through_sequence":4
                }),
            );
            assert_eq!(app.live_executions.len(), 1);
            assert_eq!(
                app.live_executions.values().next().unwrap().stage_run_id,
                "newer"
            );
        }
    }

    #[test]
    fn incomplete_checkpoint_applies_ends_but_not_starts() {
        let mut app = super::App::new();
        app.observe_live_pipeline_frame(&sequenced_boundary("ended", true, 1));
        app.observe_pipeline_frame(&serde_json::json!({
            "type":"watched", "replay":true, "id":"run"
        }));
        app.observe_pipeline_frame(&sequenced_boundary("candidate", true, 2));
        app.observe_pipeline_frame(&sequenced_boundary("ended", false, 3));
        app.observe_pipeline_frame(&sequenced_boundary("newer", false, 4));
        app.observe_live_pipeline_frame(&sequenced_boundary("newer", true, 5));
        app.observe_pipeline_frame(&serde_json::json!({
            "type":"replay_checkpoint", "complete":false, "through_sequence":6
        }));
        assert_eq!(app.live_executions.len(), 1);
        assert_eq!(
            app.live_executions.values().next().unwrap().stage_run_id,
            "newer"
        );
    }

    #[test]
    fn overlapping_cuts_cannot_erase_newer_starts_or_resurrect_ends() {
        for first in [PipelineSource::Passive, PipelineSource::Inline] {
            let second = if first == PipelineSource::Passive {
                PipelineSource::Inline
            } else {
                PipelineSource::Passive
            };
            for newer_first in [false, true] {
                let mut app = super::App::new();
                let ack = serde_json::json!({"type":"watched", "replay":true, "id":"run"});
                app.observe_pipeline_frame_from(first, &ack);
                app.observe_pipeline_frame_from(second, &ack);
                app.observe_pipeline_frame_from(first, &sequenced_boundary("ended", true, 1));
                app.observe_pipeline_frame_from(second, &sequenced_boundary("ended", false, 3));
                app.observe_pipeline_frame_from(second, &sequenced_boundary("started", true, 4));
                let old = serde_json::json!({"type":"replay_checkpoint", "complete":true, "through_sequence":2});
                let new = serde_json::json!({"type":"replay_checkpoint", "complete":true, "through_sequence":5});
                if newer_first {
                    app.observe_pipeline_frame_from(second, &new);
                    app.observe_pipeline_frame_from(first, &old);
                } else {
                    app.observe_pipeline_frame_from(first, &old);
                    app.observe_pipeline_frame_from(second, &new);
                }
                // Delayed duplicate live events are stale against the newer cut too.
                app.observe_live_pipeline_frame(&sequenced_boundary("ended", true, 1));
                assert_eq!(app.live_executions.len(), 1);
                assert_eq!(
                    app.live_executions.values().next().unwrap().stage_run_id,
                    "started"
                );
            }
        }
    }

    #[test]
    fn only_complete_lifecycle_replay_establishes_liveness() {
        let mut app = super::App::new();
        app.observe_pipeline_frame(&serde_json::json!({"type":"exec_output", "run_id":"run", "stage":"work", "stage_run_id":"output"}));
        let mut legacy = boundary("legacy", true);
        legacy.as_object_mut().unwrap().remove("stage_run_id");
        app.observe_pipeline_frame(&legacy);
        assert!(app.live_executions.is_empty());
        for complete in [false, true] {
            app.observe_pipeline_frame(&serde_json::json!({"type":"watched","replay":true}));
            app.observe_pipeline_frame(&boundary("old", true));
            app.observe_pipeline_frame(&boundary("old", false));
            app.observe_pipeline_frame(&boundary("live", true));
            assert!(app.live_executions.is_empty());
            app.observe_pipeline_frame(
                &serde_json::json!({"type":"replay_checkpoint","complete":complete}),
            );
            assert_eq!(app.live_executions.len(), usize::from(complete));
        }
        app.observe_pipeline_frame(&boundary("next", true));
        assert_eq!(app.live_executions.len(), 2);
    }
    #[test]
    fn conversation_status_persists_independently_per_target() {
        use super::{App, ConversationStatus as Status, Target};
        let mut app = App::new();
        let a = Target::StageExecution {
            run_id: "a".into(),
            stage: "agent".into(),
        };
        let b = Target::StageExecution {
            run_id: "b".into(),
            stage: "exec".into(),
        };
        assert_eq!(app.conversation_status(&a), Status::ReadOnly);
        app.set_conversation_status(a.clone(), Status::Paused);
        app.set_conversation_status(b.clone(), Status::Running);
        app.select_composer(a.clone());
        assert_eq!(app.conversation_status(&a), Status::Paused);
        app.select_composer(b.clone());
        app.set_conversation_status(a.clone(), Status::Ended);
        assert_eq!(app.conversation_status(&b), Status::Running);
        app.select_composer(a.clone());
        assert_eq!(app.conversation_status(&a), Status::Ended);
        assert_eq!(app.conversation_status(&Target::Home), Status::Idle);
    }

    #[test]
    fn home_status_defaults_to_request_state_and_accepts_explicit_updates() {
        use super::{App, ConversationStatus as Status, Target};
        let mut app = App::new();
        assert_eq!(app.conversation_status(&Target::Home), Status::Idle);
        app.active_request = true;
        assert_eq!(app.conversation_status(&Target::Home), Status::Running);
        app.active_request = false;
        assert_eq!(app.conversation_status(&Target::Home), Status::Idle);
        app.set_conversation_status(Target::Home, Status::Running);
        app.select_composer(Target::StageExecution {
            run_id: "a".into(),
            stage: "agent".into(),
        });
        assert_eq!(app.conversation_status(&Target::Home), Status::Running);
        app.set_conversation_status(Target::Home, Status::Idle);
        assert_eq!(app.conversation_status(&Target::Home), Status::Idle);
    }

    use super::*;

    #[test]
    fn newer_parked_edit_survives_a_successful_home_receipt() {
        let mut app = App::new();
        app.conversations.set_execution(
            Target::Home,
            Some(Execution {
                run_id: "run".into(),
                stage_run_id: "home".into(),
                nonce: 4,
            }),
        );
        app.insert_char('x');
        let Submission::Correction { id, .. } = app.conversations.submit(&Target::Home, &app.input)
        else {
            panic!("expected correction")
        };
        app.delete_before_cursor();
        app.insert_char('x');
        app.select_composer(Target::StageExecution {
            run_id: "other".into(),
            stage: "build".into(),
        });
        app.insert_char('y');
        app.finish_delivery(id, Ok(())).unwrap();
        assert_eq!(app.input.text(), "y");
        app.select_composer(Target::Home);
        assert_eq!(app.input.text(), "x");
        assert_eq!(app.input_cursor, 1);
    }

    #[test]
    fn cancel_search_submits_using_current_home_idle_state() {
        let mut app = App::new();
        app.conversations.set_execution(
            Target::Home,
            Some(Execution {
                run_id: "home".into(),
                stage_run_id: "original".into(),
                nonce: 5,
            }),
        );
        app.replace_input("original correction");
        app.enter_search();
        app.conversations.set_execution(Target::Home, None);
        app.search_push_char('x');
        app.cancel_search();
        assert_eq!(app.input.text(), "original correction");
        assert!(matches!(
            app.conversations.submit(&Target::Home, &app.input),
            Submission::HomePrompt
        ));
    }

    #[test]
    fn cursor_moves_left_right_on_char_boundaries() {
        let mut app = App::new();
        app.input.replace_all("aé😀b");
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
        app.input.replace_all("ab");
        app.input_cursor = 1;

        app.insert_char('😀');
        assert_eq!(app.input.text(), "a😀b");
        assert_eq!(app.input_cursor, "a😀".len());
        assert!(app.input.is_char_boundary(app.input_cursor));
    }

    #[test]
    fn backspace_deletes_char_before_cursor() {
        let mut app = App::new();
        app.input.replace_all("aé😀b");
        app.input_cursor = app.input.len();

        app.delete_before_cursor(); // delete 'b'
        assert_eq!(app.input.text(), "aé😀");
        assert_eq!(app.input_cursor, "aé😀".len());

        app.delete_before_cursor(); // delete 😀
        assert_eq!(app.input.text(), "aé");
        assert_eq!(app.input_cursor, "aé".len());

        app.delete_before_cursor(); // delete é
        assert_eq!(app.input.text(), "a");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_before_cursor(); // delete 'a'
        assert_eq!(app.input.text(), "");
        assert_eq!(app.input_cursor, 0);

        app.delete_before_cursor(); // no-op at start
        assert_eq!(app.input.text(), "");
        assert_eq!(app.input_cursor, 0);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut app = App::new();
        app.input.replace_all("aé😀b");
        app.input_cursor = "a".len();

        app.delete_at_cursor(); // delete é
        assert_eq!(app.input.text(), "a😀b");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_at_cursor(); // delete 😀
        assert_eq!(app.input.text(), "ab");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_at_cursor(); // delete 'b'
        assert_eq!(app.input.text(), "a");
        assert_eq!(app.input_cursor, "a".len());

        app.delete_at_cursor(); // no-op at end
        assert_eq!(app.input.text(), "a");
        assert_eq!(app.input_cursor, "a".len());
    }

    #[test]
    fn home_end_jump_to_ends() {
        let mut app = App::new();
        app.input.replace_all("hello");
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
    fn on_token_usage_preserves_last_turn_values_on_none() {
        let mut app = App::new();
        // Simulate first live turn event with real cache-hit data.
        app.on_token_usage(
            "g1".into(),
            Some(10_000), // prompt_tokens (cumulative)
            Some(5_000),  // completion_tokens
            Some(1_000),  // reasoning_tokens
            Some(2_000),  // cached_input_tokens (cumulative)
            Some(500),    // cache_creation_input_tokens
            2,            // turns
            Some(0.15),   // cost_usd
            Some(5_000),  // last_prompt_tokens (this turn)
            Some(2_500),  // last_cached_input_tokens (50% cache rate)
        );

        // Simulate terminal-finalize TokenUsage event — fields are None.
        app.on_token_usage(
            "g1".into(),
            Some(10_000),
            Some(5_000),
            Some(1_000),
            Some(2_000),
            Some(500),
            2,
            None, // cost_usd comes in as None
            None, // last_prompt_tokens is None from terminal
            None, // last_cached_input_tokens is None from terminal
        );

        let run = app.active_runs.get("g1").expect("run must exist");
        // Cumulative fields are overwritten (last write wins).
        assert_eq!(run.prompt_tokens, Some(10_000));
        assert_eq!(run.cached_input_tokens, Some(2_000));
        // cost_usd uses .or() — preserved from first call.
        assert_eq!(run.cost_usd, Some(0.15));
        // Last-turn fields must also preserve the earlier Some values.
        assert_eq!(
            run.last_prompt_tokens,
            Some(5_000),
            "last_prompt_tokens must not be wiped by a None event"
        );
        assert_eq!(
            run.last_cached_input_tokens,
            Some(2_500),
            "last_cached_input_tokens must not be wiped by a None event"
        );
    }

    /// After a turn with (5000 prompt, 2500 cached), a live turn reporting
    /// (1000 prompt, None cached) must show `-` (or 0%), not 250%.
    /// The two last-turn fields are replaced atomically — the stale cached
    /// value from the prior turn is overwritten with None.
    #[test]
    fn on_token_usage_live_partial_usage_replaces_both_fields() {
        let mut app = App::new();
        // Turn 1: 5000 prompt, 2500 cached → 50% cache rate.
        app.on_token_usage(
            "g1".into(),
            Some(10_000), // cumulative prompt
            Some(5_000),  // cumulative completion
            Some(1_000),  // cumulative reasoning
            Some(2_000),  // cumulative cached
            Some(500),    // cumulative cache_creation
            1,            // turns
            Some(0.10),
            Some(5_000), // last_prompt_tokens
            Some(2_500), // last_cached_input_tokens
        );

        // Turn 2: 1000 prompt, no cache hits. Both fields must be set
        // atomically — prompt→Some(1000), cached→None.
        app.on_token_usage(
            "g1".into(),
            Some(11_000), // cumulative prompt
            Some(6_000),  // cumulative completion
            Some(1_500),  // cumulative reasoning
            Some(2_000),  // cumulative cached (unchanged)
            Some(500),
            2,
            Some(0.15),
            Some(1_000), // last_prompt_tokens — this turn
            None,        // last_cached_input_tokens — no cache hits
        );

        let run = app.active_runs.get("g1").expect("run must exist");
        assert_eq!(run.last_prompt_tokens, Some(1_000));
        assert_eq!(
            run.last_cached_input_tokens, None,
            "cached must be None when the live turn reported no cache hits"
        );
    }

    /// When a live turn completes but the provider reported no token usage
    /// at all (turn_stats is None), both fields arrive as None.  This is
    /// indistinguishable from a terminal-finalize event — both paths fall
    /// through to the .or() branch and preserve existing values.
    #[test]
    fn on_token_usage_live_absent_usage_preserves_old_values() {
        let mut app = App::new();
        // Turn 1 with real data.
        app.on_token_usage(
            "g1".into(),
            Some(5_000),
            Some(2_000),
            Some(500),
            Some(1_000),
            Some(200),
            1,
            Some(0.05),
            Some(5_000),
            Some(2_500),
        );

        // Turn 2 with no provider-reported usage (both None).
        app.on_token_usage(
            "g1".into(),
            Some(5_000),
            Some(2_000),
            Some(500),
            Some(1_000),
            Some(200),
            2,
            Some(0.05),
            None, // last_prompt_tokens — absent
            None, // last_cached_input_tokens — absent
        );

        let run = app.active_runs.get("g1").expect("run must exist");
        assert_eq!(
            run.last_prompt_tokens,
            Some(5_000),
            "absent turn usage should preserve the prior turn's prompt tokens"
        );
        assert_eq!(
            run.last_cached_input_tokens,
            Some(2_500),
            "absent turn usage should preserve the prior turn's cached tokens"
        );
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
