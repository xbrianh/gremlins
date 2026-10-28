//! Heuristic signal extraction from file content.
//!
//! This module scans source files for structural signals that complement the
//! outline: file-path references, identifiers in Rust's three case conventions,
//! and a deduplicated symbol index keyed by first occurrence. Everything here
//! is regex-based; no native parsing dependencies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

/// Context for building heuristic signals from a set of file outlines.
///
/// Carries the workspace root, reserved for path relativization when the
/// signals are wired into the system-prompt path in a later phase.
#[derive(Debug, Clone)]
pub(crate) struct SignalContext {
    /// Workspace root directory (not yet used — reserved for Phase 1 wiring).
    pub root: PathBuf,
}

/// A file-path reference discovered in source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathCandidate {
    /// The raw path string as it appeared in the source.
    pub path: String,
    /// Whether the path is absolute (`/`, drive‑root, UNC, or tilde‑home).
    pub is_absolute: bool,
    /// The file in which this path reference was found.
    pub source_file: PathBuf,
    /// 1-based line number.
    pub line: usize,
}

/// The casing convention of an extracted identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentifierForm {
    /// `snake_case` — lowercase words separated by underscores.
    SnakeCase,
    /// `CamelCase` — capitalised words with no separators.
    CamelCase,
    /// `SHOUTY_SNAKE_CASE` — all-uppercase words separated by underscores.
    ShoutySnake,
}

/// An identifier extracted from source text together with its casing form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identifier {
    pub name: String,
    pub form: IdentifierForm,
    pub source_file: PathBuf,
    pub line: usize,
}

/// One entry in the deduplicated symbol index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolEntry {
    pub name: String,
    pub file: PathBuf,
    pub line: usize,
}

/// The rendered output of signal extraction.
#[derive(Debug, Clone)]
pub(crate) struct SignalRendered {
    /// Path candidates found across all scanned files.
    pub path_candidates: Vec<PathCandidate>,
    /// Identifiers found across all scanned files.
    pub identifiers: Vec<Identifier>,
    /// Deduplicated symbol index: each symbol name maps to its first
    /// occurrence in input (outline iteration) order.
    pub symbol_index: HashMap<String, SymbolEntry>,
}

// ── Regex patterns (compiled once) ────────────────────────────────────────

fn path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Matches common file-path patterns:
        //   - Absolute: /foo/bar.rs, /etc/config
        //   - Relative: ./foo.rs, ../bar/baz, foo/bar.rs
        //   - Also catches tilde-prefixed paths (~/foo) and Windows-style
        //     absolute paths (C:\foo, \\server\share).
        Regex::new(
            r#"(?x)
            (?P<abs>
                (?:/[a-zA-Z0-9_.\-][a-zA-Z0-9_.\-\+/]*)   # Unix absolute path
              |
                (?:[A-Za-z]:\\[a-zA-Z0-9_.\-\+\\]*)        # Windows drive-absolute
              |
                (?:\\\\[a-zA-Z0-9_.\-]+\\[a-zA-Z0-9_.\-\+\\]*)  # UNC path
              |
                (?:~/[a-zA-Z0-9_.\-][a-zA-Z0-9_.\-\+/]*)   # Tilde-home path
            )
            |
            (?P<rel>
                (?:\.{1,2}/[a-zA-Z0-9_.\-][a-zA-Z0-9_.\-\+/]*)  # ./ or ../ relative
            )
            |
            (?P<bare>
                (?:[a-zA-Z][a-zA-Z0-9_\-]*/[a-zA-Z0-9_.\-\+/]*)   # bare relative (no ./ prefix)
            )
            "#,
        )
        .expect("valid path regex")
    })
}

fn snake_case_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Lowercase word(s) separated by underscores: `foo_bar`, `foo_bar_baz`.
        // Must contain at least one underscore to distinguish from plain lowercase words.
        Regex::new(r"\b([a-z][a-z0-9]*_[a-z0-9_]*[a-z0-9])\b").expect("valid snake_case regex")
    })
}

fn camel_case_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Capitalised words with an internal case transition: `FooBar`, `MyStruct`.
        // Requires an initial uppercase, at least one lowercase‑then‑uppercase
        // transition, so single‑word capitals (`Config`, `Path`) are excluded.
        Regex::new(r"\b([A-Z][a-zA-Z0-9]*[a-z][A-Z][a-zA-Z0-9]*)\b").expect("valid CamelCase regex")
    })
}

