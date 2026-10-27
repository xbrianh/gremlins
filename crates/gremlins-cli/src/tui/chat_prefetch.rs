//! Message-aware context pre-fetching.
//!
//! Scans the operator's message for gremlin IDs, file paths, and definition
//! names, then fetches a bounded context block to inject into the user message
//! so the model doesn't burn its first turn on discovery tools.
//!
//! Each matcher is bounded: at most N candidates, at most B output bytes per
//! matcher. Failures (missing file, bad state.json, timeout) silently omit
//! that candidate. The overall pre-fetch block is capped at 2000 bytes so it
//! never crowds out the operator's actual message.
//!
//! This module is designed to run inside `spawn_blocking` under a timeout —
//! it performs synchronous I/O (directory scans, file reads, landscape
//! outlines) and must never execute on the TUI's async event-loop thread.

use std::path::{Path, PathBuf};
use std::time::Instant;

use gremlins::definition::r#static::loader::{fill_names, StageEntry};
use gremlins::landscape::LandscapeExplorer;

/// Maximum bytes for the entire pre-fetch block.
const MAX_BLOCK_BYTES: usize = 2000;

/// Maximum bytes per matcher section body (before the section header).
const MAX_PER_MATCHER_BYTES: usize = 800;

/// Maximum file size in bytes to pass to Landscape's outline_file.
/// Matches `MAX_OUTLINE_BYTES` from `gremlins::landscape`.
const MAX_OUTLINE_FILE_BYTES: u64 = 1024 * 1024;

const MAX_GREMLIN_IDS: usize = 3;
const MAX_FILE_PATHS: usize = 5;
const MAX_DEFINITION_NAMES: usize = 3;

/// Pre-fetch context for the operator's message.
///
/// Returns a plain-text block to inject into the user message, or an empty
/// string when nothing was pre-fetched.
///
/// `deadline` is the absolute `Instant` after which further work should be
/// abandoned. Each matcher and each candidate within a matcher checks the
/// deadline so that completed sections are returned even when the caller's
/// timeout fires mid-way.
///
/// # Degradation
///
/// Every matcher degrades gracefully: a missing `state.json`, an unreadable
/// file, or a malformed definition silently omits that candidate — the
/// pre-fetch block never fails the turn.
pub(crate) fn prefetch_context(text: &str, project_root: &str, deadline: Instant) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(s) = prefetch_gremlins(text, deadline) {
        sections.push(s);
    }
    if let Some(s) = prefetch_files(text, project_root, deadline) {
        sections.push(s);
    }
    if let Some(s) = prefetch_definitions(text, project_root, deadline) {
        sections.push(s);
    }

    if sections.is_empty() {
        return String::new();
    }

    let mut block = String::from("## Pre-fetched context\n\n");
    for section in &sections {
        block.push_str(section);
        block.push('\n');
    }

    cap_bytes(block, MAX_BLOCK_BYTES)
}

// ── Gremlin ID matcher ────────────────────────────────────────────────────

fn prefetch_gremlins(text: &str, deadline: Instant) -> Option<String> {
    // Match two ID formats:
    //   1. Legacy: gremlin-<6-12 alphanumeric chars>
    //   2. Persistent (Gremlin::init): <definition_name>-<4 hex digits>
    let re_legacy = regex::Regex::new(r"\bgremlin-[a-zA-Z0-9]{6,12}\b").ok()?;
    let re_persistent = regex::Regex::new(r"\b[a-zA-Z][a-zA-Z0-9_]*[_-][a-fA-F0-9]{4}\b").ok()?;

    let mut seen = std::collections::HashSet::new();
    let mut ids: Vec<&str> = Vec::new();

    for m in re_legacy.find_iter(text) {
        if ids.len() >= MAX_GREMLIN_IDS {
            break;
        }
        let s = m.as_str();
        if seen.insert(s.to_string()) {
            ids.push(s);
        }
    }
    for m in re_persistent.find_iter(text) {
        if ids.len() >= MAX_GREMLIN_IDS {
            break;
        }
        let s = m.as_str();
        if seen.insert(s.to_string()) {
            ids.push(s);
        }
    }

    if ids.is_empty() {
        return None;
    }

    let state_root = gremlins::config::state_root();
    let mut lines: Vec<String> = Vec::new();

    for &id in &ids {
        if Instant::now() >= deadline {
            break;
        }
        let summary = match latest_attempt_summary(id, &state_root) {
            Some(s) => s,
            None => continue,
        };
        let line = format!("{id}: {summary}");
        // Cap each individual line at ~200 chars just in case
        lines.push(cap_bytes(line, 240));
    }

    if lines.is_empty() {
        return None;
    }

    let body = lines.join("\n");
    let body = cap_bytes(body, MAX_PER_MATCHER_BYTES);
    Some(format!("### Gremlins\n{body}"))
}

