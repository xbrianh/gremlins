use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::util::ComprehensibilityScores;

/// One line in the JSONL cache file.
///
/// The cache key is model-dependent:
/// `blake3(content || "\0" || model_uri || "\0" || estimates || "\0" || config_hash)`.
///
/// **What the cache actually fast-paths:** the `raw_estimates` are restored
/// on a cache hit and fed through the same aggregation path as fresh
/// estimates.  Aggregation and scoring are pure math — always recomputed.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct CacheEntry {
    /// BLAKE3 hash of
    /// (content || "\0" || model_uri || "\0" || estimates || "\0" || config_hash),
    /// hex-encoded, first 16 chars. This is the lookup key.
    pub key: String,
    /// Absolute path of the source file at scoring time.
    pub file: String,
    /// Fully-resolved model URI used for scoring.
    pub model: String,
    /// Token count from tiktoken BPE (model-independent; stored here for convenience).
    pub token_count: usize,
    /// Number of estimates used to produce the mean.
    #[serde(default = "default_estimates")]
    pub estimates: usize,
    /// Per-estimate raw dimension scores (0–4 per dimension).
    /// Aggregation to a 0–1 score is pure math — recomputed from these.
    #[serde(default)]
    pub raw_estimates: Vec<ComprehensibilityScores>,
}

fn default_estimates() -> usize {
    1
}

/// Compute a BLAKE3-based cache key from the file's full source text,
/// the resolved model URI, the estimate count, and the survey config hash.
///
/// Key = first 16 hex chars of
/// `blake3(content || "\0" || model_uri || "\0" || estimates || "\0" || config_hash)`.
/// The 64-bit truncation is fine at realistic cache sizes but collision
/// risk grows past ~10⁵–10⁶ entries for strict uniqueness guarantees.
pub(crate) fn compute_cache_key(
    content: &str,
    model_uri: &str,
    estimates: usize,
    config_hash: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(content.as_bytes());
    hasher.update(b"\0");
    hasher.update(model_uri.as_bytes());
    hasher.update(b"\0");
    hasher.update(&estimates.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(config_hash.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

/// Load a JSONL cache file, returning a map keyed by `CacheEntry::key`.
///
/// An empty or missing file is treated as a cold cache (empty map).
/// Lines that are not valid JSON or fail to deserialize as a [`CacheEntry`]
/// are skipped with a warning (run with `--clear-cache` to rebuild).
pub(crate) fn load_cache(path: &Path) -> Result<HashMap<String, CacheEntry>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read cache file {}: {e}", path.display()))?;
    let mut map: HashMap<String, CacheEntry> = HashMap::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<CacheEntry>(line) {
            Ok(entry) => {
                match map.entry(entry.key.clone()) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        // Duplicate key: merge raw estimates but cap at the
                        // target count so stale lines from a previous
                        // interrupted run don't cause overshoot.
                        let existing = e.get_mut();
                        let needed = entry.estimates.saturating_sub(existing.raw_estimates.len());
                        existing
                            .raw_estimates
                            .extend(entry.raw_estimates.iter().take(needed).cloned());
                        existing.estimates = existing.raw_estimates.len();
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        // Normalize `estimates` to the actual raw count
                        // so a partial entry from an interrupted run
                        // (e.g. estimates=3 but only 1 raw_estimate
                        // written before crash) is treated as a cache
                        // miss rather than accepted with incomplete data.
                        let mut entry = entry;
                        entry.estimates = entry.raw_estimates.len();
                        e.insert(entry);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "  WARNING: {}:{}: skipping invalid cache entry: {e}. Run with --clear-cache to rebuild.",
                    path.display(),
                    line_no + 1
                );
            }
        }
    }
    Ok(map)
}

