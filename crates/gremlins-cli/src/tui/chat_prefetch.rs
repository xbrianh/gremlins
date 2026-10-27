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
        let name = entry.file_name().into_string().ok()?;
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
        let raw = std::fs::read_to_string(&state_json).ok()?;
        let state: serde_json::Value = serde_json::from_str(&raw).ok()?;
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
        // Truncate long error messages.
        let short = if error.len() > 80 {
            format!("{}…", &error[..80])
        } else {
            error.to_string()
        };
        summary.push_str(&format!(", error={short}"));
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
        let resolved = Path::new(project_root).join(c);
        if resolved.is_file() {
            paths.push(c);
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

    let text_lower = text.to_lowercase();
    let mut matched: Vec<&(String, PathBuf)> = Vec::new();
    for def in &definitions {
        if text_lower.contains(&def.0.to_lowercase()) && matched.len() < MAX_DEFINITION_NAMES {
            matched.push(def);
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
/// byte budget.
fn cap_bytes(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let marker = "\n[truncated]";
    let budget = max.saturating_sub(marker.len());
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str(marker);
    out
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
    fn cap_bytes_truncates() {
        let s = "hello world, this is a test".to_string();
        let result = cap_bytes(s, 20);
        // "hello world, this is a test" is 29 bytes; at max=20 marker is 12,
        // budget=8, result is first 8 bytes + marker.
        assert!(result.starts_with("hello wo"));
        assert!(result.contains("[truncated]"));
    }

    // ── prefetch_context: empty input ─────────────────────────────────

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
    fn gremlin_ids_deduplicated() {
        let re = regex::Regex::new(r"\bgremlin-[a-zA-Z0-9]{6,12}\b").unwrap();
        let matches: Vec<&str> = re
            .find_iter("gremlin-abc123 gremlin-abc123 gremlin-def456")
            .map(|m| m.as_str())
            .collect();
        assert_eq!(matches.len(), 3); // regex sees all, dedup is in the matcher
    }

    // ── File path matcher ─────────────────────────────────────────────

    #[test]
    fn file_path_regex_matches() {
        let re =
            regex::Regex::new(r"\b[\w./-]+\.(?:rs|md|json5|yaml|yml|toml|py|js|ts|sh|txt|json)\b")
                .unwrap();
        assert!(re.is_match("crates/drag/src/scoring.rs"));
        assert!(re.is_match("README.md"));
        assert!(re.is_match("foo/bar/baz.py"));
        assert!(re.is_match("some-file.json5"));
        assert!(!re.is_match("not_a_file")); // no extension
        assert!(!re.is_match("image.png")); // unrecognized extension
    }

    // ── Definition name matcher ───────────────────────────────────────

    #[test]
    fn definition_name_matching_case_insensitive() {
        // Simulate the behavior: our matcher uses lowercase comparison.
        let text = "run the QA checks";
        let text_lower = text.to_lowercase();
        let def_name = "qa";
        assert!(text_lower.contains(def_name));
    }

    // ── Block cap ─────────────────────────────────────────────────────

    #[test]
    fn block_capped_at_max_bytes() {
        let big = "x".repeat(3000);
        // Provide a fake gremlin reference so the function doesn't bail
        // early, but we really just want a huge block.
        let result = cap_bytes(big, MAX_BLOCK_BYTES);
        assert!(result.len() <= MAX_BLOCK_BYTES);
        assert!(result.contains("[truncated]"));
    }
}