/// Find the lexicographically largest attempt directory for `gremlin_id` and
/// return a compact one-line summary from its `state.json`.
///
/// Bail details are read from `bail_<attempt>.json` (the format used by
/// [`gremlins::executor::state::FileSystemStateStore::stage_error`]), not
/// from a `bail_reason` field in `state.json`.
fn latest_attempt_summary(gremlin_id: &str, state_root: &Path) -> Option<String> {
    let prefix = format!("{gremlin_id}-");
    let mut best: Option<(String, PathBuf)> = None;

    let entries = std::fs::read_dir(state_root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let suffix = &name[prefix.len()..];
        // Match both 4-digit attempt suffixes (legacy: gremlin-abc123-0001)
        // and 4-hex-digit ID suffixes (persistent: qa-ab12 where the dir is
        // qa-ab12-0001).
        if suffix.len() != 4 || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let state_json = path.join("state.json");
        if !state_json.is_file() {
            continue;
        }
        // Verify recorded id matches.
        let Ok(raw) = std::fs::read_to_string(&state_json) else {
            continue;
        };
        let Ok(state): Result<serde_json::Value, _> = serde_json::from_str(&raw) else {
            continue;
        };
        let recorded = state.get("id").and_then(|v| v.as_str());
        if recorded != Some(gremlin_id) {
            continue;
        }
        if best.as_ref().is_none_or(|(best_name, _)| name > *best_name) {
            best = Some((name, path));
        }
    }

    let (_dir_name, dir_path) = best?;
    let state_json = dir_path.join("state.json");
    let raw = std::fs::read_to_string(&state_json).ok()?;
    let state: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let status = state.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let stage = state.get("stage").and_then(|v| v.as_str()).unwrap_or("?");
    let error = state.get("error").and_then(|v| v.as_str()).unwrap_or("");

    let mut summary = format!("status={status}, stage={stage}");
    if !error.is_empty() {
        summary.push_str(&format!(", error={}", truncate_str(error, 80)));
    }

    // Read bail detail from bail_<attempt>.json, not from state.json.
    let attempt = state
        .get("attempt")
        .and_then(|v| v.as_str())
        .or_else(|| state.get("attempt").and_then(|v| v.as_i64()).map(|_| ""))
        .unwrap_or("");
    if !attempt.is_empty() {
        let bail_path = dir_path.join(format!("bail_{attempt}.json"));
        if let Ok(bail_raw) = std::fs::read_to_string(&bail_path) {
            if let Ok(bail_val) = serde_json::from_str::<serde_json::Value>(&bail_raw) {
                if let Some(detail) = bail_val.get("detail").and_then(|v| v.as_str()) {
                    summary.push_str(&format!(", bail_reason={}", truncate_str(detail, 80)));
                }
            }
        }
    }

    Some(summary)
}

// ── File path matcher ─────────────────────────────────────────────────────

