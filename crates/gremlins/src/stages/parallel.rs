//! Validation for parallel groups: name shape, concurrency, and bail policy.
//!
//! This module is deliberately free of PyO3: it owns only the pure parsing and
//! validation that the Python module used to perform, so `cargo test -p
//! gremlins` can cover the rules without an interpreter. The pyext shim is
//! responsible for turning the parsed children into live stage objects.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::stages::composite::{get_client_from_dict, ClientSpec, StageAttrs};

/// How a group decides to bail once individual children have bailed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BailPolicy {
    /// Bail as soon as any child bails.
    Any,
    /// Bail only when every child bails.
    All,
}

impl BailPolicy {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "any" => Some(BailPolicy::Any),
            "all" => Some(BailPolicy::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BailPolicy::Any => "any",
            BailPolicy::All => "all",
        }
    }
}

/// True when `name` is a legal parallel `child_id` component: ASCII letters,
/// digits, `-` and `_`, at least one character.
///
/// Mirrors the Python module's `_CHILD_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")`.
pub fn is_valid_child_id(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Debug, Clone)]
pub struct ParallelGroup {
    pub attrs: StageAttrs,
    pub max_concurrent: Option<u32>,
    pub cancel_on_bail: bool,
    pub bail_policy: BailPolicy,
    /// Raw child dicts — the pyext shim calls `parse_stages()` on these.
    pub body: Vec<Value>,
    /// Raw client string, if present — the pyext shim creates a Client from it.
    pub client: Option<ClientSpec>,
}

impl ParallelGroup {
    /// Parse and validate a `parallel:` block.
    ///
    /// `depth` is the nesting level: parallel groups may not be nested, so any
    /// `depth > 0` is rejected. Child *names* are validated here; child
    /// *dicts* are validated by the caller once `parse_stages` has named them.
    pub fn with_dict(d: &HashMap<String, Value>, depth: usize) -> Result<Self, String> {
        let name = d
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if depth > 0 {
            return Err(format!(
                "nested parallel groups are not allowed (stage {name:?})"
            ));
        }

        let body = match d.get("parallel") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(arr)) => arr.clone(),
            Some(_) => {
                return Err(format!(
                    "parallel group {name:?}: 'parallel' must be a list"
                ))
            }
        };

        let max_concurrent = match d.get("max_concurrent") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let n = v.as_u64().ok_or_else(|| {
                    format!("parallel group {name:?}: 'max_concurrent' must be a positive integer")
                })?;
                if n == 0 {
                    return Err(format!(
                        "parallel group {name:?}: 'max_concurrent' must be a positive integer"
                    ));
                }
                Some(u32::try_from(n).map_err(|_| {
                    format!(
                        "parallel group {name:?}: 'max_concurrent' must be <= {}, got {n}",
                        u32::MAX
                    )
                })?)
            }
        };

        let cancel_on_bail = match d.get("cancel_on_bail") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(format!(
                    "parallel group {name:?}: 'cancel_on_bail' must be a boolean"
                ))
            }
        };

        // `str(d.get("bail_policy") or "any")` — any falsy value falls back to "any".
        // Python's falsy set is: None, False, 0, 0.0, "", [], {}.
        let raw_policy = match d.get("bail_policy") {
            None | Some(Value::Null) => "any".to_string(),
            Some(Value::Bool(false)) => "any".to_string(),
            Some(Value::Number(n)) if n.as_f64() == Some(0.0) => "any".to_string(),
            Some(Value::String(s)) if s.is_empty() => "any".to_string(),
            Some(Value::Array(a)) if a.is_empty() => "any".to_string(),
            Some(Value::Object(o)) if o.is_empty() => "any".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
        };
        let bail_policy = BailPolicy::parse(&raw_policy).ok_or_else(|| {
            format!("parallel group {name:?}: 'bail_policy' must be 'any' or 'all'")
        })?;

        if !name.is_empty() && !is_valid_child_id(&name) {
            return Err(format!(
                "parallel group name {name:?} contains invalid characters for child_id"
            ));
        }

        let client = get_client_from_dict(d, &name)?;
        let client_explicit = client.is_some();

        let mut attrs = StageAttrs::new(name);
        attrs.stage_type = "parallel".to_string();
        attrs.client_explicit = client_explicit;

        Ok(ParallelGroup {
            attrs,
            max_concurrent,
            cancel_on_bail,
            bail_policy,
            body,
            client,
        })
    }
}

