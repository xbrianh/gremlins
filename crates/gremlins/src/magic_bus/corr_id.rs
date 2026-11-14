//! Correlation identifiers with segment-aware prefix matching.
//!
//! A [`CorrId`] names a point in the gremlin lifecycle:
//!
//! ```text
//! run -> stage -> turn -> tool -> attempt
//! ```
//!
//! Prefix matching is deliberately segment-aware rather than string-based:
//! `r:ab` must not match `r:abc` the way a raw `str::starts_with` would.
//! Missing fields in a prefix are wildcards — `r:X:s:foo` matches a full
//! identifier that additionally carries turn/tool/attempt segments.
//!
//! # String form
//!
//! The tagged form is `r:<run>:s:<stage>:t:<turn>:c:<tool>:a:<attempt>`, with
//! missing segments omitted entirely. String segment values may contain `:`
//! and `\`; both are escaped on render (`\:` and `\\`) and unescaped on parse,
//! so `to_string().parse::<CorrId>()` always round-trips for a valid
//! identifier. String segments must be non-empty.

use std::fmt;
use std::str::FromStr;

/// Fixed segment order for both rendering and parsing.
const FIELD_TAGS: &[u8] = b"rstca";

/// A segment-aware correlation identifier.
///
/// Each field is optional; omitted fields are left out of the rendered form
/// entirely (a run-level lifecycle event is just `r:<run>`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CorrId {
    run: Option<String>,
    stage: Option<String>,
    turn: Option<u64>,
    tool: Option<String>,
    attempt: Option<u32>,
}

impl CorrId {
    /// An empty correlation identifier. The empty prefix matches everything.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_run(mut self, value: impl Into<String>) -> Self {
        self.run = Some(into_segment(value));
        self
    }

    pub fn with_stage(mut self, value: impl Into<String>) -> Self {
        self.stage = Some(into_segment(value));
        self
    }

    pub fn with_turn(mut self, value: u64) -> Self {
        self.turn = Some(value);
        self
    }

    pub fn with_tool(mut self, value: impl Into<String>) -> Self {
        self.tool = Some(into_segment(value));
        self
    }

    pub fn with_attempt(mut self, value: u32) -> Self {
        self.attempt = Some(value);
        self
    }

    pub fn run(&self) -> Option<&str> {
        self.run.as_deref()
    }

    pub fn stage(&self) -> Option<&str> {
        self.stage.as_deref()
    }

    pub fn turn(&self) -> Option<u64> {
        self.turn
    }

    pub fn tool(&self) -> Option<&str> {
        self.tool.as_deref()
    }

    pub fn attempt(&self) -> Option<u32> {
        self.attempt
    }

    /// Segment-aware prefix match.
    ///
    /// Every field set on `prefix` must match the corresponding field on
    /// `self`. A `None` field in `prefix` matches any value.
    ///
    /// **Run** uses component-wise ancestor matching on colon-delimited
    /// hierarchy levels: `r:parent` matches `r:parent:child` and
    /// `r:parent:child:grand`. **Stage**, **turn**, **tool**, and **attempt**
    /// remain exact-match: `r:ab` does *not* match `r:abc`.
    pub fn matches_prefix(&self, prefix: &CorrId) -> bool {
        run_matches_ancestor(prefix.run.as_deref(), self.run.as_deref())
            && segment_matches(&self.stage, &prefix.stage)
            && segment_matches(&self.turn, &prefix.turn)
            && segment_matches(&self.tool, &prefix.tool)
            && segment_matches(&self.attempt, &prefix.attempt)
    }

    /// Parse the tagged string form: `r:<run>:s:<stage>:t:<turn>:c:<tool>:a:<attempt>`.
    ///
    /// Segments must appear in the fixed run -> stage -> turn -> tool ->
    /// attempt order and each tag may appear at most once. String values may
    /// use `\:` and `\\` escapes for literal `:` and `\` characters.
    pub fn try_parse(s: &str) -> Result<Self, ParseCorrIdError> {
        let parsed = split_segments(s.as_bytes())?;
        interpret_segments(&parsed.segments, parsed.trailing_delimiter)
    }
}