fn prefetch_files(text: &str, project_root: &str, deadline: Instant) -> Option<String> {
    // Match likely file paths. Allow a leading dot or slash so paths like
    // `.gremlins/qa.json5` and `.github/workflows/ci.yml` are matched.
    // The subsequent path checks validate each candidate.
    let re = regex::Regex::new(
        r"(?:^|\s)([/.]?[\w./-]+\.(?:rs|md|json5|yaml|yml|toml|py|js|ts|sh|txt|json))\b",
    )
    .ok()?;

    let candidates: Vec<&str> = re
        .captures_iter(text)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str())
        .collect();

    let mut paths: Vec<&str> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Canonicalize the project root once.
    let canonical_root = Path::new(project_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_root));

    for c in candidates {
        if Instant::now() >= deadline {
            break;
        }
        if !seen.insert(c.to_string()) {
            continue;
        }
        // Reject absolute paths (join discards the base) and parent-dir
        // escapes — otherwise a candidate like `src/../../secret.md` would
        // be outlined and injected into chat context.
        if c.starts_with('/') || c.contains("..") {
            continue;
        }
        let resolved = Path::new(project_root).join(c);
        if !resolved.is_file() {
            continue;
        }
        // Canonicalize the candidate and ensure it stays within the project
        // root. Symlinks can point outside the project, so we must validate
        // the real location rather than trusting the lexical path.
        let canonical_candidate = match resolved.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !canonical_candidate.starts_with(&canonical_root) {
            continue;
        }
        paths.push(c);
        if paths.len() >= MAX_FILE_PATHS {
            break;
        }
    }

    if paths.is_empty() {
        return None;
    }

    let explorer = LandscapeExplorer;
    let mut lines: Vec<String> = Vec::new();

    for &p in &paths {
        if Instant::now() >= deadline {
            break;
        }
        let full = Path::new(project_root).join(p);

        // Apply a size limit before outlining — `outline_file` reads the
        // entire file into memory, so a large file would cause an unbounded
        // allocation.
        let file_len = match std::fs::metadata(&full) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if file_len > MAX_OUTLINE_FILE_BYTES {
            continue;
        }

        let outline_str = match explorer.outline_file(&full, false) {
            Ok(items) if !items.is_empty() => items
                .iter()
                .map(|item| format!("{}|{}|{}", item.line, item.kind, item.name))
                .collect::<Vec<_>>()
                .join(" "),
            Ok(_) => String::from("(no structural items)"),
            Err(_) => continue,
        };
        lines.push(format!("{}:\n{}", p, outline_str));
    }

    if lines.is_empty() {
        return None;
    }

    let body = lines.join("\n\n");
    let body = cap_bytes(body, MAX_PER_MATCHER_BYTES);
    Some(format!("### Files\n{body}"))
}

// ── Definition name matcher ───────────────────────────────────────────────

