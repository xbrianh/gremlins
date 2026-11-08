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

/// Maximum bytes of total source content to send to the model for
/// maintainability scoring.  When a package's source exceeds this the
/// content is truncated with a note so the prompt size stays bounded.
pub(crate) const DEFAULT_BYTE_LIMIT: usize = 128_000;

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

/// Critical value of Student's t-distribution for a two-sided 95% CI
/// (α = 0.05, so α/2 = 0.025).
///
/// Uses a lookup table with linear interpolation for df 1–1000;
/// falls back to the normal quantile (1.96) for df > 1000.
fn t_critical_95(df: usize) -> f64 {
    // t_{0.975, df} for selected degrees of freedom.
    // Sorted by df so binary search + interpolation works.
    const TABLE: &[(usize, f64)] = &[
        (1, 12.706_205),
        (2, 4.302_653),
        (3, 3.182_446),
        (4, 2.776_445),
        (5, 2.570_582),
        (6, 2.446_912),
        (7, 2.364_624),
        (8, 2.306_004),
        (9, 2.262_157),
        (10, 2.228_139),
        (12, 2.178_813),
        (14, 2.144_787),
        (16, 2.119_905),
        (18, 2.100_922),
        (20, 2.085_963),
        (25, 2.059_539),
        (30, 2.042_272),
        (40, 2.021_075),
        (50, 2.008_559),
        (60, 2.000_298),
        (80, 1.990_063),
        (100, 1.983_972),
        (150, 1.975_905),
        (200, 1.971_896),
        (500, 1.964_720),
        (1000, 1.962_339),
    ];

    // Binary search for the bracket.
    let idx = TABLE.partition_point(|&(d, _)| d < df);
    if idx == 0 {
        return TABLE[0].1;
    }
    if idx >= TABLE.len() {
        // df > 1000: asymptote to normal quantile.
        return 1.959_963_984_540_054;
    }
    let (d_lo, t_lo) = TABLE[idx - 1];
    let (d_hi, t_hi) = TABLE[idx];
    if d_lo == df {
        return t_lo;
    }
    // Linear interpolation between d_lo and d_hi.
    let frac = (df - d_lo) as f64 / (d_hi - d_lo) as f64;
    t_lo + frac * (t_hi - t_lo)
}

/// Return the sample mean of a non-empty slice of f64 values.
pub(crate) fn mean(values: &[f64]) -> f64 {
    debug_assert!(!values.is_empty(), "mean of empty slice");
    let sum: f64 = values.iter().sum();
    sum / values.len() as f64
}

/// Compute the sample mean and its 95% confidence interval using
/// Student's t-distribution.
///
/// Returns `(mean, ci_lower, ci_upper)`. When n == 1 the CI is
/// degenerate: both bounds equal the mean.
pub(crate) fn mean_with_ci(values: &[f64]) -> (f64, f64, f64) {
    debug_assert!(!values.is_empty(), "mean_with_ci of empty slice");
    let n = values.len();
    let m = mean(values);
    if n < 2 {
        return (m, m, m);
    }
    let variance: f64 = values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1) as f64;
    let std_err = variance.sqrt() / (n as f64).sqrt();
    let t_crit = t_critical_95(n - 1);
    let margin = t_crit * std_err;
    (m, m - margin, m + margin)
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
