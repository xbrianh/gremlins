//! Operator-typed input text with optional path substitutions.
//!
//! `InputBuffer` separates the text the operator sees and edits from the
//! resolved (absolute-path) text sent to the model. When tab completion
//! expands a relative path like `crates/gr` to `crates/gremlins-cli/`, a
//! substitution records that the display span should resolve to the absolute
//! path `/Users/.../crates/gremlins-cli/` at submission time.

use std::ops::Range;

/// The text in the TUI input bar.
///
/// `InputBuffer` stores operator-typed display text alongside a resolution map
/// that records which byte spans have been expanded to absolute paths. The
/// input bar widget renders the display form unchanged; on submission the
/// resolved form (with absolute paths substituted) is sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBuffer {
    /// Display text — rendered as-is in the TUI input bar.
    text: String,

    /// Path substitutions applied by tab completion.
    ///
    /// Each entry maps a byte range in `text` to the absolute path that
    /// should replace it at submission time. Spans are always on `char`
    /// boundaries and are sorted by start position. A span is invalidated
    /// (removed from this map) whenever the operator's cursor or an edit
    /// crosses its interior.
    substitutions: Vec<(Range<usize>, String)>,
}

impl InputBuffer {
    // ── Construction ──

    pub fn new() -> Self {
        Self {
            text: String::new(),
            substitutions: Vec::new(),
        }
    }

    /// Clear both the display text and all substitutions.
    pub fn clear(&mut self) {
        self.text.clear();
        self.substitutions.clear();
    }

    // ── Display-side access ──

    /// The rendered text. This is what the operator sees.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte length of the display text.
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Whether the display text is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Index into `text`, checking char boundaries.
    pub fn get(&self, range: Range<usize>) -> Option<&str> {
        self.text.get(range)
    }

    /// Whether `idx` is a `char` boundary in the display text.
    pub fn is_char_boundary(&self, idx: usize) -> bool {
        self.text.is_char_boundary(idx)
    }

    // ── Editing operations (work on display text, maintain substitutions) ──

    /// Insert `ch` at byte index `idx`. Invalidates any substitution whose
    /// interior contains `idx`, and shifts later spans right.
    pub fn insert_char(&mut self, idx: usize, ch: char) {
        let idx = idx.min(self.text.len());
        // Invalidate substitutions whose interior is crossed.
        self.substitutions.retain(|(s, _)| {
            // Keep only if insertion is at or outside the span boundaries.
            idx <= s.start || idx >= s.end
        });
        // Shift remaining spans.
        let len = ch.len_utf8();
        for (s, _) in &mut self.substitutions {
            if s.start >= idx {
                s.start += len;
                s.end += len;
            }
        }
        self.text.insert(idx, ch);
    }

    /// Delete the byte range `range`. Invalidates any substitution whose
    /// interior overlaps the deletion, and shifts later spans left.
    pub fn delete_range(&mut self, range: Range<usize>) {
        let end = range.end.min(self.text.len());
        let start = range.start.min(end);
        // Invalidate substitutions whose interior overlaps the deletion.
        self.substitutions.retain(|(s, _)| {
            // Keep only if deletion is entirely outside the span's interior.
            end <= s.start || start >= s.end
        });
        // Shift remaining spans.
        let len = end - start;
        for (s, _) in &mut self.substitutions {
            if s.start >= end {
                s.start -= len;
                s.end -= len;
            }
        }
        self.text.drain(start..end);
    }

    /// Replace the byte range `range` with `replacement`.
    ///
    /// Substitutions whose interior overlaps the replaced region are dropped
    /// (the replacement rewrites those bytes, so their display→absolute
    /// mapping is no longer valid). Substitutions that start at or after the
    /// replaced region are shifted by the length delta. Completion callers
    /// immediately record a fresh substitution via [`add_substitution`]
    /// afterwards.
    pub fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        let end = range.end.min(self.text.len());
        let start = range.start.min(end);
        let old_len = end - start;
        let new_len = replacement.len();

        // Drop substitutions whose interior overlaps the replaced region.
        self.substitutions
            .retain(|(s, _)| end <= s.start || start >= s.end);

        // Shift substitutions that start at or after the replaced region.
        for (s, _) in &mut self.substitutions {
            if s.start >= end {
                s.start = s.start - old_len + new_len;
                s.end = s.end - old_len + new_len;
            }
        }