fn prefetch_definitions(text: &str, project_root: &str, deadline: Instant) -> Option<String> {
    let definitions = gremlins::core::discovery::list_definitions(PathBuf::from(project_root));

    if definitions.is_empty() {
        return None;
    }

    let mut matched: Vec<&(String, PathBuf)> = Vec::new();
    for def in &definitions {
        if Instant::now() >= deadline {
            break;
        }
        if matched.len() >= MAX_DEFINITION_NAMES {
            break;
        }
        // Match on word boundaries so short names like "qa" don't match
        // "quality" and single-letter names don't match every message.
        let escaped = regex::escape(&def.0);
        let pattern = format!(r"(?i)\b{}\b", escaped);
        if let Ok(re) = regex::Regex::new(&pattern) {
            if re.is_match(text) {
                matched.push(def);
            }
        }
    }

    if matched.is_empty() {
        return None;
    }

    let mut lines: Vec<String> = Vec::new();

    for (name, path) in &matched {
        if Instant::now() >= deadline {
            break;
        }
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let parsed: serde_json::Value = match json5::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Build StageEntry list from the raw JSON, then apply the canonical
        // naming/expansion rules via fill_names so unnamed stages get their
        // auto-generated names (e.g. "exec", "exec-2") and duplicates are
        // disambiguated.
        let mut entries: Vec<StageEntry> = parsed
            .get("stages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|s| StageEntry {
                        name: s.get("name").and_then(|n| n.as_str()).map(String::from),
                        auto_name: s.get("type").and_then(|n| n.as_str()).map(String::from),
                        stage_type: s.get("type").and_then(|n| n.as_str()).map(String::from),
                    })
                    .collect()
            })
            .unwrap_or_default();

        if entries.is_empty() {
            continue;
        }

        if fill_names(&mut entries).is_err() {
            continue;
        }

        let stage_list: Vec<String> = entries.iter().filter_map(|e| e.name.clone()).collect();

        if stage_list.is_empty() {
            continue;
        }

        let stage_list = stage_list.join(" → ");
        lines.push(format!("{name}: {stage_list}"));
    }

    if lines.is_empty() {
        return None;
    }

    let body = lines.join("\n");
    let body = cap_bytes(body, MAX_PER_MATCHER_BYTES);
    Some(format!("### Definitions\n{body}"))
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Truncate `s` to at most `max` bytes on a character boundary, appending
/// a truncation marker when bytes are dropped. The marker is included in the
/// byte budget. When `max` is smaller than the marker itself the result is a
/// plain truncation without the marker.
fn cap_bytes(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let marker = "\n[truncated]";
    if max <= marker.len() {
        // Budget too small even for the marker — return a plain char-boundary
        // truncation.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        return s[..end].to_string();
    }
    let budget = max - marker.len();
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str(marker);
    out
}

