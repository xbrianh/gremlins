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
}

impl Sequence {
    pub fn with_dict(d: &HashMap<String, Value>) -> Result<Self, String> {
        let name = d
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let body = match d.get("body") {
            Some(Value::Array(arr)) if !arr.is_empty() => arr.clone(),
            Some(Value::Array(_)) => {
                return Err(format!("stage '{name}': 'body' must not be empty"))
            }
            // Single-quoted like Python's `{name!r}` in the pre-port stage.
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
}
