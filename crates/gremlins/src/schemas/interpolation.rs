use std::collections::{HashMap, VecDeque};
use std::iter::Peekable;
use std::ops::Range;
use std::str::CharIndices;
use std::sync::LazyLock;

use regex::Regex;

static VAR_SUB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([-\w]+)\}").unwrap());

/// A `{key}` with a known value sat inside a here-document body, where not even
/// backslash escaping is available: an unquoted body expands `$`, backquotes
/// and `\` but cannot escape newlines, and a quoted body expands nothing.
#[derive(Debug, thiserror::Error)]
#[error("{key:?} is interpolated inside the here-document body delimited by {delimiter:?}; assign it outside the here-document instead")]
pub struct HereDocInterpolation {
    pub key: String,
    pub delimiter: String,
}

/// Replace `{key}` tokens with values. Unknown keys are left as-is, `${key}` is
/// skipped, and `-`/`_` are interchangeable in keys.
///
/// `values` is searched in precedence order: the first map holding a key wins.
pub fn substitute_vars(text: &str, values: &[&HashMap<String, String>]) -> String {
    VAR_SUB_RE
        .replace_all(text, |caps: &regex::Captures| {
            let whole = caps.get(0).unwrap();
            match lookup(values, caps.get(1).unwrap().as_str()) {
                Some(value) if !preceded_by_dollar(text, whole.start()) => value.clone(),
                _ => whole.as_str().to_string(),
            }
        })
        .to_string()
}

/// [`substitute_vars`] for shell command strings, escaping each value for the
/// quoting region it lands in: backslash-escaped inside `"..."`,
/// `'\''`-escaped inside `'...'`, single-quoted when unquoted. A template that
/// already quotes a placeholder therefore keeps its written form.
///
/// Interpolation inside a here-document body is refused — see
/// [`HereDocInterpolation`].
pub fn substitute_vars_into_shell(
    text: &str,
    values: &[&HashMap<String, String>],
) -> Result<String, HereDocInterpolation> {
    let bodies = scan(text).bodies;
    let mut refusal = None;
    let out = VAR_SUB_RE
        .replace_all(text, |caps: &regex::Captures| {
            let whole = caps.get(0).unwrap();
            let key = caps.get(1).unwrap().as_str();
            let value = match lookup(values, key) {
                Some(value) if !preceded_by_dollar(text, whole.start()) => value,
                _ => return whole.as_str().to_string(),
            };
            if let Some((_, delimiter)) = bodies.iter().find(|(r, _)| r.contains(&whole.start())) {
                refusal.get_or_insert_with(|| HereDocInterpolation {
                    key: key.to_string(),
                    delimiter: delimiter.clone(),
                });
                return whole.as_str().to_string();
            }
            shell_escape(&text[..whole.start()], value)
        })
        .to_string();
    refusal.map_or(Ok(out), Err)
}

fn preceded_by_dollar(text: &str, at: usize) -> bool {
    at > 0 && text.as_bytes()[at - 1] == b'$'
}

/// Exact key, then the `-`→`_` and `_`→`-` spellings, resolved per map so that
/// precedence between maps is preserved. Normalizing is skipped for the common
/// key that carries neither separator.
fn lookup<'a>(values: &[&'a HashMap<String, String>], key: &str) -> Option<&'a String> {
    let variants = [
        key.contains('-').then(|| key.replace('-', "_")),
        key.contains('_').then(|| key.replace('_', "-")),
    ];
    values.iter().find_map(|map| {
        std::iter::once(key)
            .chain(variants.iter().flatten().map(String::as_str))
            .find_map(|k| map.get(k))
    })
}

/// Quoting region a value lands in at the end of a prefix.
#[derive(PartialEq, Clone, Copy)]
enum Region {
    Single,
    Double,
}

/// Substitution forms open a frame whose quoting is independent of the
/// enclosing text. `Root` is the always-present text frame.
#[derive(Clone, Copy)]
enum Kind {
    Root,
    Subshell,
    Arithmetic,
    Backtick,
}

