use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::cancel::CancelToken;
use super::conversation::Conversation;
use super::interactive::{InteractiveSession, StreamEventSender};
use super::params::ResolvedParams;
pub(crate) use super::plug::LoopHooks;
use super::protocol::{CompactionResult, CompletedRun};
use super::task_models::TaskModelSelector;
use async_trait::async_trait;
use indexmap::IndexMap;
use rig_core::driver::DynModel;
use rig_core::operation::Completion;

pub struct RunParams {
    pub prompt: String,
    pub label: String,
    pub model: Option<String>,
    pub raw_path: Option<PathBuf>,
    pub capture_events: bool,
    pub on_timeout_prompt: Option<String>,
    pub max_retries: usize,
    pub cwd: Option<PathBuf>,
    pub artifact_dir: Option<PathBuf>,
    pub idle_timeout: Option<f64>,
    pub extra_env: Option<HashMap<String, String>>,
    pub expected_artifact_paths: Vec<PathBuf>,
    pub system_prompt: Option<String>,
    pub gremlin_id: Option<String>,
    /// Per-gremlin log channel for streaming log output.
    pub log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Base process environment for tool sandboxing.
    /// When set, replaces `std::env::vars()` as the base for bash tool env.
    pub base_env: Option<HashMap<String, String>>,
    /// Whether this run's agent loop opts out of tool-path sandboxing.
    /// `false` (the default) enforces `allowed_roots` containment; the chat
    /// gremlin sets `true` to disable it. The agent loop translates this into
    /// a [`super::tools::SandboxMode`] when assembling the `AgentLoopRuntime`.
    pub(crate) allow_all: bool,
    /// Task-client selector, built by the executor from
    /// `settings.yaml` `task-models` entries. `None` when no
    /// entries are configured. Resolution is lazy: the selector's
    /// provider-agnostic factory builds a backend only when a
    /// matching task kind actually runs. A matched entry that fails
    /// to build surfaces as a `Task error: …` string; a task kind
    /// with no configured entry keeps the parent model.
    pub task_models: Option<TaskModelSelector>,
    /// Supervisor-owned cancel token. When set, the backend uses it instead of
    /// creating its own, so `gremlins stop` cancels in-flight agent loops.
    pub cancel_token: Option<Arc<CancelToken>>,
    /// Sender for stream events. When Some, the agent loop emits
    /// every stream event (reasoning, text, tool results, turn complete, done).
    pub stream_events: Option<StreamEventSender>,
    /// The conversation this run starts from. Its history (carrying original
    /// roles) is replayed verbatim; the run's `prompt` is applied on top as
    /// the next user message.
    pub(crate) prepopulated_conversation: Box<dyn Conversation>,
    /// Whether skills are available for this chat session.
    pub(crate) skills_available: bool,
    /// Discovered skills for this session. Session-bound so concurrent chat
    /// sessions resolve their own project's skills instead of a shared slot.
    pub(crate) discovered_skills: Option<Arc<Vec<crate::skills::LoadedSkill>>>,
    /// Shared mutable reference to active skill names.
    pub active_skills: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// Sink for per-turn token-usage events emitted by the agent loop.
    /// The executor layer wires this to the daemon event broadcast.
    pub token_usage_sink: crate::clients::token_usage::TokenUsageSink,
    /// Additional filesystem roots to add to the tool sandbox beyond the
    /// worktree and artifact directory. Threaded from [`GremlinConfig`].
    pub extra_sandbox_roots: Vec<PathBuf>,
    /// Inversion-of-control hooks that carry cross-package dependencies
    /// (prompt-debug, token-usage) into the agent loop. `None` defaults
    /// to no-op hooks.
    pub hooks: Option<LoopHooks>,
}

impl Clone for RunParams {
    fn clone(&self) -> Self {
        Self {
            prompt: self.prompt.clone(),
            label: self.label.clone(),
            model: self.model.clone(),
            raw_path: self.raw_path.clone(),
            capture_events: self.capture_events,
            on_timeout_prompt: self.on_timeout_prompt.clone(),
            max_retries: self.max_retries,
            cwd: self.cwd.clone(),
            artifact_dir: self.artifact_dir.clone(),
            idle_timeout: self.idle_timeout,
            extra_env: self.extra_env.clone(),
            expected_artifact_paths: self.expected_artifact_paths.clone(),
            system_prompt: self.system_prompt.clone(),
            gremlin_id: self.gremlin_id.clone(),
            log_tx: self.log_tx.clone(),
            base_env: self.base_env.clone(),
            allow_all: self.allow_all,
            task_models: self.task_models.clone(),
            cancel_token: self.cancel_token.clone(),
            stream_events: self.stream_events.clone(),
            prepopulated_conversation: self.prepopulated_conversation.clone(),
            skills_available: self.skills_available,
            discovered_skills: self.discovered_skills.clone(),
            active_skills: self.active_skills.clone(),
            token_usage_sink: self.token_usage_sink.clone(),
            extra_sandbox_roots: self.extra_sandbox_roots.clone(),
            hooks: self.hooks.clone(),
        }
    }
}

impl std::fmt::Debug for RunParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunParams")
            .field("label", &self.label)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

pub use crate::clients::error::ClientError;

#[async_trait]
pub trait Backend: Send + Sync {
    async fn run(
        &self,
        params: RunParams,
        interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError>;

    /// The model this backend instance was built for.
    ///
    /// The [`CmdBackend`](super::backends::cmd::CmdBackend) returns the
    /// empty string — it has no single model; the command *is* the model
    /// and varies per run.
    fn model(&self) -> &str;

    fn name(&self) -> &'static str {
        "unknown"
    }

    /// List all models this backend can serve (unfiltered).
    fn models(&self) -> IndexMap<String, DynModel<Completion>> {
        IndexMap::new()
    }

    /// Return the last `RunParams` that were used, for resume support.
    fn last_run_params(&self) -> Result<RunParams, ClientError> {
        Err(ClientError::Runtime {
            message: "resume not supported".into(),
        })
    }

    /// Resume a paused or timed-out run.
    async fn resume(&self) -> Result<CompletedRun, ClientError> {
        self.run(self.last_run_params()?, None).await
    }

    /// Reap all child processes for a given gremlin ID.
    fn reap_all(&self, _gremlin_id: &str) {}

    /// Build a model from a provider-raw spec string.
    fn make_model(&self, _spec: &str) -> Option<DynModel<Completion>> {
        None
    }

    /// Total cost in USD for this session, if tracked.
    fn total_cost_usd(&self) -> Option<f64> {
        None
    }

    fn resolve_params(_raw: &IndexMap<String, String>) -> Result<ResolvedParams, String>
    where
        Self: Sized,
    {
        Ok(ResolvedParams::default())
    }

    fn resolved_params(&self) -> Option<ResolvedParams> {
        None
    }

    /// Whether this backend natively supports server-side compaction.
    /// The default is `false`; only [`AnthropicBackend`] returns `true`.
    /// Callers use this to branch: server compaction vs. local compaction
    /// is a construction-time decision, not a runtime fallback.
    fn supports_compaction(&self) -> bool {
        false
    }

    /// Request server-side compaction.
    /// Returns `Ok(None)` when the backend doesn't support compaction (the
    /// caller skips compaction for this turn).
    async fn compact(
        &self,
        _messages: &[rig_core::completion::Message],
        _system_prompt: Option<&str>,
        _instructions: Option<&str>,
    ) -> Result<Option<CompactionResult>, ClientError> {
        Ok(None)
    }
}

impl std::fmt::Debug for dyn Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("name", &self.name())
            .finish()
    }
}
