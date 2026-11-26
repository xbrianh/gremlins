use serde::{Deserialize, Serialize};

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

/// Accounting for one model request, including requests that did not complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallUsage {
    pub request_id: String,
    /// Empty for conversational requests and legacy records.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_kind: String,
    pub provider: String,
    pub model: String,
    pub agent_id: String,
    pub reported: bool,
    pub completed: bool,
    /// Provider usage metadata only; never request or response content.
    pub raw: serde_json::Value,
    pub usage: UsageStats,
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
}
