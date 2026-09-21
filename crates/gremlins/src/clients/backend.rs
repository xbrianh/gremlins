use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::protocol::CompletedRun;

#[derive(Clone)]
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
    #[allow(clippy::type_complexity)]
    pub artifact_opaque_resolver: Option<Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>>,
}

impl std::fmt::Debug for RunParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunParams")
            .field("prompt", &self.prompt)
            .field("label", &self.label)
            .field("model", &self.model)
            .field("raw_path", &self.raw_path)
            .field("capture_events", &self.capture_events)
            .field("on_timeout_prompt", &self.on_timeout_prompt)
            .field("max_retries", &self.max_retries)
            .field("cwd", &self.cwd)
            .field("artifact_dir", &self.artifact_dir)
            .field("idle_timeout", &self.idle_timeout)
            .field("extra_env", &self.extra_env)
            .field("expected_artifact_paths", &self.expected_artifact_paths)
            .field("system_prompt", &self.system_prompt)
            .field("gremlin_id", &self.gremlin_id)
            .field(
                "artifact_opaque_resolver",
                &self
                    .artifact_opaque_resolver
                    .as_ref()
                    .map(|_| "<opaque_resolver>"),
            )
            .finish()
    }
}

#[derive(Debug)]
pub enum ClientError {
    Timeout { message: String },
    ApiServerError { message: String },
    Runtime { message: String },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Timeout { message } => write!(f, "{message}"),
            ClientError::ApiServerError { message } => write!(f, "{message}"),
            ClientError::Runtime { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ClientError {}

#[async_trait]
pub trait Backend: Send + Sync {
    async fn run(&self, params: RunParams) -> Result<CompletedRun, ClientError>;

    async fn resume(&self) -> Result<CompletedRun, ClientError>;

    fn reap_all(&self, gremlin_id: &str);

    fn total_cost_usd(&self) -> Option<f64>;
}
