//! Client specifiers and lazy backend construction.
//!
//! A pipeline names its model as a single string — `provider:model`, with an
//! optional trailing `:key=value,...` parameter list. This module owns that
//! grammar ([`parse_spec`]), the set of providers the harness can actually
//! construct ([`is_known_provider`]), and [`Client`], which turns a spec into a
//! live [`Backend`] on first use.
//!
//! Backends are built lazily and memoised behind [`Client::get_or_build_backend`]
//! so that parsing a pipeline never requires credentials: whichever process
//! eventually runs a stage is the one that pays for resolving the API key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use regex::Regex;
use rig_core::providers::openai;

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::cmd_backend::CmdBackend;
use crate::clients::openai_backend::{OpenAiBackend, OpenAiProvider};
use crate::clients::openrouter_backend::OpenRouterBackend;
use crate::clients::protocol::CompletedRun;
use crate::config::{api_key, user_config_root};

/// OpenRouter is OpenAI-compatible but not an [`OpenAiProvider`]; it gets its
/// own env var, provider key in `providers.json`, and base URL.
const OPENROUTER_PROVIDER_NAME: &str = "openrouter";
const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The six tools every native agent may call.
///
/// The allowlist is deliberately fixed per-client rather than configurable:
/// the harness's safety story is that a native agent can edit the worktree and
/// run commands, and nothing else.
pub fn default_native_block() -> HashMap<String, Vec<String>> {
    HashMap::from([(
        "allowed_tools".to_string(),
        vec![
            "Bash".to_string(),
            "Edit".to_string(),
            "Read".to_string(),
            "Write".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
        ],
    )])
}

/// True when `provider` names a provider this module can construct.
///
/// Callers use this to reject a bad spec early, before any work is queued.
pub fn is_known_provider(provider: &str) -> bool {
    matches!(provider, "openai" | "xai" | "openrouter" | "cmd")
}

/// The regex matching a trailing `:k=v,k=v` parameter list.
///
/// Compiled once: spec parsing happens for every stage in every pipeline load,
/// and the pattern never changes.
fn params_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r":([a-zA-Z_][a-zA-Z0-9_]*=[^,]+)(?:,([a-zA-Z_][a-zA-Z0-9_]*=[^,]+))*$")
            .expect("invalid client params regex")
    })
}

/// Split `provider:model[:k=v,...]`. Returns `(provider, model, extra params)`.
///
/// The parameter suffix is only recognised for non-`cmd` providers: a command
/// backend's model *is* a shell command, so anything after the first colon —
/// including `--foo=bar`-looking fragments — belongs to the command.
///
/// For every other provider a colon inside the model is significant, and this
/// is deliberate. OpenRouter routes on colon suffixes (`:free`, `:online`,
/// `:nitro`), so a suffix that is *not* a `key=value` list is kept as part of
/// the model rather than rejected: `openrouter:some/model:free` names the model
/// `some/model:free`. Only a suffix that parses as a parameter list is split
/// off, and only a repeated key within one is an error. `provider_and_model` in
/// `openai_backend` re-splits a spec by the same rule, so the two agree.
pub fn parse_spec(s: &str) -> Result<(String, String, IndexMap<String, String>), String> {
    let (provider, rest) = s
        .split_once(':')
        .ok_or_else(|| format!("invalid client specifier {s:?}: expected 'provider:model'"))?;
    if provider.is_empty() {
        return Err(format!(
            "invalid client specifier {s:?}: provider must not be empty"
        ));
    }
    if rest.is_empty() {
        return Err(format!(
            "invalid client specifier {s:?}: model must not be empty"
        ));
    }
    let mut extra_params = IndexMap::new();
    let model = if provider == "cmd" {
        rest.to_string()
    } else {
        if let Some(m) = params_re().find(rest) {
            let params_str = &m.as_str()[1..];
            for pair in params_str.split(',') {
                if let Some((k, v)) = pair.split_once('=') {
                    if extra_params.contains_key(k) {
                        return Err(format!(
                            "duplicate key {k:?} in client params {params_str:?}"
                        ));
                    }
                    extra_params.insert(k.to_string(), v.to_string());
                }
            }
            rest[..m.start()].to_string()
        } else {
            rest.to_string()
        }
    };
    if model.is_empty() {
        return Err(format!(
            "invalid client specifier {s:?}: model must not be empty"
        ));
    }
    Ok((provider.to_string(), model, extra_params))
}

/// Resolve the API key for `kind`, preferring the provider's env var and
/// falling back to `providers.json`.
fn resolve_api_key(kind: OpenAiProvider) -> Option<String> {
    api_key(kind.api_key_env(), kind.name())
}

