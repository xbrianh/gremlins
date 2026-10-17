//! Line-by-line, regex-based structural outliner.
//!
//! This is deliberately heuristic: it never tries to parse a language. Unknown
//! extensions scan to an empty outline rather than erroring, so the caller can
//! always inspect a file's structure and fall back to `Read` for the body.

use std::sync::OnceLock;

use regex::Regex;

use super::{OutlineItem, CLASS, ENUM, FN, HEADING, IMPL, KEY, METHOD, MOD, STRUCT, TRAIT};

pub(crate) fn scan(extension: &str, content: &str) -> Vec<OutlineItem> {
    match extension {
        "rs" => scan_rust(content),
        "md" | "markdown" => scan_markdown(content),
        "json" => scan_json_keys(content),
        "toml" | "ini" | "cfg" | "conf" => scan_config_sections(content),
        "py" | "pyi" => scan_python(content),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => scan_javascript(content),
        _ => Vec::new(),
    }
}

pub(crate) fn recognizes(extension: &str) -> bool {
    matches!(
        extension,
        "rs" | "md"
            | "markdown"
            | "json"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "py"
            | "pyi"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "mjs"
            | "cjs"
    )
}

fn item(kind: &'static str, name: &str, line: usize) -> OutlineItem {
    OutlineItem {
        kind,
        name: name.trim().to_string(),
        line,
    }
}

fn kind_for(kind: &str) -> Option<&'static str> {
    match kind {
        "fn" => Some(FN),
        "struct" => Some(STRUCT),
        "impl" => Some(IMPL),
        "trait" => Some(TRAIT),
        "enum" => Some(ENUM),
        "mod" => Some(MOD),
        "class" => Some(CLASS),
        "method" | "def" => Some(METHOD),
        "heading" => Some(HEADING),
        "key" | "section" => Some(KEY),
        _ => None,
    }
}

fn scan_rust(content: &str) -> Vec<OutlineItem> {
    static ITEM_RE: OnceLock<Regex> = OnceLock::new();
    let item_re = ITEM_RE.get_or_init(|| {
        // Qualifiers may appear in any order and an attribute may share the
        // line with the item. `impl` is matched here too, but its display name
        // is recovered separately below (generic impls and `impl Trait for T`).
        Regex::new(
            r#"^\s*(?:(?:#\[[^\]]*\]|pub(?:\s*\([^)]*\))?|async|unsafe|const|default|extern(?:\s+"[^"]*")?)\s*)*(?P<kind>fn|struct|impl|trait|enum|mod)\b"#,
        )
        .expect("valid rust outline regex")
    });

    let mut items = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let Some(caps) = item_re.captures(line) else {
            continue;
        };
        let kind = caps
            .name("kind")
            .and_then(|m| kind_for(m.as_str()))
            .unwrap_or(KEY);
        let rest = &line[caps.get(0).unwrap().end()..];
        if let Some(name) = rust_item_name(kind, rest) {
            items.push(item(kind, name, idx + 1));
        }
    }
    items
}

fn rust_item_name<'a>(kind: &'static str, rest: &'a str) -> Option<&'a str> {
    if kind == IMPL {
        let rest = skip_angle_group(rest).unwrap_or(rest);
        if let Some(for_idx) = rest.find(" for ") {
            return leading_identifier(&rest[for_idx + 5..]);
        }
        return leading_identifier(rest);
    }
    leading_identifier(rest)
}

fn skip_angle_group(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    if !rest.starts_with('<') {
        return Some(rest);
    }

    let mut depth = 0usize;
    for (idx, ch) in rest.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&rest[idx + ch.len_utf8()..]);
                }
            }
            _ => {}
        }
    }
    None
}

fn leading_identifier(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    let bytes = rest.as_bytes();
    let first = *bytes.first()?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }

    let mut len = 0;
    while len < bytes.len() {
        let byte = bytes[len];
        if byte == b'_' || byte.is_ascii_alphanumeric() {
            len += 1;
        } else {
            break;
        }
    }
    Some(&rest[..len])
}

