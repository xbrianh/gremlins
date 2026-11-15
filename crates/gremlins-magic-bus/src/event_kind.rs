//! The single `EventKind` enum that subsumes `DaemonEvent`, `InteractiveEvent`,
//! and the stage-stream JSON-string protocol.
//!
//! The bus routes `DispatchEvent<EventKind>` values to subscribers based on
//! segment-aware `CorrId` prefix matching.

use serde::{Deserialize, Serialize};

use crate::stats::{TurnStats, UsageStats};

/// Every event kind that flows through the bus.
///
/// Tagged with `"type"` so the wire envelope for the socket forwarder matches
/// the old `DaemonEvent` format exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    // ── From DaemonEvent ──────────────────────────────────────────────
    #[serde(rename = "run_started")]
    RunStarted {
        id: String,
        definition: String,
        stage: String,
    },
    #[serde(rename = "run_completed")]
    RunCompleted { id: String, exit_code: i32 },
    #[serde(rename = "run_failed")]
    RunFailed {
        id: String,
        exit_code: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "run_stopped")]
    RunStopped { id: String },
    #[serde(rename = "run_removed")]
    RunRemoved { id: String },
    #[serde(rename = "token_usage")]
    TokenUsage {
        id: String,
        #[serde(flatten)]
        usage: UsageStats,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_prompt_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_cached_input_tokens: Option<u64>,
    },
    #[serde(rename = "stage_transition")]
    StageTransition {
        id: String,
        stage: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_stage: Option<String>,
    },
    #[serde(rename = "log_line")]
    LogLine { id: String, line: String },
    #[serde(rename = "bail")]
    Bail { id: String, reason: String },

    /// Serialized CompletionRequest for prompt-debug dumps.
    #[serde(rename = "prompt_request")]
    PromptRequest { id: String, json: String },

    // ── From InteractiveEvent ─────────────────────────────────────────
    #[serde(rename = "ready")]
    Ready { turn: usize },
    #[serde(rename = "turn_complete")]
    TurnComplete {
        turn: usize,
        text: String,
        tool_calls: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<TurnStats>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        totals: Option<UsageStats>,
    },
    #[serde(rename = "done")]
    Done {
        text: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageStats>,
    },
    #[serde(rename = "stream_chunk")]
    StreamChunk { text: String },
    #[serde(rename = "reasoning_chunk")]
    ReasoningChunk { text: String },
    #[serde(rename = "tool_output_chunk")]
    ToolOutputChunk {
        id: String,
        name: String,
        call: String,
        text: String,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        name: String,
        call: String,
        output: String,
    },
    #[serde(rename = "ended")]
    Ended { reason: String },
    #[serde(rename = "stream_reset")]
    StreamReset,
    #[serde(rename = "task_progress")]
    TaskProgress {
        /// The parent tool-call id (prefixed if nested) identifying which TUI slot this updates.
        parent_id: String,
        /// The task chain (e.g. "a3f1" or "a3f1.b72e").
        chain: String,
        /// Task kind as string: "Scout", "Implement", or "Fork".
        kind: String,
        /// Formatted tool call (e.g. `Task(kind="Scout")`) for the title row.
        call: String,
        /// Streamed reasoning delta (empty when not a reasoning chunk).
        reasoning: String,
        /// Streamed text delta (empty when not a text chunk).
        text: String,
        /// Per-turn token stats (present in turn-summary events).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<TurnStats>,
        /// Conversation-wide cumulative totals (present in turn-summary events).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        totals: Option<UsageStats>,
    },

    // ── Stage-stream protocol ─────────────────────────────────────────
    #[serde(rename = "stage_boundary")]
    StageBoundary {
        id: String,
        stage: String,
        stage_type: String,
        active: bool,
    },

    // ── Exec output (stdout/stderr from exec stages) ───────────────────
    #[serde(rename = "exec_output")]
    ExecOutput {
        id: String,
        is_stderr: bool,
        text: String,
        ts: String,
    },

    // ── Skill loading ─────────────────────────────────────────────────
    /// Emitted by the LoadSkill tool handler when a skill is successfully
    /// activated. The name is the canonical skill identifier.
    #[serde(rename = "skill_loaded")]
    SkillLoaded { id: String, name: String },

    // ── Chat error / warning ──────────────────────────────────────────
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<String>,
    },
    #[serde(rename = "system_warning")]
    SystemWarning {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<String>,
    },
}

impl EventKind {
    /// Returns `true` for events that are part of the chat streaming protocol.
    pub fn is_chat_event(&self) -> bool {
        matches!(
            self,
            EventKind::StreamChunk { .. }
                | EventKind::ReasoningChunk { .. }
                | EventKind::ToolOutputChunk { .. }
                | EventKind::ToolResult { .. }
                | EventKind::TurnComplete { .. }
                | EventKind::TaskProgress { .. }
                | EventKind::Done { .. }
                | EventKind::Ended { .. }
                | EventKind::Error { .. }
                | EventKind::SystemWarning { .. }
        )
    }

    /// Returns `true` for events that should NOT be forwarded on the control socket.
    /// Includes all stream events (chat + exec output + prompt requests + skill loaded).
    pub fn is_stream_event(&self) -> bool {
        matches!(
            self,
            EventKind::StreamChunk { .. }
                | EventKind::ReasoningChunk { .. }
                | EventKind::ToolOutputChunk { .. }
                | EventKind::ToolResult { .. }
                | EventKind::TurnComplete { .. }
                | EventKind::Done { .. }
                | EventKind::StreamReset
                | EventKind::Ready { .. }
                | EventKind::Ended { .. }
                | EventKind::ExecOutput { .. }
                | EventKind::PromptRequest { .. }
                | EventKind::SkillLoaded { .. }
                | EventKind::Error { .. }
                | EventKind::SystemWarning { .. }
        )
    }
}
