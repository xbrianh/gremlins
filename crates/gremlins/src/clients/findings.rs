//! Declarative findings ledger extracted from assistant prose.
//!
//! After each turn the agent's text is scanned for short declarative
//! sentences matching a small cue set (e.g. "the bug is", "root cause is").
//! Matches are normalized, deduplicated, and accumulated into a bounded
//! ledger (max five entries). The rendered `## Findings` block is appended
//! to the next prompt's `[seen]` block so the model does not re-derive
//! facts it already stated, even when compaction drops earlier turns.
//!
//! Extraction is heuristic and deterministic — no model calls, no async.

use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};

/// Cues that signal a declarative finding. Each sentence is lowercased
/// and checked for a prefix match against this list.
const CUE_PREFIXES: &[&str] = &[
    "the bug is",
    "the issue is",
    "the problem is",
    "root cause is",
    "root cause:",
    "i've verified",
    "i verified",
    "i've confirmed",
    "i confirmed",
    "located in",
    "this is caused by",
    "is caused by",
    "the fix is",
    "the fix:",
    "note:",
    "notably,",
    "importantly,",
    "key insight:",
    "the error is",
    "the pattern is",
];

/// Maximum number of findings to retain.
const MAX_ENTRIES: usize = 5;

/// Maximum length of a single finding entry before truncation.
const MAX_ENTRY_LEN: usize = 200;

/// Accumulated declarative findings extracted from assistant prose.
#[derive(Clone, Default)]
pub(crate) struct FindingsLedger {
    entries: VecDeque<FindingsEntry>,
    /// Set of normalized entry hashes for deduplication.
    seen: HashSet<u64>,
}

#[derive(Clone)]
struct FindingsEntry {
    /// Normalized finding text (lowercased, trimmed).
    text: String,
    /// Hash of normalized text for dedup.
    hash: u64,
}

impl FindingsLedger {
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    /// Feed the assistant's prose into the extractor. New findings that
    /// pass the cue check and are not duplicates are appended. When full,
    /// oldest entries are dropped first.
    pub(crate) fn feed(&mut self, text: &str) {
        for sentence in split_sentences(text) {
            let normalized = normalize_sentence(sentence);
            if !matches_cue(&normalized) {
                continue;
            }
            let truncated = truncate_to_sentence_boundary(&normalized, MAX_ENTRY_LEN);
            let hash = hash_str(truncated);
            if self.seen.contains(&hash) {
                continue;
            }
            self.seen.insert(hash);
            if self.entries.len() >= MAX_ENTRIES {
                if let Some(old) = self.entries.pop_front() {
                    self.seen.remove(&old.hash);
                }
            }
            self.entries.push_back(FindingsEntry {
                text: truncated.to_string(),
                hash,
            });
        }
    }

    /// Render the accumulated findings as a `## Findings` block.
    /// Returns an empty string when there are no entries.
    pub(crate) fn format(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Findings\n\n");
        for entry in &self.entries {
            out.push_str("- ");
            out.push_str(&entry.text);
            out.push('\n');
        }
        out
    }

    /// Number of currently stored findings.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger is empty.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Split prose into sentence-like spans, handling `.`, `!`, `?` terminators
/// followed by whitespace, plus newline boundaries for bullet/list text.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        let b = bytes[i];
        if b == b'.' || b == b'!' || b == b'?' {
            let next = if i + 1 < len { bytes[i + 1] } else { b' ' };
            if next == b' ' || next == b'\n' || next == b'\r' {
                let end = i + 1;
                let s = text[start..end].trim();
                if !s.is_empty() {
                    out.push(s);
                }
                start = end;
                while start < len {
                    let c = bytes[start];
                    if c == b' ' || c == b'\n' || c == b'\r' {
                        start += 1;
                    } else {
                        break;
                    }
                }
                i = start;
                continue;
            }
        }
        // Single newline acts as a boundary for bullet/list prose; we
        // skip blank-line runs.
        if b == b'\n' && i > start && i + 1 < len && bytes[i + 1] != b'\n' && bytes[i + 1] != b'\r'
        {
            let s = text[start..i].trim();
            if !s.is_empty() {
                let next_ch = text[i + 1..].chars().next().unwrap_or('\0');
                if next_ch.is_alphabetic() || next_ch == '-' || next_ch == '*' {
                    out.push(s);
                    start = i + 1;
                }
            }
        }
        i += 1;
    }
    let remainder = text[start..].trim();
    if !remainder.is_empty() {
        out.push(remainder);
    }
    out
}

