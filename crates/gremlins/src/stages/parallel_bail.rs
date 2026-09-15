//! Scanning per-child bail files and applying a group's bail policy.
//!
//! Ported from `gremlins/utils/parallel_bail.py`. Pure file I/O and JSON
//! parsing, so it lives in the PyO3-free crate and is unit-testable on its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::stages::parallel::BailPolicy;

/// One child that wrote a bail file, with the parsed payload.
#[derive(Debug, Clone, PartialEq)]
pub struct BailedChild {
    pub key: String,
    pub bail: HashMap<String, String>,
}

/// The outcome of applying a bail policy to the collected bails.
#[derive(Debug, Clone, PartialEq)]
pub struct BailDecision {
    pub should_bail: bool,
    pub first_bail: HashMap<String, String>,
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
            .and_then(|text| serde_json::from_str::<HashMap<String, String>>(&text).ok())
            .unwrap_or_else(|| {
                let mut fallback = HashMap::new();
                fallback.insert("class".to_string(), "other".to_string());
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
pub fn decide(bailed: &[BailedChild], total: usize, policy: BailPolicy) -> BailDecision {
    let should_bail = match policy {
        BailPolicy::Any => !bailed.is_empty(),
        BailPolicy::All => !bailed.is_empty() && bailed.len() == total,
    };
    BailDecision {
        should_bail,
        first_bail: bailed.first().map(|b| b.bail.clone()).unwrap_or_default(),
    }
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
        assert_eq!(bailed[0].bail.get("detail").map(String::as_str), Some("x"));
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
            bailed[0].bail.get("class").map(String::as_str),
            Some("other")
        );
    }

    #[test]
    fn decide_any_bails_on_one() {
        let bailed = vec![BailedChild {
            key: "a".into(),
            bail: HashMap::new(),
        }];
        assert!(decide(&bailed, 2, BailPolicy::Any).should_bail);
        assert!(!decide(&bailed, 2, BailPolicy::All).should_bail);
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
        assert!(decide(&bailed, 2, BailPolicy::All).should_bail);
        assert!(!decide(&[], 2, BailPolicy::Any).should_bail);
    }

    #[test]
    fn decide_reports_first_bail() {
        let mut bail = HashMap::new();
        bail.insert("class".to_string(), "other".to_string());
        let bailed = vec![BailedChild {
            key: "a".into(),
            bail: bail.clone(),
        }];
        assert_eq!(decide(&bailed, 1, BailPolicy::Any).first_bail, bail);
        assert!(decide(&[], 1, BailPolicy::Any).first_bail.is_empty());
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
}
