use super::model_context::MutableModelContext;
use super::protocol::UsageStats;
use rig_core::completion::CompletionRequest;
use std::sync::Arc;

/// Inversion-of-control plug that decouples the agent loop from
/// `crate::executor` and `crate::landscape`. Every cross-package
/// dependency is replaced by a typed handler slot that the caller
/// (in `executor/run.rs`) wires up with closures.
pub struct LoopPlug {
    /// Log sink: `(level, message)`. Prefix is already baked into the
    /// message by the agent loop call site.
    #[allow(clippy::type_complexity)]
    pub(crate) log_fn: Arc<dyn Fn(log::Level, &str) + Send + Sync>,
    /// Scratchpad reader: returns `Some(content)` when a scratchpad file
    /// exists and is readable, `None` otherwise.
    pub(crate) read_scratchpad_fn: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// Prompt debug dumper: called with every serialized
    /// [`CompletionRequest`]; the closure checks `log_level_threshold`
    /// and writes to the state store.
    pub(crate) dump_prompt_debug_fn: Arc<dyn Fn(&CompletionRequest) + Send + Sync>,
    /// Token-usage emitter: `(totals, last_prompt_tokens, last_cached_input_tokens)`.
    #[allow(clippy::type_complexity)]
    pub(crate) emit_token_usage_fn: Arc<dyn Fn(UsageStats, Option<u64>, Option<u64>) + Send + Sync>,
    /// Mutable model contexts (ToolLedger, SignalMapContext, LandscapeContext).
    /// Cloned (via `clone_box`) into each `LoopState`.
    pub(crate) contexts: Vec<Box<dyn MutableModelContext>>,
}

impl LoopPlug {
    /// Create a no-op plug. All slots default to silent discards;
    /// `contexts` is empty.
    pub(crate) fn new() -> Self {
        Self {
            log_fn: Arc::new(|_, _| {}),
            read_scratchpad_fn: Arc::new(|| None),
            dump_prompt_debug_fn: Arc::new(|_| {}),
            emit_token_usage_fn: Arc::new(|_, _, _| {}),
            contexts: Vec::new(),
        }
    }

    /// Emit a log line through the registered handler. The message already
    /// carries the stage/model/task prefix.
    pub(crate) fn log(&self, level: log::Level, msg: &str) {
        (self.log_fn)(level, msg);
    }

    /// Read the persistent scratchpad, returning `Some(content)` when one is
    /// configured and readable, `None` otherwise.
    pub(crate) fn read_scratchpad(&self) -> Option<String> {
        (self.read_scratchpad_fn)()
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
        F: Fn(log::Level, &str) + Send + Sync + 'static,
    {
        self.log_fn = Arc::new(f);
    }

    /// Register a scratchpad reader.
    pub(crate) fn register_scratchpad_reader<F>(&mut self, f: F)
    where
        F: Fn() -> Option<String> + Send + Sync + 'static,
    {
        self.read_scratchpad_fn = Arc::new(f);
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

    /// Replace the context list with a new one.
    pub(crate) fn register_contexts(&mut self, ctxs: Vec<Box<dyn MutableModelContext>>) {
        self.contexts = ctxs;
    }
}

// Manual Clone impl needed because `Box<dyn MutableModelContext>` doesn't
// auto-derive Clone — we use the trait's `clone_box()` method.
impl Clone for LoopPlug {
    fn clone(&self) -> Self {
        Self {
            log_fn: self.log_fn.clone(),
            read_scratchpad_fn: self.read_scratchpad_fn.clone(),
            dump_prompt_debug_fn: self.dump_prompt_debug_fn.clone(),
            emit_token_usage_fn: self.emit_token_usage_fn.clone(),
            contexts: self.contexts.iter().map(|c| c.clone_box()).collect(),
        }
    }
}
