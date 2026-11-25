//! Leaf module shared by the chat request handler and the turn loop:
//! usage accounting and the chat JSON-line envelope.
//!
//! Keeping these in a leaf module lets [`super::turn`] import downward instead
//! of reaching back into its parent, so `chat → turn` stays a one-way edge.
//!
//! [`PromptMsg`] is re-exported from [`super::super::prompt_msg`] so the
//! chat module and [`super::super::chat_handle`] share a neutral prompt type
//! without creating a dependency cycle.

#[cfg(test)]
use serde_json::{Map, Value};

#[cfg(test)]
use crate::clients::protocol::{add_opt, UsageStats};

pub(crate) use super::super::prompt_msg::PromptMsg;

/// Read persisted `state.json.token_usage` as [`UsageStats`].
#[cfg(test)]
pub(crate) fn usage_stats_from_state(value: Option<&Value>) -> UsageStats {
    let obj = value
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    UsageStats {
        prompt_tokens: obj.get("prompt_tokens").and_then(|v| v.as_u64()),
        completion_tokens: obj.get("completion_tokens").and_then(|v| v.as_u64()),
        cached_input_tokens: obj.get("cached_input_tokens").and_then(|v| v.as_u64()),
        cache_creation_input_tokens: obj
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64()),
        reasoning_tokens: obj.get("reasoning_tokens").and_then(|v| v.as_u64()),
        turns: obj.get("turns").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
    }
}

/// Add a per-prompt usage delta to a conversation baseline.
#[cfg(test)]
pub(crate) fn add_usage_stats(baseline: &UsageStats, delta: &UsageStats) -> UsageStats {
    UsageStats {
        prompt_tokens: add_opt(baseline.prompt_tokens, delta.prompt_tokens),
        completion_tokens: add_opt(baseline.completion_tokens, delta.completion_tokens),
        cached_input_tokens: add_opt(baseline.cached_input_tokens, delta.cached_input_tokens),
        cache_creation_input_tokens: add_opt(
            baseline.cache_creation_input_tokens,
            delta.cache_creation_input_tokens,
        ),
        reasoning_tokens: add_opt(baseline.reasoning_tokens, delta.reasoning_tokens),
        turns: baseline.turns + delta.turns,
    }
}

/// Build a chat JSON line, tagged with the client's turn id for stale-event filtering.
#[cfg(test)]
pub(crate) fn chat_payload(turn: &Option<String>, fields: Value) -> Value {
    let mut obj = match fields {
        Value::Object(obj) => obj,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    if let Some(t) = turn {
        obj.insert("chat_turn".to_string(), Value::String(t.clone()));
    }
    Value::Object(obj)
}
