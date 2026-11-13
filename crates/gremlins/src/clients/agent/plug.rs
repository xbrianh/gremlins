use crate::clients::event::EventKind;
use crate::clients::protocol::UsageStats;
use crate::clients::LogLineFn;
use rig_core::completion::CompletionRequest;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Async stream-event emitter. Called from the agent loop to send
/// stream events (StreamChunk, ReasoningChunk, ToolOutputChunk,
/// ToolResult, TurnComplete, Done, StreamReset, Ready, Ended).
/// The executor wires this to `bus.emit()` or `bus.emit_async()`.
pub(crate) type StreamEventFn =
    Arc<dyn Fn(EventKind) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Inversion-of-control hooks that decouple the agent loop from
/// `crate::executor` and `crate::landscape`. Every cross-package
/// dependency is replaced by a typed handler slot that the caller
/// (in `executor/run.rs`) wires up with closures.
pub struct LoopHooks {
    /// Prompt debug dumper: called with every serialized
    /// [`CompletionRequest`]; the closure checks `log_level_threshold`
    /// and writes to the state store.
    pub(crate) dump_prompt_debug_fn: Arc<dyn Fn(&CompletionRequest) + Send + Sync>,
    /// Token-usage emitter: `(totals, last_prompt_tokens, last_cached_input_tokens)`.
    #[allow(clippy::type_complexity)]
    pub(crate) emit_token_usage_fn: Arc<dyn Fn(UsageStats, Option<u64>, Option<u64>) + Send + Sync>,
    /// Per-gremlin log-line emitter: called with every agent-loop log message.
    pub(crate) log_line_fn: LogLineFn,
    /// Async stream-event emitter. No-ops unless [`register_stream_event`]
    /// was called.
    ///
    /// [`register_stream_event`]: LoopHooks::register_stream_event
    pub(crate) emit_stream_event_fn: StreamEventFn,
    /// True when stream-event emission is wired (the hook is registered).
    pub(crate) wants_stream_events: bool,
}

impl LoopHooks {
    /// Create a no-op hook set. All slots default to silent discards.
    pub(crate) fn new() -> Self {
        Self {
            dump_prompt_debug_fn: Arc::new(|_| {}),
            emit_token_usage_fn: Arc::new(|_, _, _| {}),
            log_line_fn: Arc::new(|_| {}),
            emit_stream_event_fn: Arc::new(|_| Box::pin(std::future::ready(()))),
            wants_stream_events: false,
        }
    }

    /// Dump a serialized request via the registered debug dumper.
    pub(crate) fn dump_prompt_debug(&self, request: &CompletionRequest) {
        (self.dump_prompt_debug_fn)(request);
    }

    /// Emit cumulative token usage via the registered handler.
    pub(crate) fn emit_token_usage(
        &self,
        totals: UsageStats,
        last_prompt_tokens: Option<u64>,
        last_cached_input_tokens: Option<u64>,
    ) {
        (self.emit_token_usage_fn)(totals, last_prompt_tokens, last_cached_input_tokens);
    }

    /// Emit a log line through the registered log handler.
    pub(crate) fn log_line(&self, msg: &str) {
        (self.log_line_fn)(msg);
    }

    /// Emit a stream event through the registered hook. No-ops when not wired.
    pub(crate) async fn emit_stream_event(&self, event: EventKind) {
        (self.emit_stream_event_fn)(event).await;
    }

    /// Register a log-line emitter.
    pub(crate) fn register_log_line<F>(&mut self, f: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.log_line_fn = Arc::new(f);
    }

    /// Register a prompt debug dumper.
    pub(crate) fn register_prompt_debug<F>(&mut self, f: F)
    where
        F: Fn(&CompletionRequest) + Send + Sync + 'static,
    {
        self.dump_prompt_debug_fn = Arc::new(f);
    }

    /// Register a token-usage emitter.
    pub(crate) fn register_token_usage<F>(&mut self, f: F)
    where
        F: Fn(UsageStats, Option<u64>, Option<u64>) + Send + Sync + 'static,
    {
        self.emit_token_usage_fn = Arc::new(f);
    }

    /// Register a stream-event emitter.
    ///
    /// The executor wires this to `bus.emit()` in a follow-up phase; until
    /// then the hook is exercised only by the agent-loop tests, so it is
    /// allowed to be dead in non-test builds.
    #[allow(dead_code)]
    pub(crate) fn register_stream_event<F, Fut>(&mut self, f: F)
    where
        F: Fn(EventKind) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.emit_stream_event_fn = Arc::new(move |event| Box::pin(f(event)));
        self.wants_stream_events = true;
    }
}

impl Clone for LoopHooks {
    fn clone(&self) -> Self {
        Self {
            dump_prompt_debug_fn: self.dump_prompt_debug_fn.clone(),
            emit_token_usage_fn: self.emit_token_usage_fn.clone(),
            log_line_fn: self.log_line_fn.clone(),
            emit_stream_event_fn: self.emit_stream_event_fn.clone(),
            wants_stream_events: self.wants_stream_events,
        }
    }
}