/// Build the rig OpenAI-compatible client shared by openai, xai and openrouter.
fn build_openai_client(
    api_key: String,
    base_url: &str,
) -> Result<openai::CompletionsClient, String> {
    openai::Client::builder()
        .api_key(rig_core::client::BearerAuth::from(api_key))
        .base_url(base_url)
        .build()
        .map(|client| client.completions_api())
        .map_err(|e| e.to_string())
}

/// The `allowed_tools` entry of `native_block`, if any.
fn tool_filter(native_block: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    native_block.get("allowed_tools").cloned()
}

/// Copy an insertion-ordered param map into the plain map the backends take.
fn string_map(params: &IndexMap<String, String>) -> HashMap<String, String> {
    params.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Where a missing credential should be added, for the error message.
fn providers_json_path() -> std::path::PathBuf {
    user_config_root().join("providers.json")
}

/// Build an OpenAI or xAI backend from a spec.
///
/// Fails when no credential is available: constructing the backend is the
/// first point at which the harness genuinely needs one, so this is where the
/// operator gets told which env var or config entry to set.
pub fn build_openai_backend(
    kind: OpenAiProvider,
    model: &str,
    native_block: &HashMap<String, Vec<String>>,
    extra_params: &IndexMap<String, String>,
) -> Result<Arc<dyn Backend>, String> {
    let key = resolve_api_key(kind).ok_or_else(|| {
        format!(
            "no API key for provider '{}': set {} or add an entry in {}",
            kind.name(),
            kind.api_key_env(),
            providers_json_path().display(),
        )
    })?;
    let client = build_openai_client(key, kind.base_url())?;
    Ok(Arc::new(OpenAiBackend::new(
        kind,
        client,
        model.to_string(),
        tool_filter(native_block),
        string_map(extra_params),
    )))
}

/// Build an OpenRouter backend from a spec.
///
/// An empty model falls back to `gpt-4o`, matching the provider's own default
/// routing; the spec grammar already rejects an empty model, so this is a
/// belt-and-braces path for programmatic callers.
pub fn build_openrouter_backend(
    model: &str,
    native_block: &HashMap<String, Vec<String>>,
    extra_params: &IndexMap<String, String>,
) -> Result<Arc<dyn Backend>, String> {
    let key = api_key(OPENROUTER_API_KEY_ENV, OPENROUTER_PROVIDER_NAME).ok_or_else(|| {
        format!(
            "no API key for provider '{OPENROUTER_PROVIDER_NAME}': set {OPENROUTER_API_KEY_ENV} or add an entry in {}",
            providers_json_path().display(),
        )
    })?;
    let client = build_openai_client(key, OPENROUTER_BASE_URL)?;
    let model = if model.is_empty() {
        "gpt-4o".to_string()
    } else {
        model.to_string()
    };
    Ok(Arc::new(OpenRouterBackend::new(
        client,
        model,
        tool_filter(native_block),
        string_map(extra_params),
    )))
}

/// A resolved `provider:model` spec plus its lazily-built backend.
///
/// The backend sits behind a mutex because building it is both expensive and
/// fallible, and because `run` takes `&self`: two stages may share one
/// `Client`, and exactly one of them should win the race to build.
#[derive(Clone)]
pub struct Client {
    provider: String,
    model: String,
    extra_params: IndexMap<String, String>,
    native_block: HashMap<String, Vec<String>>,
    inner: Arc<Mutex<Option<Arc<dyn Backend>>>>,
}

impl Client {
    /// Direct constructor. Validates the provider name. `native_block` is the
    /// default allowlist.
    ///
    /// Prefer [`Client::parse`] when the spec came from user input; this
    /// constructor exists for callers that already hold the parts.
    pub fn new(
        provider: String,
        model: String,
        extra_params: IndexMap<String, String>,
    ) -> Result<Self, String> {
        if !is_known_provider(&provider) {
            return Err(format!("unknown provider '{provider}'"));
        }
        Ok(Client {
            provider,
            model,
            extra_params,
            native_block: default_native_block(),
            inner: Arc::new(Mutex::new(None)),
        })
    }

    /// Parse a `provider:model[:k=v,...]` spec.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (provider, model, extra_params) = parse_spec(spec)?;
        Self::new(provider, model, extra_params)
    }

    /// Run one task through this client's backend, building it if needed.
    pub async fn run(&self, params: RunParams) -> Result<CompletedRun, ClientError> {
        let backend = self
            .get_or_build_backend()
            .map_err(|message| ClientError::Runtime { message })?;
        backend.run(params).await
    }

    /// Return the backend for this spec, constructing and memoising it on the
    /// first call.
    ///
    /// Construction happens *under the lock*, and the guard is only ever held
    /// across synchronous work — no `.await` runs while it is live, so an async
    /// caller can never deadlock on it. Building is the expensive, fallible step
    /// (it resolves a credential and mints an HTTP client), and two stages that
    /// share one `Client` must not race to build two: the first to arrive
    /// builds and caches, the second finds the cache already warm.
    ///
    /// The error is a `String` rather than a [`ClientError`] because every
    /// failure here is a configuration mistake (unknown provider, bad command,
    /// missing key) rather than a runtime condition — callers surface it as
    /// [`ClientError::Runtime`] at the [`Client::run`] boundary.
    pub fn get_or_build_backend(&self) -> Result<Arc<dyn Backend>, String> {
        let mut guard = self.inner.lock().expect("client backend mutex poisoned");
        if let Some(ref backend) = *guard {
            return Ok(backend.clone());
        }
        let backend = self.build_backend()?;
        *guard = Some(backend.clone());
        Ok(backend)
    }

    /// Construct the backend this client's spec names, without memoising it.
    ///
    /// The provider match lives here so that [`get_or_build_backend`] is free
    /// to be nothing but the memoisation dance. An unknown provider is caught
    /// by [`Client::new`] long before this runs, so the fallthrough arm is a
    /// guard against a future constructor that forgets to validate.
    ///
    /// [`get_or_build_backend`]: Client::get_or_build_backend
    fn build_backend(&self) -> Result<Arc<dyn Backend>, String> {
        match self.provider.as_str() {
            "cmd" => Ok(Arc::new(CmdBackend::new(&self.model)?)),
            "openai" => build_openai_backend(
                OpenAiProvider::OpenAi,
                &self.model,
                &self.native_block,
                &self.extra_params,
            ),
            "xai" => build_openai_backend(
                OpenAiProvider::Xai,
                &self.model,
                &self.native_block,
                &self.extra_params,
            ),
            "openrouter" => {
                build_openrouter_backend(&self.model, &self.native_block, &self.extra_params)
            }
            other => Err(format!("unknown provider '{other}'")),
        }
    }

    /// Cancel every in-flight process owned by the backend, if it has been
    /// built. A client that never ran anything has nothing to reap.
    pub fn reap_all(&self) {
        log::debug!(
            "Client::reap_all: provider={}, model={}",
            self.provider,
            self.model,
        );
        let guard = self.inner.lock().expect("client backend mutex poisoned");
        if let Some(ref backend) = *guard {
            backend.reap_all();
        }
    }

    /// Cumulative spend reported by the backend, when it can report one and
    /// when it has been built at all.
    pub fn total_cost_usd(&self) -> Option<f64> {
        self.inner
            .lock()
            .expect("client backend mutex poisoned")
            .as_ref()
            .and_then(|b| b.total_cost_usd())
    }

    /// The model half of the spec, after any parameter suffix was stripped.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The provider half of the spec.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The trailing `key=value` parameters, in spec order.
    pub fn extra_params(&self) -> &IndexMap<String, String> {
        &self.extra_params
    }
}

