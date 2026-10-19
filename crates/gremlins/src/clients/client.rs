//! Client specifiers and lazy backend construction.
//!
//! The model specifier grammar lives in [`crate::model_uri::ModelURI`] (re-exported
//! from this module for backwards compatibility). This module owns the set of
//! providers the harness can actually construct ([`is_known_provider`]), and
//! [`Client`], which turns a [`ModelURI`] into a live [`Backend`] on first use.
//!
//! Backends are built lazily and memoised behind [`Client::get_or_build_backend`]
//! so that parsing a definition never requires credentials: whichever process
//! eventually runs a stage is the one that pays for resolving the API key.

pub use crate::model_uri::ModelURI;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::backends;
use crate::clients::interactive::InteractiveSession;
use crate::clients::protocol::CompletedRun;

/// The seven tools every native agent may call.
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
            "OutlineFile".to_string(),
        ],
    )])
}

/// True when `provider` names a provider in the registry.
///
/// Callers use this to reject a bad spec early, before any work is queued.
pub fn is_known_provider(provider: &str) -> bool {
    backends::registry()
        .iter()
        .any(|(name, _)| *name == provider)
}

fn backend_pool() -> &'static Mutex<HashMap<String, Arc<dyn Backend>>> {
    static POOL: OnceLock<Mutex<HashMap<String, Arc<dyn Backend>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A resolved model reference plus the harness-side runtime handle.
///
/// Backend construction is delegated to a module-level pool so that every
/// [`Client`] with the same [`ModelURI`] shares one [`Backend`] (and therefore
/// one HTTP connection pool). [`Display`](Client::fmt) renders through the
/// URI, so the stub and canonical spellings of the same model share one pool
/// key.
#[derive(Clone)]
pub struct Client {
    uri: ModelURI,
    native_block: HashMap<String, Vec<String>>,
}

impl Client {
    /// Direct constructor. Validates the reference shape through
    /// [`ModelURI::from_parts`] and the provider name through
    /// [`is_known_provider`]. `native_block` is the default allowlist.
    ///
    /// Prefer [`Client::parse`] when the spec came from user input; this
    /// constructor exists for callers that already hold the parts.
    pub fn new(
        provider: String,
        model: String,
        extra_params: IndexMap<String, String>,
    ) -> Result<Self, String> {
        Self::from_uri(ModelURI::from_parts(provider, model, extra_params)?)
    }

    /// Wrap an already-parsed [`ModelURI`], validating its provider name.
    pub fn from_uri(uri: ModelURI) -> Result<Self, String> {
        if !is_known_provider(uri.provider()) {
            return Err(format!("unknown provider '{}'", uri.provider()));
        }
        Ok(Client {
            uri,
            native_block: default_native_block(),
        })
    }

    /// Parse a model spec in either the `model://provider/model[?params]`
    /// canonical form or the `provider/model[?params]` stub form.
    pub fn parse(spec: &str) -> Result<Self, String> {
        Self::from_uri(ModelURI::parse(spec)?)
    }

