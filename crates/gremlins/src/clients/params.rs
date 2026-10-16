//! Typed extra-param resolution shared by every backend.
//!
//! A client specifier's trailing `?k=v,...` list is resolved per backend into
//! a [`ResolvedParams`]: the typed `temperature` and `max_tokens` request
//! fields plus the `additional_params` object that carries every remaining
//! key.  Each backend owns its own default table and calls the strict parsers
//! here so a malformed typed value is a build-time error, never a silent
//! fallback.

use serde_json::Value;

/// The result of resolving a raw `provider/model?k=v,...` param map.
///
/// `temperature` and `max_tokens` are the typed top-level request fields; they
/// are emitted only from a resolved value and never appear in `additional`.
/// `additional` is the full `additional_params` object: unknown keys pass
/// through JSON-coerced, and backend-specific typed flags (e.g.
/// `parallel_tool_calls`, `reasoning`, `cache_control`) appear in their
/// resolved form with defaults applied.
#[derive(Debug, Clone, Default)]
pub struct ResolvedParams {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub additional: serde_json::Map<String, Value>,
}

impl ResolvedParams {
    /// The `additional_params` object, or `None` when empty.
    pub fn additional_value(&self) -> Option<Value> {
        if self.additional.is_empty() {
            None
        } else {
            Some(Value::Object(self.additional.clone()))
        }
    }
}

fn type_error(provider: &str, key: &str, expected: &str, value: &str) -> String {
    format!("{provider}: invalid value {value:?} for client param {key:?}: expected {expected}")
}

/// Parse a strict `f64` typed value. `NaN` and infinities are rejected: they
/// are not representable as JSON numbers and would serialize as `null`.
pub fn parse_f64(provider: &str, key: &str, value: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| type_error(provider, key, "a number (f64)", value))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(type_error(provider, key, "a finite number (f64)", value))
    }
}

/// Parse a strict `u64` typed value.
pub fn parse_u64(provider: &str, key: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| type_error(provider, key, "an unsigned integer (u64)", value))
}

/// Parse a strict bool typed value. Accepts `true`/`false` and `1`/`0`.
pub fn parse_bool(provider: &str, key: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(type_error(provider, key, "a boolean (true/false)", value)),
    }
}

/// Parse a strict JSON object typed value.
pub fn parse_json_object(provider: &str, key: &str, value: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(value)
        .map_err(|_| type_error(provider, key, "a JSON object", value))?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(type_error(provider, key, "a JSON object", value))
    }
}

/// JSON-coerce an unknown passthrough value: parse as JSON, falling back to a
/// plain string when the value is not valid JSON (e.g. an opaque enum like
/// `thinking=deepseek`).
pub fn coerce(value: &str) -> Value {
    match serde_json::from_str::<Value>(value) {
        Ok(parsed) => parsed,
        Err(_) => Value::String(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_f64_strict() {
        assert_eq!(parse_f64("openai", "temperature", "0.7").unwrap(), 0.7);
        let err = parse_f64("openai", "temperature", "hot").unwrap_err();
        assert!(err.contains("openai"), "{err}");
        assert!(err.contains("temperature"), "{err}");
        assert!(err.contains("hot"), "{err}");
    }

    #[test]
    fn parse_f64_rejects_non_finite() {
        for v in ["NaN", "inf", "-inf", "infinity"] {
            let err = parse_f64("openai", "temperature", v).unwrap_err();
            assert!(err.contains("temperature"), "{err}");
            assert!(err.contains(v), "{err}");
        }
    }

    #[test]
    fn parse_u64_strict() {
        assert_eq!(parse_u64("anthropic", "max_tokens", "4096").unwrap(), 4096);
        let err = parse_u64("anthropic", "max_tokens", "lots").unwrap_err();
        assert!(err.contains("max_tokens"), "{err}");
        assert!(err.contains("lots"), "{err}");
    }

    #[test]
    fn parse_bool_strict() {
        assert!(parse_bool("openai", "parallel_tool_calls", "true").unwrap());
        assert!(!parse_bool("openai", "parallel_tool_calls", "false").unwrap());
        assert!(parse_bool("openai", "parallel_tool_calls", "1").unwrap());
        assert!(!parse_bool("openai", "parallel_tool_calls", "0").unwrap());
        let err = parse_bool("openai", "parallel_tool_calls", "notabool").unwrap_err();
        assert!(err.contains("parallel_tool_calls"), "{err}");
        assert!(err.contains("notabool"), "{err}");
    }

    #[test]
    fn parse_json_object_strict() {
        let v = parse_json_object("anthropic", "cache_control", r#"{"type":"ephemeral"}"#).unwrap();
        assert_eq!(v["type"], "ephemeral");
        let err = parse_json_object("anthropic", "cache_control", "not-json").unwrap_err();
        assert!(err.contains("cache_control"), "{err}");
        let err = parse_json_object("anthropic", "cache_control", "[1,2]").unwrap_err();
        assert!(err.contains("JSON object"), "{err}");
    }

    #[test]
    fn coerce_preserves_json_types() {
        assert_eq!(coerce("0.7"), Value::from(0.7));
        assert_eq!(coerce("4096"), Value::from(4096));
        assert_eq!(coerce("true"), Value::Bool(true));
        assert_eq!(coerce("deepseek"), Value::String("deepseek".into()));
    }

    #[test]
    fn additional_value_none_when_empty() {
        let r = ResolvedParams::default();
        assert!(r.additional_value().is_none());
    }
}