/// A raw `tag:value` segment produced by the structural split.
struct Segment<'a> {
    tag: u8,
    index: usize,
    raw: &'a [u8],
}

/// The result of splitting a correlation id into tagged segments.
struct ParsedSegments<'a> {
    segments: Vec<Segment<'a>>,
    trailing_delimiter: bool,
}

/// Phase 1: split into `tag:value` segments.
///
/// This only cares about structure. A segment begins with a known tag followed
/// by `:`, and its raw value runs to the next unescaped `:` or the end of the
/// input. Only `\:` and `\\` escape pairs are skipped; an invalid escape
/// pair is rejected immediately so that structural errors never mask semantic
/// ones.
fn split_segments(bytes: &[u8]) -> Result<ParsedSegments<'_>, ParseCorrIdError> {
    let mut segments = Vec::new();
    let mut trailing_delimiter = false;
    let mut cursor = 0;

    while cursor < bytes.len() {
        let tag = bytes[cursor];
        let index = FIELD_TAGS
            .iter()
            .position(|known| *known == tag)
            .ok_or(ParseCorrIdError::UnknownTag)?;

        if cursor + 1 >= bytes.len() || bytes[cursor + 1] != b':' {
            return Err(ParseCorrIdError::MissingColon);
        }
        cursor += 2;

        let value_start = cursor;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\\' => {
                    // A trailing backslash stays in the raw value for phase 2
                    // to reject as InvalidEscape.
                    if cursor + 1 < bytes.len() {
                        match bytes[cursor + 1] {
                            b':' | b'\\' => cursor += 2,
                            _ => return Err(ParseCorrIdError::InvalidEscape),
                        }
                    } else {
                        cursor += 1;
                    }
                }
                b':' => break,
                _ => cursor += 1,
            }
        }

        segments.push(Segment {
            tag,
            index,
            raw: &bytes[value_start..cursor],
        });

        if cursor < bytes.len() {
            // The loop broke on an unescaped `:`. Step over it; a delimiter at
            // the very end of the input is reported by phase 2.
            cursor += 1;
            if cursor >= bytes.len() {
                trailing_delimiter = true;
            }
        }
    }

    Ok(ParsedSegments {
        segments,
        trailing_delimiter,
    })
}

/// Phase 2: interpret the raw segments into a [`CorrId`].
fn interpret_segments(
    segments: &[Segment<'_>],
    trailing_delimiter: bool,
) -> Result<CorrId, ParseCorrIdError> {
    let mut corr_id = CorrId::new();
    let mut last_index: Option<usize> = None;

    for (position, segment) in segments.iter().enumerate() {
        // split_segments only slices at ASCII delimiter/tag boundaries, so
        // every raw slice is a valid UTF-8 substring of the original `&str`.
        let raw = std::str::from_utf8(segment.raw)
            .expect("raw slice is always a UTF-8 substring of the original input");
        let value = unescape(raw)?;

        if value.is_empty() {
            return Err(ParseCorrIdError::EmptyValue);
        }
        if trailing_delimiter && position + 1 == segments.len() {
            return Err(ParseCorrIdError::TrailingDelimiter);
        }
        if let Some(previous) = last_index {
            if segment.index == previous {
                return Err(ParseCorrIdError::DuplicateTag);
            }
            if segment.index < previous {
                return Err(ParseCorrIdError::OutOfOrder);
            }
        }

        match segment.tag {
            b'r' => corr_id.run = Some(value),
            b's' => corr_id.stage = Some(value),
            b't' => {
                corr_id.turn = Some(
                    value
                        .parse()
                        .map_err(|_| ParseCorrIdError::InvalidInteger)?,
                )
            }
            b'c' => corr_id.tool = Some(value),
            b'a' => {
                corr_id.attempt = Some(
                    value
                        .parse()
                        .map_err(|_| ParseCorrIdError::InvalidInteger)?,
                )
            }
            _ => unreachable!("segment tags are validated by split_segments"),
        }

        last_index = Some(segment.index);
    }

    Ok(corr_id)
}

/// Expand `\:` and `\\` escapes in a raw segment value.
fn unescape(raw: &str) -> Result<String, ParseCorrIdError> {
    let mut value = String::with_capacity(raw.len());
    let mut chars = raw.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            value.push(ch);
            continue;
        }

        match chars.next() {
            Some(':') => value.push(':'),
            Some('\\') => value.push('\\'),
            _ => return Err(ParseCorrIdError::InvalidEscape),
        }
    }

    Ok(value)
}