fn shouty_snake_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // All-uppercase words separated by underscores: `MAX_SIZE`, `DEFAULT_TIMEOUT_MS`.
        // Must contain at least one underscore to distinguish from plain acronyms.
        Regex::new(r"\b([A-Z][A-Z0-9]*_[A-Z0-9_]*[A-Z0-9])\b").expect("valid SHOUTY_SNAKE regex")
    })
}

// ── Extraction helpers ────────────────────────────────────────────────────

/// Scan a single line of source text for path candidates.
fn extract_path_candidates(
    line: &str,
    line_number: usize,
    source_file: &Path,
) -> Vec<PathCandidate> {
    let re = path_regex();
    let mut candidates = Vec::new();
    for caps in re.captures_iter(line) {
        let raw = if let Some(m) = caps.name("abs") {
            (m.as_str(), true)
        } else if let Some(m) = caps.name("rel") {
            (m.as_str(), false)
        } else if let Some(m) = caps.name("bare") {
            (m.as_str(), false)
        } else {
            continue;
        };
        // Strip trailing punctuation that often follows paths in prose:
        // periods, commas, semicolons, colons, and closing brackets.
        let path = raw
            .0
            .trim_end_matches(['.', ',', ';', ':', ')', ']'])
            .to_string();
        // Don't emit an empty or single-segment result after trimming.
        if path.is_empty() || (!path.contains('/') && !path.contains('\\')) {
            continue;
        }
        candidates.push(PathCandidate {
            path,
            is_absolute: raw.1,
            source_file: source_file.to_path_buf(),
            line: line_number,
        });
    }
    candidates
}

/// Scan a single line of source text for identifiers in all three forms.
fn extract_identifiers(line: &str, line_number: usize, source_file: &Path) -> Vec<Identifier> {
    let mut identifiers = Vec::new();

    for caps in snake_case_regex().captures_iter(line) {
        let name = caps.get(1).unwrap().as_str().to_string();
        identifiers.push(Identifier {
            name,
            form: IdentifierForm::SnakeCase,
            source_file: source_file.to_path_buf(),
            line: line_number,
        });
    }

    for caps in camel_case_regex().captures_iter(line) {
        let name = caps.get(1).unwrap().as_str().to_string();
        identifiers.push(Identifier {
            name,
            form: IdentifierForm::CamelCase,
            source_file: source_file.to_path_buf(),
            line: line_number,
        });
    }

    for caps in shouty_snake_regex().captures_iter(line) {
        let name = caps.get(1).unwrap().as_str().to_string();
        identifiers.push(Identifier {
            name,
            form: IdentifierForm::ShoutySnake,
            source_file: source_file.to_path_buf(),
            line: line_number,
        });
    }

    identifiers
}

// ── Public API ────────────────────────────────────────────────────────────