impl std::fmt::Display for Client {
    /// Renders the canonical spec, so that `to_string().parse()` round-trips.
    ///
    /// Parameter order is preserved, which is what makes a spec written back
    /// out by the harness byte-identical to the one the operator typed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider, self.model)?;
        if !self.extra_params.is_empty() {
            let params: Vec<String> = self
                .extra_params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            write!(f, ":{}", params.join(","))?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Client {
    /// The backend handle is not `Debug`, so the derived form would be
    /// misleading; show only what identifies the client.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("extra_params", &self.extra_params)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Client {
    /// Two clients are the same when they would build the same backend.
    ///
    /// `native_block` and `inner` are excluded: the former is always the
    /// default allowlist, and the latter is build state, not identity.
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.extra_params == other.extra_params
    }
}

impl Eq for Client {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_basic() {
        let (p, m, e) = parse_spec("openai:gpt-4o").unwrap();
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
        assert!(e.is_empty());
    }

    #[test]
    fn parse_spec_with_params() {
        let (p, m, e) = parse_spec("openrouter:deepseek/deepseek-v4-pro:reasoning=high").unwrap();
        assert_eq!(p, "openrouter");
        assert_eq!(m, "deepseek/deepseek-v4-pro");
        assert_eq!(e.get("reasoning").unwrap(), "high");
    }

    #[test]
    fn parse_spec_cmd_ignores_params() {
        let (p, m, e) = parse_spec("cmd:echo hello:world=foo").unwrap();
        assert_eq!(p, "cmd");
        assert_eq!(m, "echo hello:world=foo");
        assert!(e.is_empty());
    }

    #[test]
    fn parse_spec_rejects_malformed() {
        for (bad, expected) in [
            ("no-colon", "expected 'provider:model'"),
            (":model", "provider must not be empty"),
            ("provider:", "model must not be empty"),
            ("openai:gpt-4:reasoning=high,reasoning=low", "duplicate key"),
        ] {
            let err = parse_spec(bad).unwrap_err();
            assert!(err.contains(expected), "{bad:?} -> {err:?}");
        }
    }

