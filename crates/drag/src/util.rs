// Shared utilities for gremlins-drag.
//
// This module holds types and functions that are used across both the binary
// orchestrator (`main.rs`) and library modules (`scoring.rs`, `maint.rs`).
// Keeping them here avoids inverted dependencies where submodules import
// from the entry-point crate root.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Concurrency limiter
// ---------------------------------------------------------------------------

/// Maximum number of concurrent ephemeral gremlin runs during scoring.
///
/// Each run opens several file descriptors (state files, log files, raw
/// stream, artifact files, sockets).  This cap keeps total open fds well
/// under typical process soft limits (256 on macOS default; 1024 on Linux).
///
/// The value also works in tandem with the jittered retry backoff in
/// `comp_agent` and `maint`: with 8 concurrent runs and backoff windows
/// up to 8 s (base 8 s ± 4 s), we avoid thundering-herd retry storms
/// against rate-limited model providers while keeping aggregate scoring
/// wall-clock time acceptable.
///
/// This is a compile-time constant that requires editing the source to
/// change.  If you see "Too many open files" errors even with this bound,
/// consider raising your process fd limit (`ulimit -n`) or lowering this
/// value and rebuilding.
pub(crate) const MAX_CONCURRENT_SCORING_RUNS: usize = 8;

// ---------------------------------------------------------------------------
// Cache state type
// ---------------------------------------------------------------------------

/// Dimension key → score (0–4). Keys are defined by the comprehensibility
/// config; a missing key means the model omitted that dimension.
pub(crate) type ComprehensibilityScores = std::collections::HashMap<String, u8>;

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
// Jittered backoff
// ---------------------------------------------------------------------------

/// Return a uniformly-distributed delay in `[base - half_window, base + half_window]`.
///
/// Combines a per-call atomic counter with system-clock subsecond nanos so
/// concurrent callers always receive distinct delays.  The result is clamped
/// to `[0, base + half_window]` so a mis-specified backoff constant can never
/// yield a negative `Duration`.
///
/// Not cryptographically random and not meant to be.
pub(crate) fn jitter(base: f64, half_window: f64) -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ctr = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let r = jitter_rng(nanos, ctr);
    (base + (r - 0.5) * half_window * 2.0).max(0.0)
}