fn scan_markdown(content: &str) -> Vec<OutlineItem> {
    static HEADING_RE: OnceLock<Regex> = OnceLock::new();
    let pattern = HEADING_RE.get_or_init(|| {
        Regex::new(r"^(?P<hash>#{1,6})\s+(?P<name>.+?)\s*$").expect("valid markdown regex")
    });

    let mut items = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if let Some(caps) = pattern.captures(line) {
            items.push(item(HEADING, &caps["name"], idx + 1));
        }
    }
    items
}

fn scan_json_keys(content: &str) -> Vec<OutlineItem> {
    // Track brace/bracket depth and emit keys only at depth 1, i.e. direct
    // children of the root object/array. Indentation is intentionally ignored
    // so minified, tab-indented, and 4-space JSON all produce the same outline.
    let bytes = content.as_bytes();
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut line = 1usize;
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'\n' => {
                line += 1;
                idx += 1;
            }
            b'"' => {
                if let Some((name, next)) = json_string_at(content, idx) {
                    if depth == 1 && json_key_colon_follows(content, next) {
                        items.push(item(KEY, name, line));
                    }
                    idx = next;
                } else {
                    // Unterminated string: skip the quote rather than loop.
                    idx += 1;
                }
            }
            b'{' | b'[' => {
                depth += 1;
                idx += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                idx += 1;
            }
            _ => idx += 1,
        }
    }
    items
}

fn json_string_at(content: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = content.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }

    let mut idx = start + 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\\' => idx += 2,
            b'"' => return Some((&content[start + 1..idx], idx + 1)),
            _ => idx += 1,
        }
    }
    None
}

fn json_key_colon_follows(content: &str, start: usize) -> bool {
    let bytes = content.as_bytes();
    let mut idx = start;
    while idx < bytes.len() {
        match bytes[idx] {
            b' ' | b'\t' | b'\r' | b'\n' => idx += 1,
            b':' => return true,
            _ => return false,
        }
    }
    false
}

fn scan_config_sections(content: &str) -> Vec<OutlineItem> {
    static SECTION_RE: OnceLock<Regex> = OnceLock::new();
    let pattern = SECTION_RE.get_or_init(|| {
        Regex::new(r"^\s*\[{1,2}(?P<name>[^\]\[]+)\]{1,2}\s*(?:#.*)?$")
            .expect("valid section regex")
    });

    let mut items = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if let Some(caps) = pattern.captures(line) {
            items.push(item(KEY, &caps["name"], idx + 1));
        }
    }
    items
}

fn scan_python(content: &str) -> Vec<OutlineItem> {
    static CLASS_RE: OnceLock<Regex> = OnceLock::new();
    static DEF_RE: OnceLock<Regex> = OnceLock::new();
    let class = CLASS_RE.get_or_init(|| {
        Regex::new(r"^\s*class\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)")
            .expect("valid python class regex")
    });
    let def = DEF_RE.get_or_init(|| {
        Regex::new(r"^(?P<indent>\s*)(?:async\s+)?def\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)")
            .expect("valid python def regex")
    });

    let mut items = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if let Some(caps) = class.captures(line) {
            items.push(item(CLASS, &caps["name"], idx + 1));
        } else if let Some(caps) = def.captures(line) {
            let kind = if caps
                .name("indent")
                .map(|m| m.as_str())
                .unwrap_or("")
                .is_empty()
            {
                FN
            } else {
                METHOD
            };
            items.push(item(kind, &caps["name"], idx + 1));
        }
    }
    items
}