#[derive(Clone, Copy)]
struct Frame {
    quote: Option<Region>,
    kind: Kind,
}

struct Scan {
    /// Quoting region open at the end of the scanned text.
    quote: Option<Region>,
    /// Byte range of every here-document body, with its delimiter.
    bodies: Vec<(Range<usize>, String)>,
}

fn shell_escape(prefix: &str, value: &str) -> String {
    match scan(prefix).quote {
        Some(Region::Double) => escape_double_quoted(value),
        Some(Region::Single) => escape_single_quoted(value),
        None => format!("'{}'", escape_single_quoted(value)),
    }
}

fn escape_single_quoted(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn escape_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '`' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Quoting region active at the end of `prefix`. Whole-line quote counting is
/// not enough: `$(git rev-list --count "{n}..HEAD")` holds two quotes yet the
/// placeholder sits inside a double-quoted region, and `git diff "{a}..HEAD"`
/// holds four of them, so a naive scan would find the placeholder unquoted.
/// Here-document bodies are recorded rather than descended into: their contents
/// are literal text until the terminator line, whatever quotes they hold.
fn scan(text: &str) -> Scan {
    let mut frames = vec![Frame {
        quote: None,
        kind: Kind::Root,
    }];
    let mut pending: VecDeque<(String, bool)> = VecDeque::new();
    let mut bodies = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        let quote = frames.last().unwrap().quote;
        match c {
            // An unquoted newline ends the command line, so the here-documents
            // announced on it start contributing bodies, in announcement order.
            '\n' if quote.is_none() => {
                let mut start = idx + 1;
                while let Some((delimiter, strip_tabs)) = pending.pop_front() {
                    match body_end(text, start, &delimiter, strip_tabs) {
                        Some(end) => bodies.push((start..end, delimiter)),
                        None => {
                            bodies.push((start..text.len(), delimiter));
                            return Scan {
                                quote: None,
                                bodies,
                            };
                        }
                    }
                    start = bodies.last().unwrap().0.end;
                }
                while chars.peek().is_some_and(|(i, _)| *i < start) {
                    chars.next();
                }
            }
            // Backslash escapes the next character outside single quotes.
            '\\' if quote != Some(Region::Single) => {
                chars.next();
            }
            '\'' if quote != Some(Region::Double) => toggle(&mut frames, Region::Single),
            '"' if quote != Some(Region::Single) => toggle(&mut frames, Region::Double),
            '`' if quote != Some(Region::Single) => toggle_backtick(&mut frames),
            '<' if quote.is_none()
                && !matches!(frames.last().unwrap().kind, Kind::Arithmetic)
                && chars.peek().is_some_and(|(_, c)| *c == '<') =>
            {
                chars.next();
                if let Some((delimiter, strip_tabs)) = heredoc_word(&mut chars) {
                    pending.push_back((delimiter, strip_tabs));
                }
            }
            '$' if quote != Some(Region::Single)
                && chars.peek().is_some_and(|(_, c)| *c == '(') =>
            {
                chars.next();
                let arithmetic = chars.peek().is_some_and(|(_, c)| *c == '(');
                if arithmetic {
                    chars.next();
                }
                frames.push(Frame {
                    quote: None,
                    kind: if arithmetic {
                        Kind::Arithmetic
                    } else {
                        Kind::Subshell
                    },
                });
            }
            ')' if quote.is_none() => match frames.last().unwrap().kind {
                Kind::Arithmetic if chars.peek().is_some_and(|(_, c)| *c == ')') => {
                    chars.next();
                    frames.pop();
                }
                Kind::Subshell => {
                    frames.pop();
                }
                _ => {}
            },
            _ => {}
        }
    }
    Scan {
        quote: frames.last().unwrap().quote,
        bodies,
    }
}

