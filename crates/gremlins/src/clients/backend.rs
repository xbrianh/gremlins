use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rig_core::driver::DynModel;
use rig_core::operation::Completion;

use super::agent_loop::CancelToken;
use super::interactive::{InteractiveEvent, InteractiveSession};
use super::protocol::CompletedRun;
use super::task::TaskModelSelector;

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
    /// Task-client selector, pre-built by the executor from
    /// `settings.yaml` `task-clients` entries. `None` when no
    /// entries are configured or when the backend cannot serve
    /// any of them (e.g. `cmd`).
    pub task_clients: Option<TaskModelSelector<DynModel<Completion>>>,
    /// Supervisor-owned cancel token. When set, the backend uses it instead of
    /// creating its own, so `gremlins stop` cancels in-flight agent loops.
    pub cancel_token: Option<Arc<CancelToken>>,
    /// Broadcast sender for stream events. When Some, the agent loop emits
    /// every stream event (reasoning, text, tool results, turn complete, done).
    pub stream_events: Option<tokio::sync::broadcast::Sender<InteractiveEvent>>,
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
            task_clients: self.task_clients.clone(),
            cancel_token: self.cancel_token.clone(),
            stream_events: self.stream_events.clone(),
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Timeout { message: String },
    ApiServerError { message: String },
    Runtime { message: String },
    Bail { reason: String },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Timeout { message } => write!(f, "{message}"),
            ClientError::ApiServerError { message } => write!(f, "{message}"),
            ClientError::Runtime { message } => write!(f, "{message}"),
            ClientError::Bail { reason } => write!(f, "{reason}"),
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

    /// Build a [`DynModel`] from a `provider:model` spec, or `None` when
    /// the spec names a different provider or the model cannot be built.
    ///
    /// Called by the executor to pre-build the `task-clients` selector
    /// before the agent loop starts.
    fn make_model(&self, spec: &str) -> Option<DynModel<Completion>>;

    async fn resume(&self) -> Result<CompletedRun, ClientError>;

    fn reap_all(&self, gremlin_id: &str);

    fn total_cost_usd(&self) -> Option<f64>;
}
