use crate::clients::protocol::UsageStats;
use rig_core::completion::Usage;

/// Rig already folds disjoint provider counters into inclusive input/output
/// totals. Cache and reasoning counters remain subsets, not additional tokens.
/// Missing counters stay missing, even for completed requests.
pub(super) fn normalize(usage: &Usage, completed: bool) -> UsageStats {
    UsageStats {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        turns: usize::from(completed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn normalized(usage: Usage, completed: bool) -> Value {
        serde_json::to_value(normalize(&usage, completed)).unwrap()
    }

    #[test]
    fn inclusive_fifty_thousand_prompt_does_not_add_cache_again() {
        // rig-core 0.46.0: providers/openai/embedding.rs:169-206,
        // Usage::input_tokens / to_normalized preserve inclusive prompt counts.
        let wire: rig_core::providers::openai::Usage = serde_json::from_value(json!({
            "prompt_tokens": 50_000,
            "completion_tokens": 1_000,
            "total_tokens": 51_000,
            "prompt_tokens_details": {"cached_tokens": 40_000}
        }))
        .unwrap();
        let usage: Usage = wire.into();
        assert_eq!(usage.total_tokens, Some(51_000));
        assert_eq!(
            normalized(usage, true),
            json!({
                "prompt_tokens": 50_000,
                "completion_tokens": 1_000,
                "cached_input_tokens": 40_000,
                "turns": 1
            })
        );
    }

    #[test]
    fn disjoint_provider_sixty_thousand_prompt_is_already_folded_by_rig() {
        use rig_core::completion::CompletionRequest;
        use rig_core::providers::anthropic::AnthropicConfig;
        use rig_core::wire::{Mode, WireFrame};

        // rig-core 0.46.0: providers/anthropic/streaming.rs:98-126,
        // Counts::of / usage add disjoint reads and writes, not thinking.
        let frames = [
            json!({"type": "message_start", "message": {
                "id": "msg_usage", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-6", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 10_000, "output_tokens": 0,
                    "cache_read_input_tokens": 40_000,
                    "cache_creation_input_tokens": 10_000}
            }}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2_000,
                    "output_tokens_details": {"thinking_tokens": 500}}}),
            json!({"type": "message_stop"}),
        ];
        let response = rig_core::wire::decode_reply(
            &AnthropicConfig::new("unused-test-key").completion("claude-sonnet-4-6"),
            &CompletionRequest::new("hello"),
            Mode::Streaming,
            frames
                .iter()
                .map(|frame| WireFrame::Text(frame.to_string())),
        )
        .unwrap();
        let usage = response.usage;
        assert_eq!(usage.total_tokens, Some(62_000));
        assert_eq!(
            normalized(usage, true),
            json!({
                "prompt_tokens": 60_000,
                "completion_tokens": 2_000,
                "cached_input_tokens": 40_000,
                "cache_creation_input_tokens": 10_000,
                "reasoning_tokens": 500,
                "turns": 1
            })
        );
    }

    #[test]
    fn absent_counters_are_not_measured_zeros() {
        assert_eq!(normalized(Usage::new(), false), json!({"turns": 0}));
        assert_eq!(normalized(Usage::new(), true), json!({"turns": 1}));
    }

    #[test]
    fn reported_zeros_survive() {
        let usage = Usage::new()
            .input_tokens(0)
            .output_tokens(0)
            .cached_input_tokens(0)
            .cache_creation_input_tokens(0)
            .reasoning_tokens(0);
        assert_eq!(
            normalized(usage, true),
            json!({
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "cached_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "reasoning_tokens": 0,
                "turns": 1
            })
        );
    }

    #[test]
    fn partial_request_retains_usage_without_counting_a_completed_turn() {
        let usage = Usage::new()
            .input_tokens(50_000)
            .cached_input_tokens(40_000);
        assert_eq!(
            normalized(usage, false),
            json!({
                "prompt_tokens": 50_000,
                "cached_input_tokens": 40_000,
                "turns": 0
            })
        );
    }

    #[test]
    fn subset_and_total_counters_do_not_invent_missing_totals() {
        let usage = Usage::new()
            .total_tokens(60_000)
            .cached_input_tokens(40_000)
            .cache_creation_input_tokens(10_000)
            .reasoning_tokens(500);
        assert_eq!(
            normalized(usage, false),
            json!({
                "cached_input_tokens": 40_000,
                "cache_creation_input_tokens": 10_000,
                "reasoning_tokens": 500,
                "turns": 0
            })
        );
    }

    #[test]
    fn tool_prompt_and_reasoning_are_not_added_twice() {
        // Gemini's adapter includes tool prompt in input and thoughts in output.
        let usage = Usage::new()
            .input_tokens(600)
            .tool_use_prompt_tokens(100)
            .output_tokens(300)
            .reasoning_tokens(200);
        let stats = normalize(&usage, true);
        assert_eq!(stats.prompt_tokens, Some(600));
        assert_eq!(stats.completion_tokens, Some(300));
        assert_eq!(stats.reasoning_tokens, Some(200));
    }

    #[test]
    fn normalization_is_lossless_even_for_inconsistent_subset_metadata() {
        let usage = Usage::new()
            .input_tokens(1)
            .output_tokens(2)
            .cached_input_tokens(u64::MAX)
            .cache_creation_input_tokens(u64::MAX)
            .reasoning_tokens(u64::MAX);
        let stats = normalize(&usage, false);
        assert_eq!(stats.prompt_tokens, Some(1));
        assert_eq!(stats.completion_tokens, Some(2));
        assert_eq!(stats.cached_input_tokens, Some(u64::MAX));
        assert_eq!(stats.cache_creation_input_tokens, Some(u64::MAX));
        assert_eq!(stats.reasoning_tokens, Some(u64::MAX));
    }
}