/// Index just past the terminator line of the body starting at `start`, or
/// `None` while the body is still open at the end of `text`. A trailing line
/// with no newline never terminates a body, not even when it spells the
/// delimiter: whatever is substituted after it would join that same line.
fn body_end(text: &str, start: usize, delimiter: &str, strip_tabs: bool) -> Option<usize> {
    let mut i = start;
    while let Some(offset) = text[i..].find('\n') {
        let line = &text[i..i + offset];
        i += offset + 1;
        let line = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if line == delimiter {
            return Some(i);
        }
    }
    None
}

/// Parse the word of a here-document operator just past its `<<`: the delimiter
/// with its quoting removed, and whether `<<-` strips leading tabs. `None` when
/// the word is empty or starts with `&`/`<`, as in `<<<`.
fn heredoc_word(chars: &mut Peekable<CharIndices<'_>>) -> Option<(String, bool)> {
    let strip_tabs = chars.peek().is_some_and(|(_, c)| *c == '-');
    if strip_tabs {
        chars.next();
    }
    while chars.peek().is_some_and(|(_, c)| *c == ' ' || *c == '\t') {
        chars.next();
    }
    let mut delimiter = String::new();
    while let Some((_, c)) = chars.peek().copied() {
        match c {
            ' ' | '\t' | '\n' | ';' | '|' | '(' | ')' => break,
            '<' | '>' | '&' => return None,
            '\'' | '"' => {
                chars.next();
            }
            '\\' => {
                chars.next();
                if let Some((_, escaped)) = chars.next() {
                    delimiter.push(escaped);
                }
            }
            _ => {
                chars.next();
                delimiter.push(c);
            }
        }
    }
    (!delimiter.is_empty()).then_some((delimiter, strip_tabs))
}

fn toggle(frames: &mut [Frame], region: Region) {
    let frame = &mut frames[frames.len() - 1];
    frame.quote = if frame.quote == Some(region) {
        None
    } else {
        Some(region)
    };
}

