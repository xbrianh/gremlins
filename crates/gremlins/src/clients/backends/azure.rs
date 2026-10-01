use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::providers::azure::{self, AzureOpenAIAuth};

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::openai_protocol;
use crate::clients::protocol::CompletedRun;
use crate::clients::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};

const PROVIDER_NAME: &str = "azure";

// ── AzureRunState ────────────────────────────────────────────────────────

struct AzureRunState {
    client: azure::Client,
    model: String,
    tool_filter: Option<Vec<String>>,
    client_params: HashMap<String, String>,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>,
    next_id: AtomicU64,
    log_label: String,
}

impl AzureRunState {
    fn extra_params(&self) -> Option<serde_json::Value> {
        openai_protocol::build_extra_params(&self.client_params)
    }

    fn effective_model(&self, override_model: Option<&str>) -> String {
        match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        }
    }
}

// ── AzureBackend ─────────────────────────────────────────────────────────

pub struct AzureBackend {
    state: AzureRunState,
}

impl AzureBackend {
    /// Build an Azure backend.
    ///
    /// Auth precedence:
    /// 1. `AZURE_OPENAI_TOKEN` env var → `AzureOpenAIAuth::Token`
    /// 2. `AZURE_OPENAI_API_KEY` env var → `AzureOpenAIAuth::ApiKey`
    /// 3. `providers.json` `"azure"` entry → `AzureOpenAIAuth::ApiKey`
    ///
    /// `AZURE_OPENAI_ENDPOINT` is required.
    /// `AZURE_OPENAI_API_VERSION` defaults to `"2024-10-21"`.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let endpoint = crate::config::azure_endpoint().ok_or_else(|| {
            "AZURE_OPENAI_ENDPOINT is required for the Azure backend".to_string()
        })?;

        let api_version = crate::config::azure_api_version();

        let auth = resolve_auth()?;

        let client = azure::Client::builder()
            .api_key(auth)
            .api_version(&api_version)
            .azure_endpoint(endpoint)
            .build()
            .map_err(|e| format!("failed to build Azure client: {e}"))?;

        let model = if model.is_empty() {
            return Err("azure backend requires a deployment name (e.g. azure:gpt-4o)".into());
        } else {
            model.to_string()
        };

        let tool_filter = openai_protocol::tool_filter(native_block);
        let client_params = openai_protocol::string_map(extra_params);

        Ok(Arc::new(Self {
            state: AzureRunState {
                client,
                model,
                tool_filter,
                client_params,
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "AzureBackend".to_string(),
            },
        }))
    }
}

/// Resolve Azure auth credentials.
fn resolve_auth() -> Result<AzureOpenAIAuth, String> {
    // 1. AZURE_OPENAI_TOKEN → bearer token
    if let Some(token) = crate::config::azure_auth_token() {
        return Ok(AzureOpenAIAuth::Token(token));
    }

    // 2. AZURE_OPENAI_API_KEY → api-key header
    if let Ok(key) = std::env::var("AZURE_OPENAI_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(AzureOpenAIAuth::ApiKey(key));
        }
    }

    // 3. providers.json "azure" entry
    if let Some(key) = crate::config::api_key("", PROVIDER_NAME) {
        return Ok(AzureOpenAIAuth::ApiKey(key));
    }

    Err(format!(
        "no credentials for provider '{PROVIDER_NAME}': set AZURE_OPENAI_TOKEN, \
         AZURE_OPENAI_API_KEY, or add an entry in {}",
        crate::config::user_config_root()
            .join("providers.json")
            .display(),
    ))
}

#[async_trait]
impl Backend for AzureBackend {
    async fn run(&self, mut params: RunParams) -> Result<CompletedRun, ClientError> {
        validate_max_retries(params.max_retries).map_err(|m| ClientError::Runtime { message: m })?;

        // Snatch interactive session before params.clone() drops the receiver.
        let interactive = params.interactive.take();

        let idle_timeout = params
            .idle_timeout
            .unwrap_or_else(crate::config::stream_idle_timeout);
        let prefix = if params.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", params.label)
        };
        let mut ctx = RunContext {
            params: params.clone(),
            prefix: prefix.clone(),
            idle_timeout,
            expected_artifact_paths: params.expected_artifact_paths.clone(),
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
        };
        ctx.params.interactive = interactive;
        *self.state.last_ctx.lock().unwrap() = Some(ctx.clone());

        let prompt = Mutex::new(params.prompt.clone());
        let timeout_prompt = params.on_timeout_prompt.clone();
        let backoff = &STREAM_IDLE_BACKOFF[..params.max_retries];

