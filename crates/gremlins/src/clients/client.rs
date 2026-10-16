//! Client specifiers and lazy backend construction.
//!
//! A definition names its model as a single string using the URI grammar:
//! `model://provider/model[?key=value,...]`, or the scheme-omitted stub
//! `provider/model[?key=value,...]`. This module owns that grammar
//! ([`ModelURI::parse`]), the set of providers the harness can actually
//! construct ([`is_known_provider`]), and [`Client`], which turns a
//! [`ModelURI`] into a live [`Backend`] on first use.
//!
//! Backends are built lazily and memoised behind [`Client::get_or_build_backend`]
//! so that parsing a definition never requires credentials: whichever process
//! eventually runs a stage is the one that pays for resolving the API key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::backends;
use crate::clients::interactive::InteractiveSession;
use crate::clients::protocol::CompletedRun;

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

/// True when `provider` names a provider in the registry.
///
/// Callers use this to reject a bad spec early, before any work is queued.
pub fn is_known_provider(provider: &str) -> bool {
    backends::registry()
        .iter()
        .any(|(name, _)| *name == provider)
}

/// A trailing `k=v,k=v,...` parameter list, or a signal that the suffix is
/// not a list at all.
enum ParamsList {
    NotAList,
    Duplicate(String),
    Ok(IndexMap<String, String>),
}

/// True when `k` is a valid client-param key (`[a-zA-Z_][a-zA-Z0-9_]*`).
fn is_valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a trailing `k=v,k=v,...` list.
///
/// Commas inside balanced JSON braces/brackets or double-quoted strings do not
/// split parameters, so a value such as `cache_control={"type":"ephemeral","ttl":"1h"}`
/// keeps its embedded commas. Returns [`ParamsList::NotAList`] when `s` is not
/// a well-formed list, [`ParamsList::Duplicate`] on a repeated key, and
/// [`ParamsList::Ok`] otherwise.
fn parse_params_list(s: &str) -> ParamsList {
    let mut params = IndexMap::new();
    let bytes = s.as_bytes();
    let mut seg_start = 0usize;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    let seg = &s[seg_start..i];
                    let Some((k, v)) = seg.split_once('=') else {
                        return ParamsList::NotAList;
                    };
                    if !is_valid_key(k) || v.is_empty() {
                        return ParamsList::NotAList;
                    }
                    if params.contains_key(k) {
                        return ParamsList::Duplicate(k.to_string());
                    }
                    params.insert(k.to_string(), v.to_string());
                    seg_start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }

    let seg = &s[seg_start..];
    let Some((k, v)) = seg.split_once('=') else {
        return ParamsList::NotAList;
    };
    if !is_valid_key(k) || v.is_empty() {
        return ParamsList::NotAList;
    }
    if params.contains_key(k) {
        return ParamsList::Duplicate(k.to_string());
    }
    params.insert(k.to_string(), v.to_string());
    ParamsList::Ok(params)
}

/// The normalized model reference.
///
/// A [`ModelURI`] owns its provider, model, and extra-param map. It is the
/// canonical parse type for the client specifier grammar; [`Client`] adds the
/// provider allowlist and the lazy backend pool on top of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelURI {
    provider: String,
    model: String,
    extra_params: IndexMap<String, String>,
}