fn toggle_backtick(frames: &mut Vec<Frame>) {
    if matches!(frames.last().unwrap().kind, Kind::Backtick) {
        frames.pop();
    } else {
        frames.push(Frame {
            quote: None,
            kind: Kind::Backtick,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(k: &str, v: &str) -> HashMap<String, String> {
        HashMap::from([(k.to_string(), v.to_string())])
    }

    fn plain(text: &str, k: &str, v: &str) -> String {
        substitute_vars(text, &[&one(k, v)])
    }

    fn shell(text: &str, k: &str, v: &str) -> String {
        substitute_vars_into_shell(text, &[&one(k, v)]).unwrap()
    }

    fn shell_err(text: &str, k: &str, v: &str) -> HereDocInterpolation {
        substitute_vars_into_shell(text, &[&one(k, v)]).unwrap_err()
    }

    #[test]
    fn test_substitute_vars_basic() {
        assert_eq!(plain("hello {var}", "var", "world"), "hello world");
    }

    #[test]
    fn test_substitute_vars_precedence_first_map_wins() {
        let high = one("k", "high");
        let low = one("k", "low");
        assert_eq!(substitute_vars("{k}", &[&high, &low]), "high");
        assert_eq!(substitute_vars("{k}", &[&low, &high]), "low");
    }

    #[test]
    fn test_substitute_vars_precedence_falls_through_to_later_map() {
        let empty = HashMap::new();
        let one_map = one("k", "v");
        assert_eq!(substitute_vars("{k}", &[&empty, &one_map]), "v");
    }

    #[test]
    fn test_substitute_vars_hyphen_normalization() {
        assert_eq!(plain("{child-plan}", "child_plan", "value"), "value");
        assert_eq!(plain("{review-one}", "review-one", "done"), "done");
        assert_eq!(plain("{a-b-c}", "a_b_c", "v"), "v");
    }

    #[test]
    fn test_substitute_vars_underscore_normalization() {
        assert_eq!(plain("{child_plan}", "child-plan", "value"), "value");
        assert_eq!(plain("{review_one}", "review_one", "done"), "done");
        assert_eq!(plain("{a_b_c}", "a-b-c", "v"), "v");
    }

    #[test]
    fn test_substitute_vars_hyphen_aliases_respect_precedence() {
        let high = one("child_plan", "high");
        let low = one("child-plan", "low");
        assert_eq!(substitute_vars("{child-plan}", &[&high, &low]), "high");
        assert_eq!(substitute_vars("{child_plan}", &[&high, &low]), "high");
    }

    #[test]
    fn test_substitute_vars_underscore_aliases_respect_precedence() {
        let high = one("child-plan", "high");
        let low = one("child_plan", "low");
        assert_eq!(substitute_vars("{child_plan}", &[&high, &low]), "high");
        assert_eq!(substitute_vars("{child-plan}", &[&high, &low]), "high");
    }

    #[test]
    fn test_substitute_vars_unknown_token() {
        assert_eq!(plain("hello {unknown}", "other", "v"), "hello {unknown}");
    }

    #[test]
    fn test_substitute_vars_dollar_skip() {
        assert_eq!(plain("${x}", "x", "y"), "${x}");
    }

    #[test]
    fn test_substitute_vars_doubled_braces() {
        assert_eq!(plain("{{name}}", "name", "value"), "{value}");
    }

    #[test]
    fn test_substitute_vars_no_match() {
        let empty = HashMap::new();
        assert_eq!(
            substitute_vars("no braces here", &[&empty]),
            "no braces here"
        );
        assert_eq!(substitute_vars("", &[&empty]), "");
    }

    #[test]
    fn test_substitute_vars_multiple_tokens() {
        let values = HashMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        assert_eq!(substitute_vars("{a} and {b}", &[&values]), "1 and 2");
    }

    #[test]
    fn test_substitute_vars_into_shell_quotes_bare_values() {
        assert_eq!(
            shell("test -n {x}", "x", "do the thing"),
            "test -n 'do the thing'"
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_keeps_double_quoted_templates_readable() {
        assert_eq!(
            shell(r#"test -n "{x}""#, "x", "do the thing"),
            r#"test -n "do the thing""#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_blocks_double_quote_breakout() {
        assert_eq!(
            shell(r#"echo "{x}""#, "x", r#""; rm -rf /; echo ""#),
            r#"echo "\"; rm -rf /; echo \"""#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_blocks_command_substitution() {
        assert_eq!(
            shell(r#"echo "{x}""#, "x", "$(touch /tmp/pwned)"),
            r#"echo "\$(touch /tmp/pwned)""#
        );
        assert_eq!(shell(r#"echo "{x}""#, "x", "`id`"), r#"echo "\`id\`""#);
    }

    #[test]
    fn test_substitute_vars_into_shell_blocks_single_quote_breakout() {
        assert_eq!(
            shell("echo '{x}'", "x", "'; rm -rf /; echo '"),
            r#"echo ''\''; rm -rf /; echo '\'''"#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_bare_injection_is_quoted_away() {
        assert_eq!(
            shell("echo {x}", "x", "'; rm -rf /; echo 'pwned"),
            r#"echo ''\''; rm -rf /; echo '\''pwned'"#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_ignores_quotes_after_token() {
        assert_eq!(
            shell(r#"echo {x} "tail""#, "x", "a;b"),
            r#"echo 'a;b' "tail""#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_escaped_quote_does_not_open_region() {
        assert_eq!(shell(r#"echo \" {x}"#, "x", "a;b"), r#"echo \" 'a;b'"#);
    }

    #[test]
    fn test_substitute_vars_into_shell_dollar_token_skipped() {
        assert_eq!(shell(r#"echo "${x}""#, "x", "y"), r#"echo "${x}""#);
    }

    #[test]
    fn test_substitute_vars_into_shell_hyphen_normalization() {
        assert_eq!(shell("echo {child-plan}", "child_plan", "v"), "echo 'v'");
        assert_eq!(shell("echo {child_plan}", "child-plan", "v"), "echo 'v'");
    }

    #[test]
    fn test_substitute_vars_into_shell_empty_value() {
        assert_eq!(shell(r#"test -n "{x}""#, "x", ""), r#"test -n """#);
    }

    #[test]
    fn test_substitute_vars_into_shell_quote_inside_command_substitution() {
        // Two quotes in the line, but the token sits inside the inner quotes.
        assert_eq!(
            shell(
                r#"test "$(git rev-list --count "{n}..HEAD")" -gt 0"#,
                "n",
                "a;b"
            ),
            r#"test "$(git rev-list --count "a;b..HEAD")" -gt 0"#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_four_quotes_around_token() {
        assert_eq!(
            shell(r#"echo "$(date)"; cp "{a}" /tmp"#, "a", "/x y"),
            r#"echo "$(date)"; cp "/x y" /tmp"#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_closed_quotes_leave_token_bare() {
        assert_eq!(
            shell(r#"echo "head" {x}"#, "x", "a;b"),
            r#"echo "head" 'a;b'"#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_backtick_substitution() {
        assert_eq!(
            shell(r#"echo `date` {x}"#, "x", "a;b"),
            r#"echo `date` 'a;b'"#
        );
        assert_eq!(
            shell(r#"echo `echo "{x}"`"#, "x", "a;b"),
            r#"echo `echo "a;b"`"#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_nested_substitution_resumes_outer_quote() {
        // The inner quotes belong to the subshell frame, so the outer string is
        // still open when the token lands.
        assert_eq!(
            shell(r#"echo "$(echo "$(inner)") {x}""#, "x", "a;b"),
            r#"echo "$(echo "$(inner)") a;b""#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_refuses_heredoc_body() {
        // Quotes are literal in an unquoted body, so `'\''…` would not quote.
        assert_eq!(shell_err("cat <<EOF\n{x}\nEOF", "x", "a").delimiter, "EOF");
    }

    #[test]
    fn test_substitute_vars_into_shell_refuses_quoted_heredoc_body() {
        assert_eq!(
            shell_err("cat <<'EOF'\n{x}\nEOF", "x", "a").delimiter,
            "EOF"
        );
        assert_eq!(
            shell_err("cat <<\\EOF\n{x}\nEOF", "x", "a").delimiter,
            "EOF"
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_refuses_tab_stripped_heredoc_body() {
        assert_eq!(
            shell_err("cat <<-EOF\n\t{x}\n\tEOF", "x", "a").delimiter,
            "EOF"
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_refuses_unterminated_heredoc() {
        assert_eq!(shell_err("cat <<EOF\n{x}", "x", "a").delimiter, "EOF");
        // The value would join the partial terminator line below it.
        assert_eq!(
            shell_err("cat <<EOF\nEOF{x}\nEOF", "x", "a").delimiter,
            "EOF"
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_resumes_after_heredoc() {
        assert_eq!(
            shell("cat <<EOF\nbody\nEOF\necho {x}", "x", "a;b"),
            "cat <<EOF\nbody\nEOF\necho 'a;b'"
        );
        assert_eq!(
            shell("echo {x} <<EOF\nbody\nEOF", "x", "a;b"),
            "echo 'a;b' <<EOF\nbody\nEOF"
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_ignores_heredoc_lookalikes() {
        assert_eq!(shell("echo $((1 << {x}))", "x", "2"), "echo $((1 << '2'))");
        assert_eq!(shell("cat <<<{x}", "x", "a;b"), "cat <<<'a;b'");
        assert_eq!(
            shell(r#"echo "<<EOF" {x}"#, "x", "a;b"),
            r#"echo "<<EOF" 'a;b'"#
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_unknown_token_in_heredoc_left_verbatim() {
        assert_eq!(
            shell("cat <<EOF\n{unknown}\nEOF", "x", "a"),
            "cat <<EOF\n{unknown}\nEOF"
        );
    }
}
