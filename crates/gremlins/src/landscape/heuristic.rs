//! Line-by-line, regex-based structural outliner.
//!
//! This is deliberately heuristic: it never tries to parse a language. Unknown
//! extensions scan to an empty outline rather than erroring, so the caller can
//! always inspect a file's structure and fall back to `Read` for the body.

use std::sync::OnceLock;

use regex::Regex;

use super::{OutlineItem, CLASS, ENUM, FN, HEADING, IMPL, KEY, METHOD, MOD, STRUCT, TRAIT};

pub(crate) fn scan(extension: &str, content: &str, collapse_tests: bool) -> Vec<OutlineItem> {
    match extension {
        "rs" => scan_rust(content, collapse_tests),
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
        "rs" | "json"
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

fn scan_rust(content: &str, collapse_tests: bool) -> Vec<OutlineItem> {
    static ITEM_RE: OnceLock<Regex> = OnceLock::new();
    static MOD_OPEN_RE: OnceLock<Regex> = OnceLock::new();
    let item_re = ITEM_RE.get_or_init(|| {
        // Qualifiers may appear in any order and an attribute may share the
        // line with the item. `impl` is matched here too, but its display name
        // is recovered separately below (generic impls and `impl Trait for T`).
        Regex::new(
            r#"^\s*(?:(?:#\[[^\]]*\]|pub(?:\s*\([^)]*\))?|async|unsafe|const|default|extern(?:\s+"[^"]*")?)\s*)*(?P<kind>fn|struct|impl|trait|enum|mod)\b"#,
        )
        .expect("valid rust outline regex")
    });
    let mod_open_re = MOD_OPEN_RE.get_or_init(|| {
        Regex::new(r#"^\s*mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<brace>\{)"#)
            .expect("valid rust mod-open regex")
    });

    let mut items = Vec::new();
    // Depth of an open `#[cfg(test)] mod tests { ... }` body. While positive,
    // every line is skipped so test fixtures don't flood the block with
    // hundreds of `fn test_*` rows; the body collapses to one `mod|tests` item.
    let mut test_mod_depth = 0usize;
    // A backslash-continued (or otherwise multi-line) string literal. Structure
    // found on its interior lines is test-fixture text, not real items.
    let mut in_multiline_string = false;
    // The immediately preceding line was `#[cfg(test)]`, so a following
    // `mod NAME {` is a test module even when it isn't named `tests`.
    let mut prev_cfg_test = false;

    for (idx, line) in content.lines().enumerate() {
        if collapse_tests && test_mod_depth > 0 {
            test_mod_depth = brace_depth_after_line(line, test_mod_depth);
            continue;
        }

        if in_multiline_string {
            // A line with an odd number of unescaped quotes closes the string.
            if count_unescaped_quotes(line) % 2 == 1 {
                in_multiline_string = false;
            }
            continue;
        }

        if let Some(caps) = mod_open_re.captures(line) {
            let name = caps.name("name").unwrap().as_str();
            let brace_idx = caps.name("brace").unwrap().start();
            let has_cfg_test = prev_cfg_test || line.contains("#[cfg(test)]");
            if name == "tests" || has_cfg_test {
                items.push(item(MOD, name, idx + 1));
                prev_cfg_test = false;
                if collapse_tests {
                    test_mod_depth = brace_depth_after_line(&line[brace_idx + 1..], 1);
                }
                continue;
            }
        }

        // A line with an odd number of unescaped quotes opens a multi-line
        // string (Rust only continues normal strings across lines when the
        // newline is escaped, so this is rare outside test fixtures). Lines
        // inside that string are deliberately not scanned for items.
        if count_unescaped_quotes(line) % 2 == 1 {
            in_multiline_string = true;
            prev_cfg_test = line.trim() == "#[cfg(test)]";
            continue;
        }

        if let Some(caps) = item_re.captures(line) {
            let kind = caps
                .name("kind")
                .and_then(|m| kind_for(m.as_str()))
                .unwrap_or(KEY);
            let rest = &line[caps.get(0).unwrap().end()..];
            if let Some(name) = rust_item_name(kind, rest) {
                items.push(item(kind, &name, idx + 1));
            }
        }

        prev_cfg_test = line.trim() == "#[cfg(test)]";
    }
    items
}

/// Count `"` characters not preceded by a backslash escape.
fn count_unescaped_quotes(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut count = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\\' => idx += 2,
            b'"' => {
                count += 1;
                idx += 1;
            }
            _ => idx += 1,
        }
    }
    count
}

/// Update a brace-depth counter from one line, returning 0 once the body
/// closes on this line.
fn brace_depth_after_line(line: &str, mut depth: usize) -> usize {
    for ch in line.chars() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return 0;
                }
            }
            _ => {}
        }
    }
    depth
}

