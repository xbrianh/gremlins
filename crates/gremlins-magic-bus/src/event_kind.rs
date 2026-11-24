//! The single `EventKind` enum that subsumes `DaemonEvent`, `InteractiveEvent`,
//! and native attachment display records.
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
    #[serde(rename = "turn_started")]
    TurnStarted { execution_id: String },
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
        // Legacy boundaries predate invocation identities; new producers always supply one.
        #[serde(default)]
        stage_id: String,
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

    // ── Operator messages ─────────────────────────────────────────────
    /// An operator message (correction) was delivered to the execution.
    #[serde(rename = "operator_message")]
    OperatorMessage { id: String, text: String },

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
    /// OperatorMessage is included so that a listener watching stream events
    /// also receives the delivered-message notification.
    pub fn is_chat_event(&self) -> bool {
        matches!(
            self,
            EventKind::TurnStarted { .. }
                | EventKind::StreamChunk { .. }
                | EventKind::ReasoningChunk { .. }
                | EventKind::ToolOutputChunk { .. }
                | EventKind::ToolResult { .. }
                | EventKind::TurnComplete { .. }
                | EventKind::TaskProgress { .. }
                | EventKind::Done { .. }
                | EventKind::Ended { .. }
                | EventKind::Error { .. }
                | EventKind::SystemWarning { .. }
                | EventKind::OperatorMessage { .. }
        )
    }

    /// Returns `true` for events that should NOT be forwarded on the control socket.
    /// Includes all chat stream events (prompt requests, skill loaded, operator messages).
    ///
    /// Exec output events are intentionally excluded so the TUI receives them
    /// via the control socket for stage-specific transcript routing.
    ///
    /// When adding a variant here, also update `conversation_display_event` in
    /// `crate::executor::supervisor::attachment` and the debug event match.
    pub fn is_stream_event(&self) -> bool {
        matches!(
            self,
            EventKind::StreamChunk { .. }
                | EventKind::ReasoningChunk { .. }
                | EventKind::ToolOutputChunk { .. }
                | EventKind::ToolResult { .. }
                | EventKind::TaskProgress { .. }
                | EventKind::TurnComplete { .. }
                | EventKind::Done { .. }
                | EventKind::StreamReset
                | EventKind::Ready { .. }
                | EventKind::Ended { .. }
                | EventKind::PromptRequest { .. }
                | EventKind::SkillLoaded { .. }
                | EventKind::Error { .. }
                | EventKind::SystemWarning { .. }
                | EventKind::OperatorMessage { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::EventKind;

    #[test]
    fn stage_boundary_accepts_legacy_payload_and_preserves_new_identity() {
        let legacy = serde_json::json!({
            "type": "stage_boundary",
            "id": "run",
            "stage": "agent",
            "stage_type": "agent",
            "active": true
        });
        let event: EventKind = serde_json::from_value(legacy.clone()).unwrap();
        assert!(matches!(event, EventKind::StageBoundary { stage_id, .. } if stage_id.is_empty()));

        let mut current = legacy;
        current["stage_id"] = "invocation-id".into();
        let event: EventKind = serde_json::from_value(current.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), current);
    }
}