        self.text.replace_range(start..end, replacement);
    }

    /// Replace the entire display text with `text` and clear all
    /// substitutions. Used for history recall and editor write-back.
    pub fn replace_all(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.substitutions.clear();
    }

    // ── Completion ──

    /// Record a path substitution. The span lies in `text` and `absolute` is
    /// the full absolute path to send to the model for this span. Any
    /// existing substitution overlapping `span` is replaced.
    pub fn add_substitution(&mut self, span: Range<usize>, absolute: String) {
        // Remove any existing substitution that overlaps this span.
        self.substitutions
            .retain(|(s, _)| s.end <= span.start || s.start >= span.end);
        // Insert, maintaining sorted order.
        let pos = self
            .substitutions
            .binary_search_by(|(s, _)| s.start.cmp(&span.start))
            .unwrap_or_else(|i| i);
        self.substitutions.insert(pos, (span, absolute));
    }

    /// Remove all substitutions.
    ///
    /// Used when the input bar is being reset wholesale. Ordinary edits
    /// preserve non-overlapping substitutions; see [`insert_char`] and
    /// [`delete_range`].
    pub fn clear_substitutions(&mut self) {
        self.substitutions.clear();
    }

    // ── Resolution (called once, on Enter) ──

    /// Produce the text to send to the model.
    ///
    /// Substitutions are applied from right to left so earlier spans stay
    /// valid during replacement. Spans that are out of bounds or no longer
    /// sit on `char` boundaries are skipped (stale guard).
    pub fn resolve(self) -> String {
        let mut result = self.text;
        // Apply substitutions from right to left.
        for (span, absolute) in self.substitutions.iter().rev() {
            if result.get(span.clone()).is_none() {
                continue;
            }
            result.replace_range(span.clone(), absolute);
        }
        result
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let buf = InputBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn insert_char_shifts_substitutions() {
        let mut buf = InputBuffer::new();
        buf.text = "hello world".to_string();
        buf.substitutions = vec![(6..11, "/abs/path".to_string())];

        buf.insert_char(0, '!');
        assert_eq!(buf.text(), "!hello world");
        assert_eq!(buf.substitutions, vec![(7..12, "/abs/path".to_string())]);
    }

    #[test]
    fn insert_char_inside_substitution_invalidates() {
        let mut buf = InputBuffer::new();
        buf.text = "hello world".to_string();
        buf.substitutions = vec![(6..11, "/abs/path".to_string())];

        buf.insert_char(8, 'X'); // inside "world"
        assert_eq!(buf.text(), "hello woXrld");
        assert!(buf.substitutions.is_empty());
    }

    #[test]
    fn insert_char_at_substitution_boundary_preserves() {
        let mut buf = InputBuffer::new();
        buf.text = "hello world".to_string();
        buf.substitutions = vec![(6..11, "/abs/path".to_string())];

        buf.insert_char(6, 'X'); // at start boundary
        assert_eq!(buf.text(), "hello Xworld");
        assert_eq!(buf.substitutions, vec![(7..12, "/abs/path".to_string())]);

        buf.insert_char(13, 'Y'); // at end boundary (after shift)
        assert_eq!(buf.text(), "hello XworldY");
        assert_eq!(buf.substitutions, vec![(7..12, "/abs/path".to_string())]);
    }

    #[test]
    fn delete_range_shifts_substitutions() {
        let mut buf = InputBuffer::new();
        buf.text = "hello brave world".to_string();
        buf.substitutions = vec![(12..17, "/abs/path".to_string())];

        buf.delete_range(6..12); // delete "brave "
        assert_eq!(buf.text(), "hello world");
        assert_eq!(buf.substitutions, vec![(6..11, "/abs/path".to_string())]);
    }

    #[test]
    fn delete_range_overlapping_substitution_invalidates() {
        let mut buf = InputBuffer::new();
        buf.text = "hello world".to_string();
        buf.substitutions = vec![(6..11, "/abs/path".to_string())];

        buf.delete_range(4..8); // "o wo"
        assert_eq!(buf.text(), "hellrld");
        assert!(buf.substitutions.is_empty());
    }

    #[test]
    fn replace_range_invalidates_overlapping_substitution() {
        let mut buf = InputBuffer::new();
        buf.text = "hello world".to_string();
        buf.substitutions = vec![(6..11, "/abs/path".to_string())];

        buf.replace_range(6..11, "there");
        assert_eq!(buf.text(), "hello there");
        assert!(buf.substitutions.is_empty());
    }

    #[test]
    fn replace_range_shifts_later_substitutions() {
        let mut buf = InputBuffer::new();
        buf.text = "hello brave world".to_string();
        buf.substitutions = vec![(12..17, "/abs/path".to_string())];

        buf.replace_range(6..12, "");
        assert_eq!(buf.text(), "hello world");
        assert_eq!(buf.substitutions, vec![(6..11, "/abs/path".to_string())]);
    }

    #[test]
    fn replace_all_clears_substitutions() {
        let mut buf = InputBuffer::new();
        buf.text = "hello".to_string();
        buf.substitutions = vec![(0..5, "/abs/hello".to_string())];

        buf.replace_all("goodbye");
        assert_eq!(buf.text(), "goodbye");
        assert!(buf.substitutions.is_empty());
    }

    #[test]
    fn add_substitution_replaces_overlapping() {
        let mut buf = InputBuffer::new();
        buf.text = "crates/gremlins-cli/".to_string();

        buf.add_substitution(0..20, "/abs/crates/gremlins-cli/".to_string());
        assert_eq!(buf.substitutions.len(), 1);

        // Same span, different absolute path.
        buf.add_substitution(0..20, "/abs/crates/gremlins/".to_string());
        assert_eq!(buf.substitutions.len(), 1);
        assert_eq!(buf.substitutions[0].1, "/abs/crates/gremlins/");
    }

    #[test]
    fn resolve_applies_substitutions_right_to_left() {
        let mut buf = InputBuffer::new();
        buf.text = "read crates/gr".to_string();

        buf.add_substitution(5..14, "/abs/path/to/crates/gr".to_string());

        let resolved = buf.resolve();
        assert_eq!(resolved, "read /abs/path/to/crates/gr");
    }

    #[test]
    fn resolve_with_stale_span_leaves_text_as_is() {
        let mut buf = InputBuffer::new();
        buf.text = "short".to_string();

        // Stale span extends past the text; it must be skipped, not clipped.
        buf.add_substitution(0..20, "/very/long/path".to_string());

        let resolved = buf.resolve();
        assert_eq!(resolved, "short");
    }

    #[test]
    fn resolve_skips_span_entirely_past_text() {
        let mut buf = InputBuffer::new();
        buf.text = "abc".to_string();
        buf.add_substitution(5..8, "/very/long/path".to_string());

        assert_eq!(buf.resolve(), "abc");
    }

    #[test]
    fn resolve_applies_multiple_substitutions() {
        let mut buf = InputBuffer::new();
        buf.text = "read crates/gr and src/ma".to_string();
        buf.add_substitution(5..14, "/abs/crates/gr".to_string());
        buf.add_substitution(19..25, "/abs/src/ma".to_string());

        assert_eq!(buf.resolve(), "read /abs/crates/gr and /abs/src/ma");
    }

    #[test]
    fn edit_between_substitutions_preserves_both() {
        let mut buf = InputBuffer::new();
        buf.text = "a b".to_string();
        buf.add_substitution(0..1, "/A".to_string());
        buf.add_substitution(2..3, "/B".to_string());

        buf.insert_char(1, '>');
        assert_eq!(buf.text(), "a> b");
        assert_eq!(
            buf.substitutions,
            vec![(0..1, "/A".to_string()), (3..4, "/B".to_string())]
        );
        assert_eq!(buf.resolve(), "/A> /B");
    }

    #[test]
    fn clear_removes_text_and_substitutions() {
        let mut buf = InputBuffer::new();
        buf.text = "hello".to_string();
        buf.substitutions = vec![(0..5, "/abs/hello".to_string())];

        buf.clear();
        assert!(buf.is_empty());
        assert!(buf.substitutions.is_empty());
    }

    #[test]
    fn add_substitution_maintains_sort_order() {
        let mut buf = InputBuffer::new();
        buf.text = "a b c d e".to_string();

        buf.add_substitution(4..5, "/abs/c".to_string());
        buf.add_substitution(0..1, "/abs/a".to_string());
        buf.add_substitution(2..3, "/abs/b".to_string());
        buf.add_substitution(6..7, "/abs/d".to_string());
        buf.add_substitution(8..9, "/abs/e".to_string());

        let starts: Vec<usize> = buf.substitutions.iter().map(|(s, _)| s.start).collect();
        assert_eq!(starts, vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn is_char_boundary_delegates() {
        let buf = InputBuffer {
            text: "aé".to_string(),
            substitutions: Vec::new(),
        };
        assert!(buf.is_char_boundary(0));
        assert!(buf.is_char_boundary(1));
        assert!(buf.is_char_boundary(3));
        assert!(!buf.is_char_boundary(2)); // middle of é
    }

    #[test]
    fn get_delegates_to_text() {
        let buf = InputBuffer {
            text: "hello".to_string(),
            substitutions: Vec::new(),
        };
        assert_eq!(buf.get(1..4), Some("ell"));
        assert_eq!(buf.get(10..15), None);
    }
}
