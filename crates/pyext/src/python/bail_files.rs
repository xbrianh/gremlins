//! Reading and writing per-child bail shards, and applying a group's error
//! policy to the collected bails.
//!
//! This is subprocess-runtime machinery: a child that bails writes
//! `bail_<attempt>.json` into the state directory, and the group's fan-in
//! aggregates those shards through [`decide`]. The in-process `gremlins`
//! executor has no bail files, so the module lives here beside the runtime that
//! needs it. Pure file I/O and JSON parsing, so it is unit-testable on its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gremlins::executor::state::{now_iso, read_state_json, token_hex};
use gremlins::stages::parallel::ErrorPolicy;
use serde_json::Value;

/// One child that wrote a bail file, with the parsed payload.
///
/// Values are kept as raw JSON so a bail file whose attributes are not strings
/// (e.g. `{"class": 1}`) still parses instead of silently degrading to the
/// `{"class": "other"}` fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct BailedChild {
    pub key: String,
    pub bail: HashMap<String, Value>,
}

/// The outcome of applying an error policy to the collected bails.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BailDecision {
    pub should_bail: bool,
    pub first_bail: HashMap<String, Value>,
}

impl BailDecision {
    /// The bail class to record, mirroring Python's `first_bail.get("class") or "other"`.
    pub fn bail_class(&self) -> String {
        field_or(&self.first_bail, "class", "other")
    }

    /// The bail detail to record, mirroring Python's `first_bail.get("detail") or ""`.
    pub fn bail_detail(&self) -> String {
        field_or(&self.first_bail, "detail", "")
    }
}

/// Read a bail field as a string, applying Python's `value or default` semantics:
/// a missing, null, or otherwise falsy value falls back to `default`, while a
/// truthy non-string value is stringified.
fn field_or(bail: &HashMap<String, Value>, key: &str, default: &str) -> String {
    match bail.get(key) {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) if s.is_empty() => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(false)) => default.to_string(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => default.to_string(),
        Some(Value::Array(a)) if a.is_empty() => default.to_string(),
        Some(Value::Object(o)) if o.is_empty() => default.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Collect the bail files for `child_keys`, resolving each child's attempt id
/// through `parallel_attempts`.
///
/// A child with no recorded attempt, or no bail file on disk, is skipped. A
/// bail file that fails to parse contributes `{"class": "other"}`, matching the
/// Python helper's fallback.
pub fn collect_bails(
    state_dir: &Path,
    child_keys: &[String],
    parallel_attempts: &HashMap<String, String>,
) -> Vec<BailedChild> {
    let mut result = Vec::new();
    for key in child_keys {
        let attempt = parallel_attempts.get(key).map(String::as_str).unwrap_or("");
        if attempt.is_empty() {
            continue;
        }
        let bail_file = state_dir.join(format!("bail_{attempt}.json"));
        if !bail_file.exists() {
            continue;
        }
        let bail = std::fs::read_to_string(&bail_file)
            .ok()
            .and_then(|text| serde_json::from_str::<HashMap<String, Value>>(&text).ok())
            .unwrap_or_else(|| {
                let mut fallback = HashMap::new();
                fallback.insert("class".to_string(), Value::String("other".to_string()));
                fallback
            });
        result.push(BailedChild {
            key: key.clone(),
            bail,
        });
    }
    result
}

/// Apply `policy` to `bailed` out of `total` children.
///
/// `Any` bails when at least one child bailed; `All` bails only when every
/// child bailed. `first_bail` is the first collected bail, or empty.
pub fn decide(bailed: &[BailedChild], total: usize, policy: ErrorPolicy) -> BailDecision {
    let should_bail = match policy {
        ErrorPolicy::Any => !bailed.is_empty(),
        ErrorPolicy::All => !bailed.is_empty() && bailed.len() == total,
    };
    BailDecision {
        should_bail,
        first_bail: bailed.first().map(|b| b.bail.clone()).unwrap_or_default(),
    }
}

/// Write `bail_<attempt>.json` for a parallel child.
///
/// The attempt is resolved through `parallel_attempts[child_key]`, falling back
/// to the top-level `attempt`. An existing bail file is never clobbered, so the
/// first child to bail wins.
pub fn write_parallel_bail(state_file: Option<&Path>, child_key: &str, reason: &str) {
    let Some(sf) = state_file else { return };
    if !sf.exists() {
        return;
    }
    let data = read_state_json(Some(sf));
    let attempt = data
        .get("parallel_attempts")
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(child_key))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            data.get("attempt")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        });
    let Some(attempt) = attempt else { return };
    let Some(state_dir) = sf.parent() else { return };
    let bail_path = state_dir.join(format!("bail_{attempt}.json"));
    let payload = serde_json::json!({
        "class": "other",
        "detail": reason,
        "ts": now_iso(),
    });
    write_bail_atomically(state_dir, &bail_path, &attempt, &payload);
}

