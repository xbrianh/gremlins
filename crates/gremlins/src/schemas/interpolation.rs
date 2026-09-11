use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

static VAR_SUB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([-\w]+)\}").unwrap());

/// Replace `{key}` tokens with values. Unknown keys are left as-is, `${key}` is
/// skipped, and underscore keys also match their hyphenated form.
///
/// `values` is searched in precedence order: the first map holding a key wins.
pub fn substitute_vars(text: &str, values: &[&HashMap<String, String>]) -> String {
    substitute(text, values, false)
}

/// [`substitute_vars`] for shell command strings, escaping each value for the
/// quoting context it lands in: backslash-escaped inside `"..."`,
/// `'\''`-escaped inside `'...'`, single-quoted when unquoted. A template that
/// already quotes a placeholder therefore keeps its written form.
pub fn substitute_vars_into_shell(text: &str, values: &[&HashMap<String, String>]) -> String {
    substitute(text, values, true)
}

fn substitute(text: &str, values: &[&HashMap<String, String>], shell: bool) -> String {
    VAR_SUB_RE
        .replace_all(text, |caps: &regex::Captures| {
            let whole = caps.get(0).unwrap();
            if whole.start() > 0 && text.as_bytes()[whole.start() - 1] == b'$' {
                return whole.as_str().to_string();
            }
            match lookup(values, caps.get(1).unwrap().as_str()) {
                Some(v) if shell => shell_escape(&text[..whole.start()], v),
                Some(v) => v.clone(),
                None => whole.as_str().to_string(),
            }
        })
        .to_string()
}

/// Exact key, then the `-`→`_` variant, resolved per map so that precedence
/// between maps is preserved. Normalizing is skipped for the common key that
/// carries no hyphen.
fn lookup<'a>(values: &[&'a HashMap<String, String>], key: &str) -> Option<&'a String> {
    let underscored = key.contains('-').then(|| key.replace('-', "_"));
    for map in values {
        let hit = map
            .get(key)
            .or_else(|| underscored.as_ref().and_then(|alt| map.get(alt)));
        if let Some(v) = hit {
            return Some(v);
        }
    }
    None
}

enum Quote {
    None,
    Single,
    Double,
}

/// Quoting region a value lands in at the end of a prefix.
#[derive(PartialEq, Clone, Copy)]
enum Region {
    Single,
    Double,
}

/// Substitution forms open a frame whose quoting is independent of the
/// enclosing text. `Root` is the always-present text frame.
enum Kind {
    Root,
    Subshell,
    Backtick,
}

struct Frame {
    quote: Option<Region>,
    kind: Kind,
}

fn shell_escape(prefix: &str, value: &str) -> String {
    match quoting_context(prefix) {
        Quote::Double => escape_double_quoted(value),
        Quote::Single => escape_single_quoted(value),
        Quote::None => format!("'{}'", escape_single_quoted(value)),
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
fn quoting_context(prefix: &str) -> Quote {
    let mut frames = vec![Frame {
        quote: None,
        kind: Kind::Root,
    }];
    let mut chars = prefix.chars().peekable();
    while let Some(c) = chars.next() {
        let quote = frames.last().unwrap().quote;
        match c {
            // Backslash escapes the next character outside single quotes.
            '\\' if quote != Some(Region::Single) => {
                chars.next();
            }
            '\'' if quote != Some(Region::Double) => toggle(&mut frames, Region::Single),
            '"' if quote != Some(Region::Single) => toggle(&mut frames, Region::Double),
            '`' if quote != Some(Region::Single) => toggle_backtick(&mut frames),
            '$' if quote != Some(Region::Single) && chars.peek() == Some(&'(') => {
                chars.next();
                frames.push(Frame {
                    quote: None,
                    kind: Kind::Subshell,
                });
            }
            ')' if quote.is_none() && matches!(frames.last().unwrap().kind, Kind::Subshell) => {
                frames.pop();
            }
            _ => {}
        }
    }
    match frames.last().unwrap().quote {
        Some(Region::Single) => Quote::Single,
        Some(Region::Double) => Quote::Double,
        None => Quote::None,
    }
}

fn toggle(frames: &mut [Frame], region: Region) {
    let frame = frames.last_mut().unwrap();
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
        substitute_vars_into_shell(text, &[&one(k, v)])
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
    fn test_substitute_vars_hyphen_aliases_respect_precedence() {
        let high = one("child_plan", "high");
        let low = one("child-plan", "low");
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
}