    #[test]
    fn parse_spec_multiple_params() {
        let (_, _, e) = parse_spec(
            "openrouter:deepseek/deepseek-v4-pro:reasoning=high,thinking=deepseek,foo=bar",
        )
        .unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e.get("reasoning").unwrap(), "high");
        assert_eq!(e.get("thinking").unwrap(), "deepseek");
        assert_eq!(e.get("foo").unwrap(), "bar");
    }

    #[test]
    fn parse_spec_roundtrips() {
        for spec in [
            "openai:gpt-4o-mini",
            "xai:grok-4",
            "openrouter:openai/gpt-4o",
            "openrouter:deepseek/deepseek-v4-pro:reasoning=high",
            "openrouter:deepseek/deepseek-v4-pro:reasoning=high,thinking=deepseek",
            "xai:grok-4:reasoning=low,foo=bar",
        ] {
            let (p, m, e) = parse_spec(spec).unwrap();
            let mut s = format!("{p}:{m}");
            if !e.is_empty() {
                let params: Vec<String> = e.iter().map(|(k, v)| format!("{k}={v}")).collect();
                s.push(':');
                s.push_str(&params.join(","));
            }
            assert_eq!(s, spec);
        }
    }

    #[test]
    fn default_allowlist_has_six_tools() {
        let block = default_native_block();
        let tools = block.get("allowed_tools").unwrap();
        assert_eq!(tools.len(), 6);
        for name in ["Bash", "Edit", "Read", "Write", "Grep", "Glob"] {
            assert!(tools.contains(&name.to_string()), "missing {name}");
        }
    }

    #[test]
    fn unknown_provider_rejected() {
        assert!(Client::new("fake".into(), "m".into(), IndexMap::new()).is_err());
        assert!(Client::parse("fake:m").is_err());
    }

    #[test]
    fn parse_spec_keeps_non_parameter_colon_suffixes() {
        // A colon suffix that is not a `key=value` list belongs to the model.
        // OpenRouter routes on such suffixes, so rejecting them would break
        // `openrouter:some/model:free`; the rule is intentional, not laxness.
        for (spec, provider, model) in [
            ("openai:gpt-4:bad", "openai", "gpt-4:bad"),
            (
                "openrouter:anthropic/claude-sonnet-4:free",
                "openrouter",
                "anthropic/claude-sonnet-4:free",
            ),
            (
                "openrouter:google/gemini-2.5-flash:online",
                "openrouter",
                "google/gemini-2.5-flash:online",
            ),
            // A list whose last segment lacks `=` cannot be a params list.
            (
                "openai:gpt-4:reasoning=high,bad",
                "openai",
                "gpt-4:reasoning=high,bad",
            ),
        ] {
            let (p, m, e) = parse_spec(spec).unwrap();
            assert_eq!(p, provider, "{spec:?}");
            assert_eq!(m, model, "{spec:?}");
            assert!(e.is_empty(), "{spec:?} -> {e:?}");
        }
    }

    #[test]
    fn client_display_and_equality() {
        let a = Client::parse("xai:grok-4:reasoning=low").unwrap();
        assert_eq!(a.to_string(), "xai:grok-4:reasoning=low");

        let b = Client::parse("xai:grok-4:reasoning=low").unwrap();
        assert_eq!(a, b);

        let c = Client::parse("xai:grok-5:reasoning=low").unwrap();
        assert_ne!(a, c);

        let plain = Client::parse("openai:gpt-4o-mini").unwrap();
        assert_eq!(plain.to_string(), "openai:gpt-4o-mini");
        assert_eq!(plain.provider(), "openai");
        assert_eq!(plain.model(), "gpt-4o-mini");
        assert!(plain.extra_params().is_empty());
    }

    #[test]
    fn known_providers_are_the_four_backends() {
        for p in ["openai", "xai", "openrouter", "cmd"] {
            assert!(is_known_provider(p), "{p}");
            assert!(Client::parse(&format!("{p}:model")).is_ok(), "{p}");
        }
        assert!(!is_known_provider("anthropic"));
    }

    #[test]
    fn backend_is_built_once_under_concurrency() {
        // `cmd` is the one provider whose backend needs no credential, so it is
        // the only one a unit test can drive end to end. Every racer must come
        // back with the *same* backend, which is exactly the memoisation a
        // parallel fan-out relies on.
        let client = std::sync::Arc::new(Client::parse("cmd:true").unwrap());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let client = std::sync::Arc::clone(&client);
                std::thread::spawn(move || client.get_or_build_backend().unwrap())
            })
            .collect();
        let backends: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for backend in &backends[1..] {
            assert!(std::sync::Arc::ptr_eq(&backends[0], backend));
        }
    }
}