/// Build heuristic signals by scanning the content of every file in the
/// provided outlines.
///
/// Files with unrecognized extensions (the [`skipped`] marker) and files
/// whose content cannot be read as UTF-8 are silently skipped.
///
/// [`skipped`]: super::FileOutline::skipped
pub(crate) fn build_signals(
    _ctx: &SignalContext,
    outlines: &[super::FileOutline],
) -> SignalRendered {
    let mut path_candidates: Vec<PathCandidate> = Vec::new();
    let mut identifiers: Vec<Identifier> = Vec::new();
    let mut symbol_index: HashMap<String, SymbolEntry> = HashMap::new();

    for outline in outlines {
        // Skip files that were marked as skipped (unrecognized extension).
        if outline.skipped {
            continue;
        }

        let content = match std::fs::read_to_string(&outline.path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for (idx, line) in content.lines().enumerate() {
            let line_number = idx + 1;

            // Extract path candidates.
            let paths = extract_path_candidates(line, line_number, &outline.path);
            path_candidates.extend(paths);

            // Extract identifiers.
            let idents = extract_identifiers(line, line_number, &outline.path);
            for ident in &idents {
                symbol_index
                    .entry(ident.name.clone())
                    .or_insert_with(|| SymbolEntry {
                        name: ident.name.clone(),
                        file: ident.source_file.clone(),
                        line: ident.line,
                    });
            }
            identifiers.extend(idents);
        }
    }

    SignalRendered {
        path_candidates,
        identifiers,
        symbol_index,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::landscape::{FileOutline, OutlineItem};

    // ── helpers ────────────────────────────────────────────────────────

    fn write_file(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    fn make_outline(path: &Path, skipped: bool, items: Vec<OutlineItem>) -> FileOutline {
        FileOutline {
            path: path.to_path_buf(),
            items,
            skipped,
        }
    }

    // ── path_candidates_find_relative_and_absolute ─────────────────────

    #[test]
    fn path_candidates_find_relative_and_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let content = "\
// Absolute paths
use /etc/config;
// Relative paths
mod foo;
// Bare relative
mod bar/baz;
// ./ prefix
include!(\"./generated.rs\");
// ../ prefix
#[path = \"../shared/lib.rs\"]
";
        let path = write_file(&dir, "src/main.rs", content);
        let outline = make_outline(&path, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline]);

        // Absolute paths
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "/etc/config" && c.is_absolute));

        // Relative: bare form
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "bar/baz" && !c.is_absolute));

        // Relative: ./ form
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "./generated.rs" && !c.is_absolute));

        // Relative: ../ form
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "../shared/lib.rs" && !c.is_absolute));
    }

    // ── identifier_extraction_finds_all_three_forms ────────────────────

    #[test]
    fn identifier_extraction_finds_all_three_forms() {
        let dir = tempfile::tempdir().unwrap();
        let content = "\
fn my_function_name() {
    let MyStruct = ...;
    const MAX_RETRIES: u32 = 3;
}
";
        let path = write_file(&dir, "src/lib.rs", content);
        let outline = make_outline(&path, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline]);

        // Snake case
        assert!(result
            .identifiers
            .iter()
            .any(|i| i.name == "my_function_name" && matches!(i.form, IdentifierForm::SnakeCase)));

        // Camel case
        assert!(result
            .identifiers
            .iter()
            .any(|i| i.name == "MyStruct" && matches!(i.form, IdentifierForm::CamelCase)));

        // Shouty snake
        assert!(result
            .identifiers
            .iter()
            .any(|i| i.name == "MAX_RETRIES" && matches!(i.form, IdentifierForm::ShoutySnake)));
    }

    // ── symbol_index_keeps_first_occurrence ────────────────────────────

    #[test]
    fn symbol_index_keeps_first_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let content_a = "fn get_value() -> u32 { 0 }\nfn set_value(v: u32) {}\n";
        let content_b = "fn get_value() -> u32 { 42 }\n";

        let path_a = write_file(&dir, "src/a.rs", content_a);
        let path_b = write_file(&dir, "src/b.rs", content_b);

        let outline_a = make_outline(&path_a, false, vec![]);
        let outline_b = make_outline(&path_b, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline_a, outline_b]);

        // get_value — should keep the first occurrence (a.rs line 1).
        let entry = result
            .symbol_index
            .get("get_value")
            .expect("get_value should be indexed");
        assert_eq!(entry.file, path_a);
        assert_eq!(entry.line, 1);

        // set_value — only in a.rs.
        let entry = result
            .symbol_index
            .get("set_value")
            .expect("set_value should be indexed");
        assert_eq!(entry.file, path_a);
        assert_eq!(entry.line, 2);
    }

    // ── build_signals_end_to_end ───────────────────────────────────────

    #[test]
    fn build_signals_end_to_end() {
        let dir = tempfile::tempdir().unwrap();

        let content_main = "\
use crate::utils::helper_fn;
mod bar;
const MAX_CONNECTIONS: usize = 64;

fn main() {
    let config = std::fs::read_to_string(\"./config.toml\").unwrap();
    helper_fn(&config);
}
";
        let content_bar = "\
//! Bar module — see also /etc/app/config.json.

pub struct BarConfig {
    pub timeout_ms: u64,
}

pub fn bar_init() -> BarConfig {
    BarConfig { timeout_ms: 5000 }
}
";
        let path_main = write_file(&dir, "src/main.rs", content_main);
        let path_bar = write_file(&dir, "src/bar.rs", content_bar);

        let outline_main = make_outline(&path_main, false, vec![]);
        let outline_bar = make_outline(&path_bar, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline_main, outline_bar]);

        // ── Path candidates ──────────────────────────────────────────

        // Absolute
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "/etc/app/config.json" && c.is_absolute));

        // Relative
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "./config.toml" && !c.is_absolute));

        // ── Identifiers: all three forms ─────────────────────────────

        // Snake
        assert!(result
            .identifiers
            .iter()
            .any(|i| i.name == "helper_fn" && matches!(i.form, IdentifierForm::SnakeCase)));
        assert!(result
            .identifiers
            .iter()
            .any(|i| i.name == "bar_init" && matches!(i.form, IdentifierForm::SnakeCase)));

        // Camel
        assert!(result
            .identifiers
            .iter()
            .any(|i| i.name == "BarConfig" && matches!(i.form, IdentifierForm::CamelCase)));

        // Shouty snake
        assert!(result
            .identifiers
            .iter()
            .any(|i| i.name == "MAX_CONNECTIONS" && matches!(i.form, IdentifierForm::ShoutySnake)));

        // ── Symbol index ─────────────────────────────────────────────

        // BarConfig only appears in bar.rs.
        let bar_config = result
            .symbol_index
            .get("BarConfig")
            .expect("BarConfig should be indexed");
        assert_eq!(bar_config.file, path_bar);

        // helper_fn first appears in main.rs use statement
        let helper = result
            .symbol_index
            .get("helper_fn")
            .expect("helper_fn should be indexed");
        assert_eq!(helper.file, path_main);

        // timeout_ms only appears in bar.rs
        let timeout = result
            .symbol_index
            .get("timeout_ms")
            .expect("timeout_ms should be indexed");
        assert_eq!(timeout.file, path_bar);
    }

    // ── edge cases ────────────────────────────────────────────────────

    #[test]
    fn build_signals_skips_skipped_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "notes.txt", "./foo/bar.rs\nsnake_case_name\n");
        let outline = make_outline(&path, true, vec![]); // skipped = true

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline]);

        assert!(result.path_candidates.is_empty());
        assert!(result.identifiers.is_empty());
        assert!(result.symbol_index.is_empty());
    }

    #[test]
    fn build_signals_handles_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "empty.rs", "");
        let outline = make_outline(&path, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline]);

        assert!(result.path_candidates.is_empty());
        assert!(result.identifiers.is_empty());
        assert!(result.symbol_index.is_empty());
    }

    // ── Windows, UNC, and tilde paths ─────────────────────────────────

    #[test]
    fn path_candidates_detect_windows_unc_and_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let content = "\
// Windows drive-absolute
use C:\\foo\\bar.rs;
// UNC path
use \\\\server\\share\\baz.rs;
// Tilde-home
cfg!(~/.config/app.toml);
";
        let path = write_file(&dir, "src/win.rs", content);
        let outline = make_outline(&path, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline]);

        // Windows drive-absolute
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "C:\\foo\\bar.rs" && c.is_absolute));

        // UNC
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "\\\\server\\share\\baz.rs" && c.is_absolute));

        // Tilde-home
        assert!(result
            .path_candidates
            .iter()
            .any(|c| c.path == "~/.config/app.toml" && c.is_absolute));
    }

    // ── negative tests: patterns that should NOT be extracted ─────────

    #[test]
    fn path_candidates_reject_prose_false_positives() {
        let dir = tempfile::tempdir().unwrap();
        // `and/or` and similar word/word sequences are intentionally
        // accepted — they are indistinguishable from real bare relative
        // paths without deeper context. Digits-only segments like `1/2`
        // are rejected because the first segment must start with a letter.
        let content = "\
let ratio = 1/2;
let phrase = and/or;
let div = a / b;
";
        let path = write_file(&dir, "src/prose.rs", content);
        let outline = make_outline(&path, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline]);

        // Digits-only first segment (`1/2`) must be rejected.
        assert!(result.path_candidates.iter().all(|c| c.path != "1/2"));

        // Spaced slashes (`a / b`) must not be captured.
        assert!(result.path_candidates.iter().all(|c| c.path != "a/b"));

        // `and/or` is accepted — documented false positive.
        assert!(result.path_candidates.iter().any(|c| c.path == "and/or"));
    }

    #[test]
    fn camel_case_rejects_single_word_capitals() {
        let dir = tempfile::tempdir().unwrap();
        let content = "\
use std::path::Path;
let config = Config::default();
// Rust in prose.
";
        let path = write_file(&dir, "src/single.rs", content);
        let outline = make_outline(&path, false, vec![]);

        let ctx = SignalContext {
            root: dir.path().to_path_buf(),
        };
        let result = build_signals(&ctx, &[outline]);

        // None of these single-word capitals should be CamelCase identifiers.
        let bogus: Vec<_> = result
            .identifiers
            .iter()
            .filter(|i| i.name == "Path" || i.name == "Config" || i.name == "Rust")
            .collect();
        assert!(
            bogus.is_empty(),
            "expected single-word capitals to be rejected, got {bogus:?}"
        );
    }
}