/// Write `payload` to `bail_path` with first-writer-wins semantics.
///
/// The destination is created with no-replace semantics via a hard link from a
/// uniquely named temporary file, so two concurrent writers can never clobber
/// each other's payload: the loser's link fails with `AlreadyExists` and its
/// temporary file is removed. This replaces the racy `exists()`-then-`rename`
/// check-then-act sequence.
fn write_bail_atomically(state_dir: &Path, bail_path: &Path, attempt: &str, payload: &Value) {
    let tmp = state_dir.join(format!(".bail_{attempt}_{}.tmp", token_hex(4)));
    if std::fs::write(&tmp, payload.to_string()).is_err() {
        return;
    }
    // `hard_link` fails if the destination already exists, giving us an atomic
    // create-if-absent without a separate existence check. Either way the
    // temporary file is no longer needed.
    let _ = std::fs::hard_link(&tmp, bail_path);
    let _ = std::fs::remove_file(&tmp);
}

/// The state directory and per-child attempt map used to scan for bails.
pub fn read_bail_scan_inputs(
    state_file: Option<&Path>,
) -> (Option<PathBuf>, HashMap<String, String>) {
    let Some(sf) = state_file else {
        return (None, HashMap::new());
    };
    if !sf.exists() {
        return (None, HashMap::new());
    }
    let Ok(text) = std::fs::read_to_string(sf) else {
        return (None, HashMap::new());
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (None, HashMap::new());
    };
    let attempts = data
        .get("parallel_attempts")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    (sf.parent().map(Path::to_path_buf), attempts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn write_bail(dir: &Path, attempt: &str, body: &str) {
        std::fs::write(dir.join(format!("bail_{attempt}.json")), body).unwrap();
    }

    #[test]
    fn collect_skips_missing_attempt_and_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let keys = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut attempts = HashMap::new();
        attempts.insert("a".to_string(), "attempt-a".to_string());
        attempts.insert("b".to_string(), "attempt-b".to_string());
        // c has no attempt; b's file is absent.
        write_bail(tmp.path(), "attempt-a", r#"{"class":"other","detail":"x"}"#);

        let bailed = collect_bails(tmp.path(), &keys, &attempts);
        assert_eq!(bailed.len(), 1);
        assert_eq!(bailed[0].key, "a");
        assert_eq!(
            bailed[0].bail.get("detail"),
            Some(&Value::String("x".to_string()))
        );
    }

    #[test]
    fn collect_keeps_non_string_attributes() {
        let tmp = tempfile::tempdir().unwrap();
        let keys = vec!["a".to_string()];
        let mut attempts = HashMap::new();
        attempts.insert("a".to_string(), "attempt-a".to_string());
        // A non-string `class` must not collapse the whole payload to the fallback.
        write_bail(tmp.path(), "attempt-a", r#"{"class":1,"detail":"boom"}"#);

        let bailed = collect_bails(tmp.path(), &keys, &attempts);
        assert_eq!(bailed.len(), 1);
        assert_eq!(bailed[0].bail.get("class"), Some(&Value::from(1)));
        assert_eq!(bailed[0].bail.get("detail"), Some(&Value::from("boom")));
    }

    #[test]
    fn collect_falls_back_on_unparseable_bail() {
        let tmp = tempfile::tempdir().unwrap();
        let keys = vec!["a".to_string()];
        let mut attempts = HashMap::new();
        attempts.insert("a".to_string(), "attempt-a".to_string());
        write_bail(tmp.path(), "attempt-a", "not json");

        let bailed = collect_bails(tmp.path(), &keys, &attempts);
        assert_eq!(bailed.len(), 1);
        assert_eq!(
            bailed[0].bail.get("class"),
            Some(&Value::String("other".to_string()))
        );
    }

    #[test]
    fn decide_any_bails_on_one() {
        let bailed = vec![BailedChild {
            key: "a".into(),
            bail: HashMap::new(),
        }];
        assert!(decide(&bailed, 2, ErrorPolicy::Any).should_bail);
        assert!(!decide(&bailed, 2, ErrorPolicy::All).should_bail);
    }

    #[test]
    fn decide_all_requires_every_child() {
        let bailed = vec![
            BailedChild {
                key: "a".into(),
                bail: HashMap::new(),
            },
            BailedChild {
                key: "b".into(),
                bail: HashMap::new(),
            },
        ];
        assert!(decide(&bailed, 2, ErrorPolicy::All).should_bail);
        assert!(!decide(&[], 2, ErrorPolicy::Any).should_bail);
    }

    #[test]
    fn decide_reports_first_bail() {
        let mut bail = HashMap::new();
        bail.insert("class".to_string(), Value::String("other".to_string()));
        let bailed = vec![BailedChild {
            key: "a".into(),
            bail: bail.clone(),
        }];
        assert_eq!(decide(&bailed, 1, ErrorPolicy::Any).first_bail, bail);
        assert!(decide(&[], 1, ErrorPolicy::Any).first_bail.is_empty());
    }

    #[test]
    fn bail_class_and_detail_apply_python_falsy_defaults() {
        let mut bail = HashMap::new();
        bail.insert("class".to_string(), Value::from(7));
        bail.insert("detail".to_string(), Value::Null);
        let decision = BailDecision {
            should_bail: true,
            first_bail: bail,
        };
        assert_eq!(decision.bail_class(), "7");
        assert_eq!(decision.bail_detail(), "");
        assert_eq!(BailDecision::default().bail_class(), "other");
    }

    #[test]
    fn read_scan_inputs_reads_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let sf = tmp.path().join("state.json");
        std::fs::write(&sf, r#"{"id":"g","parallel_attempts":{"a":"attempt-a"}}"#).unwrap();
        let (dir, attempts) = read_bail_scan_inputs(Some(&sf));
        assert_eq!(dir.as_deref(), Some(tmp.path()));
        assert_eq!(attempts.get("a").map(String::as_str), Some("attempt-a"));
    }

    #[test]
    fn read_scan_inputs_handles_missing_file() {
        let (dir, attempts) = read_bail_scan_inputs(Some(Path::new("/nonexistent/state.json")));
        assert!(dir.is_none());
        assert!(attempts.is_empty());
    }

    #[test]
    fn write_parallel_bail_uses_child_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let sf = tmp.path().join("state.json");
        std::fs::write(&sf, r#"{"id":"g","parallel_attempts":{"a":"attempt-a"}}"#).unwrap();
        write_parallel_bail(Some(&sf), "a", "boom");
        let bail = tmp.path().join("bail_attempt-a.json");
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&bail).unwrap()).unwrap();
        assert_eq!(raw["class"], "other");
        assert_eq!(raw["detail"], "boom");
    }

    #[test]
    fn write_parallel_bail_falls_back_to_top_level_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let sf = tmp.path().join("state.json");
        std::fs::write(&sf, r#"{"id":"g","attempt":"top-attempt"}"#).unwrap();
        write_parallel_bail(Some(&sf), "a", "boom");
        assert!(tmp.path().join("bail_top-attempt.json").exists());
    }

    #[test]
    fn write_parallel_bail_does_not_clobber() {
        let tmp = tempfile::tempdir().unwrap();
        let sf = tmp.path().join("state.json");
        std::fs::write(&sf, r#"{"id":"g","parallel_attempts":{"a":"attempt-a"}}"#).unwrap();
        write_parallel_bail(Some(&sf), "a", "first");
        write_parallel_bail(Some(&sf), "a", "second");
        let bail = tmp.path().join("bail_attempt-a.json");
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&bail).unwrap()).unwrap();
        assert_eq!(raw["detail"], "first");
    }

    #[test]
    fn write_parallel_bail_no_attempt_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let sf = tmp.path().join("state.json");
        std::fs::write(&sf, r#"{"id":"g"}"#).unwrap();
        write_parallel_bail(Some(&sf), "a", "boom");
        assert!(std::fs::read_dir(tmp.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("bail_")));
    }
}