/// Append one [`CacheEntry`] as a JSON line to the cache file.
///
/// Creates the file (and parent directories) if they do not exist.
pub(crate) fn append_cache_entry(path: &Path, entry: &CacheEntry) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create cache dir {}: {e}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open cache file {}: {e}", path.display()))?;
    let mut line = serde_json::to_string(entry).map_err(|e| format!("serialization error: {e}"))?;
    line.push('\n');
    std::io::Write::write_all(&mut file, line.as_bytes())
        .map_err(|e| format!("failed to write cache entry: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_deterministic() {
        let k1 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1, "cfg-a");
        let k2 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1, "cfg-a");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16);
        // Different content → different key.
        let k3 = compute_cache_key("fn main() { /* changed */ }", "openai/gpt-4o", 1, "cfg-a");
        assert_ne!(k1, k3);
        // Different model → different key.
        let k4 = compute_cache_key("fn main() {}", "anthropic/claude", 1, "cfg-a");
        assert_ne!(k1, k4);
        // Different estimates → different key.
        let k5 = compute_cache_key("fn main() {}", "openai/gpt-4o", 5, "cfg-a");
        assert_ne!(k1, k5);
        // Different config hash → different key.
        let k6 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1, "cfg-b");
        assert_ne!(k1, k6);
    }

    #[test]
    fn load_cache_missing_file_is_cold() {
        let map = load_cache(Path::new("/nonexistent/cache/file.jsonl")).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn load_cache_blank_lines_skipped() {
        let dir = std::env::temp_dir().join("drag-test-blank-line");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        // Line 2 is blank; it should be skipped, leaving the valid entry.
        std::fs::write(
            &cache_path,
            "{\"key\":\"abc\",\"file\":\"/f.rs\",\"model\":\"m\",\"token_count\":10,\"raw_estimates\":[{\"naming\":2,\"control_flow\":2,\"data_flow\":2,\"abstractions\":2,\"cohesion\":1,\"sizing\":1,\"test_hygiene\":1}]}\n\n",
        )
        .unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("abc"));
    }

    #[test]
    fn load_cache_malformed_line_skipped() {
        let dir = std::env::temp_dir().join("drag-test-malformed");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        std::fs::write(&cache_path, "not valid json\n").unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(map.is_empty());
    }

    #[test]
    fn load_cache_valid_entries() {
        let dir = std::env::temp_dir().join("drag-test-valid");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let line = "{\"key\":\"deadbeef12345678\",\"file\":\"/src/main.rs\",\"model\":\"openai/gpt-4o\",\"token_count\":42,\"raw_estimates\":[{\"naming\":3,\"control_flow\":3,\"data_flow\":3,\"abstractions\":3,\"cohesion\":3,\"sizing\":3,\"test_hygiene\":3}]}\n";
        std::fs::write(&cache_path, line).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let entry = map.get("deadbeef12345678").unwrap();
        assert_eq!(entry.raw_estimates.len(), 1);
        assert_eq!(entry.raw_estimates[0].get("naming"), Some(&3));
    }

    #[test]
    fn load_cache_accepts_entries_ignoring_extra_fields() {
        let dir = std::env::temp_dir().join("drag-test-v1-reject");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let line = "{\"key\":\"deadbeef12345678\",\"file\":\"/src/main.rs\",\"model\":\"openai/gpt-4o\",\"token_count\":42,\"fan_in\":1,\"fan_out\":2,\"graph_complexity\":3,\"raw_estimates\":[{\"naming\":3,\"control_flow\":3,\"data_flow\":3,\"abstractions\":3,\"cohesion\":3,\"sizing\":3,\"test_hygiene\":3}]}\n";
        std::fs::write(&cache_path, line).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("deadbeef12345678"));
    }

    #[test]
    fn load_cache_deser_old_format_falls_back() {
        // Old cache entries with comprehensibility/breakdown (instead of raw_estimates)
        // should deserialize with an empty raw_estimates vec, triggering re-score.
        let dir = std::env::temp_dir().join("drag-test-comp-oldformat");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let line = "{\"key\":\"oldkey0000000000\",\"file\":\"/f.rs\",\"model\":\"m\",\"token_count\":10,\"comprehensibility\":0.5,\"breakdown\":{\"naming\":2}}\n";
        std::fs::write(&cache_path, line).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let entry = map.get("oldkey0000000000").unwrap();
        assert!(entry.raw_estimates.is_empty());
    }

    #[test]
    fn append_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("drag-test-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let raw_estimates: Vec<ComprehensibilityScores> = vec![[
            ("naming", 2),
            ("control_flow", 2),
            ("data_flow", 2),
            ("abstractions", 2),
            ("cohesion", 0),
            ("sizing", 0),
            ("test_hygiene", 0),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()];
        let entry = CacheEntry {
            key: "abcd1234abcd1234".to_string(),
            file: "/abs/path/module.rs".to_string(),
            model: "openai/gpt-4o".to_string(),
            token_count: 100,
            estimates: 1,
            raw_estimates,
        };
        append_cache_entry(&cache_path, &entry).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let loaded = map.get("abcd1234abcd1234").unwrap();
        assert_eq!(loaded.raw_estimates.len(), 1);
        assert_eq!(loaded.raw_estimates[0].get("naming"), Some(&2));
        assert_eq!(loaded.file, "/abs/path/module.rs");
        assert_eq!(loaded.model, "openai/gpt-4o");
    }
}