/// Deterministic jitter PRF: mixes a nanosecond timestamp with an atomic
/// counter so concurrent callers produce distinct delays even when the
/// counter only differs in low bits.
///
/// Mixing approach:
/// - Multiply `nanos` and `ctr` independently by a large odd constant
///   (Knuth's MMIX multiplier) to spread their bits across all 64 positions.
/// - Add the two products so both inputs influence every output bit.
/// - Convert to `[0.0, 1.0)` via division by `u64::MAX`.
fn jitter_rng(nanos: u32, ctr: u64) -> f64 {
    const MUL: u64 = 6364136223846793005; // Knuth MMIX
    let mixed = (nanos as u64)
        .wrapping_mul(MUL)
        .wrapping_add(ctr.wrapping_mul(MUL));
    (mixed as f64) / (u64::MAX as f64)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_is_char_boundary_safe() {
        // "é" is two bytes; cutting at byte 1 would be a non-boundary.
        assert_eq!(truncate("éx", 1), "");
        assert_eq!(truncate("abc", 2), "ab");
    }

    // ── JSON extraction tests ──

    #[test]
    fn extract_plain_json() {
        let raw = r#"{"naming": 3, "control_flow": 2, "data_flow": 2, "abstractions": 3}"#;
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
        assert_eq!(scores["control_flow"], 2);
    }

    #[test]
    fn extract_json_fenced() {
        let raw = "```json\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n```";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_fenced_no_lang() {
        let raw =
            "```\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n```";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_text_before() {
        let raw = "Here is my assessment of the file:\n\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n\nHope that helps.";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_trailing_comma() {
        let raw = "{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3,}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_line_comment() {
        let raw = "{\n// scoring notes\n\"naming\": 3,\n\"control_flow\": 2,\n\"data_flow\": 2,\n\"abstractions\": 3\n}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_block_comment() {
        let raw = "{\n/* scoring notes */\n\"naming\": 3,\n\"control_flow\": 2,\n\"data_flow\": 2,\n\"abstractions\": 3\n}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_fenced_with_text_and_trailing_comma() {
        // The most common failure mode: markdown fence, trailing comma, extra text.
        let raw = "Here's the score:\n\n```json\n{\n  \"naming\": 3,\n  \"control_flow\": 2,\n  \"data_flow\": 2,\n  \"abstractions\": 3,\n}\n```\n\nLet me know if you need anything else.";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_missing_closing_fence() {
        let raw =
            "```json\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_nested_braces_in_string() {
        // Braces inside a JSON string should not confuse the matcher.
        let raw = r#"{"naming": 3, "note": "use {braces} here", "control_flow": 2, "data_flow": 2, "abstractions": 3}"#;
        let extracted = extract_json(raw).unwrap();
        // This test just needs valid JSON out — extra field is fine for serde.
        let _: serde_json::Value = serde_json::from_str(&extracted).unwrap();
    }

    #[test]
    fn extract_json_no_braces_returns_none() {
        assert!(extract_json("just some text, no json here").is_none());
    }

    // ── mean_with_ci tests ──

    #[test]
    fn mean_three_values() {
        let vals = [1.0, 2.0, 3.0];
        let (m, lo, hi) = mean_with_ci(&vals);
        assert!((m - 2.0).abs() < 1e-12);
        // n=3, df=2, t_crit ≈ 4.3027, sd=1.0, se=1/√3≈0.577, margin≈2.484
        assert!((lo - (2.0 - 2.484_05)).abs() < 1e-4);
        assert!((hi - (2.0 + 2.484_05)).abs() < 1e-4);
    }

    #[test]
    fn mean_single_value_ci_degenerate() {
        let vals = [7.0];
        let (m, lo, hi) = mean_with_ci(&vals);
        assert!((m - 7.0).abs() < 1e-12);
        assert!((lo - 7.0).abs() < 1e-12);
        assert!((hi - 7.0).abs() < 1e-12);
    }

    #[test]
    fn mean_identical_values_ci_zero() {
        let vals = [0.5, 0.5, 0.5];
        let (m, lo, hi) = mean_with_ci(&vals);
        assert!((m - 0.5).abs() < 1e-12);
        // All identical → variance = 0 → no margin.
        assert!((lo - 0.5).abs() < 1e-12);
        assert!((hi - 0.5).abs() < 1e-12);
    }

    // ── jitter tests ──

    #[test]
    fn jitter_stays_in_window() {
        let base = 2.0;
        let half = 1.0;
        for _ in 0..1000 {
            let d = jitter(base, half);
            assert!(
                d >= base - half,
                "delay {d} below lower bound {}",
                base - half
            );
            assert!(
                d <= base + half,
                "delay {d} above upper bound {}",
                base + half
            );
        }
    }

    #[test]
    fn jitter_is_nonnegative_for_shipped_constants() {
        for (base, half) in [(2.0, 1.0), (8.0, 4.0)] {
            for _ in 0..100 {
                assert!(jitter(base, half) >= 0.0);
            }
        }
    }

    #[test]
    fn jitter_clamps_negative_to_zero() {
        // When half_window > base (mis-specification), jitter must not return negative.
        let d = jitter(1.0, 5.0);
        assert!(d >= 0.0, "jitter returned negative delay: {d}");
        assert!(d <= 6.0, "jitter exceeded base + half_window");
    }

    #[test]
    fn jitter_concurrent_callers_diverge() {
        // Simulate a herd of concurrent callers at the same wall-clock instant
        // by calling jitter in a tight loop.  Not all values should be identical.
        let vals: Vec<f64> = (0..100).map(|_| jitter(8.0, 4.0)).collect();
        let unique: std::collections::HashSet<u64> =
            vals.iter().map(|v| (v * 1_000_000.0) as u64).collect();
        assert!(
            unique.len() > 1,
            "all concurrent jitter calls returned the same value"
        );
    }

    #[test]
    fn jitter_fixed_timestamp_different_counters_diverge() {
        // Regression: with a fixed nanosecond timestamp, different counters
        // must produce different delays.  Without counter mixing, low-bit
        // differences are lost during u64 → f64 conversion.
        let nanos: u32 = 123456789;
        let vals: Vec<f64> = (0..100).map(|ctr| jitter_rng(nanos, ctr)).collect();
        // Quantize to microsecond precision so rounding doesn't hide real differences.
        let unique: std::collections::HashSet<u64> =
            vals.iter().map(|v| (v * 1_000_000.0) as u64).collect();
        assert!(
            unique.len() > 99,
            "counters 0–99 with fixed nanos={nanos} only produced {} unique values",
            unique.len()
        );
    }
}
