//! The single `EventKind` enum that subsumes `DaemonEvent`, `InteractiveEvent`,
//! and native attachment display records.
//!
//! The bus routes `DispatchEvent<EventKind>` values to subscribers based on
//! segment-aware `CorrId` prefix matching.

use serde::{Deserialize, Serialize};

use crate::stats::{ModelCallUsage, TurnStats, UsageStats};

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

    /// Per-request accounting, not a chat or display event.
    ModelCallUsage { usage: ModelCallUsage },

    // ── From InteractiveEvent ─────────────────────────────────────────
    #[serde(rename = "turn_started")]
    TurnStarted { stage_run_id: String },
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
        // Legacy boundaries predate stage-run identities; new producers always supply one.
        #[serde(default)]
        stage_run_id: String,
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
    /// Accept a boundary only when its payload agrees with the routed identity.
    pub fn invocation_metadata(event: &crate::DispatchEvent<Self>) -> Option<(String, bool)> {
        let Self::StageBoundary {
            id,
            stage,
            stage_run_id,
            stage_type,
            active,
        } = event.kind()
        else {
            return None;
        };
        let corr = event.corr_id();
        (corr.run() == Some(id.as_str())
            && corr.stage() == Some(stage.as_str())
            && corr.stage_run_id() == Some(stage_run_id.as_str()))
        .then(|| (stage_type.clone(), *active))
    }

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
    /// When adding a variant here, also update `run_display_kind` in
    /// `gremlins::executor::supervisor::attachment` and the supervisor watch framing tests.
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
    use crate::ModelCallUsage;

    #[test]
    fn active_producer_events_round_trip_with_stream_classification() {
        // Retries and execution endings still publish these events.
        let fixtures = [
            (serde_json::json!({"type": "stream_reset"}), false),
            (
                serde_json::json!({"type": "ended", "reason": "bailed"}),
                true,
            ),
        ];
        for (wire, chat) in fixtures {
            let event: EventKind = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&event).unwrap(), wire);
            assert!(event.is_stream_event());
            assert_eq!(event.is_chat_event(), chat);
        }
    }

    #[test]
    fn obsolete_ready_frame_is_rejected() {
        assert!(serde_json::from_value::<EventKind>(
            serde_json::json!({"type": "ready", "turn": 3})
        )
        .is_err());
    }

    #[test]
    fn model_call_usage_round_trips_nested_accounting_payload() {
        let payload = serde_json::json!({
            "request_id": "request-1",
            "provider": "provider",
            "model": "model",
            "agent_id": "agent-1",
            "reported": true,
            "completed": true,
            "raw": {"input_tokens": 12, "output_tokens": 0, "details": {"cached": 3}},
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 0,
                "cached_input_tokens": 3,
                "cache_creation_input_tokens": 0,
                "reasoning_tokens": 0,
                "turns": 1
            }
        });
        let usage: ModelCallUsage = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(serde_json::to_value(&usage).unwrap(), payload);
        let event = EventKind::ModelCallUsage { usage };
        let expected = serde_json::json!({"type": "model_call_usage", "usage": payload});
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        let decoded: EventKind = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        assert!(!event.is_chat_event());
        assert!(!event.is_stream_event());
    }

    #[test]
    fn model_call_usage_preserves_missing_usage_and_incomplete_request() {
        let json = serde_json::json!({
            "type": "model_call_usage",
            "usage": {
                "request_id": "request-2",
                "provider": "provider",
                "model": "model",
                "agent_id": "agent-2",
                "reported": false,
                "completed": false,
                "raw": null,
                "usage": {"turns": 0}
            }
        });
        let event: EventKind = serde_json::from_value(json.clone()).unwrap();
        let EventKind::ModelCallUsage { usage } = &event else {
            panic!("expected model call usage");
        };
        assert!(!usage.reported);
        assert!(!usage.completed);
        assert!(usage.raw.is_null());
        assert_eq!(usage.usage.prompt_tokens, None);
        assert_eq!(usage.usage.completion_tokens, None);
        assert_eq!(usage.usage.cached_input_tokens, None);
        assert_eq!(usage.usage.cache_creation_input_tokens, None);
        assert_eq!(usage.usage.reasoning_tokens, None);
        assert_eq!(serde_json::to_value(event).unwrap(), json);
    }

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
        assert!(
            matches!(event, EventKind::StageBoundary { stage_run_id, .. } if stage_run_id.is_empty())
        );

        let mut current = legacy;
        current["stage_run_id"] = "stage-run-id".into();
        let event: EventKind = serde_json::from_value(current.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), current);
    }
}