/// Validate and own a string segment value.
fn into_segment(value: impl Into<String>) -> String {
    let value = value.into();
    assert!(
        !value.is_empty(),
        "correlation-id string segments must be non-empty"
    );
    value
}

/// Escape a string segment value for the tagged form.
fn escape_segment(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            ':' => escaped.push_str("\\:"),
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Check whether `prefix_run` is an ancestor of `event_run` in the
/// colon-delimited hierarchy. A `None` prefix matches anything.
///
/// ```text
/// prefix: "parent"       → matches "parent", "parent:child", "parent:child:grand"
/// prefix: "parent:child" → matches "parent:child", "parent:child:grand"
/// prefix: "parent:child:grand" → matches only "parent:child:grand"
/// ```
///
/// Matching operates on the unescaped run value. A literal colon that was
/// escaped in the CorrId string form (e.g. `r:parent\:child`) is
/// indistinguishable from a hierarchy separator at match time: the prefix
/// `r:parent` matches the run value `parent:child` as an ancestor.
fn run_matches_ancestor(prefix_run: Option<&str>, event_run: Option<&str>) -> bool {
    match (prefix_run, event_run) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(prefix), Some(event)) => {
            let mut ev = event.split(':');
            prefix.split(':').all(|p| ev.next() == Some(p))
        }
    }
}

/// A comparison over optional segment values: `None` is a wildcard.
fn segment_matches<T: PartialEq>(actual: &Option<T>, prefix: &Option<T>) -> bool {
    match prefix {
        None => true,
        Some(expected) => actual.as_ref() == Some(expected),
    }
}

impl fmt::Display for CorrId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut segments: Vec<String> = Vec::new();

        if let Some(run) = &self.run {
            segments.push(format!("r:{}", escape_segment(run)));
        }
        if let Some(stage) = &self.stage {
            segments.push(format!("s:{}", escape_segment(stage)));
        }
        if let Some(turn) = self.turn {
            segments.push(format!("t:{turn}"));
        }
        if let Some(tool) = &self.tool {
            segments.push(format!("c:{}", escape_segment(tool)));
        }
        if let Some(attempt) = self.attempt {
            segments.push(format!("a:{attempt}"));
        }

        f.write_str(&segments.join(":"))
    }
}

impl FromStr for CorrId {
    type Err = ParseCorrIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        CorrId::try_parse(s)
    }
}

/// Error returned when a string is not a well-formed correlation identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseCorrIdError {
    /// A segment starts with a tag other than `r`, `s`, `t`, `c`, or `a`.
    UnknownTag,
    /// A tag is not followed by `:`.
    MissingColon,
    /// A segment has no value.
    EmptyValue,
    /// The same segment tag appears more than once.
    DuplicateTag,
    /// Segments appear out of the run -> stage -> turn -> tool -> attempt order.
    OutOfOrder,
    /// A `\` escape is not followed by `\` or `:`.
    InvalidEscape,
    /// The string ends with a `:` that is not followed by a segment tag.
    TrailingDelimiter,
    /// A `turn` or `attempt` value is not a valid integer.
    InvalidInteger,
}