/// Validate the resolved child names of a parallel group: each must be a legal
/// `child_id` component, and no two may collide.
///
/// This runs *after* `parse_stages` has assigned names, because a raw
/// `parallel:` entry may omit its `name` and inherit one from its `type`.
pub fn validate_child_names(group_name: &str, names: &[String]) -> Result<(), String> {
    let mut seen: HashSet<&str> = HashSet::new();
    for child in names {
        if !seen.insert(child.as_str()) {
            return Err(format!(
                "parallel group {group_name:?}: duplicate child name {child:?}"
            ));
        }
    }
    for child in names {
        if !is_valid_child_id(child) {
            return Err(format!(
                "parallel child name {child:?} contains invalid characters for child_id"
            ));
        }
    }
    Ok(())
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

    fn children() -> Value {
        json!([{"type": "exec", "name": "shard-1"}, {"type": "exec", "name": "shard-2"}])
    }

    #[test]
    fn with_dict_basic() {
        let d = dict(&[("name", json!("reviews")), ("parallel", children())]);
        let group = ParallelGroup::with_dict(&d, 0).unwrap();
        assert_eq!(group.attrs.name, "reviews");
        assert_eq!(group.attrs.stage_type, "parallel");
        assert_eq!(group.body.len(), 2);
        assert_eq!(group.max_concurrent, None);
        assert!(!group.cancel_on_bail);
        assert_eq!(group.bail_policy, BailPolicy::Any);
    }

    #[test]
    fn with_dict_accepts_all_options() {
        let d = dict(&[
            ("name", json!("reviews")),
            ("parallel", children()),
            ("max_concurrent", json!(3)),
            ("cancel_on_bail", json!(true)),
            ("bail_policy", json!("all")),
        ]);
        let group = ParallelGroup::with_dict(&d, 0).unwrap();
        assert_eq!(group.max_concurrent, Some(3));
        assert!(group.cancel_on_bail);
        assert_eq!(group.bail_policy, BailPolicy::All);
    }

    #[test]
    fn with_dict_rejects_nesting() {
        let d = dict(&[("name", json!("outer")), ("parallel", children())]);
        let err = ParallelGroup::with_dict(&d, 1).unwrap_err();
        assert!(err.contains("nested parallel groups are not allowed"));
    }

    #[test]
    fn with_dict_rejects_non_list_children() {
        let d = dict(&[("name", json!("g")), ("parallel", json!("nope"))]);
        let err = ParallelGroup::with_dict(&d, 0).unwrap_err();
        assert!(err.contains("'parallel' must be a list"));
    }

    #[test]
    fn validate_child_names_rejects_duplicates() {
        let names = vec!["dup".to_string(), "dup".to_string()];
        let err = validate_child_names("g", &names).unwrap_err();
        assert!(err.contains("duplicate child name"));
    }

    #[test]
    fn validate_child_names_rejects_invalid() {
        let names = vec!["bad/name".to_string()];
        let err = validate_child_names("g", &names).unwrap_err();
        assert!(err.contains("invalid characters for child_id"));
    }

    #[test]
    fn validate_child_names_accepts_clean_names() {
        let names = vec!["shard-1".to_string(), "shard_2".to_string()];
        assert!(validate_child_names("g", &names).is_ok());
    }

    #[test]
    fn with_dict_rejects_zero_and_negative_max_concurrent() {
        for raw in [json!(0), json!(-1)] {
            let d = dict(&[
                ("name", json!("g")),
                ("parallel", children()),
                ("max_concurrent", raw),
            ]);
            let err = ParallelGroup::with_dict(&d, 0).unwrap_err();
            assert!(
                err.contains("'max_concurrent' must be a positive integer"),
                "{err}"
            );
        }
    }

    #[test]
    fn with_dict_rejects_non_bool_cancel_on_bail() {
        let d = dict(&[
            ("name", json!("g")),
            ("parallel", children()),
            ("cancel_on_bail", json!("yes")),
        ]);
        let err = ParallelGroup::with_dict(&d, 0).unwrap_err();
        assert!(err.contains("'cancel_on_bail' must be a boolean"));
    }

    #[test]
    fn with_dict_rejects_unknown_bail_policy() {
        let d = dict(&[
            ("name", json!("g")),
            ("parallel", children()),
            ("bail_policy", json!("sometimes")),
        ]);
        let err = ParallelGroup::with_dict(&d, 0).unwrap_err();
        assert!(err.contains("'bail_policy' must be 'any' or 'all'"));
    }

    #[test]
    fn with_dict_treats_falsy_bail_policy_as_any() {
        // Python's `d.get("bail_policy") or "any"` maps every falsy value to "any".
        for raw in [
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            let d = dict(&[
                ("name", json!("g")),
                ("parallel", children()),
                ("bail_policy", raw.clone()),
            ]);
            let group = ParallelGroup::with_dict(&d, 0)
                .unwrap_or_else(|e| panic!("bail_policy={raw} should default to any: {e}"));
            assert_eq!(group.bail_policy, BailPolicy::Any, "bail_policy={raw}");
        }
    }

    #[test]
    fn with_dict_rejects_invalid_group_name() {
        let d = dict(&[("name", json!("has space")), ("parallel", children())]);
        let err = ParallelGroup::with_dict(&d, 0).unwrap_err();
        assert!(err.contains("invalid characters for child_id"));
    }

    #[test]
    fn with_dict_allows_empty_children() {
        let d = dict(&[("name", json!("g")), ("parallel", json!([]))]);
        assert!(ParallelGroup::with_dict(&d, 0).unwrap().body.is_empty());
    }

    #[test]
    fn with_dict_allows_missing_children() {
        let d = dict(&[("name", json!("g"))]);
        assert!(ParallelGroup::with_dict(&d, 0).unwrap().body.is_empty());
    }

    #[test]
    fn with_dict_carries_client() {
        let d = dict(&[
            ("name", json!("g")),
            ("parallel", children()),
            ("client", json!("xai:grok-5")),
        ]);
        let group = ParallelGroup::with_dict(&d, 0).unwrap();
        assert_eq!(group.client, Some(ClientSpec("xai:grok-5".into())));
        assert!(group.attrs.client_explicit);
    }

    #[test]
    fn child_id_validation() {
        assert!(is_valid_child_id("abc-123_X"));
        assert!(!is_valid_child_id(""));
        assert!(!is_valid_child_id("a/b"));
        assert!(!is_valid_child_id("a b"));
    }

    #[test]
    fn bail_policy_round_trips() {
        assert_eq!(BailPolicy::Any.as_str(), "any");
        assert_eq!(BailPolicy::All.as_str(), "all");
    }
}
