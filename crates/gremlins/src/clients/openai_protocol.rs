use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use rig_core::http_client::DynHttpClient;
use rig_core::providers::openai::wire::Dialect;
use rig_core::providers::openai::{OpenAI, OpenAIConfig};

use super::agent_loop::{run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext};
use super::backend::{ClientError, RunParams};
use super::interactive::InteractiveSession;
use super::params::{self, ResolvedParams};
use super::protocol::CompletedRun;

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
    pub(crate) resolved: ResolvedParams,
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
        resolved: ResolvedParams,
        log_label: String,
    ) -> Self {
        Self {
            client,
            model,
            tool_filter,
            resolved,
            last_ctx: Mutex::new(None),
            cancels: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            log_label,
        }
    }

    pub(crate) fn extra_params(&self) -> Option<serde_json::Value> {
        self.resolved.additional_value()
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
/// `task-models` specs).
pub(crate) async fn run_openai_compat(
    state: &OpenAiRunState,
    params: RunParams,
    interactive: Option<InteractiveSession>,
    classify_error: Option<ErrorClassifier>,
    _provider_name: &str, // kept for logging; unused after task-models moved to Backend trait
) -> Result<CompletedRun, ClientError> {
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
        prepopulated_messages: params.prepopulated_messages.clone(),
    };
    *state.last_ctx.lock().unwrap() = Some(ctx.clone());

    // Use the supervisor's cancel token when available; otherwise create one.
    let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);

    if cancel.is_cancelled() {
        return Err(ClientError::Runtime {
            message: "cancelled".into(),
        });
    }

    let gremlin_id = params.gremlin_id.clone().unwrap_or_default();
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    state
        .cancels
        .lock()
        .unwrap()
        .entry(gremlin_id.clone())
        .or_default()
        .insert(id, cancel.clone());

    let model_name = state.effective_model(params.model.as_deref());
    let result = run_with_agent_loop(
        &state.client,
        &model_name,
        &params.prompt,
        &ctx,
        cancel,
        state.extra_params(),
        state.tool_filter.as_deref(),
        classify_error,
        state.resolved.temperature,
        state.resolved.max_tokens,
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

// ── shared helpers ───────────────────────────────────────────────────────

/// Resolve raw client params for an OpenAI-protocol backend.
///
/// `temperature` and `max_tokens` are typed top-level fields.  `reasoning` is
/// a shaped param (client-param-first, env-var-fallback) expanded to the
/// harness `{"effort": .., "summary": "auto"}` object, and
/// `parallel_tool_calls` is a typed bool defaulted to `true`.  Every other key
/// passes through JSON-coerced.
pub(crate) fn resolve_openai_protocol(
    provider: &str,
    raw: &IndexMap<String, String>,
) -> Result<ResolvedParams, String> {
    let mut additional = serde_json::Map::new();
    // Default: parallel tool calls enabled.
    additional.insert(
        "parallel_tool_calls".to_string(),
        serde_json::Value::Bool(true),
    );

    let mut temperature = None;
    let mut max_tokens = None;
    // reasoning effort: client param > env var. The explicit value is resolved
    // and validated first; the env fallback is only read/validated when the
    // spec omits the key, so a valid override wins over a malformed env var.
    let mut reasoning_effort = None;

    for (k, v) in raw {
        match k.as_str() {
            "temperature" => temperature = Some(params::parse_f64(provider, k, v)?),
            "max_tokens" => max_tokens = Some(params::parse_u64(provider, k, v)?),
            "reasoning" => reasoning_effort = Some(validate_reasoning(provider, v)?),
            "parallel_tool_calls" => {
                let b = params::parse_bool(provider, k, v)?;
                additional.insert(k.clone(), serde_json::Value::Bool(b));
            }
            _ => {
                additional.insert(k.clone(), params::coerce(v));
            }
        }
    }

    if reasoning_effort.is_none() {
        reasoning_effort = match crate::config::reasoning_effort() {
            Some(effort) => Some(validate_reasoning(provider, &effort)?),
            None => None,
        };
    }

    // `openrouter` and `xai` reason by default: when neither the per-client
    // param nor the env var supplied an effort, default these two providers to
    // `high`. `openai` and `azure-openai` stay opt-in.
    if reasoning_effort.is_none() && matches!(provider, "openrouter" | "xai") {
        reasoning_effort = Some("high".to_string());
    }

    if let Some(effort) = reasoning_effort {
        additional.insert(
            "reasoning".to_string(),
            serde_json::json!({ "effort": effort, "summary": "auto" }),
        );
    }

    Ok(ResolvedParams {
        temperature,
        max_tokens,
        additional,
    })
}

/// Validate a `reasoning` effort value (`low`/`medium`/`high`).
fn validate_reasoning(provider: &str, value: &str) -> Result<String, String> {
    match value {
        "low" | "medium" | "high" => Ok(value.to_string()),
        _ => Err(format!(
            "{provider}: invalid value {value:?} for client param \"reasoning\": expected one of low, medium, high"
        )),
    }
}

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
    temperature: Option<f64>,
    max_tokens: Option<u64>,
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
            temperature,
            max_tokens,
        },
        interactive,
    )
    .await
}

