use crate::clients::protocol::UsageStats;
use rig_core::completion::CompletionRequest;
use std::sync::Arc;

/// Inversion-of-control hooks that decouple the agent loop from
/// `crate::executor` and `crate::landscape`. Every cross-package
/// dependency is replaced by a typed handler slot that the caller
/// (in `executor/run.rs`) wires up with closures.
pub struct LoopHooks {
    /// Log sink: `(message)`. Prefix is already baked into the
    /// message by the agent loop call site. The default no-op handler
    /// discards; the executor wires this to the per-gremlin log
    /// channel.
    #[allow(clippy::type_complexity)]
    pub(crate) log_fn: Arc<dyn Fn(&str) + Send + Sync>,
    /// Prompt debug dumper: called with every serialized
    /// [`CompletionRequest`]; the closure checks `log_level_threshold`
    /// and writes to the state store.
    pub(crate) dump_prompt_debug_fn: Arc<dyn Fn(&CompletionRequest) + Send + Sync>,
    /// Token-usage emitter: `(totals, last_prompt_tokens, last_cached_input_tokens)`.
    #[allow(clippy::type_complexity)]
    pub(crate) emit_token_usage_fn: Arc<dyn Fn(UsageStats, Option<u64>, Option<u64>) + Send + Sync>,
}

impl LoopHooks {
    /// Create a no-op hook set. All slots default to silent discards.
    pub(crate) fn new() -> Self {
        Self {
            log_fn: Arc::new(|_| {}),
            dump_prompt_debug_fn: Arc::new(|_| {}),
            emit_token_usage_fn: Arc::new(|_, _, _| {}),
        }
    }

    /// Emit a log line through the registered handler. The message
    /// already carries the stage/model/task prefix.
    pub(crate) fn log(&self, msg: &str) {
        (self.log_fn)(msg);
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

    /// Register a log handler.
    pub(crate) fn register_logger<F>(&mut self, f: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.log_fn = Arc::new(f);
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
}

impl Clone for LoopHooks {
    fn clone(&self) -> Self {
        Self {
            log_fn: self.log_fn.clone(),
            dump_prompt_debug_fn: self.dump_prompt_debug_fn.clone(),
            emit_token_usage_fn: self.emit_token_usage_fn.clone(),
        }
    }
}
