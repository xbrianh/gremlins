//! Client specifiers and lazy backend construction.
//!
//! A definition names its model as a single string — `provider:model`, with an
//! optional trailing `:key=value,...` parameter list. This module owns that
//! grammar ([`parse_spec`]), the set of providers the harness can actually
//! construct ([`is_known_provider`]), and [`Client`], which turns a spec into a
//! live [`Backend`] on first use.
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

/// A trailing `:k=v,k=v,...` parameter list, or a signal that the suffix is
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

/// The result of splitting a spec's model/params suffix: the model portion
/// plus the parsed params, or `None` when the suffix is not a params list.
type SplitParams<'a> = Option<(&'a str, IndexMap<String, String>)>;

/// Split `rest` into `(model, params)` at the leftmost colon whose remainder
/// is a valid parameter list, mirroring the old anchored-regex behaviour.
fn split_params_suffix(rest: &str) -> Result<SplitParams<'_>, String> {
    for (i, b) in rest.bytes().enumerate() {
        if b == b':' {
            match parse_params_list(&rest[i + 1..]) {
                ParamsList::Ok(params) => return Ok(Some((&rest[..i], params))),
                ParamsList::Duplicate(k) => {
                    return Err(format!(
                        "duplicate key {k:?} in client params {:?}",
                        &rest[i + 1..]
                    ));
                }
                ParamsList::NotAList => {}
            }
        }
    }
    Ok(None)
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
/// `openai_protocol` calls [`split_spec`] and discards the params component, so
/// the two cannot drift.
pub fn parse_spec(s: &str) -> Result<(String, String, IndexMap<String, String>), String> {
    let (provider, model, extra_params) = split_spec(s)?;
    Ok((provider.to_string(), model.to_string(), extra_params))
}

/// Borrowed form of [`parse_spec`]: split `provider:model[:k=v,...]` into the
/// three subslices/components.
///
/// This is the grammar implementation shared by [`parse_spec`] and
/// `openai_protocol::provider_and_model`. It owns the canonical split — the
/// first colon delimits the provider, the model is kept verbatim for `cmd`,
/// and for every other provider the leftmost colon whose remainder is a valid
/// parameter list is stripped. The owning parser only converts the borrowed
/// parts into heap strings; the adapter in `openai_protocol` discards params.
pub(crate) fn split_spec(s: &str) -> Result<(&str, &str, IndexMap<String, String>), String> {
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
        rest
    } else {
        match split_params_suffix(rest)? {
            Some((model, params)) => {
                extra_params = params;
                model
            }
            None => rest,
        }
    };
    if model.is_empty() {
        return Err(format!(
            "invalid client specifier {s:?}: model must not be empty"
        ));
    }
    Ok((provider, model, extra_params))
}

fn backend_pool() -> &'static Mutex<HashMap<String, Arc<dyn Backend>>> {
    static POOL: OnceLock<Mutex<HashMap<String, Arc<dyn Backend>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A resolved `provider:model` spec.
///
/// Backend construction is delegated to a module-level pool so that every
/// [`Client`] with the same spec shares one [`Backend`] (and therefore one
/// HTTP connection pool).  Callers see no difference: [`Client::parse`],
/// [`Client::run`], and [`Gremlin::client`] all keep their current signatures.
#[derive(Clone)]
pub struct Client {
    provider: String,
    model: String,
    extra_params: IndexMap<String, String>,
    native_block: HashMap<String, Vec<String>>,
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
        })
    }

    /// Parse a `provider:model[:k=v,...]` spec.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (provider, model, extra_params) = parse_spec(spec)?;
        Self::new(provider, model, extra_params)
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

        let base_url = match self.provider.as_str() {
            "openai" => "https://api.openai.com/v1",
            "xai" => "https://api.x.ai/v1",
            "openrouter" => "https://openrouter.ai/api/v1",
            "cmd" => "(shell)",
            _ => "(unknown)",
        };
        log::info!(
            "Built new backend: provider={}, model={}, base_url={base_url}",
            self.provider,
            self.model,
        );

        pool.insert(spec, backend.clone());
        Ok(backend)
    }

    /// Construct the backend this client's spec names, without memoising it.
    ///
    /// Looks up the provider name in [`backends::registry`] and delegates to
    /// the registered `build` function.  An unknown provider is caught by
    /// [`Client::new`] long before this runs, so the fallthrough arm is a
    /// guard against a future constructor that forgets to validate.
    fn build_backend(&self) -> Result<Arc<dyn Backend>, String> {
        for (name, build) in backends::registry() {
            if name == self.provider.as_str() {
                return build(&self.model, &self.native_block, &self.extra_params);
            }
        }
        Err(format!("unknown provider '{}'", self.provider))
    }

    /// Cancel every in-flight process owned by the backend, if it has been
    /// built. A client that never ran anything has nothing to reap.
    pub fn reap_all(&self, gremlin_id: &str) {
        log::debug!(
            "Client::reap_all: gremlin_id={gremlin_id}, provider={}, model={}",
            self.provider,
            self.model,
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
    /// `native_block` is excluded: it is always the default allowlist.
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
    fn parse_spec_json_value_with_commas() {
        let (p, m, e) = parse_spec(
            r#"anthropic:claude-sonnet-4:cache_control={"type":"ephemeral","ttl":"1h"},max_tokens=4096"#,
        )
        .unwrap();
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-sonnet-4");
        assert_eq!(
            e.get("cache_control").unwrap(),
            r#"{"type":"ephemeral","ttl":"1h"}"#
        );
        assert_eq!(e.get("max_tokens").unwrap(), "4096");
    }

    #[test]
    fn parse_spec_json_array_value_with_commas() {
        let (_, _, e) = parse_spec("openai:gpt-4o:stop=[\"END\",\"STOP\"],foo=bar").unwrap();
        assert_eq!(e.get("stop").unwrap(), "[\"END\",\"STOP\"]");
        assert_eq!(e.get("foo").unwrap(), "bar");
    }

    #[test]
    fn parse_spec_json_value_roundtrips() {
        let spec = r#"anthropic:claude:cache_control={"type":"ephemeral","ttl":"1h"}"#;
        let (p, m, e) = parse_spec(spec).unwrap();
        let mut s = format!("{p}:{m}");
        if !e.is_empty() {
            let params: Vec<String> = e.iter().map(|(k, v)| format!("{k}={v}")).collect();
            s.push(':');
            s.push_str(&params.join(","));
        }
        assert_eq!(s, spec);
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
            assert!(Client::parse(&format!("{p}:model")).is_ok(), "{p}");
        }
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
