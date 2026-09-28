use std::collections::HashMap;

use serde_json::Value;

use crate::stages::composite::{get_client_from_dict, ClientSpec, StageAttrs};

#[derive(Debug, Clone)]
pub struct Sequence {
    pub attrs: StageAttrs,
    /// Raw body dicts — the pyext shim calls parse_stages() on these.
    pub body: Vec<Value>,
    /// Raw client string, if present — the pyext shim creates a Client from it.
    pub client: Option<ClientSpec>,
    /// Maximum number of iterations (default 1).
    pub max_iterations: u32,
    /// Seconds between iterations.
    pub interval: Option<f64>,
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python-style `int(v)`: bools, integers, integral floats, and numeric strings.
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Python-style `float(v)`: bools, numbers and numeric strings.
fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(*b as i64 as f64),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

impl Sequence {
    pub fn with_dict(d: &HashMap<String, Value>) -> Result<Self, String> {
        let name = d
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Read max-iterations from top-level only — default 1.
        let max_iterations = match d.get("max-iterations").filter(|v| truthy(v)) {
            Some(v) => {
                let n = as_int(v).ok_or_else(|| {
                    format!("stage '{name}': 'max_iterations' must be an integer, got {v:?}")
                })?;
                if n < 1 {
                    return Err(format!(
                        "stage '{name}': max_iterations must be >= 1, got {n}"
                    ));
                }
                u32::try_from(n).map_err(|_| {
                    format!(
                        "stage '{name}': 'max_iterations' must be <= {}, got {n}",
                        u32::MAX
                    )
                })?
            }
            None => 1,
        };

        let interval = match d.get("interval").filter(|v| !v.is_null()) {
            None | Some(Value::Null) => None,
            Some(v) => Some(as_float(v).ok_or_else(|| {
                format!("stage '{name}': 'interval' must be a number, got {v:?}")
            })?),
        };

        let body = match d.get("body") {
            Some(Value::Array(arr)) if !arr.is_empty() => arr.clone(),
            Some(Value::Array(_)) => {
                return Err(format!("stage '{name}': 'body' must not be empty"))
            }
            Some(_) => return Err(format!("stage '{name}': 'body' must be a list")),
            None => return Err(format!("stage '{name}': 'body' is required")),
        };

        let client = get_client_from_dict(d, &name)?;
        let client_explicit = client.is_some();

        let mut attrs = StageAttrs::new(name);
        attrs.stage_type = "sequence".to_string();
        attrs.client_explicit = client_explicit;

        Ok(Sequence {
            attrs,
            body,
            client,
            max_iterations,
            interval,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dict(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn with_dict_basic() {
        let d = dict(&[
            ("name", json!("my-seq")),
            ("type", json!("sequence")),
            (
                "body",
                json!([{"type": "exec", "name": "step-a"}, {"type": "exec", "name": "step-b"}]),
            ),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.attrs.name, "my-seq");
        assert_eq!(seq.attrs.stage_type, "sequence");
        assert_eq!(seq.body.len(), 2);
        assert_eq!(seq.body[0]["name"], "step-a");
        assert_eq!(seq.max_iterations, 1);
        assert_eq!(seq.interval, None);
    }

    #[test]
    fn with_dict_no_body_is_error() {
        let d = dict(&[("name", json!("minimal"))]);
        let err = Sequence::with_dict(&d).unwrap_err();
        assert!(err.contains("required"));
    }

    #[test]
    fn with_dict_client() {
        let d = dict(&[
            ("name", json!("s")),
            ("client", json!("xai:grok-5")),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.client, Some(ClientSpec("xai:grok-5".into())));
        assert!(seq.attrs.client_explicit);
    }

    #[test]
    fn with_dict_client_none() {
        let d = dict(&[
            ("name", json!("s")),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.client, None);
        assert!(!seq.attrs.client_explicit);
    }

    #[test]
    fn with_dict_body_null_is_error() {
        let d = dict(&[("name", json!("s")), ("body", Value::Null)]);
        let err = Sequence::with_dict(&d).unwrap_err();
        assert!(err.contains("must be a list"));
    }

    #[test]
    fn with_dict_body_empty_is_error() {
        let d = dict(&[("name", json!("s")), ("body", json!([]))]);
        let err = Sequence::with_dict(&d).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn with_dict_rejects_non_list_body() {
        let d = dict(&[("name", json!("bad")), ("body", json!("not-a-list"))]);
        let err = Sequence::with_dict(&d).unwrap_err();
        assert!(err.contains("must be a list"));
    }

    #[test]
    fn with_dict_rejects_non_string_client() {
        let d = dict(&[("name", json!("s")), ("client", json!(42))]);
        assert!(Sequence::with_dict(&d).is_err());
    }

    #[test]
    fn with_dict_parses_max_iterations_from_top_level() {
        let d = dict(&[
            ("name", json!("s")),
            ("max-iterations", json!(7)),
            ("options", json!({"max_iterations": 2})),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.max_iterations, 7);
    }

    #[test]
    fn with_dict_ignores_options_max_iterations() {
        let d = dict(&[
            ("name", json!("s")),
            ("options", json!({"max_iterations": 4})),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        // options.max_iterations is not read — defaults to 1
        assert_eq!(seq.max_iterations, 1);
    }

    #[test]
    fn with_dict_parses_interval_from_top_level() {
        let d = dict(&[
            ("name", json!("s")),
            ("interval", json!(2.5)),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.interval, Some(2.5));
    }

    #[test]
    fn with_dict_defaults_max_iterations_to_one() {
        let d = dict(&[
            ("name", json!("s")),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.max_iterations, 1);
    }

    #[test]
    fn with_dict_rejects_max_iterations_below_one() {
        let d = dict(&[
            ("name", json!("s")),
            ("max-iterations", json!(-1)),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let err = Sequence::with_dict(&d).unwrap_err();
        assert!(err.contains("must be >= 1"));
    }

    #[test]
    fn with_dict_zero_top_level_falls_back_to_default() {
        let d = dict(&[
            ("name", json!("s")),
            ("max-iterations", json!(0)),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        assert_eq!(Sequence::with_dict(&d).unwrap().max_iterations, 1);
    }

    #[test]
    fn with_dict_parses_string_max_iterations() {
        let d = dict(&[
            ("name", json!("s")),
            ("max-iterations", json!("3")),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.max_iterations, 3);
    }

    #[test]
    fn with_dict_parses_bool_interval() {
        let d = dict(&[
            ("name", json!("s")),
            ("interval", json!(true)),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        assert_eq!(Sequence::with_dict(&d).unwrap().interval, Some(1.0));
    }

    #[test]
    fn with_dict_parses_string_interval() {
        let d = dict(&[
            ("name", json!("s")),
            ("interval", json!("20")),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let seq = Sequence::with_dict(&d).unwrap();
        assert_eq!(seq.interval, Some(20.0));
    }

    #[test]
    fn with_dict_accepts_bool_max_iterations() {
        let d = dict(&[
            ("name", json!("s")),
            ("max-iterations", json!(true)),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        assert_eq!(Sequence::with_dict(&d).unwrap().max_iterations, 1);
    }

    #[test]
    fn with_dict_rejects_max_iterations_above_u32() {
        let d = dict(&[
            ("name", json!("s")),
            ("max-iterations", json!(u32::MAX as i64 + 1)),
            ("body", json!([{"type": "exec", "name": "step"}])),
        ]);
        let err = Sequence::with_dict(&d).unwrap_err();
        assert!(err.contains("must be <="), "{err}");
    }
}