/// Normalize a sentence: lowercase, collapse whitespace, and strip leading
/// bullets, dashes, numbered prefixes, and backticks.
fn normalize_sentence(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("• "))
        .unwrap_or(s);
    // Strip leading number+dot like "1. ".
    let s = if let Some(after_digits) = s
        .find(|c: char| !c.is_ascii_digit())
        .and_then(|idx| s[idx..].strip_prefix(". "))
    {
        after_digits
    } else {
        s
    };
    let s = s.strip_prefix('`').unwrap_or(s);
    let s = s.strip_suffix('.').unwrap_or(s);
    collapse_whitespace(&s.to_lowercase())
}

/// Check whether a normalized sentence starts with a cue prefix.
fn matches_cue(normalized: &str) -> bool {
    CUE_PREFIXES.iter().any(|cue| normalized.starts_with(cue))
}

/// Collapse consecutive whitespace to single spaces.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

/// Truncate a normalized sentence to `max_len` bytes, breaking at a space
/// boundary when possible.
fn truncate_to_sentence_boundary(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && s.as_bytes()[end] != b' ' {
        end -= 1;
    }
    if end == 0 {
        // Walk back to a valid UTF-8 character boundary.
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    } else {
        &s[..end]
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_captures_matching_cues() {
        let mut ledger = FindingsLedger::new();
        ledger.feed("The bug is in the parser module. Some other text here.");
        assert_eq!(ledger.len(), 1);
        assert!(ledger.format().contains("the bug is in the parser module"));
    }

    #[test]
    fn feed_ignores_non_cue_sentences() {
        let mut ledger = FindingsLedger::new();
        ledger.feed("Let me think about this. I'll check the codebase.");
        assert!(ledger.is_empty());
        assert!(ledger.format().is_empty());
    }

    #[test]
    fn deduplicates_identical_findings() {
        let mut ledger = FindingsLedger::new();
        ledger.feed("The bug is in the parser.\nThe bug is in the parser.");
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn caps_at_five_entries() {
        let mut ledger = FindingsLedger::new();
        ledger.feed(
            "The bug is one.\nThe bug is two.\nThe bug is three.\n\
             The bug is four.\nThe bug is five.\nThe bug is six.",
        );
        assert_eq!(ledger.len(), 5);
        let formatted = ledger.format();
        // Oldest entry ("one") is dropped; "six" replaces it.
        assert!(!formatted.contains("one"));
        assert!(formatted.contains("six"));
    }

    #[test]
    fn root_cause_cue_matches() {
        let mut ledger = FindingsLedger::new();
        ledger.feed("Root cause is a missing null check in the handler.");
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn ive_verified_cue_matches() {
        let mut ledger = FindingsLedger::new();
        ledger.feed("i've verified that the patch compiles cleanly.");
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn empty_prose_does_nothing() {
        let mut ledger = FindingsLedger::new();
        ledger.feed("");
        assert!(ledger.is_empty());
        assert!(ledger.format().is_empty());
    }

    #[test]
    fn format_emits_correct_block() {
        let mut ledger = FindingsLedger::new();
        ledger.feed("The bug is in main.rs.");
        assert_eq!(ledger.format(), "## Findings\n\n- the bug is in main.rs\n");
    }

    #[test]
    fn normalize_strips_bullets() {
        assert_eq!(normalize_sentence("- the bug is here"), "the bug is here");
        assert_eq!(normalize_sentence("* the bug is here"), "the bug is here");
    }

    #[test]
    fn normalize_strips_numbered_prefix() {
        assert_eq!(normalize_sentence("1. the bug is here"), "the bug is here");
        assert_eq!(normalize_sentence("42. the bug is here"), "the bug is here");
    }

    #[test]
    fn split_sentences_handles_basic_text() {
        let result = split_sentences("First sentence. Second sentence.");
        assert_eq!(result, vec!["First sentence.", "Second sentence."]);
    }

    #[test]
    fn split_sentences_handles_newline_boundary() {
        let result = split_sentences("First thought\nSecond thought");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn matches_cue_prefixes() {
        assert!(matches_cue("the bug is in foo.rs"));
        assert!(matches_cue("root cause is null pointer"));
        assert!(!matches_cue("let me think"));
    }

    #[test]
    fn truncate_at_space_boundary() {
        let s = "this is a very long sentence that should be truncated at a space boundary somewhere in the middle of the text and then we drop the rest";
        let t = truncate_to_sentence_boundary(s, 50);
        assert!(t.len() <= 50);
        assert!(!t.ends_with(' '));
    }
}
