use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::agent_loop::CancelToken;
use super::interactive::InteractiveSession;
use super::protocol::CompletedRun;

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
    /// Task-client override maps from config (exact + prefix).
    /// When both are empty, task_model_selector returns None.
    pub task_clients_exact: HashMap<String, String>,
    pub task_clients_prefix: HashMap<String, String>,
    /// Supervisor-owned cancel token. When set, the backend uses it instead of
    /// creating its own, so `gremlins stop` cancels in-flight agent loops.
    pub cancel_token: Option<Arc<CancelToken>>,
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
            task_clients_exact: self.task_clients_exact.clone(),
            task_clients_prefix: self.task_clients_prefix.clone(),
            cancel_token: self.cancel_token.clone(),
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

    async fn resume(&self) -> Result<CompletedRun, ClientError>;

    fn reap_all(&self, gremlin_id: &str);

    fn total_cost_usd(&self) -> Option<f64>;
}