/// Split a model specifier into `(provider, model)`, or `None` when the
/// provider or model part is empty or the spec is otherwise malformed.
///
/// This is a thin adapter over [`crate::clients::client::ModelURI`], so it
/// derives the provider and model from the exact same grammar as
/// [`crate::clients::client::Client`] and discards any parsed parameter
/// suffix. Both URI spellings are accepted: `openai/gpt-4o?foo=bar` yields
/// `("openai", "gpt-4o")`, as does `model://openai/gpt-4o?foo=bar`. An
/// OpenRouter model ID with a colon suffix such as
/// `openrouter/some/model:free` yields `("openrouter", "some/model:free")`.
pub(crate) fn provider_and_model(spec: &str) -> Option<(String, String)> {
    let uri = crate::clients::client::ModelURI::parse(spec).ok()?;
    Some((uri.provider().to_string(), uri.model().to_string()))
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(raw: &[(&str, &str)]) -> ResolvedParams {
        // `resolve_openai_protocol` falls back to `GREMLINS_REASONING_EFFORT`
        // from the environment. Take the shared EnvGuard lock so these
        // env-independent cases serialize with the tests that set that
        // variable (e.g. `malformed_env_reasoning_fails`) instead of racing
        // them and observing a transient value.
        let _guard = crate::test_support::EnvGuard::lock();
        let map: IndexMap<String, String> = raw
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        resolve_openai_protocol("openai", &map).unwrap()
    }

    #[test]
    fn default_parallel_tool_calls_true() {
        let r = resolve(&[]);
        let additional = r.additional_value().unwrap();
        assert_eq!(additional["parallel_tool_calls"], true);
        assert!(additional.get("reasoning").is_none());
    }

    #[test]
    fn client_reasoning_overrides() {
        let r = resolve(&[("reasoning", "low")]);
        let additional = r.additional_value().unwrap();
        assert_eq!(additional["reasoning"]["effort"], "low");
        assert_eq!(additional["reasoning"]["summary"], "auto");
    }

    /// `openrouter` and `xai` default `reasoning` to `high` when neither a
    /// per-client `reasoning=` param nor `GREMLINS_REASONING_EFFORT` is set,
    /// while `openai` and `azure-openai` stay opt-in.
    #[test]
    fn reasoning_by_default_for_openrouter_and_xai() {
        let _guard = crate::test_support::EnvGuard::lock();
        let map: IndexMap<String, String> = IndexMap::new();

        for provider in ["openrouter", "xai"] {
            let r = resolve_openai_protocol(provider, &map).unwrap();
            let additional = r.additional_value().unwrap();
            assert_eq!(additional["reasoning"]["effort"], "high");
            assert_eq!(additional["reasoning"]["summary"], "auto");
        }

        for provider in ["openai", "azure-openai"] {
            let r = resolve_openai_protocol(provider, &map).unwrap();
            let additional = r.additional_value().unwrap();
            assert!(additional.get("reasoning").is_none());
        }
    }

    /// An explicit `reasoning=` client param still wins over the `high`
    /// default for the reasoning-by-default providers.
    #[test]
    fn explicit_reasoning_overrides_default_for_openrouter_and_xai() {
        let _guard = crate::test_support::EnvGuard::lock();
        for provider in ["openrouter", "xai"] {
            let map: IndexMap<String, String> = [("reasoning".to_string(), "low".to_string())]
                .into_iter()
                .collect();
            let r = resolve_openai_protocol(provider, &map).unwrap();
            let additional = r.additional_value().unwrap();
            assert_eq!(additional["reasoning"]["effort"], "low");
        }
    }

    #[test]
    fn reasoning_effort_extraction_locked() {
        let r = resolve(&[("reasoning", "low")]);
        let effort = r
            .additional_value()
            .and_then(|v| v.get("reasoning").cloned())
            .and_then(|v| v.get("effort").and_then(|e| e.as_str().map(String::from)));
        assert_eq!(effort.as_deref(), Some("low"));

        let r = resolve(&[]);
        let effort = r
            .additional_value()
            .and_then(|v| v.get("reasoning").cloned())
            .and_then(|v| v.get("effort").and_then(|e| e.as_str().map(String::from)));
        assert_eq!(effort, None);
    }

    #[test]
    fn client_passthrough() {
        let r = resolve(&[("thinking", "deepseek"), ("foo", "bar")]);
        let additional = r.additional_value().unwrap();
        assert_eq!(additional["thinking"], "deepseek");
        assert_eq!(additional["foo"], "bar");
        assert!(additional.get("reasoning").is_none());
    }

    #[test]
    fn typed_fields_not_in_passthrough() {
        let r = resolve(&[
            ("temperature", "0.7"),
            ("max_tokens", "4096"),
            ("top_p", "0.95"),
            ("stream", "true"),
            ("stop", "[\"END\"]"),
        ]);
        assert_eq!(r.temperature, Some(0.7));
        assert_eq!(r.max_tokens, Some(4096));
        let additional = r.additional_value().unwrap();
        assert!(additional.get("temperature").is_none());
        assert!(additional.get("max_tokens").is_none());
        assert_eq!(additional["top_p"], serde_json::json!(0.95));
        assert_eq!(additional["stream"], serde_json::json!(true));
        assert_eq!(additional["stop"], serde_json::json!(["END"]));
    }

    #[test]
    fn reasoning_plus_passthrough() {
        let r = resolve(&[("reasoning", "high"), ("thinking", "deepseek")]);
        let additional = r.additional_value().unwrap();
        assert_eq!(additional["reasoning"]["effort"], "high");
        assert_eq!(additional["thinking"], "deepseek");
        assert_eq!(additional["parallel_tool_calls"], true);
    }

    #[test]
    fn parallel_tool_calls_override_and_malformed() {
        let r = resolve(&[("parallel_tool_calls", "false")]);
        assert_eq!(r.additional_value().unwrap()["parallel_tool_calls"], false);

        let map: IndexMap<String, String> =
            [("parallel_tool_calls".to_string(), "notabool".to_string())]
                .into_iter()
                .collect();
        let err = resolve_openai_protocol("openai", &map).unwrap_err();
        assert!(err.contains("parallel_tool_calls"), "{err}");
        assert!(err.contains("notabool"), "{err}");
    }

    #[test]
    fn malformed_temperature_and_max_tokens_fail() {
        let map: IndexMap<String, String> = [("temperature".to_string(), "hot".to_string())]
            .into_iter()
            .collect();
        let err = resolve_openai_protocol("openai", &map).unwrap_err();
        assert!(err.contains("temperature"), "{err}");

        let map: IndexMap<String, String> = [("max_tokens".to_string(), "lots".to_string())]
            .into_iter()
            .collect();
        let err = resolve_openai_protocol("openai", &map).unwrap_err();
        assert!(err.contains("max_tokens"), "{err}");
    }

    #[test]
    fn malformed_reasoning_fails() {
        let map: IndexMap<String, String> = [("reasoning".to_string(), "extreme".to_string())]
            .into_iter()
            .collect();
        let err = resolve_openai_protocol("openai", &map).unwrap_err();
        assert!(err.contains("reasoning"), "{err}");
        assert!(err.contains("extreme"), "{err}");
    }

    /// An invalid `GREMLINS_REASONING_EFFORT` env fallback must fail the
    /// client build, not silently produce a malformed `reasoning` object.
    #[test]
    fn malformed_env_reasoning_fails() {
        let mut guard = crate::test_support::EnvGuard::lock();
        guard.set("GREMLINS_REASONING_EFFORT", "extreme");

        let map: IndexMap<String, String> = IndexMap::new();
        let err = resolve_openai_protocol("openai", &map).unwrap_err();
        assert!(err.contains("reasoning"), "{err}");
        assert!(err.contains("extreme"), "{err}");
    }

    /// A valid client `reasoning=` override wins over a malformed env var:
    /// the explicit value is resolved and validated before the env fallback is
    /// ever read, so the build succeeds.
    #[test]
    fn client_reasoning_overrides_malformed_env() {
        let mut guard = crate::test_support::EnvGuard::lock();
        guard.set("GREMLINS_REASONING_EFFORT", "extreme");

        let map: IndexMap<String, String> = [("reasoning".to_string(), "low".to_string())]
            .into_iter()
            .collect();
        let r = resolve_openai_protocol("openai", &map).unwrap();
        let additional = r.additional_value().unwrap();
        assert_eq!(additional["reasoning"]["effort"], "low");
    }

    fn assert_provider_and_model_agrees_with_model_uri(spec: &str) {
        match crate::clients::client::ModelURI::parse(spec) {
            Ok(uri) => {
                assert_eq!(
                    provider_and_model(spec),
                    Some((uri.provider().to_string(), uri.model().to_string())),
                    "{spec:?}"
                );
            }
            Err(_) => {
                assert_eq!(provider_and_model(spec), None, "{spec:?}");
            }
        }
    }

    #[test]
    fn provider_and_model_agrees_with_model_uri_on_plain_and_param_specs() {
        for (spec, expected) in [
            ("openai/gpt-4o", ("openai", "gpt-4o")),
            ("openai/gpt-4o?foo=bar", ("openai", "gpt-4o")),
            ("openai/gpt-4o?top_p=0.7,n=3", ("openai", "gpt-4o")),
            (
                "openrouter/anthropic/claude-sonnet-4:free",
                ("openrouter", "anthropic/claude-sonnet-4:free"),
            ),
            (
                "openrouter/google/gemini-2.5-flash:online",
                ("openrouter", "google/gemini-2.5-flash:online"),
            ),
            (
                "openrouter/some/model:free",
                ("openrouter", "some/model:free"),
            ),
            ("model://openai/gpt-4o?foo=bar", ("openai", "gpt-4o")),
            ("cmd/echo hello:world=foo", ("cmd", "echo hello:world=foo")),
        ] {
            assert_provider_and_model_agrees_with_model_uri(spec);
            assert_eq!(
                provider_and_model(spec),
                Some((expected.0.to_string(), expected.1.to_string())),
                "{spec:?}"
            );
        }
    }

    #[test]
    fn provider_and_model_agrees_with_model_uri_on_malformed_specs() {
        const LEGACY_COLON_SPEC: &str = concat!("openai", ":", "gpt-4o");

        for spec in [
            "",
            "only",
            LEGACY_COLON_SPEC,
            "/model",
            "openai/",
            "openai/gpt-4o?reasoning=high,bad",
            "model://cmd/true",
        ] {
            assert_provider_and_model_agrees_with_model_uri(spec);
        }

        // Belt-and-braces: the malformed/empty-parts contract remains None.
        assert_eq!(provider_and_model(""), None);
        assert_eq!(provider_and_model("only"), None);
        assert_eq!(provider_and_model(LEGACY_COLON_SPEC), None);
        assert_eq!(provider_and_model("openai/"), None);
    }
}