impl ModelURI {
    /// Parse a model URI in either spelling.
    ///
    /// Accepts the canonical `model://provider/model[?params]` form and the
    /// scheme-omitted stub `provider/model[?params]`.
    ///
    /// The provider is always the first path segment. Everything after that
    /// first `provider/` — up to the first `?`, or the end of the string — is
    /// the model, verbatim: slashes and colons are preserved and never
    /// interpreted. The portion after the first `?` is parsed with
    /// [`parse_params_list`].
    ///
    /// The provider name `profile` is reserved for load-time model-profile
    /// references (`profile/<name>`). It is rejected here as a provider so a
    /// profile reference can never be misparsed as a literal
    /// `provider/model` spec.
    ///
    /// `cmd` is stub-only and verbatim: everything after `cmd/` is the shell
    /// command, including any `?`, `&`, or `/`. The command backend never has
    /// params, and `model://cmd/...` is rejected.
    ///
    /// The `model:` single-slash spelling is rejected.
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(rest) = s.strip_prefix("model://") {
            Self::parse_canonical(rest, s)
        } else if s.starts_with("model:") {
            Err(format!(
                "invalid client specifier {s:?}: 'model:' is not supported; use 'model://provider/model' or 'provider/model'"
            ))
        } else {
            Self::parse_stub(s)
        }
    }

    fn parse_canonical(rest: &str, original: &str) -> Result<Self, String> {
        let (provider, rest) = rest.split_once('/').ok_or_else(|| {
            format!("invalid client specifier {original:?}: expected 'model://provider/model'")
        })?;
        if provider.is_empty() {
            return Err(format!(
                "invalid client specifier {original:?}: provider must not be empty"
            ));
        }
        if provider == "cmd" {
            return Err(format!(
                "invalid client specifier {original:?}: the command backend is stub-only; use 'cmd/<command>'"
            ));
        }
        let (model, params) = Self::split_model_params(rest, original)?;
        let extra_params = Self::parse_params(params, original)?;
        Self::from_parts(provider.to_string(), model.to_string(), extra_params)
    }

    fn parse_stub(s: &str) -> Result<Self, String> {
        let (provider, rest) = s.split_once('/').ok_or_else(|| {
            format!(
                "invalid client specifier {s:?}: expected 'provider/model' or 'model://provider/model'"
            )
        })?;
        if provider.is_empty() {
            return Err(format!(
                "invalid client specifier {s:?}: provider must not be empty"
            ));
        }
        if provider == "cmd" {
            if rest.is_empty() {
                return Err(format!(
                    "invalid client specifier {s:?}: model must not be empty"
                ));
            }
            // `cmd` never parses params; the entire remainder is the shell
            // command, including any `?`, `&`, or `/`.
            return Self::from_parts(provider.to_string(), rest.to_string(), IndexMap::new());
        }
        let (model, params) = Self::split_model_params(rest, s)?;
        let extra_params = Self::parse_params(params, s)?;
        Self::from_parts(provider.to_string(), model.to_string(), extra_params)
    }

    fn split_model_params<'a>(
        rest: &'a str,
        original: &str,
    ) -> Result<(&'a str, Option<&'a str>), String> {
        let (model, params) = match rest.split_once('?') {
            Some((model, params)) => (model, Some(params)),
            None => (rest, None),
        };
        if model.is_empty() {
            return Err(format!(
                "invalid client specifier {original:?}: model must not be empty"
            ));
        }
        Ok((model, params))
    }

    fn parse_params(
        params: Option<&str>,
        original: &str,
    ) -> Result<IndexMap<String, String>, String> {
        let Some(params) = params else {
            return Ok(IndexMap::new());
        };
        match parse_params_list(params) {
            ParamsList::Ok(extra_params) => Ok(extra_params),
            ParamsList::Duplicate(k) => Err(format!(
                "duplicate key {k:?} in client params {params:?}"
            )),
            ParamsList::NotAList => Err(format!(
                "invalid client params {params:?}: expected 'key=value,...' (full specifier {original:?})"
            )),
        }
    }

    /// Validate raw parts and build a normalized reference.
    ///
    /// This is the shape-only invariant chokepoint shared by the parsers and
    /// [`Client::new`]: the raw `(provider, model, params)` triple is checked
    /// against the same grammar rules as a parsed spec, without going through
    /// a string round-trip.
    fn from_parts(
        provider: String,
        model: String,
        extra_params: IndexMap<String, String>,
    ) -> Result<Self, String> {
        let full = format!("{provider}/{model}");
        if provider.is_empty() {
            return Err(format!(
                "invalid client specifier {full:?}: provider must not be empty"
            ));
        }
        if provider.contains('/') {
            return Err(format!(
                "invalid client specifier {full:?}: provider must not contain '/'"
            ));
        }
        if provider == "profile" {
            return Err(format!(
                "invalid client specifier {full:?}: 'profile' is a reserved profile-reference prefix and cannot be used as a provider"
            ));
        }
        if provider == "cmd" {
            if model.is_empty() {
                return Err(format!(
                    "invalid client specifier {full:?}: model must not be empty"
                ));
            }
            if !extra_params.is_empty() {
                return Err(format!(
                    "invalid client specifier {full:?}: the command backend does not accept params"
                ));
            }
            return Ok(Self {
                provider,
                model,
                extra_params,
            });
        }
        if model.is_empty() {
            return Err(format!(
                "invalid client specifier {full:?}: model must not be empty"
            ));
        }
        if model.contains('?') {
            return Err(format!(
                "invalid client specifier {full:?}: '?' is the params separator and cannot appear in the model"
            ));
        }
        if !extra_params.is_empty() {
            let rendered = render_params(&extra_params);
            match parse_params_list(&rendered) {
                ParamsList::Ok(reparsed) if reparsed == extra_params => {}
                _ => {
                    return Err(format!(
                        "invalid client params {rendered:?}: expected 'key=value,...' (full specifier {full:?})"
                    ));
                }
            }
        }
        Ok(Self {
            provider,
            model,
            extra_params,
        })
    }

    /// The provider half of the reference.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The model half of the reference, with any params stripped.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The trailing `key=value` parameters, in spec order.
    pub fn extra_params(&self) -> &IndexMap<String, String> {
        &self.extra_params
    }
}

