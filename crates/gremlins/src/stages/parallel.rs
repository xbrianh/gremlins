//! Validation for parallel groups: name shape, concurrency, and error policy.
//!
//! This module is deliberately free of PyO3: it owns only the pure parsing and
//! validation that the Python module used to perform, so `cargo test -p
//! gremlins` can cover the rules without an interpreter. The pyext shim is
//! responsible for turning the parsed children into live stage objects.

use std::collections::HashSet;

use serde_json::Value;

use crate::stages::composite::{ClientSpec, StageAttrs};

/// How a group decides to fail once individual children have errored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPolicy {
    /// Fail as soon as any child errors.
    Any,
    /// Fail only when every child errors.
    All,
}

impl ErrorPolicy {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "any" => Some(ErrorPolicy::Any),
            "all" => Some(ErrorPolicy::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorPolicy::Any => "any",
            ErrorPolicy::All => "all",
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
    pub cancel_on_error: bool,
    pub error_policy: ErrorPolicy,
    pub body: Vec<Value>,
    pub client: Option<ClientSpec>,
}

/// Validate the resolved child names of a parallel group: each must be a legal
/// `child_id` component, and no two may collide.
///
/// This runs *after* name-filling, because a raw
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
    fn child_id_validation() {
        assert!(is_valid_child_id("abc-123_X"));
        assert!(!is_valid_child_id(""));
        assert!(!is_valid_child_id("a/b"));
        assert!(!is_valid_child_id("a b"));
    }

    #[test]
    fn error_policy_round_trips() {
        assert_eq!(ErrorPolicy::Any.as_str(), "any");
        assert_eq!(ErrorPolicy::All.as_str(), "all");
    }
}
