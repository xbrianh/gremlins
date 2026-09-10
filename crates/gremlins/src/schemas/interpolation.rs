use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

static VAR_SUB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([-\w]+)\}").unwrap());

/// Replace `{key}` tokens with values. Unknown keys are left as-is, `${key}` is
/// skipped, and underscore keys also match their hyphenated form.
pub fn substitute_vars(text: &str, values: &HashMap<String, String>) -> String {
    let mut vals = values.clone();
    let hyphenated: Vec<(String, String)> = values
        .iter()
        .filter(|(k, _)| k.contains('_'))
        .map(|(k, v)| (k.replace('_', "-"), v.clone()))
        .collect();
    for (k, v) in hyphenated {
        vals.entry(k).or_insert(v);
    }

    VAR_SUB_RE
        .replace_all(text, |caps: &regex::Captures| {
            let whole = caps.get(0).unwrap();
            if whole.start() > 0 && text.as_bytes()[whole.start() - 1] == b'$' {
                return whole.as_str().to_string();
            }
            let key = caps.get(1).unwrap().as_str();
            if let Some(val) = vals.get(key) {
                return val.clone();
            }
            let alt = key.replace('-', "_");
            vals.get(&alt)
                .cloned()
                .unwrap_or_else(|| whole.as_str().to_string())
        })
        .to_string()
}

/// [`substitute_vars`] with every value single-quoted, so untrusted values
/// cannot inject shell metacharacters into a command string.
pub fn substitute_vars_into_shell(text: &str, values: &HashMap<String, String>) -> String {
    let quoted: HashMap<String, String> = values
        .iter()
        .map(|(k, v)| (k.clone(), shell_quote(v)))
        .collect();
    substitute_vars(text, &quoted)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str, k: &str, v: &str) -> String {
        substitute_vars(text, &HashMap::from([(k.to_string(), v.to_string())]))
    }

    fn shell(text: &str, k: &str, v: &str) -> String {
        substitute_vars_into_shell(text, &HashMap::from([(k.to_string(), v.to_string())]))
    }

    #[test]
    fn test_substitute_vars_basic() {
        assert_eq!(plain("hello {var}", "var", "world"), "hello world");
    }

    #[test]
    fn test_substitute_vars_hyphen_normalization() {
        assert_eq!(plain("{child-plan}", "child_plan", "value"), "value");
        assert_eq!(plain("{review-one}", "review-one", "done"), "done");
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
        assert_eq!(
            substitute_vars("no braces here", &HashMap::new()),
            "no braces here"
        );
        assert_eq!(substitute_vars("", &HashMap::new()), "");
    }

    #[test]
    fn test_substitute_vars_into_shell_quotes_values() {
        assert_eq!(
            shell("echo {x}", "x", "do the thing"),
            "echo 'do the thing'"
        );
    }

    #[test]
    fn test_substitute_vars_into_shell_blocks_injection() {
        assert_eq!(
            shell("echo {x}", "x", "'; rm -rf /; echo 'pwned"),
            "echo ''\\''; rm -rf /; echo '\\''pwned'"
        );
    }
}
