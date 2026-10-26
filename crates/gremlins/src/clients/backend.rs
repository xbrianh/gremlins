use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use rig_core::completion::Message;

use super::cancel::CancelToken;
pub(crate) use super::compaction::{CompactionOpts, Compactor};
use super::interactive::{InteractiveSession, StreamEventSender};
use super::model::GremlinsModel;
use super::params::ResolvedParams;
use super::protocol::CompletedRun;
use super::task_models::TaskModelSelector;

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
    /// Pre-populated message history. When set, the agent loop starts with
    /// these messages already in its internal history (carrying their original
    /// roles) instead of building from a bare system-prompt + user-prompt pair.
    /// The user prompt is still appended as the next user message.
    pub prepopulated_messages: Option<Vec<Message>>,
    /// Chat-only soft compaction settings. `Some` enables mid-loop compaction
    /// (and the first-request/read-time pass inside the agent loop); `None`
    /// keeps the loop's byte-accounting behavior unchanged.
    pub(crate) compaction_opts: Option<CompactionOpts>,
    /// Whether skills are available for this chat session.
    pub(crate) skills_available: bool,
    /// Discovered skills for this session. Session-bound so concurrent chat
    /// sessions resolve their own project's skills instead of a shared slot.
    pub(crate) discovered_skills: Option<Arc<Vec<crate::skills::LoadedSkill>>>,
    /// Shared mutable reference to active skill names.
    pub active_skills: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// Compaction algorithm (set alongside compaction_opts).
    pub(crate) compactor: Option<Arc<dyn Compactor>>,
    /// Sink for per-turn token-usage events emitted by the agent loop.
    /// The executor layer wires this to the daemon event broadcast.
    pub token_usage_sink: crate::clients::token_usage::TokenUsageSink,
    /// Path to the persistent stage scratchpad file (`_scratchpad.md`).
    /// When `Some`, the agent loop reads and re-injects scratchpad content
    /// on each turn. `None` disables the per-turn injection.
    pub scratchpad_path: Option<PathBuf>,
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
            prepopulated_messages: self.prepopulated_messages.clone(),
            compaction_opts: self.compaction_opts,
            skills_available: self.skills_available,
            discovered_skills: self.discovered_skills.clone(),
            active_skills: self.active_skills.clone(),
            compactor: self.compactor.clone(),
            token_usage_sink: self.token_usage_sink.clone(),
            scratchpad_path: self.scratchpad_path.clone(),
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

    /// Build a [`GremlinsModel`] from a `provider/model` spec, or `None` when
    /// the spec names a different provider or the model cannot be built.
    fn model_for(&self, _model: &str, _resolved: Option<&ResolvedParams>) -> Option<GremlinsModel> {
        None
    }

    fn name(&self) -> &'static str {
        "unknown"
    }

    /// List all models this backend can serve (unfiltered).
    fn models(&self) -> IndexMap<String, GremlinsModel> {
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
    fn make_model(&self, _spec: &str) -> Option<GremlinsModel> {
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
}

impl std::fmt::Debug for dyn Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("name", &self.name())
            .finish()
    }
}