fn rust_item_name(kind: &'static str, rest: &str) -> Option<String> {
    if kind == IMPL {
        let rest = skip_angle_group(rest).unwrap_or(rest);
        if let Some(for_idx) = rest.find(" for ") {
            // `impl Trait for Type`: emit `Trait for Type` rather than just
            // the target type, so `From<T> for String` and `GremlinDefinition
            // for Box<dyn ...>` stay distinguishable rows instead of many
            // identical `impl|String` / `impl|Box` rows.
            let trait_part = rest[..for_idx].trim();
            let type_part = leading_identifier(&rest[for_idx + 5..])?;
            if trait_part.is_empty() {
                return Some(type_part.to_string());
            }
            return Some(format!("{trait_part} for {type_part}"));
        }
        return leading_identifier(rest).map(str::to_string);
    }
    leading_identifier(rest).map(str::to_string)
}

fn skip_angle_group(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    if !rest.starts_with('<') {
        return Some(rest);
    }

    let mut depth = 0usize;
    let mut prev_dash = false;
    for (idx, ch) in rest.char_indices() {
        // A `>` immediately after `-` is the `->` operator inside a bound
        // (`impl<F: Fn() -> u8>`), not the generic group's closing bracket.
        if ch == '>' && prev_dash {
            prev_dash = false;
            continue;
        }
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
        prev_dash = ch == '-';
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
        let items = scan("rs", src, false);
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
        let items = scan("rs", src, false);
        assert_eq!(items[0].name, "Foo");
        assert_eq!(items[1].name, "Iterator for Foo");
        assert_eq!(items[2].kind, FN);
        assert_eq!(items[2].name, "a");
        assert_eq!(items[3].kind, FN);
        assert_eq!(items[3].name, "b");
        assert_eq!(items[4].kind, FN);
        assert_eq!(items[4].name, "c");
    }

    #[test]
    fn rust_scanner_collapses_cfg_test_module_bodies() {
        let src = "\
#[cfg(test)]
mod tests {
    fn test_foo() {}
    fn test_bar() {}
}
fn real() {}
";
        let items = scan("rs", src, true);
        let kinds: Vec<&str> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec![MOD, FN]);
        assert_eq!(items[0].name, "tests");
        assert_eq!(items[1].name, "real");
    }

    #[test]
    fn rust_scanner_shows_test_functions_when_not_collapsed() {
        let src = "\
#[cfg(test)]
mod tests {
    fn test_foo() {}
    fn test_bar() {}
}
fn real() {}
";
        let items = scan("rs", src, false);
        let kinds: Vec<&str> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec![MOD, FN, FN, FN]);
        assert_eq!(items[0].name, "tests");
        assert_eq!(items[1].name, "test_foo");
        assert_eq!(items[2].name, "test_bar");
        assert_eq!(items[3].name, "real");
    }

    #[test]
    fn rust_scanner_handles_single_line_cfg_test_when_not_collapsed() {
        let src = "\
#[cfg(test)] mod integration {
    fn test_integration() {}
}
fn real() {}
";
        let items = scan("rs", src, false);
        let kinds: Vec<&str> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec![MOD, FN, FN]);
        assert_eq!(items[0].name, "integration");
        assert_eq!(items[1].name, "test_integration");
        assert_eq!(items[2].name, "real");
    }

    #[test]
    fn rust_scanner_handles_arrow_in_generic_bounds() {
        let src = "\
impl<F: Fn() -> u8> StateBlob {}
impl<F: Fn() -> u8> StateBlob for T {}
";
        let items = scan("rs", src, false);
        assert_eq!(items[0].name, "StateBlob");
        assert_eq!(items[1].name, "StateBlob for T");
    }

    #[test]
    fn rust_scanner_skips_items_inside_multiline_strings() {
        let src = "let src = \"\\\nfn main() {}\nstruct Foo;\n\";\nfn real() {}\n";
        let items = scan("rs", src, false);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    #[test]
    fn json_scanner_returns_top_level_keys_by_depth() {
        let pretty = scan(
            "json",
            "{\n  \"name\": \"x\",\n  \"deps\": {\n    \"nested\": 1\n  }\n}\n",
            false,
        );
        assert_eq!(pretty.len(), 2);
        assert!(pretty.iter().all(|i| i.kind == KEY));
        assert_eq!(pretty[0].name, "name");
        assert_eq!(pretty[1].name, "deps");

        let indented = scan(
            "json",
            "{\n    \"name\": \"x\",\n    \"deps\": {\n        \"nested\": 1\n    }\n}\n",
            false,
        );
        assert_eq!(indented.len(), 2);
        assert_eq!(indented[0].name, "name");
        assert_eq!(indented[1].name, "deps");

        let minified = scan("json", r#"{"name":"x","deps":{"nested":1}}"#, false);
        assert_eq!(minified.len(), 2);
        assert_eq!(minified[0].name, "name");
        assert_eq!(minified[1].name, "deps");
    }

    #[test]
    fn config_scanner_finds_sections_and_array_tables() {
        let items = scan(
            "toml",
            "[package]\nname = \"g\"\n\n[[bin]]\npath = \"main.rs\"\n\n[dependencies]\nregex = \"1\"\n",
            false,
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
            false,
        );
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, CLASS);
        assert_eq!(items[1].kind, METHOD);
        assert_eq!(items[2].kind, FN);
        assert_eq!(items[2].name, "helper");
    }

    #[test]
    fn unknown_extension_scans_empty() {
        assert!(scan("txt", "hello\nworld\n", false).is_empty());
    }
}
