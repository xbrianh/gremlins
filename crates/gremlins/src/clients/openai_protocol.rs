use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use rig_core::driver::DynModel;
use rig_core::http_client::DynHttpClient;
use rig_core::operation::Completion;
use rig_core::providers::openai::wire::Dialect;
use rig_core::providers::openai::{OpenAI, OpenAIConfig};

use super::agent_loop::{run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext};
use super::backend::{ClientError, RunParams};
use super::interactive::InteractiveSession;
use super::protocol::CompletedRun;
use super::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use super::task::TaskModelSelector;

/// The completion model type shared by every OpenAI-compatible backend.
pub(crate) type OpenAiModel = DynModel<Completion>;

/// Shared state for OpenAI-protocol backends.
///
/// Every field that was duplicated between `OpenAiBackend` and
/// `OpenRouterBackend` lives here.  The two backends differ only in their
/// error classifier and provider name, which are passed as parameters to
/// [`run_openai_compat`] and [`reap_openai_compat`].
pub(crate) struct OpenAiRunState {
    pub(crate) client: OpenAI,
    pub(crate) model: String,
    pub(crate) tool_filter: Option<Vec<String>>,
    pub(crate) client_params: HashMap<String, String>,
    pub(crate) last_ctx: Mutex<Option<RunContext>>,
    pub(crate) cancels: Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>,
    pub(crate) next_id: AtomicU64,
    pub(crate) log_label: String,
}

impl OpenAiRunState {
    pub(crate) fn new(
        client: OpenAI,
        model: String,
        tool_filter: Option<Vec<String>>,
        client_params: HashMap<String, String>,
        log_label: String,
    ) -> Self {
        Self {
            client,
            model,
            tool_filter,
            client_params,
            last_ctx: Mutex::new(None),
            cancels: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            log_label,
        }
    }

    pub(crate) fn extra_params(&self) -> Option<serde_json::Value> {
        build_extra_params(&self.client_params)
    }

    pub(crate) fn effective_model(&self, override_model: Option<&str>) -> String {
        match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        }
    }
}

// ── run_openai_compat ────────────────────────────────────────────────────

