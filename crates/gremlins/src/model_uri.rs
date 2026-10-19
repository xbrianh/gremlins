//! The model specifier grammar.
//!
//! A definition names its model as a single string using the URI grammar:
//! `model://provider/model[?key=value,...]`, or the scheme-omitted stub
//! `provider/model[?key=value,...]`. This module owns that grammar and its
//! canonical parse type, [`ModelURI`].
//!
//! [`ModelURI`] is a pure data type — it parses a model specifier string and
//! stores the result. It has no runtime behavior, no network, no credentials,
//! and no code dependency on any other crate module.

use indexmap::IndexMap;
use std::fmt;

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
///
/// [`Client`]: crate::clients::client::Client
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
    /// `Client::new`: the raw `(provider, model, params)` triple is checked
    /// against the same grammar rules as a parsed spec, without going through
    /// a string round-trip.
    ///
    /// `pub(crate)` so that `Client::new` can call it; all other crate
    /// modules should go through [`ModelURI::parse`].
    pub(crate) fn from_parts(
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

impl fmt::Display for ModelURI {
    /// Renders the canonical form: `model://provider/model[?params]`, or
    /// `cmd/<command>` for the command backend.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

/// Render an extra-param map as the `k=v,k=v,...` list used after `?`.
fn render_params(params: &IndexMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
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
        // `ModelURI::from_parts` chokepoint, so it must reject `profile`
        // at the grammar level.
        let err =
            ModelURI::from_parts("profile".into(), "foo".into(), IndexMap::new()).unwrap_err();
        assert!(err.contains("reserved profile-reference prefix"), "{err:?}");
    }
}