        let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);

        retry::with_retry(
            backoff,
            |e: &ClientError| {
                matches!(
                    e,
                    ClientError::Timeout { .. } | ClientError::ApiServerError { .. }
                )
            },
            |attempt, e, wait| {
                let next = match e {
                    ClientError::Timeout { .. } => timeout_prompt
                        .clone()
                        .unwrap_or_else(|| prompt.lock().unwrap().clone()),
                    _ => prompt.lock().unwrap().clone(),
                };
                *prompt.lock().unwrap() = next;
                let cause = match e {
                    ClientError::Timeout { .. } => "stream idle timeout",
                    ClientError::ApiServerError { .. } => "transient-error",
                    _ => "error",
                };
                log::warn!(
                    "{prefix}stream {cause}, retrying in {wait}s ({}/{})...",
                    attempt + 1,
                    params.max_retries
                );
            },
            || {
                let p = prompt.lock().unwrap().clone();
                let ctx = ctx.clone();
                let cancel = cancel.clone();
                async move {
                    let gremlin_id = ctx.params.gremlin_id.clone().unwrap_or_default();
                    let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);

                    // Check+insert under the cancels lock so reap_all cannot
                    // remove the gremlin entry between the check and insert.
                    {
                        let mut guard = self.state.cancels.lock().unwrap();
                        if cancel.is_cancelled() {
                            return Err(ClientError::Runtime {
                                message: "cancelled".into(),
                            });
                        }
                        guard
                            .entry(gremlin_id.clone())
                            .or_default()
                            .insert(id, cancel.clone());
                    }

                    let model_name = self.state.effective_model(ctx.params.model.as_deref());
                    let model = self.state.client.completion_model(&model_name);
                    let result = run_agent_loop(
                        &model,
                        &p,
                        ctx,
                        cancel,
                        LoopOpts {
                            extra: self.state.extra_params(),
                            tool_filter: self.state.tool_filter.as_deref(),
                            classify_error: Some(default_classify as ErrorClassifier),
                        },
                        None, // task_model_selector deferred
                    )
                    .await;

                    if let Ok(mut guard) = self.state.cancels.lock() {
                        if let Some(inner) = guard.get_mut(&gremlin_id) {
                            inner.remove(&id);
                            if inner.is_empty() {
                                guard.remove(&gremlin_id);
                            }
                        }
                    }
                    result
                }
            },
        )
        .await
    }

    async fn resume(&self) -> Result<CompletedRun, ClientError> {
        let params = {
            let guard = self.state.last_ctx.lock().unwrap();
            let ctx = guard.as_ref().ok_or_else(|| ClientError::Runtime {
                message: "resume() called before run()".into(),
            })?;
            ctx.params.clone()
        };
        self.run(params).await
    }

    fn reap_all(&self, gremlin_id: &str) {
        if let Ok(mut guard) = self.state.cancels.lock() {
            let tokens: Vec<_> = guard
                .remove(gremlin_id)
                .into_iter()
                .flat_map(|m| m.into_values())
                .collect();
            let count = tokens.len();
            log::debug!(
                "{}::reap_all: cancelling {count} in-flight token(s) for gremlin_id={gremlin_id} (model={})",
                self.state.log_label,
                self.state.model,
            );
            for token in &tokens {
                token.cancel();
            }
        }
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::super::agent_loop::CancelToken;
    use super::*;
    use crate::test_support::EnvGuard;

    fn scrub_azure_env(guard: &mut EnvGuard) {
        guard.remove("AZURE_OPENAI_TOKEN");
        guard.remove("AZURE_OPENAI_API_KEY");
        guard.remove("AZURE_OPENAI_ENDPOINT");
        guard.remove("AZURE_OPENAI_API_VERSION");
    }

    fn isolated_env() -> EnvGuard {
        let mut guard = EnvGuard::lock();
        scrub_azure_env(&mut guard);
        let tmp = tempfile::tempdir().unwrap().keep();
        guard.set("GREMLINS_SANDBOX_ROOT", &tmp);
        guard.set("HOME", &tmp);
        guard
    }

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "azure");
    }

    #[test]
    fn build_rejects_missing_endpoint() {
        let _guard = isolated_env();

        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("AZURE_OPENAI_ENDPOINT"),
            "got: {err}"
        );
    }

    #[test]
    fn build_rejects_empty_model() {
        let mut guard = isolated_env();
        guard.set("AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("AZURE_OPENAI_API_KEY", "fake-key");

        let result = AzureBackend::build(
            "",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("deployment name"),
            "got: {err}"
        );
    }

    #[test]
    fn build_rejects_missing_credentials() {
        let mut guard = isolated_env();
        guard.set("AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");

        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("no credentials for provider 'azure'"),
            "got: {err}"
        );
    }

    // ── auth precedence tests ─────────────────────────────────────────

    #[test]
    fn auth_precedence_token_over_api_key() {
        let mut guard = isolated_env();
        guard.set("AZURE_OPENAI_TOKEN", "bearer-token-123");
        guard.set("AZURE_OPENAI_API_KEY", "api-key-456");

        let auth = resolve_auth().unwrap();
        assert!(
            matches!(auth, AzureOpenAIAuth::Token(t) if t == "bearer-token-123"),
            "AZURE_OPENAI_TOKEN should win over AZURE_OPENAI_API_KEY"
        );
    }

    #[test]
    fn auth_precedence_api_key_over_providers_json() {
        let mut guard = isolated_env();
        guard.set("AZURE_OPENAI_API_KEY", "env-api-key");

        // Write a providers.json so we can prove the env var wins.
        let sandbox_root = std::env::var("GREMLINS_SANDBOX_ROOT").unwrap();
        let config_dir = std::path::PathBuf::from(&sandbox_root).join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.json"),
            r#"{"azure": {"api-key": "providers-json-key"}}"#,
        )
        .unwrap();

        let auth = resolve_auth().unwrap();
        assert!(
            matches!(auth, AzureOpenAIAuth::ApiKey(k) if k == "env-api-key"),
            "AZURE_OPENAI_API_KEY should win over providers.json"
        );
    }

    #[test]
    fn auth_precedence_providers_json_fallback() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let providers_path = config_dir.join("providers.json");
        std::fs::write(
            &providers_path,
            r#"{"azure": {"api-key": "providers-json-key"}}"#,
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());

        let auth = resolve_auth().unwrap();
        assert!(
            matches!(auth, AzureOpenAIAuth::ApiKey(k) if k == "providers-json-key"),
            "providers.json should be the fallback when no env vars are set"
        );
    }

    #[test]
    fn api_version_defaults() {
        let mut guard = isolated_env();
        guard.remove("AZURE_OPENAI_API_VERSION");
        assert_eq!(crate::config::azure_api_version(), "2024-10-21");
    }

    #[test]
    fn api_version_from_env() {
        let mut guard = isolated_env();
        guard.set("AZURE_OPENAI_API_VERSION", "2025-01-01");
        assert_eq!(crate::config::azure_api_version(), "2025-01-01");
    }

    #[test]
    fn extra_params_passthrough() {
        let mut extra = indexmap::IndexMap::new();
        extra.insert("max_tokens".into(), "1024".into());
        extra.insert("temperature".into(), "0.7".into());

        let client_params = openai_protocol::string_map(&extra);
        let state = AzureRunState {
            client: azure::Client::builder()
                .api_key(AzureOpenAIAuth::ApiKey("fake-key".into()))
                .api_version("2024-10-21")
                .azure_endpoint("https://example.openai.azure.com".to_string())
                .build()
                .unwrap(),
            model: "gpt-4o".into(),
            tool_filter: None,
            client_params,
            last_ctx: Mutex::new(None),
            cancels: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            log_label: "test".into(),
        };

        let obj = state.extra_params().expect("extra_params should be Some");
        assert_eq!(obj["max_tokens"], 1024);
        assert_eq!(obj["temperature"], 0.7);
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let mut guard = isolated_env();
        guard.set("AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("AZURE_OPENAI_API_KEY", "fake-key");

        let client = azure::Client::builder()
            .api_key(AzureOpenAIAuth::ApiKey("fake-key".into()))
            .api_version("2024-10-21")
            .azure_endpoint("https://example.openai.azure.com".to_string())
            .build()
            .unwrap();

        let backend = AzureBackend {
            state: AzureRunState {
                client,
                model: "gpt-4o".into(),
                tool_filter: None,
                client_params: HashMap::new(),
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "test".into(),
            },
        };
        let a = CancelToken::new();
        let b = CancelToken::new();
        let sibling = CancelToken::new();
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-test".to_string())
            .or_default()
            .insert(1, a.clone());
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-test".to_string())
            .or_default()
            .insert(2, b.clone());
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-sibling".to_string())
            .or_default()
            .insert(3, sibling.clone());
        backend.reap_all("gr-test");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-test")
            .is_none());
        assert!(!sibling.is_cancelled());
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-sibling")
            .is_some());
    }
}
