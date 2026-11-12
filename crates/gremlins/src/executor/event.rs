//! The single `EventKind` enum that subsumes `DaemonEvent`, `InteractiveEvent`,
//! and the stage-stream JSON-string protocol.
//!
//! The bus routes `DispatchEvent<EventKind>` values to subscribers based on
//! segment-aware `CorrId` prefix matching.

use serde::{Deserialize, Serialize};

use crate::clients::protocol::{TurnStats, UsageStats};

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
    StageTransition { id: String, stage: String },
    #[serde(rename = "log_line")]
    LogLine { id: String, line: String },
    #[serde(rename = "bail")]
    Bail { id: String, reason: String },

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

    // ── Stage-stream protocol ─────────────────────────────────────────
    #[serde(rename = "stage_boundary")]
    StageBoundary {
        id: String,
        stage: String,
        stage_type: String,
        active: bool,
    },
}
