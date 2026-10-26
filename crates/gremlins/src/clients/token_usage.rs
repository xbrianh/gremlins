use super::protocol::UsageStats;

/// Payload emitted by the agent loop after each turn for live token/cost
/// tracking. The executor layer wires a sink that maps this into a
/// [`DaemonEvent::TokenUsage`](crate::executor::supervisor::DaemonEvent).
#[derive(Debug, Clone)]
pub struct TokenUsageEvent {
    #[allow(dead_code)]
    pub id: String,
    pub usage: UsageStats,
    pub cost_usd: Option<f64>,
    pub last_prompt_tokens: Option<u64>,
    pub last_cached_input_tokens: Option<u64>,
}

/// Sink for [`TokenUsageEvent`]s fired at the end of every agent-loop turn.
///
/// The executor layer wires this to the daemon event broadcast so the TUI
/// can update token/cost columns live without polling.
#[derive(Clone)]
pub struct TokenUsageSink(pub Option<std::sync::Arc<dyn Fn(TokenUsageEvent) + Send + Sync>>);

impl std::fmt::Debug for TokenUsageSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TokenUsageSink")
            .field(&self.0.is_some())
            .finish()
    }
}