/// Execute a full run through an OpenAI-compatible backend.
///
/// This is the single retry-loop + cancel-token implementation shared by
/// [`OpenAiBackend`] and [`OpenRouterBackend`].  The two differ only in
/// `classify_error` (the per-attempt error classifier passed through to
/// [`run_with_agent_loop`]) and `provider_name` (used to match
/// `task-clients` specs).
pub(crate) async fn run_openai_compat(
    state: &OpenAiRunState,
    params: RunParams,
    mut interactive: Option<InteractiveSession>,
    classify_error: Option<ErrorClassifier>,
    provider_name: &str,
) -> Result<CompletedRun, ClientError> {
    validate_max_retries(params.max_retries).map_err(|m| ClientError::Runtime { message: m })?;

    let idle_timeout = params
        .idle_timeout
        .unwrap_or_else(crate::config::stream_idle_timeout);
    let prefix = if params.label.is_empty() {
        String::new()
    } else {
        format!("[{}] ", params.label)
    };
    let ctx = RunContext {
        params: params.clone(),
        prefix: prefix.clone(),
        idle_timeout,
        expected_artifact_paths: params.expected_artifact_paths.clone(),
        reminder_budget: crate::config::artifact_reminder_budget(),
        completion_nudge_budget: crate::config::completion_nudge_budget(),
        stream_events: params.stream_events.clone(),
    };
    *state.last_ctx.lock().unwrap() = Some(ctx.clone());

    let prompt = Mutex::new(params.prompt.clone());
    let timeout_prompt = params.on_timeout_prompt.clone();
    let backoff = &STREAM_IDLE_BACKOFF[..params.max_retries];

    // Use the supervisor's cancel token when available; otherwise create one.
    let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);

    retry::with_retry(
        backoff,
        classify_retryable,
        |attempt, e, wait| {
            let next = retry_prompt(e, &prompt.lock().unwrap(), timeout_prompt.as_deref());
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
            // Move interactive on first attempt; subsequent retries get None.
            let interactive = interactive.take();
            async move {
                // Before each retry attempt, check if we've been cancelled.
                if cancel.is_cancelled() {
                    return Err(ClientError::Runtime {
                        message: "cancelled".into(),
                    });
                }

                let gremlin_id = ctx.params.gremlin_id.clone().unwrap_or_default();
                let id = state.next_id.fetch_add(1, Ordering::Relaxed);
                state
                    .cancels
                    .lock()
                    .unwrap()
                    .entry(gremlin_id.clone())
                    .or_default()
                    .insert(id, cancel.clone());

                let model_name = state.effective_model(ctx.params.model.as_deref());
                let result = run_with_agent_loop(
                    &state.client,
                    &model_name,
                    &p,
                    &ctx,
                    cancel,
                    state.extra_params(),
                    state.tool_filter.as_deref(),
                    classify_error,
                    task_model_selector(
                        &state.client,
                        provider_name,
                        &ctx.params.task_clients_exact,
                        &ctx.params.task_clients_prefix,
                    ),
                    interactive,
                )
                .await;

                if let Ok(mut guard) = state.cancels.lock() {
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

/// Cancel every in-flight token for `gremlin_id`.
pub(crate) fn reap_openai_compat(state: &OpenAiRunState, gremlin_id: &str) {
    if let Ok(mut guard) = state.cancels.lock() {
        let tokens: Vec<_> = guard
            .remove(gremlin_id)
            .into_iter()
            .flat_map(|m| m.into_values())
            .collect();
        let count = tokens.len();
        log::debug!(
            "{}::reap_all: cancelling {count} in-flight token(s) for gremlin_id={gremlin_id} (model={})",
            state.log_label,
            state.model,
        );
        for token in &tokens {
            token.cancel();
        }
    }
}

// ── HTTP client pool + builder ──────────────────────────────────────────

fn http_client_pool() -> &'static Mutex<HashMap<(String, String), DynHttpClient>> {
    static POOL: OnceLock<Mutex<HashMap<(String, String), DynHttpClient>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Return the last `n` characters of `s`, or the whole string if it's shorter.
///
/// This is character-aware: it will never split a multi-byte UTF-8 sequence.
fn last_n_chars(s: &str, n: usize) -> &str {
    let char_count = s.chars().count();
    if char_count <= n {
        s
    } else {
        let skip = char_count - n;
        s.char_indices()
            .nth(skip)
            .map(|(idx, _)| &s[idx..])
            .unwrap_or(s)
    }
}

/// Build the rig OpenAI-compatible client shared by openai, xai and openrouter.
///
/// HTTP clients are pooled by `(base_url, api_key)` so that every backend
/// targeting the same provider endpoint shares one connection pool.  The pool
/// lock is held across construction so that concurrent callers cannot race to
/// build duplicate clients.
pub(crate) fn build_openai_client(
    api_key: &str,
    base_url: &str,
    dialect: &Dialect,
) -> Result<OpenAI, String> {
    let cache_key = (base_url.to_string(), api_key.to_string());

    let http_client = {
        let mut pool = http_client_pool()
            .lock()
            .expect("http client pool poisoned");
        if let Some(client) = pool.get(&cache_key) {
            log::debug!(
                "HTTP client cache hit for {base_url} (key ...{})",
                last_n_chars(api_key, 4)
            );
            client.clone()
        } else {
            log::info!("Creating new HTTP client for provider at {base_url}");
            let client = DynHttpClient::new(rig_reqwest::ReqwestClient::default());
            pool.insert(cache_key, client.clone());
            client
        }
    };

    Ok(OpenAIConfig::with_key(dialect, api_key)
        .with_base_url(base_url)
        .connect(http_client))
}

/// The `allowed_tools` entry of `native_block`, if any.
pub(crate) fn tool_filter(native_block: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    native_block.get("allowed_tools").cloned()
}

/// Copy an insertion-ordered param map into the plain map the backends take.
pub(crate) fn string_map(params: &IndexMap<String, String>) -> HashMap<String, String> {
    params.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

// ── shared helpers ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_with_agent_loop(
    client: &OpenAI,
    model_name: &str,
    prompt: &str,
    ctx: &RunContext,
    cancel: Arc<CancelToken>,
    extra: Option<serde_json::Value>,
    tool_filter: Option<&[String]>,
    classify_error: Option<ErrorClassifier>,
    task_model_selector: Option<TaskModelSelector<OpenAiModel>>,
    interactive: Option<InteractiveSession>,
) -> Result<CompletedRun, ClientError> {
    let model = client.completion(model_name).erase();
    let mut ctx = ctx.clone();
    ctx.params.model = Some(model_name.to_string());

    run_agent_loop(
        model,
        prompt,
        ctx,
        cancel,
        LoopOpts {
            extra,
            tool_filter,
            classify_error,
            max_tokens: None,
            skip_temperature: false,
        },
        task_model_selector,
        interactive,
    )
    .await
}

/// Build the `task-clients` selector for an OpenAI-compatible client, or `None`
/// when `settings.yaml` declares no entries this backend can serve.
///
/// The config is read once per run; the returned selector is shared behind an
/// `Arc`, so each Task clones a pointer rather than the maps themselves. When
/// nothing is configured the selector is `None` and the common path is free.
pub(super) fn task_model_selector(
    client: &OpenAI,
    provider_name: &str,
    task_clients_exact: &HashMap<String, String>,
    task_clients_prefix: &HashMap<String, String>,
) -> Option<TaskModelSelector<OpenAiModel>> {
    if task_clients_exact.is_empty() && task_clients_prefix.is_empty() {
        return None;
    }

    let exact = task_clients_exact.clone();
    let prefix = task_clients_prefix.clone();
    let client = client.clone();
    let provider_name = provider_name.to_string();
    TaskModelSelector::new(
        exact,
        prefix,
        Arc::new(move |spec: &str| {
            let (provider, model) = provider_and_model(spec)?;
            if provider == provider_name {
                Some(client.completion(model).erase())
            } else {
                log::warn!(
                    "task-clients entry spec {spec:?} names provider {provider:?}, but this \
                     backend serves {provider_name:?} — falling back to parent model"
                );
                None
            }
        }),
    )
}

/// Split a client specifier into `(provider, model)`, or `None` when the
/// provider or model part is empty.
///
/// The provider is everything before the first `:`; the remainder is the model
/// identifier.  A trailing `:k=v,...` parameter suffix (where the segment after
/// the last `:` contains `=`) is stripped, so `openai:gpt-4o:foo=bar` yields
/// `("openai", "gpt-4o")`.  OpenRouter model IDs that carry colon suffixes
/// like `:free` or `:online` are preserved — `openrouter:some/model:free`
/// yields `("openrouter", "some/model:free")`.
pub(crate) fn provider_and_model(spec: &str) -> Option<(&str, &str)> {
    let (provider, rest) = spec.split_once(':')?;
    if provider.is_empty() || rest.is_empty() {
        return None;
    }
    // Strip a trailing `:k=v,...` params suffix.  That suffix always contains
    // `=` in the segment following the last colon.
    let model = if let Some(colon_pos) = rest.rfind(':') {
        let after_last_colon = &rest[colon_pos + 1..];
        if after_last_colon.contains('=') {
            &rest[..colon_pos]
        } else {
            rest
        }
    } else {
        rest
    };
    if model.is_empty() {
        return None;
    }
    Some((provider, model))
}

pub(crate) fn build_extra_params(
    client_params: &HashMap<String, String>,
) -> Option<serde_json::Value> {
    let mut params = serde_json::Map::new();

    params.insert("parallel_tool_calls".into(), serde_json::Value::Bool(true));

    // reasoning effort: client param > env var
    let effort = client_params
        .get("reasoning")
        .cloned()
        .or_else(crate::config::reasoning_effort);
    if let Some(effort) = effort {
        params.insert(
            "reasoning".into(),
            serde_json::json!({"effort": effort, "summary": "auto"}),
        );
    }

    // Pass through any other client params. Parse each value as JSON so
    // numbers/bools survive as their natural types; fall back to a plain
    // string if the value isn't valid JSON (e.g. an opaque enum like
    // thinking=deepseek).
    // "reasoning" and "parallel_tool_calls" are excluded — reserved keys with
    // provider-specific handling above.
    for (k, v) in client_params {
        if k != "reasoning" && k != "parallel_tool_calls" {
            let val = match serde_json::from_str::<serde_json::Value>(v) {
                Ok(parsed) => parsed,
                Err(_) => serde_json::Value::String(v.clone()),
            };
            params.insert(k.clone(), val);
        }
    }

    if params.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(params))
    }
}

fn classify_retryable(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Timeout { .. } | ClientError::ApiServerError { .. }
    )
}

fn retry_prompt(err: &ClientError, prompt: &str, on_timeout_prompt: Option<&str>) -> String {
    match err {
        ClientError::Timeout { .. } => on_timeout_prompt.unwrap_or(prompt).to_string(),
        _ => prompt.to_string(),
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_params_default_no_reasoning() {
        let p = build_extra_params(&HashMap::new()).unwrap();
        assert_eq!(p["parallel_tool_calls"], true);
        assert!(p.get("reasoning").is_none());
    }

    #[test]
    fn extra_params_client_reasoning_overrides() {
        let mut cp = HashMap::new();
        cp.insert("reasoning".into(), "low".into());
        let p = build_extra_params(&cp).unwrap();
        assert_eq!(p["reasoning"]["effort"], "low");
        assert_eq!(p["reasoning"]["summary"], "auto");
    }

    /// Lock the contract between [`build_extra_params`] output shape and the
    /// `opts.extra -> reasoning.effort` extraction in [`run_agent_loop`]. If
    /// the JSON structure produced by `build_extra_params` ever changes, this
    /// test must also change — preventing silent `reasoning_effort=default`
    /// degradation in stage-init telemetry.
    #[test]
    fn build_extra_params_to_reasoning_effort_extraction_locked() {
        let mut cp = HashMap::new();
        cp.insert("reasoning".into(), "low".into());
        let extra = build_extra_params(&cp).unwrap();
        let effort = extra
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|e| e.as_str());
        assert_eq!(effort, Some("low"));

        // Without reasoning, extraction returns None
        let extra = build_extra_params(&HashMap::new()).unwrap();
        let effort = extra
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|e| e.as_str());
        assert_eq!(effort, None);
    }

    #[test]
    fn extra_params_client_passthrough() {
        let mut cp = HashMap::new();
        cp.insert("thinking".into(), "deepseek".into());
        cp.insert("foo".into(), "bar".into());
        let p = build_extra_params(&cp).unwrap();
        assert_eq!(p["thinking"], "deepseek");
        assert_eq!(p["foo"], "bar");
        assert!(p.get("reasoning").is_none());
    }

    #[test]
    fn extra_params_client_passthrough_json_types() {
        let mut cp = HashMap::new();
        cp.insert("temperature".into(), "0.7".into());
        cp.insert("top_p".into(), "0.95".into());
        cp.insert("stream".into(), "true".into());
        cp.insert("max_tokens".into(), "4096".into());
        cp.insert("stop".into(), "[\"END\"]".into()); // JSON array
        let p = build_extra_params(&cp).unwrap();
        // numbers
        assert_eq!(p["temperature"], serde_json::json!(0.7));
        assert_eq!(p["top_p"], serde_json::json!(0.95));
        assert_eq!(p["max_tokens"], serde_json::json!(4096));
        // bool
        assert_eq!(p["stream"], serde_json::json!(true));
        // JSON array passthrough
        assert_eq!(p["stop"], serde_json::json!(["END"]));
    }

    #[test]
    fn extra_params_client_reasoning_plus_passthrough() {
        let mut cp = HashMap::new();
        cp.insert("reasoning".into(), "high".into());
        cp.insert("thinking".into(), "deepseek".into());
        let p = build_extra_params(&cp).unwrap();
        assert_eq!(p["reasoning"]["effort"], "high");
        assert_eq!(p["thinking"], "deepseek");
        // xai auto-inserts parallel_tool_calls alongside reasoning + passthrough
        assert_eq!(p["parallel_tool_calls"], true);
    }

    #[test]
    fn retry_prompt_swaps_only_on_timeout() {
        let timeout = ClientError::Timeout {
            message: "idle".into(),
        };
        let api = ClientError::ApiServerError {
            message: "rate limit".into(),
        };
        assert_eq!(
            retry_prompt(&timeout, "orig", Some("timeout-prompt")),
            "timeout-prompt"
        );
        assert_eq!(retry_prompt(&api, "orig", Some("timeout-prompt")), "orig");
        assert_eq!(retry_prompt(&timeout, "orig", None), "orig");
    }

    #[test]
    fn provider_and_model_plain() {
        assert_eq!(
            provider_and_model("openai:gpt-4o"),
            Some(("openai", "gpt-4o"))
        );
    }

    #[test]
    fn provider_and_model_strips_params_suffix() {
        assert_eq!(
            provider_and_model("openai:gpt-4o:foo=bar"),
            Some(("openai", "gpt-4o"))
        );
        assert_eq!(
            provider_and_model("openai:gpt-4o:top_p=0.7,n=3"),
            Some(("openai", "gpt-4o"))
        );
    }

    #[test]
    fn provider_and_model_preserves_openrouter_colon_suffixes() {
        // OpenRouter model IDs can contain `:free`, `:online`, etc.
        assert_eq!(
            provider_and_model("openrouter:anthropic/claude-sonnet-4:free"),
            Some(("openrouter", "anthropic/claude-sonnet-4:free"))
        );
        assert_eq!(
            provider_and_model("openrouter:google/gemini-2.5-flash:online"),
            Some(("openrouter", "google/gemini-2.5-flash:online"))
        );
    }

    #[test]
    fn provider_and_model_rejects_empty_parts() {
        assert_eq!(provider_and_model("only"), None);
        assert_eq!(provider_and_model(":model"), None);
        assert_eq!(provider_and_model("provider:"), None);
        assert_eq!(provider_and_model(""), None);
    }
}
