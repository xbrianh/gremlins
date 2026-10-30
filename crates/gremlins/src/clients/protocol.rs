use serde::{Deserialize, Serialize};

use super::model_context::MutableModelContext;
use rig_core::completion::Message;

/// Aggregated token usage summary from an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    pub turns: usize,
}

/// One turn's token/cache delta.
///
/// Field names intentionally mirror [`UsageStats`] so chat consumers see the
/// same canonical wire keys for both per-turn and aggregate usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStats {
    /// The agent-loop turn index that produced this usage.
    pub turn: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompletedRun {
    pub exit_code: i32,
    pub text_result: Option<String>,
    /// The `Done` tool's `summary` argument, preserved distinctly from
    /// `text_result` so the chat TUI can append it at turn end.
    pub summary: Option<String>,
    pub events: Option<Vec<serde_json::Value>>,
    pub cost_usd: Option<f64>,
    pub token_usage: Option<UsageStats>,
    /// The agent loop's accumulated message history (all roles), when the
    /// backend captures it. Used by the chat gremlin to persist full model
    /// context between stages via `artifact://conversation.json`.
    pub messages: Option<Vec<Message>>,
    /// File observations (Read/Grep/Landscape + mutation paths) collected
    /// during the agent stage. Populated for all agent loops so that
    /// cross-stage landscape pruning can skip already-explored files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_observations: Option<FileObservations>,
    /// Mutable model contexts (ToolLedger, LandscapeContext, SignalMapContext)
    /// carried back from the child loop for fork + join propagation.
    /// Never serialized; consumed by the parent loop after `join`.
    #[serde(skip)]
    pub contexts: Vec<Box<dyn MutableModelContext>>,
}

// Manual Clone impl needed because `Box<dyn MutableModelContext>` doesn't
// auto-derive Clone — we use the trait's `clone_box()` method.
impl Clone for CompletedRun {
    fn clone(&self) -> Self {
        Self {
            exit_code: self.exit_code,
            text_result: self.text_result.clone(),
            summary: self.summary.clone(),
            events: self.events.clone(),
            cost_usd: self.cost_usd,
            token_usage: self.token_usage.clone(),
            messages: self.messages.clone(),
            file_observations: self.file_observations.clone(),
            contexts: self.contexts.iter().map(|c| c.clone_box()).collect(),
        }
    }
}

/// Sum two optional token counters.
///
/// When both sides are present their values are added together. If only one
/// side is present it is propagated unchanged, and when both are absent the
/// total stays absent. A reported zero (`Some(0)`) is preserved, so absence
/// is never turned into a measured zero and a measured zero is never treated
/// as missing.
pub fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    }
}

/// One file observation captured from a worker agent's Read, Grep, or Landscape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileObservation {
    /// Tool name: "Read", "Grep", "DirLs", or "Landscape".
    pub tool: String,
    /// File path for Read; search path for Grep; directory for Landscape.
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub limit: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Exact tool output captured by the worker.
    pub output: String,
}

/// Aggregated file observations from a nested agent loop plus mutation paths
/// for parent-ledger invalidation.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FileObservations {
    pub items: Vec<FileObservation>,
    /// True when observations were truncated to fit the configured output budget.
    #[serde(default)]
    pub truncated: bool,
    /// Files the worker modified (Write/Edit), used to invalidate stale parent-ledger reads.
    #[serde(default)]
    pub written_paths: Vec<String>,
}

impl FileObservations {
    /// Whether this snapshot carries nothing worth reporting: no reads, no
    /// mutation paths, and no truncation marker.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.written_paths.is_empty() && !self.truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_opt_sums_and_propagates_absence() {
        assert_eq!(add_opt(Some(2), Some(3)), Some(5));
        assert_eq!(add_opt(Some(2), None), Some(2));
        assert_eq!(add_opt(None, Some(3)), Some(3));
        assert_eq!(add_opt(None, None), None);
        assert_eq!(add_opt(Some(0), None), Some(0));
    }

    #[test]
    fn usage_stats_round_trips_omitted_counter_fields() {
        let stats = UsageStats {
            prompt_tokens: Some(10),
            completion_tokens: None,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: Some(2),
            turns: 4,
        };

        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["prompt_tokens"], 10);
        assert_eq!(json["reasoning_tokens"], 2);
        assert!(json.get("completion_tokens").is_none());
        assert!(json.get("cached_input_tokens").is_none());
        assert!(json.get("cache_creation_input_tokens").is_none());

        let decoded: UsageStats = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.prompt_tokens, Some(10));
        assert_eq!(decoded.completion_tokens, None);
        assert_eq!(decoded.cached_input_tokens, None);
        assert_eq!(decoded.cache_creation_input_tokens, None);
        assert_eq!(decoded.reasoning_tokens, Some(2));
        assert_eq!(decoded.turns, 4);
    }

    #[test]
    fn is_empty_no_content_no_truncation() {
        let obs = FileObservations {
            items: vec![],
            truncated: false,
            written_paths: vec![],
        };
        assert!(obs.is_empty());
    }

    #[test]
    fn is_empty_with_truncation() {
        let obs = FileObservations {
            items: vec![],
            truncated: true,
            written_paths: vec![],
        };
        assert!(!obs.is_empty());
    }

    #[test]
    fn is_empty_with_written_paths() {
        let obs = FileObservations {
            items: vec![],
            truncated: false,
            written_paths: vec!["src/lib.rs".into()],
        };
        assert!(!obs.is_empty());
    }
}