impl fmt::Display for ParseCorrIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnknownTag => "unknown segment tag",
            Self::MissingColon => "segment tag not followed by ':'",
            Self::EmptyValue => "empty segment value",
            Self::DuplicateTag => "duplicate segment tag",
            Self::OutOfOrder => "segment tags out of order",
            Self::InvalidEscape => "invalid escape sequence",
            Self::TrailingDelimiter => "trailing ':' without a segment tag",
            Self::InvalidInteger => "invalid integer segment value",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ParseCorrIdError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> CorrId {
        CorrId::new()
            .with_run("abc123")
            .with_stage("reviews/checks")
            .with_turn(3)
            .with_tool("call_0")
            .with_attempt(1)
    }

    #[test]
    fn display_omits_missing_segments() {
        assert_eq!(CorrId::new().to_string(), "");
        assert_eq!(CorrId::new().with_run("X").to_string(), "r:X");
        assert_eq!(CorrId::new().with_turn(3).to_string(), "t:3");
    }

    #[test]
    fn full_round_trip() {
        let corr_id = full();
        let rendered = "r:abc123:s:reviews/checks:t:3:c:call_0:a:1";
        assert_eq!(corr_id.to_string(), rendered);
        assert_eq!(rendered.parse::<CorrId>().unwrap(), corr_id);
    }

    #[test]
    fn empty_string_round_trips() {
        assert_eq!(CorrId::new().to_string(), "");
        assert_eq!("".parse::<CorrId>().unwrap(), CorrId::new());
    }

    #[test]
    fn values_with_colons_and_backslashes_round_trip() {
        let corr_id = CorrId::new()
            .with_run("a:b")
            .with_stage("x:s:y")
            .with_tool("c\\d");

        let rendered = corr_id.to_string();
        assert_eq!(rendered, r"r:a\:b:s:x\:s\:y:c:c\\d");
        assert_eq!(rendered.parse::<CorrId>().unwrap(), corr_id);
    }

    #[test]
    fn prefix_is_segment_aware() {
        let corr_id = full();

        assert!(corr_id.matches_prefix(
            &CorrId::new()
                .with_run("abc123")
                .with_stage("reviews/checks")
        ));
        assert!(corr_id.matches_prefix(&CorrId::new()));
        assert!(corr_id.matches_prefix(&CorrId::new().with_attempt(1)));

        assert!(!corr_id.matches_prefix(&CorrId::new().with_run("ab")));
        assert!(!corr_id.matches_prefix(&CorrId::new().with_run("abc123").with_turn(9)));
        assert!(!corr_id.matches_prefix(&CorrId::new().with_stage("reviews")));
    }

    #[test]
    fn run_ancestor_matches_self_and_descendants() {
        // "parent" matches "parent", "parent:child", "parent:child:grand"
        let prefix = CorrId::new().with_run("parent");

        assert!(CorrId::new().with_run("parent").matches_prefix(&prefix));
        assert!(CorrId::new()
            .with_run("parent:child")
            .matches_prefix(&prefix));
        assert!(CorrId::new()
            .with_run("parent:child:grand")
            .matches_prefix(&prefix));

        // Does NOT match a different run
        assert!(!CorrId::new().with_run("other").matches_prefix(&prefix));
        // Does NOT match substring prefix
        assert!(!CorrId::new().with_run("par").matches_prefix(&prefix));
    }

    #[test]
    fn run_ancestor_nested_prefix_matches_descendants_only() {
        // "parent:child" matches "parent:child" and "parent:child:grand"
        let prefix = CorrId::new().with_run("parent:child");

        assert!(CorrId::new()
            .with_run("parent:child")
            .matches_prefix(&prefix));
        assert!(CorrId::new()
            .with_run("parent:child:grand")
            .matches_prefix(&prefix));

        // Does NOT match parent or sibling
        assert!(!CorrId::new().with_run("parent").matches_prefix(&prefix));
        assert!(!CorrId::new()
            .with_run("parent:sibling")
            .matches_prefix(&prefix));
    }

    #[test]
    fn run_ancestor_exact_leaf_matches_self_only() {
        // "parent:child:grand" matches only itself
        let prefix = CorrId::new().with_run("parent:child:grand");

        assert!(CorrId::new()
            .with_run("parent:child:grand")
            .matches_prefix(&prefix));
        assert!(!CorrId::new()
            .with_run("parent:child")
            .matches_prefix(&prefix));
        assert!(!CorrId::new().with_run("parent").matches_prefix(&prefix));
    }

    #[test]
    fn run_ancestor_with_other_segments_still_matches() {
        // run ancestor match combined with exact match on other fields
        let prefix = CorrId::new().with_run("parent").with_stage("build");

        let child_event = CorrId::new()
            .with_run("parent:child-a")
            .with_stage("build")
            .with_turn(1);

        assert!(child_event.matches_prefix(&prefix));

        // Stage mismatch still fails
        assert!(!CorrId::new()
            .with_run("parent:child-a")
            .with_stage("test")
            .matches_prefix(&prefix));
    }

    #[test]
    fn escaped_colon_is_indistinguishable_from_hierarchy_separator() {
        // r:parent\:child — the \: is a literal colon in the run value, not a hierarchy separator
        let corr_id: CorrId = "r:parent\\:child:s:build".parse().unwrap();
        assert_eq!(corr_id.run(), Some("parent:child"));
        assert_eq!(corr_id.stage(), Some("build"));

        // The run value "parent:child" has a literal colon. Because matching
        // operates on the *unescaped* string value (escape information is not
        // threaded through `run_matches_ancestor`), a literal colon is
        // indistinguishable from a hierarchy separator here: the escaped run
        // "parent:child" IS matched by the prefix "parent".
        let prefix = CorrId::new().with_run("parent:child");
        assert!(corr_id.matches_prefix(&prefix));

        // A prefix run of "parent" matches the run value "parent:child" as an
        // ancestor. This is the documented behavior of ancestor matching.
        let parent_prefix = CorrId::new().with_run("parent");
        assert!(corr_id.matches_prefix(&parent_prefix));
    }

    #[test]
    fn order_and_uniqueness_are_enforced() {
        assert_eq!(
            CorrId::try_parse("t:3:r:X").unwrap_err(),
            ParseCorrIdError::OutOfOrder
        );
        assert_eq!(
            CorrId::try_parse("r:A:r:B").unwrap_err(),
            ParseCorrIdError::DuplicateTag
        );
        assert_eq!(
            CorrId::try_parse("x:1").unwrap_err(),
            ParseCorrIdError::UnknownTag
        );
        assert_eq!(
            CorrId::try_parse("r:").unwrap_err(),
            ParseCorrIdError::EmptyValue
        );
        assert_eq!(
            CorrId::try_parse("r").unwrap_err(),
            ParseCorrIdError::MissingColon
        );
        assert!(CorrId::try_parse("a:1").is_ok());
    }

    #[test]
    fn parser_rejects_tricky_boundaries() {
        // A value whose first character is a tag char is not a panic; the
        // parser simply sees a tag that is missing its own colon.
        assert!(CorrId::try_parse("r:a:s").is_err());
        assert_eq!(
            CorrId::try_parse("r:X:").unwrap_err(),
            ParseCorrIdError::TrailingDelimiter
        );
        assert_eq!(
            CorrId::try_parse(r"r:a\q").unwrap_err(),
            ParseCorrIdError::InvalidEscape
        );
        assert_eq!(
            CorrId::try_parse(r"r:a\").unwrap_err(),
            ParseCorrIdError::InvalidEscape
        );
    }

    #[test]
    fn invalid_integers_are_reported() {
        assert_eq!(
            CorrId::try_parse("t:not-a-number").unwrap_err(),
            ParseCorrIdError::InvalidInteger
        );
    }

    #[test]
    #[should_panic(expected = "non-empty")]
    fn empty_string_segments_are_rejected() {
        let _ = CorrId::new().with_run("");
    }
}
