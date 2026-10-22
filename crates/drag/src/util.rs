// Shared utilities for gremlins-drag.
//
// This module holds types and functions that are used across both the binary
// orchestrator (`main.rs`) and library modules (`scoring.rs`, `maint.rs`).
// Keeping them here avoids inverted dependencies where submodules import
// from the entry-point crate root.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Cache state type
// ---------------------------------------------------------------------------

/// Shared comprehensibility/maintainability cache: the JSONL file path.
pub(crate) type CacheState = PathBuf;

// ---------------------------------------------------------------------------
// CompAgentError
// ---------------------------------------------------------------------------

/// Classification of an error from an ephemeral agent stage.
#[derive(Debug)]
pub(crate) enum CompAgentError {
    /// A transient failure worth retrying (model produced no/malformed JSON).
    Retryable(String),
    /// A non-retryable failure (bail, missing artifact after agent loop exhausted retries).
    Fatal(String),
}

impl std::fmt::Display for CompAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompAgentError::Retryable(s) | CompAgentError::Fatal(s) => write!(f, "{s}"),
        }
    }
}

// ---------------------------------------------------------------------------
// String utilities
// ---------------------------------------------------------------------------

/// Truncate `s` to `max_len` bytes, respecting UTF-8 character boundaries.
pub(crate) fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Return the median of a non-empty slice of f64 values.
/// Odd-length: middle element after sort. Even-length: average of the two
/// middle elements.
pub(crate) fn median(values: &[f64]) -> f64 {
    debug_assert!(!values.is_empty(), "median of empty slice");
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

// ---------------------------------------------------------------------------
// JSON extraction (for LLM responses)
// ---------------------------------------------------------------------------

/// Extract a JSON object from an LLM response, handling common failure modes.
///
/// Tries these strategies in order:
/// 1. Strip markdown code fences.
/// 2. Find the outermost `{...}` pair (handles text before/after JSON).
/// 3. Clean common JSON formatting errors (trailing commas, comments).
pub(crate) fn extract_json(text: &str) -> Option<String> {
    let text = text.trim();

    // ── 1. Strip markdown code fences ──
    let mut inner = text;

    // Strip opening fence: ```json or ``` (possibly with no newline after).
    inner = inner
        .strip_prefix("```json")
        .or_else(|| inner.strip_prefix("```"))
        .unwrap_or(inner)
        .trim();

    // Strip closing fence: last ``` in the text.
    if let Some(pos) = inner.rfind("```") {
        let tail = inner[pos..].trim();
        if tail == "```" || tail.starts_with("```\n") || tail.starts_with("```\r") {
            inner = inner[..pos].trim();
        }
    }

    let json_str = find_json_object(inner)?;
    Some(clean_json(&json_str))
}

/// Find the outermost `{...}` pair in text, respecting string boundaries.
fn find_json_object(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut start = None;
    let mut depth = 0u32;
    let mut in_string = false;
    let mut escape = false;

    for (i, &ch) in chars.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if ch == '{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if ch == '}' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                let s = start?;
                return Some(chars[s..=i].iter().collect());
            }
        }
    }
    None
}

/// Clean common JSON formatting errors in LLM output:
/// - `//` line comments
/// - `/* */` block comments
/// - Trailing commas before `}` or `]`
fn clean_json(json: &str) -> String {
    let mut result = String::with_capacity(json.len());
    let chars: Vec<char> = json.chars().collect();
    let mut in_string = false;
    let mut escape = false;
    let mut in_block_comment = false;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if in_block_comment {
            if ch == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if escape {
            escape = false;
            result.push(ch);
            i += 1;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            result.push(ch);
            i += 1;
            continue;
        }

        if ch == '"' {
            in_string = !in_string;
            result.push(ch);
            i += 1;
            continue;
        }

        if in_string {
            result.push(ch);
            i += 1;
            continue;
        }

        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            in_block_comment = true;
            i += 2;
            continue;
        }

        result.push(ch);
        i += 1;
    }

    remove_trailing_commas(&result)
}

/// Remove commas immediately before `}` or `]`, ignoring whitespace.
fn remove_trailing_commas(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len());
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let ch = chars[i];

        if ch == ',' {
            let mut j = i + 1;
            while j < len
                && (chars[j] == ' ' || chars[j] == '\t' || chars[j] == '\n' || chars[j] == '\r')
            {
                j += 1;
            }
            if j < len && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                while i < j {
                    out.push(chars[i]);
                    i += 1;
                }
                continue;
            }
        }

        out.push(ch);
        i += 1;
    }

    out
}
