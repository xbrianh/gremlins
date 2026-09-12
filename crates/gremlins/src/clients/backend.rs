use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;

use super::protocol::CompletedRun;

#[derive(Debug, Clone)]
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
    pub artifact_reminder_count: usize,
    pub system_prompt: Option<String>,
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

    fn reap_all(&self);

    fn total_cost_usd(&self) -> Option<f64>;
}