    /// Run one task through this client's backend, building it if needed.
    pub async fn run(
        &self,
        params: RunParams,
        interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError> {
        let backend = self
            .get_or_build_backend()
            .map_err(|message| ClientError::Runtime { message })?;
        backend.run(params, interactive).await
    }

    /// Return the backend for this spec, looking it up in the module-level
    /// pool or constructing it on first use.
    ///
    /// The pool lock is never held across an `.await`, so an async caller
    /// cannot deadlock on it.  Building is the expensive, fallible step (it
    /// resolves a credential and mints an HTTP client).
    ///
    /// The error is a `String` rather than a [`ClientError`] because every
    /// failure here is a configuration mistake (unknown provider, bad command,
    /// missing key) rather than a runtime condition — callers surface it as
    /// [`ClientError::Runtime`] at the [`Client::run`] boundary.
    pub fn get_or_build_backend(&self) -> Result<Arc<dyn Backend>, String> {
        let spec = self.to_string();

        let mut pool = backend_pool().lock().expect("backend pool poisoned");
        if let Some(backend) = pool.get(&spec) {
            log::debug!("Backend cache hit for spec {spec}");
            return Ok(backend.clone());
        }

        // Build while holding the lock so concurrent callers for the same spec
        // cannot race to construct duplicate backends.  build_backend() is
        // synchronous (no .await), so the lock is never held across an async
        // yield point.
        let backend = self.build_backend()?;

        let base_url = match self.uri.provider() {
            "openai" => "https://api.openai.com/v1",
            "xai" => "https://api.x.ai/v1",
            "openrouter" => "https://openrouter.ai/api/v1",
            "cmd" => "(shell)",
            _ => "(unknown)",
        };
        log::info!(
            "Built new backend: provider={}, model={}, base_url={base_url}",
            self.uri.provider(),
            self.uri.model(),
        );

        pool.insert(spec, backend.clone());
        Ok(backend)
    }

    /// Construct the backend this client's spec names, without memoising it.
    ///
    /// Looks up the provider name in [`backends::registry`] and delegates to
    /// the registered `build` function.  An unknown provider is caught by
    /// [`Client::from_uri`] long before this runs, so the fallthrough arm is a
    /// guard against a future constructor that forgets to validate.
    fn build_backend(&self) -> Result<Arc<dyn Backend>, String> {
        for (name, build) in backends::registry() {
            if name == self.uri.provider() {
                return build(
                    self.uri.model(),
                    &self.native_block,
                    self.uri.extra_params(),
                );
            }
        }
        Err(format!("unknown provider '{}'", self.uri.provider()))
    }

    /// Cancel every in-flight process owned by the backend, if it has been
    /// built. A client that never ran anything has nothing to reap.
    pub fn reap_all(&self, gremlin_id: &str) {
        log::debug!(
            "Client::reap_all: gremlin_id={gremlin_id}, provider={}, model={}",
            self.uri.provider(),
            self.uri.model(),
        );
        let spec = self.to_string();
        let pool = backend_pool().lock().expect("backend pool poisoned");
        if let Some(backend) = pool.get(&spec) {
            backend.reap_all(gremlin_id);
        }
    }

    /// Cumulative spend reported by the backend, when it can report one and
    /// when it has been built at all.
    pub fn total_cost_usd(&self) -> Option<f64> {
        let spec = self.to_string();
        let pool = backend_pool().lock().expect("backend pool poisoned");
        pool.get(&spec).and_then(|b| b.total_cost_usd())
    }

    /// The model half of the spec, after any parameter suffix was stripped.
    pub fn model(&self) -> &str {
        self.uri.model()
    }

    /// The provider half of the spec.
    pub fn provider(&self) -> &str {
        self.uri.provider()
    }

    /// The trailing `key=value` parameters, in spec order.
    pub fn extra_params(&self) -> &IndexMap<String, String> {
        self.uri.extra_params()
    }

    /// Whether the effective allowlist permits `name`.
    ///
    /// The `cmd` backend has no native tool layer (its model is the shell
    /// command itself), so even though its `native_block` looks like the
    /// native backend's, none of the native tools are present.
    pub fn has_tool(&self, name: &str) -> bool {
        if self.uri.provider() == "cmd" {
            return false;
        }
        self.native_block
            .get("allowed_tools")
            .map(|tools| tools.iter().any(|tool| tool == name))
            .unwrap_or(false)
    }
}

impl std::fmt::Display for Client {
    /// Renders through [`ModelURI`], so `to_string().parse()` round-trips and
    /// stub/canonical inputs share a single backend-pool key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.uri)
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("uri", &self.uri)
            .field("native_block", &self.native_block)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Client {
    /// Two clients are the same when they would build the same backend.
    ///
    /// `native_block` is excluded: it is always the default allowlist.
    fn eq(&self, other: &Self) -> bool {
        self.uri == other.uri
    }
}

impl Eq for Client {}

/// Validate a concrete `provider/model[?k=v]` string for use as a
/// `task-models` entry.
///
/// This is the cheap, settings-free check run at definition load. It parses
/// the spec with [`ModelURI::parse`], rejects the `cmd` provider (Task
/// overrides must be a model backend, not a shell command), and rejects any
/// provider absent from [`backends::registry`]. It deliberately performs no
/// backend or credential resolution so loading a definition stays lazy.
pub(crate) fn validate_task_model_spec(spec: &str) -> Result<ModelURI, String> {
    let uri = ModelURI::parse(spec)?;
    if uri.provider() == "cmd" {
        return Err(format!(
            "task-model spec {spec:?} names the command backend, which cannot run task overrides"
        ));
    }
    if !is_known_provider(uri.provider()) {
        return Err(format!(
            "task-model spec {spec:?} names unknown provider '{}'",
            uri.provider()
        ));
    }
    Ok(uri)
}

/// Provider-agnostic model resolver for `task-models` entries.
///
/// Parses `spec` with [`Client`], builds or looks up the provider's backend in
/// the shared lazy pool, and delegates to [`Backend::make_model`] and
/// [`Backend::resolved_params`]. The returned [`TaskModel`] carries the target
/// provider's own request options, so a cross-provider override never inherits
/// the parent model's `temperature`/`max_tokens`/`extra`. Every failure —
/// malformed spec, unknown provider, `cmd`, and backend-build failure — is
/// returned as an `Err` with a reason instead of a silent fallback.
pub(crate) fn resolve_model(spec: &str) -> Result<super::task::TaskModel, String> {
    let uri = validate_task_model_spec(spec)?;
    let client = Client::from_uri(uri)?;
    let backend = client.get_or_build_backend()?;
    let model = backend.make_model(spec).ok_or_else(|| {
        format!(
            "backend for provider '{}' could not build a model for {spec:?}",
            client.provider()
        )
    })?;
    let resolved = backend.resolved_params().ok_or_else(|| {
        format!(
            "backend for provider '{}' could not resolve request options for {spec:?}",
            client.provider()
        )
    })?;
    Ok(super::task::TaskModel {
        model,
        temperature: resolved.temperature,
        max_tokens: resolved.max_tokens,
        extra: resolved.additional_value(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allowlist_has_seven_tools() {
        let block = default_native_block();
        let tools = block.get("allowed_tools").unwrap();
        assert_eq!(tools.len(), 7);
        for name in [
            "Bash",
            "Edit",
            "Read",
            "Write",
            "Grep",
            "Glob",
            "OutlineFile",
        ] {
            assert!(tools.contains(&name.to_string()), "missing {name}");
        }
    }

    #[test]
    fn unknown_provider_rejected() {
        assert!(Client::new("fake".into(), "m".into(), IndexMap::new()).is_err());
        assert!(Client::parse("fake/m").is_err());
        assert!(Client::from_uri(ModelURI::parse("fake/m").unwrap()).is_err());
    }

    #[test]
    fn resolve_model_returns_err_for_malformed_specs() {
        for spec in ["no-slash", "/model", "openai/", "model://openai"] {
            assert!(
                resolve_model(spec).is_err(),
                "malformed spec {spec:?} must not resolve"
            );
        }
    }

    #[test]
    fn resolve_model_returns_err_for_unknown_provider() {
        assert!(resolve_model("fake/model").is_err());
    }

    #[test]
    fn resolve_model_returns_err_for_cmd() {
        assert!(resolve_model("cmd/true").is_err());
    }

    #[test]
    fn validate_task_model_spec_rejects_malformed_unknown_and_cmd() {
        assert!(validate_task_model_spec("no-slash").is_err());
        assert!(validate_task_model_spec("fake/model").is_err());
        assert!(validate_task_model_spec("cmd/true").is_err());
    }

    #[test]
    fn validate_task_model_spec_accepts_registered_providers() {
        for spec in [
            "openai/gpt-4o",
            "model://anthropic/claude-sonnet-4-20250514",
            "openrouter/deepseek/deepseek-v4-pro?reasoning=high",
        ] {
            let uri = validate_task_model_spec(spec).unwrap();
            assert_eq!(uri, ModelURI::parse(spec).unwrap(), "{spec:?}");
        }
    }

    #[test]
    fn client_new_revalidates_shape_invariants() {
        assert!(Client::new("openai".into(), String::new(), IndexMap::new()).is_err());
        assert!(Client::new("openai".into(), "gpt?x=1".into(), IndexMap::new()).is_err());
        assert!(Client::new(String::new(), "gpt".into(), IndexMap::new()).is_err());
        assert!(Client::new("openai/x".into(), "gpt".into(), IndexMap::new()).is_err());

        let err = Client::new(
            "cmd".into(),
            "true".into(),
            IndexMap::from([("x".to_string(), "1".to_string())]),
        )
        .unwrap_err();
        assert!(
            err.contains("command backend does not accept params"),
            "{err:?}"
        );

        let err = Client::new(
            "openai".into(),
            "gpt-4o".into(),
            IndexMap::from([("bad-key".to_string(), "1".to_string())]),
        )
        .unwrap_err();
        assert!(err.contains("invalid client params"), "{err:?}");

        // `profile` is rejected by `ModelURI::from_parts` before
        // `Client::new` ever runs the provider allowlist check.
        let err = Client::new("profile".into(), "foo".into(), IndexMap::new()).unwrap_err();
        assert!(err.contains("reserved profile-reference prefix"), "{err:?}");

        assert!(Client::new("cmd".into(), "echo hi?y=1".into(), IndexMap::new()).is_ok());
    }

    #[test]
    fn client_display_and_equality() {
        let a = Client::parse("xai/grok-4?reasoning=low").unwrap();
        assert_eq!(a.to_string(), "model://xai/grok-4?reasoning=low");
        assert_eq!(a.provider(), "xai");
        assert_eq!(a.model(), "grok-4");
        assert_eq!(a.extra_params().get("reasoning").unwrap(), "low");

        let b = Client::parse("model://xai/grok-4?reasoning=low").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), b.to_string());

        let c = Client::parse("xai/grok-5?reasoning=low").unwrap();
        assert_ne!(a, c);

        let plain = Client::parse("openai/gpt-4o-mini").unwrap();
        assert_eq!(plain.to_string(), "model://openai/gpt-4o-mini");
        assert_eq!(plain.provider(), "openai");
        assert_eq!(plain.model(), "gpt-4o-mini");
        assert!(plain.extra_params().is_empty());
    }

    #[test]
    fn known_providers_are_the_seven_backends() {
        for p in [
            "copilot",
            "openai",
            "xai",
            "openrouter",
            "cmd",
            "azure-openai",
            "anthropic",
        ] {
            assert!(is_known_provider(p), "{p}");
            assert!(Client::parse(&format!("{p}/model")).is_ok(), "{p}");
        }
    }

    #[test]
    fn backend_is_built_once_under_concurrency() {
        // `cmd` is the one provider whose backend needs no credential, so it is
        // the only one a unit test can drive end to end. Every racer must come
        // back with the *same* backend, which is exactly the memoisation a
        // parallel fan-out relies on.
        let client = std::sync::Arc::new(Client::parse("cmd/true").unwrap());
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
