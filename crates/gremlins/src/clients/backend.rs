use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use rig_core::completion::Message;

use super::agent_loop::CancelToken;
use super::interactive::{InteractiveSession, StreamEventSender};
use super::model::GremlinsModel;
use super::params::ResolvedParams;
use super::protocol::CompletedRun;
use super::task::TaskModelSelector;
use crate::executor::conversation::CompactionOpts;

#[derive(Debug)]
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
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Timeout { message: String },
    ApiServerError { message: String },
    RetryExhausted { message: String },
    Runtime { message: String },
    Bail { reason: String },
    PromptTooLarge { limit: u64, actual: u64 },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Timeout { message } => write!(f, "{message}"),
            ClientError::ApiServerError { message } => write!(f, "{message}"),
            ClientError::RetryExhausted { message } => write!(f, "{message}"),
            ClientError::Runtime { message } => write!(f, "{message}"),
            ClientError::Bail { reason } => write!(f, "{reason}"),
            ClientError::PromptTooLarge { limit, actual } => write!(
                f,
                "prompt size {actual} bytes exceeds configured limit of {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for ClientError {}

#[async_trait]
pub trait Backend: Send + Sync {
    async fn run(
        &self,
        params: RunParams,
        interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError>;

    /// Build a [`GremlinsModel`] from a `provider/model` spec, or `None` when
    /// the spec names a different provider or the model cannot be built.
    ///
    /// Called by the provider-agnostic selector factory when a `task-models`
    /// override matches a task kind that is actually running.
    fn make_model(&self, spec: &str) -> Option<GremlinsModel>;

    /// The request options this backend resolved for the spec it was built
    /// from (`temperature`, `max_tokens`, and the passthrough `extra` object).
    ///
    /// The provider-agnostic selector pairs this with [`Backend::make_model`]
    /// so a cross-provider `task-models` override carries the target backend's
    /// own defaults instead of the parent's. Backends with no request options
    /// (the command backend) return `None`.
    fn resolved_params(&self) -> Option<ResolvedParams> {
        None
    }

    async fn resume(&self) -> Result<CompletedRun, ClientError>;

    fn reap_all(&self, gremlin_id: &str);

    fn total_cost_usd(&self) -> Option<f64>;

    /// Resolve the raw `provider/model?k=v,...` param map into typed request
    /// fields plus the `additional_params` passthrough object.
    ///
    /// This is the single place extra-param meaning is decided: a key either
    /// sets a typed field (overriding this backend's default for that key) or
    /// passes through unchanged.  A malformed typed value is a build-time
    /// error, never a silent fallback.
    fn resolve_params(raw: &IndexMap<String, String>) -> Result<ResolvedParams, String>
    where
        Self: Sized;
}
