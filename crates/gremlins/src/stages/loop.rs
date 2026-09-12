use std::collections::HashMap;

use serde_json::Value;

use crate::stages::composite::{get_client_from_dict, ClientSpec, StageAttrs};

#[derive(Debug, Clone)]
pub struct Loop {
    pub attrs: StageAttrs,
    /// Raw body dicts — the pyext shim calls parse_stages() on these.
    pub body: Vec<Value>,
    pub max_iterations: u32,
    pub stop_when_exists: Option<String>,
    pub interval: Option<f64>,
    /// Raw client string, if present — the pyext shim creates a Client from it.
    pub client: Option<ClientSpec>,
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

/// Python-style `int(v)`: integers, integral floats, and numeric strings.
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Python-style `float(v)`: numbers and numeric strings.
fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

impl Loop {
    pub fn with_dict(d: &HashMap<String, Value>) -> Result<Self, String> {
        let name = d
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let options = match d.get("options") {
            None | Some(Value::Null) => HashMap::new(),
            Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Some(_) => return Err(format!("stage '{name}': 'options' must be a mapping")),
        };

        // `d.get("max-iterations") or options.get("max_iterations", 3)`
        let raw_max = d
            .get("max-iterations")
            .filter(|v| truthy(v))
            .or_else(|| options.get("max_iterations").filter(|v| !v.is_null()));
        let max_iterations = match raw_max {
            Some(v) => {
                let n = as_int(v).ok_or_else(|| {
                    format!("stage '{name}': 'max_iterations' must be an integer, got {v:?}")
                })?;
                if n < 1 {
                    return Err(format!(
                        "Loop '{name}': max_iterations must be >= 1, got {n}"
                    ));
                }
                n as u32
            }
            None => 3,
        };

        let interval = match options.get("interval") {
            None | Some(Value::Null) => None,
            Some(v) => Some(as_float(v).ok_or_else(|| {
                format!("stage '{name}': 'interval' must be a number, got {v:?}")
            })?),
        };

        let stop_when_exists = match d.get("stop_when_exists") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(v) => {
                return Err(format!(
                    "stage '{name}': 'stop_when_exists' must be a string, got {v:?}"
                ))
            }
        };

        let body = match d.get("body") {
            Some(Value::Array(arr)) if !arr.is_empty() => arr.clone(),
            _ => return Err(format!("stage '{name}': 'body' must not be empty")),
        };

        let client = get_client_from_dict(d, &name)?;
        let client_explicit = client.is_some();

        let mut attrs = StageAttrs::new(name);
        attrs.stage_type = "loop".to_string();
        attrs.client_explicit = client_explicit;

        Ok(Loop {
            attrs,
            body,
            max_iterations,
            stop_when_exists,
            interval,
            client,
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

    fn body() -> Value {
        json!([{"type": "exec", "name": "step"}])
    }

    #[test]
    fn with_dict_basic() {
        let d = dict(&[
            ("name", json!("my-loop")),
            ("type", json!("loop")),
            ("max-iterations", json!(5)),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.attrs.name, "my-loop");
        assert_eq!(lp.attrs.stage_type, "loop");
        assert_eq!(lp.body.len(), 1);
        assert_eq!(lp.body[0]["name"], "step");
        assert_eq!(lp.max_iterations, 5);
        assert_eq!(lp.stop_when_exists, None);
        assert_eq!(lp.interval, None);
    }

    #[test]
    fn with_dict_defaults_max_iterations_to_three() {
        let d = dict(&[("name", json!("lp")), ("body", body())]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.max_iterations, 3);
    }

    #[test]
    fn with_dict_parses_stop_when_exists() {
        let d = dict(&[
            ("name", json!("lp")),
            ("stop_when_exists", json!("artifact://done.txt")),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.stop_when_exists.as_deref(), Some("artifact://done.txt"));
    }

    #[test]
    fn with_dict_parses_interval_from_options() {
        let d = dict(&[
            ("name", json!("lp")),
            ("options", json!({"interval": 2.5})),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.interval, Some(2.5));
    }

    #[test]
    fn with_dict_parses_string_interval() {
        let d = dict(&[
            ("name", json!("lp")),
            ("options", json!({"interval": "20"})),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.interval, Some(20.0));
    }

    #[test]
    fn with_dict_parses_max_iterations_from_top_level() {
        let d = dict(&[
            ("name", json!("lp")),
            ("max-iterations", json!(7)),
            ("options", json!({"max_iterations": 2})),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.max_iterations, 7);
    }

    #[test]
    fn with_dict_parses_max_iterations_from_options() {
        let d = dict(&[
            ("name", json!("lp")),
            ("options", json!({"max_iterations": 4})),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.max_iterations, 4);
    }

    #[test]
    fn with_dict_parses_string_max_iterations() {
        let d = dict(&[
            ("name", json!("lp")),
            ("max-iterations", json!("3")),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.max_iterations, 3);
    }

    #[test]
    fn with_dict_rejects_max_iterations_below_one() {
        // Via options: 0 is not filtered by the `or` fallback there, so it
        // reaches the >= 1 check (unlike a falsy top-level 'max-iterations').
        let d = dict(&[
            ("name", json!("lp")),
            ("options", json!({"max_iterations": 0})),
            ("body", body()),
        ]);
        let err = Loop::with_dict(&d).unwrap_err();
        assert!(err.contains("must be >= 1"));
    }

    #[test]
    fn with_dict_rejects_negative_max_iterations() {
        let d = dict(&[
            ("name", json!("lp")),
            ("max-iterations", json!(-5)),
            ("body", body()),
        ]);
        let err = Loop::with_dict(&d).unwrap_err();
        assert!(err.contains("must be >= 1"));
    }

    #[test]
    fn with_dict_zero_top_level_falls_back_to_default() {
        // Python's `d.get('max-iterations') or ...` treats 0 as absent.
        let d = dict(&[
            ("name", json!("lp")),
            ("max-iterations", json!(0)),
            ("body", body()),
        ]);
        assert_eq!(Loop::with_dict(&d).unwrap().max_iterations, 3);
    }

    #[test]
    fn with_dict_rejects_empty_body() {
        let d = dict(&[("name", json!("lp")), ("body", json!([]))]);
        let err = Loop::with_dict(&d).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn with_dict_rejects_missing_body() {
        let d = dict(&[("name", json!("lp"))]);
        let err = Loop::with_dict(&d).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn with_dict_rejects_non_list_body() {
        let d = dict(&[("name", json!("lp")), ("body", json!("not-a-list"))]);
        let err = Loop::with_dict(&d).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn with_dict_client() {
        let d = dict(&[
            ("name", json!("lp")),
            ("client", json!("xai:grok-5")),
            ("body", body()),
        ]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.client, Some(ClientSpec("xai:grok-5".into())));
        assert!(lp.attrs.client_explicit);
    }

    #[test]
    fn with_dict_client_none() {
        let d = dict(&[("name", json!("lp")), ("body", body())]);
        let lp = Loop::with_dict(&d).unwrap();
        assert_eq!(lp.client, None);
        assert!(!lp.attrs.client_explicit);
    }

    #[test]
    fn with_dict_rejects_non_string_client() {
        let d = dict(&[
            ("name", json!("lp")),
            ("client", json!(42)),
            ("body", body()),
        ]);
        assert!(Loop::with_dict(&d).is_err());
    }
}