/// Truncate `s` to at most `max_chars` characters on a character boundary,
/// appending `…` when characters are dropped.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // Serialize tests that mutate the process-wide `GREMLINS_SANDBOX_ROOT`
    // and `gremlins::config::clear_global()`. This lock is shared with the
    // `chat_context` test module because both mutate the same process-global
    // environment/config; a per-module lock would let them race.
    use crate::tui::test_support::CONFIG_ENV_LOCK as GLOBAL_CONFIG_LOCK;

    /// Set up a temporary sandbox directory as `GREMLINS_SANDBOX_ROOT`.
    /// Returns the guard (must be held for the test duration) and the
    /// sandbox path. On drop the env var is restored and global config
    /// is cleared.
    struct SandboxGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
        prev: Option<String>,
    }

    impl SandboxGuard {
        fn new() -> Self {
            let lock = GLOBAL_CONFIG_LOCK.lock().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let prev = std::env::var("GREMLINS_SANDBOX_ROOT").ok();
            std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
            gremlins::config::clear_global();
            SandboxGuard {
                _lock: lock,
                dir,
                prev,
            }
        }

        fn path(&self) -> &std::path::Path {
            self.dir.path()
        }
    }

    impl Drop for SandboxGuard {
        fn drop(&mut self) {
            match self.prev.as_ref() {
                Some(v) => std::env::set_var("GREMLINS_SANDBOX_ROOT", v),
                None => std::env::remove_var("GREMLINS_SANDBOX_ROOT"),
            }
            gremlins::config::clear_global();
        }
    }

    // ── cap_bytes ─────────────────────────────────────────────────────

    #[test]
    fn cap_bytes_no_truncation() {
        let s = "hello".to_string();
        assert_eq!(cap_bytes(s, 10), "hello");
    }

    #[test]
    fn cap_bytes_exact_boundary() {
        let s = "hello".to_string();
        assert_eq!(cap_bytes(s, 5), "hello");
    }

    #[test]
    fn cap_bytes_truncates() {
        let s = "hello world, this is a test".to_string();
        let result = cap_bytes(s, 20);
        assert!(result.starts_with("hello wo"));
        assert!(result.contains("[truncated]"));
        assert!(result.len() <= 20);
    }

    #[test]
    fn cap_bytes_below_marker_size() {
        // max is smaller than the "\n[truncated]" marker (12 bytes).
        let s = "hello world".to_string();
        let result = cap_bytes(s, 5);
        assert!(!result.contains("[truncated]"));
        assert_eq!(result, "hello");
    }

    #[test]
    fn cap_bytes_zero_max() {
        let s = "abc".to_string();
        let result = cap_bytes(s, 0);
        assert!(result.is_empty());
    }

    // ── truncate_str ──────────────────────────────────────────────────

    #[test]
    fn truncate_str_no_truncation() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_truncates() {
        let result = truncate_str("hello world this is long", 10);
        assert!(result.starts_with("hello worl"));
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_str_utf8_boundary() {
        // Multi-byte characters — must not panic.
        let result = truncate_str("héllo wörld with ünicode", 8);
        assert!(result.ends_with('…'));
        // Should have exactly 8 chars + ellipsis.
        assert_eq!(result.chars().count(), 9); // 8 + '…'
    }

    // ── prefetch_context: empty / no-match ────────────────────────────

    #[test]
    fn empty_text_returns_empty() {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = prefetch_context("", "/tmp", deadline);
        assert!(result.is_empty());
    }

    #[test]
    fn no_matches_returns_empty() {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = prefetch_context("just some random text here", "/tmp", deadline);
        assert!(result.is_empty());
    }

    // ── File path matcher ─────────────────────────────────────────────

    #[test]
    fn file_path_matcher_finds_real_files() {
        let _guard = GLOBAL_CONFIG_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let rs_path = dir.path().join("src").join("lib.rs");
        std::fs::create_dir_all(rs_path.parent().unwrap()).unwrap();
        std::fs::write(&rs_path, "pub fn foo() {}").unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let text = "what does src/lib.rs do?";
        let result = prefetch_files(text, dir.path().to_str().unwrap(), deadline);
        assert!(result.is_some());
        let body = result.unwrap();
        assert!(body.contains("src/lib.rs"));
        // Landscape should find the function.
        assert!(body.contains("fn|foo"));
    }

    #[test]
    fn file_path_matcher_rejects_parent_dir_escape() {
        let _guard = GLOBAL_CONFIG_LOCK.lock().unwrap();
        // Create an owned temporary parent containing both the project
        // directory and the external fixture, rather than writing into
        // the shared system temp directory.
        let parent = tempfile::tempdir().unwrap();
        let project = parent.path().join("project");
        std::fs::create_dir(&project).unwrap();

        // Create a file outside the project root (but inside the owned parent).
        let outside = parent.path().join("secret.txt");
        std::fs::write(&outside, "top secret").unwrap();

        // Reference it via .. from inside the project.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let text = "look at ../secret.txt";
        let result = prefetch_files(text, project.to_str().unwrap(), deadline);
        // Must not include the escaped file.
        if let Some(body) = result {
            assert!(!body.contains("secret.txt"));
        }
    }

    #[test]
    fn file_path_matcher_rejects_absolute_paths() {
        let _guard = GLOBAL_CONFIG_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Create a file at an absolute path inside the temp dir.
        let abs_file = dir.path().join("foo.rs");
        std::fs::write(&abs_file, "// test").unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        // Reference it by absolute path.
        let text = format!("look at {}", abs_file.display());
        let result = prefetch_files(&text, dir.path().to_str().unwrap(), deadline);
        // Absolute paths must be rejected.
        if let Some(body) = result {
            assert!(!body.contains("foo.rs"));
        }
    }

    #[test]
    fn file_path_matcher_skips_nonexistent() {
        let _guard = GLOBAL_CONFIG_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let text = "check src/lib.rs and src/main.rs";
        // Neither file exists.
        let result = prefetch_files(text, dir.path().to_str().unwrap(), deadline);
        assert!(result.is_none());
    }

    #[test]
    fn file_path_matcher_matches_leading_dot_paths() {
        let _guard = GLOBAL_CONFIG_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let gremlins_dir = dir.path().join(".gremlins");
        std::fs::create_dir(&gremlins_dir).unwrap();
        std::fs::write(gremlins_dir.join("qa.json5"), "stages: []").unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let text = "check .gremlins/qa.json5 for the stages";
        let result = prefetch_files(text, dir.path().to_str().unwrap(), deadline);
        assert!(result.is_some());
        let body = result.unwrap();
        assert!(body.contains(".gremlins/qa.json5"));
    }

    #[test]
    fn file_path_matcher_rejects_symlink_outside_root() {
        let _guard = GLOBAL_CONFIG_LOCK.lock().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let project = parent.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let outside = parent.path().join("outside.rs");
        std::fs::write(&outside, "pub fn secret() {}").unwrap();

        // Create a symlink inside the project pointing outside.
        let link = project.join("linked.rs");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let text = "look at linked.rs";
        let result = prefetch_files(text, project.to_str().unwrap(), deadline);
        // Must not include the symlinked outside file.
        if let Some(body) = result {
            assert!(!body.contains("outside.rs"));
        }
    }

    // ── Definition name matcher: word-boundary matching ───────────────

    #[test]
    fn definition_word_boundary_matches() {
        // "qa" should match standalone "qa" but not "equal" or "quality".
        let escaped = regex::escape("qa");
        let re = regex::Regex::new(&format!(r"(?i)\b{}\b", escaped)).unwrap();
        assert!(re.is_match("run the qa stage"));
        assert!(re.is_match("QA checks"));
        assert!(!re.is_match("quality assurance"));
        assert!(!re.is_match("equal measure"));
    }

    #[test]
    fn definition_short_name_not_substring() {
        // "rs" should not match every message containing "vers" or "first".
        let escaped = regex::escape("rs");
        let re = regex::Regex::new(&format!(r"(?i)\b{}\b", escaped)).unwrap();
        assert!(re.is_match("the rs file"));
        assert!(!re.is_match("first attempt"));
        assert!(!re.is_match("version 2"));
    }

    // ── Gremlin ID matcher ────────────────────────────────────────────

    #[test]
    fn gremlin_id_regex_matches_legacy() {
        let re = regex::Regex::new(r"\bgremlin-[a-zA-Z0-9]{6,12}\b").unwrap();
        assert!(re.is_match("gremlin-abc123"));
        assert!(re.is_match("prefix gremlin-abc123def suffix"));
        assert!(re.is_match("gremlin-ZZZzzz999"));
        assert!(re.is_match("gremlin-abc123456789")); // 12 chars, max
        assert!(!re.is_match("gremlin-ab")); // too short (2 chars)
        assert!(!re.is_match("gremlin-abc123def4567")); // too long (13 chars)
    }

    #[test]
    fn gremlin_id_regex_matches_persistent() {
        // Persistent IDs from Gremlin::init: <definition_name>-<4 hex digits>
        let re = regex::Regex::new(r"\b[a-zA-Z][a-zA-Z0-9_]*[_-][a-fA-F0-9]{4}\b").unwrap();
        assert!(re.is_match("qa-ab12"));
        assert!(re.is_match("look at qa-ab12 state"));
        assert!(re.is_match("my_def-ABCD"));
        assert!(re.is_match("foo_bar-00ff"));
        assert!(!re.is_match("ab12")); // no leading alpha
        assert!(!re.is_match("qa-ab123")); // 5 hex digits
        assert!(!re.is_match("qa-ab1")); // 3 hex digits
    }

    #[test]
    fn prefetch_gremlins_finds_persistent_id() {
        let sandbox = SandboxGuard::new();
        let state_dir = sandbox.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let attempt_dir = state_dir.join("qa-ab12-0001");
        std::fs::create_dir(&attempt_dir).unwrap();
        let state_json = serde_json::json!({
            "id": "qa-ab12",
            "status": "done",
            "stage": "review",
            "error": "",
        });
        std::fs::write(
            attempt_dir.join("state.json"),
            serde_json::to_string(&state_json).unwrap(),
        )
        .unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = prefetch_gremlins("what about qa-ab12?", deadline);
        let body = result.unwrap();
        assert!(body.contains("qa-ab12"));
        assert!(body.contains("status=done"));
    }

    #[test]
    fn prefetch_gremlins_deduplicates_and_caps() {
        let sandbox = SandboxGuard::new();
        let state_dir = sandbox.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let attempt_dir = state_dir.join("gremlin-abc123-0001");
        std::fs::create_dir(&attempt_dir).unwrap();
        let state_json = serde_json::json!({
            "id": "gremlin-abc123",
            "status": "done",
            "stage": "implement",
            "error": "",
        });
        std::fs::write(
            attempt_dir.join("state.json"),
            serde_json::to_string(&state_json).unwrap(),
        )
        .unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = prefetch_gremlins(
            "what about gremlin-abc123? and gremlin-abc123 again? and gremlin-def456?",
            deadline,
        );

        let body = result.unwrap();
        // Should mention the found gremlin.
        assert!(body.contains("gremlin-abc123"));
        assert!(body.contains("status=done"));
        // "gremlin-def456" has no state dir and should be silently omitted.
        assert!(!body.contains("gremlin-def456"));
        // Dedup: gremlin-abc123 appears only once in the output.
        let count = body.matches("gremlin-abc123").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn prefetch_gremlins_degraded_by_bad_entry() {
        let sandbox = SandboxGuard::new();
        let state_dir = sandbox.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();

        // Bad entry: directory with no state.json — should be skipped.
        let bad_dir = state_dir.join("gremlin-xyz789-0002");
        std::fs::create_dir(&bad_dir).unwrap();
        // No state.json — should be skipped.

        let good_dir = state_dir.join("gremlin-xyz789-0001");
        std::fs::create_dir(&good_dir).unwrap();
        let state_json = serde_json::json!({
            "id": "gremlin-xyz789",
            "status": "bailed",
            "stage": "review",
            "error": "something went wrong",
            "attempt": "0001",
        });
        std::fs::write(
            good_dir.join("state.json"),
            serde_json::to_string(&state_json).unwrap(),
        )
        .unwrap();

        // Write bail_0001.json with the real bail detail.
        let bail_json = serde_json::json!({
            "class": "Bail",
            "detail": "tests failed",
            "ts": "2025-01-01T00:00:00Z",
        });
        std::fs::write(
            good_dir.join("bail_0001.json"),
            serde_json::to_string(&bail_json).unwrap(),
        )
        .unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = prefetch_gremlins("gremlin-xyz789", deadline);

        let body = result.unwrap();
        assert!(body.contains("status=bailed"));
        assert!(body.contains("error=something went wrong"));
        // Bail reason comes from bail_0001.json.
        assert!(body.contains("bail_reason=tests failed"));
    }

    // ── Block cap ─────────────────────────────────────────────────────

    #[test]
    fn prefetch_context_block_capped() {
        let _guard = GLOBAL_CONFIG_LOCK.lock().unwrap();
        // Create a temp project with many matching files to blow past the
        // 2000-byte block cap.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            let path = dir.path().join(format!("mod{i}.rs"));
            let content =
                format!("// Module {i}\npub fn func_{i}() {{}}\npub struct Struct{i} {{}}\n");
            std::fs::write(&path, content).unwrap();
        }

        // Build a message referencing all of them.
        let refs: Vec<String> = (0..20).map(|i| format!("mod{i}.rs")).collect();
        let text = refs.join(" ");

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = prefetch_context(&text, dir.path().to_str().unwrap(), deadline);

        // Must not exceed the overall cap.
        assert!(
            result.len() <= MAX_BLOCK_BYTES,
            "block {result_len} > max {MAX_BLOCK_BYTES}",
            result_len = result.len()
        );
        // If it was truncated, the marker should be present.
        if result.len() > 1000 {
            assert!(result.contains("[truncated]"));
        }
    }
}