fn scan_javascript(content: &str) -> Vec<OutlineItem> {
    static FUNCTION_RE: OnceLock<Regex> = OnceLock::new();
    static CLASS_RE: OnceLock<Regex> = OnceLock::new();
    let function = FUNCTION_RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s+(?P<name>[A-Za-z_$][A-Za-z0-9_$]*)",
        )
        .expect("valid js function regex")
    });
    let class = CLASS_RE.get_or_init(|| {
        Regex::new(r"^\s*(?:export\s+)?(?:default\s+)?class\s+(?P<name>[A-Za-z_$][A-Za-z0-9_$]*)")
            .expect("valid js class regex")
    });

    let mut items = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if let Some(caps) = function.captures(line) {
            items.push(item(FN, &caps["name"], idx + 1));
        } else if let Some(caps) = class.captures(line) {
            items.push(item(CLASS, &caps["name"], idx + 1));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_scanner_finds_all_supported_kinds() {
        let src = "\
fn main() {}
struct Foo;
impl Foo {
    fn run(&self) {}
}
pub trait Shape {
    fn area(&self) -> f64;
}
mod inner;
pub enum Color {}
";
        let items = scan("rs", src);
        let kinds: Vec<&str> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec![FN, STRUCT, IMPL, FN, TRAIT, FN, MOD, ENUM]);
        assert_eq!(items[0].line, 1);
        assert_eq!(items[1].name, "Foo");
        assert_eq!(items[7].line, 10);
    }

    #[test]
    fn rust_scanner_handles_generic_impls_and_qualifier_orders() {
        let src = "\
impl<T> Foo {}
impl Iterator for Foo {}
const unsafe fn a() {}
unsafe async fn b() {}
#[inline]
pub(crate) async fn c() {}
";
        let items = scan("rs", src);
        assert_eq!(items[0].name, "Foo");
        assert_eq!(items[1].name, "Foo");
        assert_eq!(items[2].kind, FN);
        assert_eq!(items[2].name, "a");
        assert_eq!(items[3].kind, FN);
        assert_eq!(items[3].name, "b");
        assert_eq!(items[4].kind, FN);
        assert_eq!(items[4].name, "c");
    }

    #[test]
    fn markdown_scanner_finds_headings() {
        let items = scan("md", "# Title\nsome text\n## Subtitle\n");
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.kind == HEADING));
        assert_eq!(items[0].name, "Title");
        assert_eq!(items[0].line, 1);
        assert_eq!(items[1].name, "Subtitle");
        assert_eq!(items[1].line, 3);
    }

    #[test]
    fn json_scanner_returns_top_level_keys_by_depth() {
        let pretty = scan(
            "json",
            "{\n  \"name\": \"x\",\n  \"deps\": {\n    \"nested\": 1\n  }\n}\n",
        );
        assert_eq!(pretty.len(), 2);
        assert!(pretty.iter().all(|i| i.kind == KEY));
        assert_eq!(pretty[0].name, "name");
        assert_eq!(pretty[1].name, "deps");

        let indented = scan(
            "json",
            "{\n    \"name\": \"x\",\n    \"deps\": {\n        \"nested\": 1\n    }\n}\n",
        );
        assert_eq!(indented.len(), 2);
        assert_eq!(indented[0].name, "name");
        assert_eq!(indented[1].name, "deps");

        let minified = scan("json", r#"{"name":"x","deps":{"nested":1}}"#);
        assert_eq!(minified.len(), 2);
        assert_eq!(minified[0].name, "name");
        assert_eq!(minified[1].name, "deps");
    }

    #[test]
    fn config_scanner_finds_sections_and_array_tables() {
        let items = scan(
            "toml",
            "[package]\nname = \"g\"\n\n[[bin]]\npath = \"main.rs\"\n\n[dependencies]\nregex = \"1\"\n",
        );
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|i| i.kind == KEY));
        assert_eq!(items[0].name, "package");
        assert_eq!(items[1].name, "bin");
        assert_eq!(items[2].name, "dependencies");
    }

    #[test]
    fn python_scanner_distinguishes_functions_from_methods() {
        let items = scan(
            "py",
            "class A:\n    def run(self):\n        pass\n\nasync def helper():\n    pass\n",
        );
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, CLASS);
        assert_eq!(items[1].kind, METHOD);
        assert_eq!(items[2].kind, FN);
        assert_eq!(items[2].name, "helper");
    }

    #[test]
    fn unknown_extension_scans_empty() {
        assert!(scan("txt", "hello\nworld\n").is_empty());
    }
}
