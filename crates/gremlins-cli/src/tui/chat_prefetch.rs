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

use gremlins::landscape::LandscapeExplorer;

/// Maximum bytes for the entire pre-fetch block.
const MAX_BLOCK_BYTES: usize = 2000;

/// Maximum bytes per matcher section body (before the section header).
const MAX_PER_MATCHER_BYTES: usize = 800;

const MAX_GREMLIN_IDS: usize = 3;
const MAX_FILE_PATHS: usize = 5;
const MAX_DEFINITION_NAMES: usize = 3;

/// Pre-fetch context for the operator's message.
///
/// Returns a plain-text block to inject into the user message, or an empty
/// string when nothing was pre-fetched.
///
/// # Degradation
///
/// Every matcher degrades gracefully: a missing `state.json`, an unreadable
/// file, or a malformed definition silently omits that candidate — the
/// pre-fetch block never fails the turn.
pub(crate) fn prefetch_context(text: &str, project_root: &str) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(s) = prefetch_gremlins(text) {
        sections.push(s);
    }
    if let Some(s) = prefetch_files(text, project_root) {
        sections.push(s);
    }
    if let Some(s) = prefetch_definitions(text, project_root) {
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

fn prefetch_gremlins(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"\bgremlin-[a-zA-Z0-9]{6,12}\b").ok()?;
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<&str> = re
        .find_iter(text)
        .map(|m| m.as_str())
        .filter(|id| seen.insert(id.to_string()))
        .take(MAX_GREMLIN_IDS)
        .collect();

    if ids.is_empty() {
        return None;
    }

    let state_root = gremlins::config::state_root();
    let mut lines: Vec<String> = Vec::new();

    for &id in &ids {
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
    let bail_reason = state
        .get("bail_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut summary = format!("status={status}, stage={stage}");
    if !error.is_empty() {
        summary.push_str(&format!(", error={}", truncate_str(error, 80)));
    }
    if !bail_reason.is_empty() {
        summary.push_str(&format!(", bail_reason={}", truncate_str(bail_reason, 80)));
    }
    Some(summary)
}

// ── File path matcher ─────────────────────────────────────────────────────

fn prefetch_files(text: &str, project_root: &str) -> Option<String> {
    // Match likely file paths: word chars, slashes, hyphens, dots, ending
    // with a known extension.
    let re = regex::Regex::new(r"\b[\w./-]+\.(?:rs|md|json5|yaml|yml|toml|py|js|ts|sh|txt|json)\b")
        .ok()?;

    let candidates: Vec<&str> = re.find_iter(text).map(|m| m.as_str()).collect();

    let mut paths: Vec<&str> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in candidates {
        if !seen.insert(c.to_string()) {
            continue;
        }
        let c_str = c;
        // Reject absolute paths (join discards the base) and parent-dir
        // escapes — otherwise a candidate like `src/../../secret.md` would
        // be outlined and injected into chat context.
        if c_str.starts_with('/') || c_str.contains("..") {
            continue;
        }
        let resolved = Path::new(project_root).join(c_str);
        if resolved.is_file() {
            paths.push(c_str);
        }
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
        let full = Path::new(project_root).join(p);
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

fn prefetch_definitions(text: &str, project_root: &str) -> Option<String> {
    let definitions = gremlins::core::discovery::list_definitions(PathBuf::from(project_root));

    if definitions.is_empty() {
        return None;
    }

    let mut matched: Vec<&(String, PathBuf)> = Vec::new();
    for def in &definitions {
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
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let parsed: serde_json::Value = match json5::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let stage_names: Vec<String> = parsed
            .get("stages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if stage_names.is_empty() {
            continue;
        }

        let stage_list = stage_names.join(" → ");
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
        let result = prefetch_context("", "/tmp");
        assert!(result.is_empty());
    }

    #[test]
    fn no_matches_returns_empty() {
        let result = prefetch_context("just some random text here", "/tmp");
        assert!(result.is_empty());
    }

    // ── File path matcher ─────────────────────────────────────────────

    #[test]
    fn file_path_matcher_finds_real_files() {
        let dir = tempfile::tempdir().unwrap();
        let rs_path = dir.path().join("src").join("lib.rs");
        std::fs::create_dir_all(rs_path.parent().unwrap()).unwrap();
        std::fs::write(&rs_path, "pub fn foo() {}").unwrap();

        let text = "what does src/lib.rs do?";
        let result = prefetch_files(text, dir.path().to_str().unwrap());
        assert!(result.is_some());
        let body = result.unwrap();
        assert!(body.contains("src/lib.rs"));
        // Landscape should find the function.
        assert!(body.contains("fn|foo"));
    }

    #[test]
    fn file_path_matcher_rejects_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        // Create a file outside the project root.
        let outside = dir.path().parent().unwrap().join("secret.txt");
        std::fs::write(&outside, "top secret").unwrap();

        // Reference it via .. from inside the project.
        let text = "look at ../secret.txt";
        let result = prefetch_files(text, dir.path().to_str().unwrap());
        // Must not include the escaped file.
        if let Some(body) = result {
            assert!(!body.contains("secret.txt"));
        }
    }

    #[test]
    fn file_path_matcher_rejects_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        // Create a file at an absolute path inside the temp dir.
        let abs_file = dir.path().join("foo.rs");
        std::fs::write(&abs_file, "// test").unwrap();

        // Reference it by absolute path.
        let text = format!("look at {}", abs_file.display());
        let result = prefetch_files(&text, dir.path().to_str().unwrap());
        // Absolute paths must be rejected.
        if let Some(body) = result {
            assert!(!body.contains("foo.rs"));
        }
    }

    #[test]
    fn file_path_matcher_skips_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let text = "check src/lib.rs and src/main.rs";
        // Neither file exists.
        let result = prefetch_files(text, dir.path().to_str().unwrap());
        assert!(result.is_none());
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
    fn gremlin_id_regex_matches() {
        let re = regex::Regex::new(r"\bgremlin-[a-zA-Z0-9]{6,12}\b").unwrap();
        assert!(re.is_match("gremlin-abc123"));
        assert!(re.is_match("prefix gremlin-abc123def suffix"));
        assert!(re.is_match("gremlin-ZZZzzz999"));
        assert!(re.is_match("gremlin-abc123456789")); // 12 chars, max
        assert!(!re.is_match("gremlin-ab")); // too short (2 chars)
        assert!(!re.is_match("gremlin-abc123def4567")); // too long (13 chars)
    }

    #[test]
    fn prefetch_gremlins_deduplicates_and_caps() {
        // Create a temp sandbox so state_root() resolves to
        // $GREMLINS_SANDBOX_ROOT/state.
        let sandbox = tempfile::tempdir().unwrap();
        let state_dir = sandbox.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let attempt_dir = state_dir.join("gremlin-abc123-0001");
        std::fs::create_dir(&attempt_dir).unwrap();
        let state_json = serde_json::json!({
            "id": "gremlin-abc123",
            "status": "done",
            "stage": "implement",
            "error": "",
            "bail_reason": ""
        });
        std::fs::write(
            attempt_dir.join("state.json"),
            serde_json::to_string(&state_json).unwrap(),
        )
        .unwrap();

        let prev = std::env::var("GREMLINS_SANDBOX_ROOT").ok();
        unsafe { std::env::set_var("GREMLINS_SANDBOX_ROOT", sandbox.path()) };
        // Invalidate the cached global config so the new env var takes effect.
        gremlins::config::clear_global();

        let result = prefetch_gremlins(
            "what about gremlin-abc123? and gremlin-abc123 again? and gremlin-def456?",
        );

        // Restore.
        if let Some(v) = prev {
            unsafe { std::env::set_var("GREMLINS_SANDBOX_ROOT", v) };
        } else {
            unsafe { std::env::remove_var("GREMLINS_SANDBOX_ROOT") };
        }
        gremlins::config::clear_global();

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
        // A malformed attempt directory must not prevent finding a valid one.
        let sandbox = tempfile::tempdir().unwrap();
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
            "bail_reason": "tests failed"
        });
        std::fs::write(
            good_dir.join("state.json"),
            serde_json::to_string(&state_json).unwrap(),
        )
        .unwrap();

        let prev = std::env::var("GREMLINS_SANDBOX_ROOT").ok();
        unsafe { std::env::set_var("GREMLINS_SANDBOX_ROOT", sandbox.path()) };
        gremlins::config::clear_global();

        let result = prefetch_gremlins("gremlin-xyz789");

        if let Some(v) = prev {
            unsafe { std::env::set_var("GREMLINS_SANDBOX_ROOT", v) };
        } else {
            unsafe { std::env::remove_var("GREMLINS_SANDBOX_ROOT") };
        }
        gremlins::config::clear_global();

        let body = result.unwrap();
        assert!(body.contains("status=bailed"));
        assert!(body.contains("error=something went wrong"));
        assert!(body.contains("bail_reason=tests failed"));
    }

    // ── Block cap ─────────────────────────────────────────────────────

    #[test]
    fn prefetch_context_block_capped() {
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

        let result = prefetch_context(&text, dir.path().to_str().unwrap());

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