impl std::fmt::Display for ModelURI {
    /// Renders the canonical form: `model://provider/model[?params]`, or
    /// `cmd/<command>` for the command backend.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.provider == "cmd" {
            return write!(f, "cmd/{}", self.model);
        }
        write!(f, "model://{}/{}", self.provider, self.model)?;
        if !self.extra_params.is_empty() {
            write!(f, "?{}", render_params(&self.extra_params))?;
        }
        Ok(())
    }
}

fn backend_pool() -> &'static Mutex<HashMap<String, Arc<dyn Backend>>> {
    static POOL: OnceLock<Mutex<HashMap<String, Arc<dyn Backend>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Render an extra-param map as the `k=v,k=v,...` list used after `?`.
fn render_params(params: &IndexMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
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

/// Provider-agnostic model resolver for `task-models` entries.
///
/// Parses `spec` with [`Client`], builds or looks up the provider's backend in
/// the shared lazy pool, and delegates to [`Backend::make_model`] and
/// [`Backend::resolved_params`]. The returned [`TaskModel`] carries the target
/// provider's own request options, so a cross-provider override never inherits
/// the parent model's `temperature`/`max_tokens`/`extra`. Returns `None` for
/// malformed specs, unknown providers, the `cmd` backend, and backend-build
/// failures — callers fall back to the parent model without surfacing an error.
pub(crate) fn resolve_model(spec: &str) -> Option<super::task::TaskModel> {
    let client = Client::parse(spec).ok()?;
    if client.provider() == "cmd" {
        return None;
    }
    let backend = client.get_or_build_backend().ok()?;
    let model = backend.make_model(spec)?;
    let resolved = backend.resolved_params()?;
    Some(super::task::TaskModel {
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
    fn model_uri_parse_stub_basic() {
        let uri = ModelURI::parse("openai/gpt-4o").unwrap();
        assert_eq!(uri.provider(), "openai");
        assert_eq!(uri.model(), "gpt-4o");
        assert!(uri.extra_params().is_empty());
    }

    #[test]
    fn model_uri_parse_canonical_basic() {
        let uri = ModelURI::parse("model://openai/gpt-4o").unwrap();
        assert_eq!(uri.provider(), "openai");
        assert_eq!(uri.model(), "gpt-4o");
        assert!(uri.extra_params().is_empty());
    }

    #[test]
    fn model_uri_parse_with_params() {
        let uri = ModelURI::parse("openrouter/deepseek/deepseek-v4-pro?reasoning=high").unwrap();
        assert_eq!(uri.provider(), "openrouter");
        assert_eq!(uri.model(), "deepseek/deepseek-v4-pro");
        assert_eq!(uri.extra_params().get("reasoning").unwrap(), "high");
    }

    #[test]
    fn model_uri_parse_canonical_with_params() {
        let uri =
            ModelURI::parse("model://openrouter/deepseek/deepseek-v4-pro?reasoning=high").unwrap();
        assert_eq!(uri.provider(), "openrouter");
        assert_eq!(uri.model(), "deepseek/deepseek-v4-pro");
        assert_eq!(uri.extra_params().get("reasoning").unwrap(), "high");
    }

    #[test]
    fn model_uri_parse_cmd_keeps_rest_verbatim() {
        let uri = ModelURI::parse("cmd/echo hello?world=foo&bar/baz").unwrap();
        assert_eq!(uri.provider(), "cmd");
        assert_eq!(uri.model(), "echo hello?world=foo&bar/baz");
        assert!(uri.extra_params().is_empty());
    }

    #[test]
    fn model_uri_parse_cmd_keeps_colons() {
        let uri = ModelURI::parse("cmd/echo hello:world=foo").unwrap();
        assert_eq!(uri.provider(), "cmd");
        assert_eq!(uri.model(), "echo hello:world=foo");
        assert!(uri.extra_params().is_empty());
    }

    const LEGACY_COLON_SPEC: &str = concat!("openai", ":", "gpt-4o");

    #[test]
    fn model_uri_parse_rejects_malformed() {
        for (bad, expected) in [
            ("no-slash", "expected 'provider/model'"),
            (LEGACY_COLON_SPEC, "expected 'provider/model'"),
            ("/model", "provider must not be empty"),
            ("openai/", "model must not be empty"),
            ("model://openai", "expected 'model://provider/model'"),
            ("model://openai/", "model must not be empty"),
            ("model:gpt-4o", "'model:' is not supported"),
            (
                "openai/gpt-4o?reasoning=high,reasoning=low",
                "duplicate key",
            ),
            ("openai/gpt-4o?bad-suffix", "invalid client params"),
            ("openai/gpt-4o?", "invalid client params"),
            ("model://cmd/true", "command backend is stub-only"),
        ] {
            let err = ModelURI::parse(bad).unwrap_err();
            assert!(err.contains(expected), "{bad:?} -> {err:?}");
        }
    }

    #[test]
    fn model_uri_parse_multiple_params() {
        let uri = ModelURI::parse(
            "openrouter/deepseek/deepseek-v4-pro?reasoning=high,thinking=deepseek,foo=bar",
        )
        .unwrap();
        assert_eq!(uri.provider(), "openrouter");
        assert_eq!(uri.model(), "deepseek/deepseek-v4-pro");
        assert_eq!(uri.extra_params().len(), 3);
        assert_eq!(uri.extra_params().get("reasoning").unwrap(), "high");
        assert_eq!(uri.extra_params().get("thinking").unwrap(), "deepseek");
        assert_eq!(uri.extra_params().get("foo").unwrap(), "bar");
    }

    #[test]
    fn model_uri_parse_json_value_with_commas() {
        let uri = ModelURI::parse(
            r#"anthropic/claude-sonnet-4?cache_control={"type":"ephemeral","ttl":"1h"},max_tokens=4096"#,
        )
        .unwrap();
        assert_eq!(uri.provider(), "anthropic");
        assert_eq!(uri.model(), "claude-sonnet-4");
        assert_eq!(
            uri.extra_params().get("cache_control").unwrap(),
            r#"{"type":"ephemeral","ttl":"1h"}"#
        );
        assert_eq!(uri.extra_params().get("max_tokens").unwrap(), "4096");
    }

    #[test]
    fn model_uri_parse_json_array_value_with_commas() {
        let uri = ModelURI::parse("openai/gpt-4o?stop=[\"END\",\"STOP\"],foo=bar").unwrap();
        assert_eq!(
            uri.extra_params().get("stop").unwrap(),
            "[\"END\",\"STOP\"]"
        );
        assert_eq!(uri.extra_params().get("foo").unwrap(), "bar");
    }

    #[test]
    fn model_uri_display_roundtrips() {
        for spec in [
            "openai/gpt-4o-mini",
            "model://openai/gpt-4o-mini",
            "xai/grok-4",
            "openrouter/openai/gpt-4o",
            "openrouter/deepseek/deepseek-v4-pro?reasoning=high",
            "openrouter/deepseek/deepseek-v4-pro?reasoning=high,thinking=deepseek",
            "xai/grok-4?reasoning=low,foo=bar",
            "model://xai/grok-4?reasoning=low,foo=bar",
            "openrouter/some/model:free",
            "model://openrouter/some/model:free",
            "openrouter/some/model:free?reasoning=high",
        ] {
            let uri = ModelURI::parse(spec).unwrap();
            let rendered = uri.to_string();
            assert_eq!(ModelURI::parse(&rendered).unwrap(), uri, "{spec:?}");
            assert_eq!(
                rendered,
                format!("model://{}/{}", uri.provider(), uri.model())
                    + params_suffix(uri.extra_params()).as_str()
            );
        }
    }

    fn params_suffix(params: &IndexMap<String, String>) -> String {
        if params.is_empty() {
            String::new()
        } else {
            let params: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("?{}", params.join(","))
        }
    }

    #[test]
    fn model_uri_keeps_non_parameter_colon_suffixes() {
        // Colon no longer has any structural meaning: everything up to the
        // first `?` is model content. OpenRouter routes on `:free`, `:online`,
        // `:nitro`, so those suffixes must stay inside the model.
        for (spec, provider, model) in [
            (
                "openrouter/some/model:free",
                "openrouter",
                "some/model:free",
            ),
            (
                "openrouter/anthropic/claude-sonnet-4:free",
                "openrouter",
                "anthropic/claude-sonnet-4:free",
            ),
            (
                "openrouter/google/gemini-2.5-flash:online",
                "openrouter",
                "google/gemini-2.5-flash:online",
            ),
            (
                "openrouter/some/model:free?reasoning=high",
                "openrouter",
                "some/model:free",
            ),
        ] {
            let uri = ModelURI::parse(spec).unwrap();
            assert_eq!(uri.provider(), provider, "{spec:?}");
            assert_eq!(uri.model(), model, "{spec:?}");
        }
    }

    #[test]
    fn cmd_has_no_params_and_no_canonical_form() {
        let uri = ModelURI::parse("cmd/echo hello?x=1").unwrap();
        assert_eq!(uri.to_string(), "cmd/echo hello?x=1");
        assert!(ModelURI::parse("model://cmd/echo hello").is_err());
    }

    #[test]
    fn profile_provider_is_reserved_in_model_grammar() {
        for spec in ["profile/foo", "model://profile/foo"] {
            let err = ModelURI::parse(spec).unwrap_err();
            assert!(
                err.contains("reserved profile-reference prefix"),
                "{spec:?} -> {err:?}"
            );
        }

        // The old `profile:<name>` colon spelling is not a profile lookup
        // anymore; it fails through the normal stub grammar.
        let err = ModelURI::parse("profile:foo").unwrap_err();
        assert!(err.contains("expected 'provider/model'"), "{err:?}");

        // The parts-based constructor funnels through the same
        // [`ModelURI::from_parts`] chokepoint, so it must reject `profile`
        // before any provider allowlist check runs.
        let err = Client::new("profile".into(), "foo".into(), IndexMap::new()).unwrap_err();
        assert!(err.contains("reserved profile-reference prefix"), "{err:?}");
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
        assert!(Client::parse("fake/m").is_err());
        assert!(Client::from_uri(ModelURI::parse("fake/m").unwrap()).is_err());
    }

    #[test]
    fn resolve_model_returns_none_for_malformed_specs() {
        assert!(resolve_model("no-slash").is_none());
        assert!(resolve_model("/model").is_none());
        assert!(resolve_model("openai/").is_none());
        assert!(resolve_model("model://openai").is_none());
    }

    #[test]
    fn resolve_model_returns_none_for_unknown_provider() {
        assert!(resolve_model("fake/model").is_none());
    }

    #[test]
    fn resolve_model_returns_none_for_cmd() {
        assert!(resolve_model("cmd/true").is_none());
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
