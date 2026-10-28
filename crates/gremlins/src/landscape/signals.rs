//! Heuristic signal extraction from file content.
//!
//! This module scans source files for structural signals that complement the
//! outline: file-path references, identifiers in Rust's three case conventions,
//! and a deduplicated symbol index keyed by first occurrence. Everything here
//! is regex-based; no native parsing dependencies.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;

use crate::clients::tools::{canonicalize_or_ancestor, within_worktree};

/// Context for building heuristic signals from a set of file outlines.
///
/// Carries the worktree root, used for sandbox containment checks and path
/// relativization.
#[derive(Debug, Clone)]
pub(crate) struct SignalContext {
    /// Worktree root directory used for sandbox containment checks.
    pub cwd: PathBuf,
    /// Set of already-observed file paths to exclude from signal sections.
    /// Absolute canonical paths.
    pub pruned_files: Option<HashSet<PathBuf>>,
    /// Which signal sections to emit, derived from stage artifact inputs.
    pub profile: SignalProfile,
}

/// Which signal sections fire and how the skeleton is scoped.
///
/// Derived from the stage's artifact inputs rather than from naming
/// conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalProfile {
    /// All signal sections fire; full-tree skeleton.
    Full,
    /// Changed-files section is primary; skeleton and other sections are
    /// scoped to files appearing in `git diff --name-only`.
    DiffFocused,
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
    ///
    /// Populated by [`build_signals`] and consumed by the identifier-extraction
    /// tests today; a future phase wires identifier-derived sections into the
    /// landscape block.
    #[allow(dead_code)]
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

// ── Sandbox containment ───────────────────────────────────────────────────

/// Canonicalize `path` and confirm it stays inside `root`.
///
/// Returns the canonical path when containment passes, `None` when the
/// (possibly symlinked) path escapes the workspace root.
fn contained_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = canonicalize_or_ancestor(path);
    if within_worktree(&canonical, &[root.to_path_buf()], Some(root)) {
        Some(canonical)
    } else {
        None
    }
}

// ── Section builders ──────────────────────────────────────────────────────

/// Render the list of files changed relative to `HEAD~10` via `git diff`.
///
/// Uses `HEAD~10` as a fixed-depth snapshot of recent changes — deep enough
/// to capture meaningful context across a typical multi-commit feature branch
/// but bounded to keep the list concise. Uncommitted / staged changes are not
/// included; the landscape block is a stage-start snapshot, and uncommitted
/// work is already visible to the agent via tool calls.
///
/// Resolves git-reported paths against the repository root (obtained from
/// `git rev-parse --show-toplevel`) rather than the gremlin cwd so paths
/// resolve correctly even when the agent is launched from a subdirectory.
///
/// Returns `None` when git is unavailable, the directory is not a
/// repository, or every reported path either falls outside the sandbox or no
/// longer exists on disk.
///
/// The third return element is the set of canonical paths of the changed
/// files, used by `DiffFocused` profile to scope the skeleton and other
/// sections.
fn build_changed_files(ctx: &SignalContext) -> (Option<String>, Vec<PathBuf>, usize) {
    // Obtain the git repository root first; `diff --name-only` emits
    // paths relative to the repo root, not the current directory.
    let Ok(repo_root_output) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&ctx.cwd)
        .output()
    else {
        return (None, Vec::new(), 0);
    };
    if !repo_root_output.status.success() {
        return (None, Vec::new(), 0);
    }
    let Ok(repo_root) = String::from_utf8(repo_root_output.stdout) else {
        return (None, Vec::new(), 0);
    };
    let repo_root = PathBuf::from(repo_root.trim());

    let Ok(output) = Command::new("git")
        .args(["diff", "--name-only", "HEAD~10"])
        .current_dir(&ctx.cwd)
        .output()
    else {
        return (None, Vec::new(), 0);
    };
    if !output.status.success() {
        return (None, Vec::new(), 0);
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return (None, Vec::new(), 0);
    };
    let mut skipped = 0usize;
    let mut canonical_paths = Vec::new();
    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|p| {
            let abs = repo_root.join(p);
            let canonical = contained_path(&ctx.cwd, &abs)?;
            // Skip paths that no longer exist (deleted since HEAD~10).
            if !canonical.is_file() {
                return None;
            }
            // Skip paths already observed by a prior stage.
            if let Some(ref pruned) = ctx.pruned_files {
                if pruned.contains(&canonical) {
                    skipped += 1;
                    return None;
                }
            }
            canonical_paths.push(canonical);
            Some(p.to_string())
        })
        .collect();
    if paths.is_empty() {
        return (None, Vec::new(), skipped);
    }
    (
        Some(format!("<!-- Changed files -->\n{}\n", paths.join("\n"))),
        canonical_paths,
        skipped,
    )
}

/// Render file paths referenced from source text that resolve to existing
/// files inside the sandbox.
///
/// Paths that escape the sandbox are silently dropped.
fn build_referenced_paths(
    ctx: &SignalContext,
    candidates: &[PathCandidate],
) -> (Option<String>, usize) {
    let root_canonical = canonicalize_or_ancestor(&ctx.cwd);
    let mut resolved = std::collections::BTreeSet::new();
    let mut skipped = 0usize;
    for c in candidates {
        // Resolve relative paths from the source file's directory.
        let resolved_path = if c.is_absolute {
            PathBuf::from(&c.path)
        } else {
            c.source_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&c.path)
        };
        let Some(canonical) = contained_path(&ctx.cwd, &resolved_path) else {
            continue;
        };
        // Only include paths that actually exist as files.
        if canonical.is_file() {
            // Skip paths already observed by a prior stage.
            if let Some(ref pruned) = ctx.pruned_files {
                if pruned.contains(&canonical) {
                    skipped += 1;
                    continue;
                }
            }
            if let Ok(rel) = canonical.strip_prefix(&root_canonical) {
                resolved.insert(format!("./{}", rel.display()));
            }
        }
    }
    if resolved.is_empty() {
        return (None, skipped);
    }
    let lines: Vec<_> = resolved.into_iter().collect();
    (
        Some(format!(
            "<!-- Referenced files (from source) -->\n{}\n",
            lines.join("\n")
        )),
        skipped,
    )
}

/// Render the deduplicated symbol index, sorted by name and capped to 100
/// entries to avoid bloating the landscape block.
fn build_symbol_map(ctx: &SignalContext, symbol_index: &HashMap<String, SymbolEntry>) -> String {
    let mut entries: Vec<&SymbolEntry> = symbol_index.values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if entries.is_empty() {
        return String::new();
    }
    let root_canonical = canonicalize_or_ancestor(&ctx.cwd);
    let max_show = 100usize;
    let lines: Vec<String> = entries
        .iter()
        .take(max_show)
        .map(|e| {
            let file = canonicalize_or_ancestor(&e.file);
            let display = file
                .strip_prefix(&root_canonical)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| e.file.display().to_string());
            format!("{}\t{}:{}", e.name, display, e.line)
        })
        .collect();
    let mut out = format!(
        "<!-- Symbol map (deduplicated, first occurrence) -->\n{}\n",
        lines.join("\n")
    );
    if entries.len() > max_show {
        out.push_str(&format!(
            "<!-- ... and {} more -->\n",
            entries.len() - max_show
        ));
    }
    out
}

/// Phase 1 entry point: render every signal section that has content.
///
/// Each returned element is `(section_name, rendered_text)`; sections that
/// produce nothing are omitted. The third return element carries the
/// canonical paths of changed files when the `DiffFocused` profile is active
/// — used by the caller to scope the landscape skeleton.
pub(crate) fn render_signal_sections(
    ctx: &SignalContext,
    outlines: &[super::FileOutline],
) -> (Vec<(String, String)>, usize, Vec<PathBuf>) {
    let signals = build_signals(ctx, outlines);
    let mut sections = Vec::new();
    let mut total_skipped = 0usize;

    // Changed files (via git) — always attempt regardless of profile.
    let (changed, diff_paths, n) = build_changed_files(ctx);
    total_skipped += n;
    if let Some(changed) = changed {
        sections.push(("Changed files".to_string(), changed));
    }

    // When DiffFocused with non-empty diff, scope symbol map and referenced
    // files to the diff files; otherwise emit the full (unscoped) sections.
    if ctx.profile == SignalProfile::DiffFocused && !diff_paths.is_empty() {
        let diff_set: HashSet<&Path> = diff_paths.iter().map(|p| p.as_path()).collect();

        // Symbol map: scoped to symbols whose files appear in the diff.
        if !signals.symbol_index.is_empty() {
            let filtered: HashMap<String, SymbolEntry> = signals
                .symbol_index
                .iter()
                .filter(|(_, entry)| diff_set.contains(entry.file.as_path()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !filtered.is_empty() {
                sections.push(("Symbol map".to_string(), build_symbol_map(ctx, &filtered)));
            }
        }

        // Referenced files: scoped to paths whose source files appear in
        // the diff.
        let scoped_candidates: Vec<PathCandidate> = signals
            .path_candidates
            .iter()
            .filter(|c| diff_set.contains(c.source_file.as_path()))
            .cloned()
            .collect();
        if !scoped_candidates.is_empty() {
            let (referenced, n) = build_referenced_paths(ctx, &scoped_candidates);
            total_skipped += n;
            if let Some(referenced) = referenced {
                sections.push(("Referenced files".to_string(), referenced));
            }
        }

        (sections, total_skipped, diff_paths)
    } else {
        // Full profile (or DiffFocused with empty diff): emit unscoped
        // symbol map and referenced-files sections.
        if !signals.symbol_index.is_empty() {
            sections.push((
                "Symbol map".to_string(),
                build_symbol_map(ctx, &signals.symbol_index),
            ));
        }

        let (referenced, n) = build_referenced_paths(ctx, &signals.path_candidates);
        total_skipped += n;
        if let Some(referenced) = referenced {
            sections.push(("Referenced files".to_string(), referenced));
        }

        (sections, total_skipped, Vec::new())
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
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

    // ── sandbox containment ────────────────────────────────────────────

    #[test]
    fn referenced_paths_rejects_sandbox_escape() {
        let dir = tempfile::tempdir().unwrap();
        // A file that references an absolute path outside the sandbox.
        let content = "let path = \"/etc/passwd\";\n";
        let path = write_file(&dir, "src/main.rs", content);
        let outline = make_outline(&path, false, vec![]);

        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
        };
        let result = build_signals(&ctx, &[outline]);

        // /etc/passwd should still be extracted as a path candidate.
        assert!(
            result
                .path_candidates
                .iter()
                .any(|c| c.path == "/etc/passwd"),
            "/etc/passwd should be extracted"
        );

        // build_referenced_paths must reject /etc/passwd: it resolves outside
        // the sandbox. When all candidates are rejected, the function returns None.
        let (referenced, _) = build_referenced_paths(&ctx, &result.path_candidates);
        assert!(
            referenced.is_none(),
            "sandbox-escape path should be rejected, got: {referenced:?}"
        );
    }

    // ── pruning ────────────────────────────────────────────────────────

    #[test]
    fn pruned_files_mixed_keep_purge_referenced_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(&dir, "a.rs", "// a\n");
        let _b = write_file(&dir, "b.rs", "// b\n");
        let source = dir.path().join("main.rs");
        let candidates = vec![
            PathCandidate {
                path: "a.rs".into(),
                is_absolute: false,
                source_file: source.clone(),
                line: 1,
            },
            PathCandidate {
                path: "b.rs".into(),
                is_absolute: false,
                source_file: source.clone(),
                line: 2,
            },
        ];
        let a_canon = contained_path(dir.path(), &a).unwrap();
        let pruned = std::collections::HashSet::from([a_canon]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: Some(pruned),
            profile: SignalProfile::Full,
        };
        let (out, skipped) = build_referenced_paths(&ctx, &candidates);
        let out = out.expect("section should remain when some files are unpruned");
        assert!(skipped > 0, "pruned file should be counted as skipped");
        assert!(
            !out.contains("./a.rs"),
            "pruned file should not appear: {out}"
        );
        assert!(out.contains("./b.rs"), "unpruned file should appear: {out}");
    }

    #[test]
    fn pruned_files_fully_pruned_section_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(&dir, "a.rs", "// a\n");
        let source = dir.path().join("main.rs");
        let candidates = vec![PathCandidate {
            path: "a.rs".into(),
            is_absolute: false,
            source_file: source,
            line: 1,
        }];
        let a_canon = contained_path(dir.path(), &a).unwrap();
        let pruned = std::collections::HashSet::from([a_canon]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: Some(pruned),
            profile: SignalProfile::Full,
        };
        let (out, skipped) = build_referenced_paths(&ctx, &candidates);
        assert!(skipped > 0, "pruned file should be counted");
        assert!(out.is_none(), "entirely pruned section should return None");
    }

    #[test]
    fn pruned_files_empty_set_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let _a = write_file(&dir, "a.rs", "// a\n");
        let source = dir.path().join("main.rs");
        let candidates = vec![PathCandidate {
            path: "a.rs".into(),
            is_absolute: false,
            source_file: source,
            line: 1,
        }];
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: Some(std::collections::HashSet::new()),
            profile: SignalProfile::Full,
        };
        let (out, skipped) = build_referenced_paths(&ctx, &candidates);
        let out = out.expect("empty prune set should not drop section");
        assert_eq!(skipped, 0, "empty prune set should skip nothing");
        assert!(
            out.contains("./a.rs"),
            "file should appear with empty prune set: {out}"
        );
    }

    #[test]
    fn pruned_files_symbol_map_unaffected() {
        // Even when every file in the outline is pruned, the symbol map
        // (which is structural, not file-level content) is still emitted.
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "lib.rs", "fn helper_fn() {}\n");
        let outline = make_outline(&path, false, vec![]);
        let file_canon = contained_path(dir.path(), &path).unwrap();
        let pruned = std::collections::HashSet::from([file_canon]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: Some(pruned),
            profile: SignalProfile::Full,
        };
        let result = build_signals(&ctx, &[outline]);
        assert!(
            result.symbol_index.contains_key("helper_fn"),
            "symbol map should be unaffected by pruning"
        );
    }

    // ── profile tests ────────────────────────────────────────────────

    /// Initialise a git repository with *n* source files across *n* commits
    /// (one file per commit) so that `HEAD~offset` references a real
    /// ancestor.  Returns the tempdir or `None` when git is missing.
    fn setup_git_repo(n: usize) -> Option<tempfile::TempDir> {
        if !std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::git(dir.path(), &["init", "-q"]);
        // First commit: baseline.
        std::fs::write(dir.path().join("README.md"), "# repo\n").unwrap();
        crate::test_support::git(dir.path(), &["add", "README.md"]);
        crate::test_support::git(
            dir.path(),
            &[
                "-c",
                "user.email=test@test",
                "-c",
                "user.name=T",
                "commit",
                "-q",
                "-m",
                "c0",
            ],
        );
        for i in 0..n {
            let name = format!("src/file_{i:02}.rs");
            let dir_path = dir.path().join("src");
            std::fs::create_dir_all(dir_path).unwrap();
            std::fs::write(
                dir.path().join(&name),
                format!("fn func_{i:02}() {{}}\nuse ./helper_{i:02}.rs;\n"),
            )
            .unwrap();
            crate::test_support::git(dir.path(), &["add", &name]);
            crate::test_support::git(
                dir.path(),
                &[
                    "-c",
                    "user.email=test@test",
                    "-c",
                    "user.name=T",
                    "commit",
                    "-q",
                    "-m",
                    &format!("c{i}"),
                ],
            );
        }
        Some(dir)
    }

    fn outline_for(dir: &tempfile::TempDir, rel: &str) -> FileOutline {
        let path = dir.path().join(rel);
        let canonical = contained_path(dir.path(), &path).expect("must be inside tempdir");
        make_outline(&canonical, false, vec![])
    }

    #[test]
    fn profile_full_emits_symbol_map_and_referenced() {
        // Full profile fires all sections.  Use a non-git cwd so that
        // changed-files section is absent (git not available), but the
        // other two sections still fire when outlines contain signals.
        let dir = tempfile::tempdir().unwrap();
        // Create the referenced file too so build_referenced_paths resolves it.
        write_file(&dir, "src/util.rs", "// empty\n");
        let path = write_file(&dir, "src/lib.rs", "fn helper_fn() {}\nuse ./util.rs;\n");
        let outline = make_outline(&path, false, vec![]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
        };
        let (sections, _skipped, _diff) = render_signal_sections(&ctx, &[outline]);
        let names: Vec<&str> = sections.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"Symbol map"),
            "Full profile should emit symbol map; got {names:?}"
        );
        assert!(
            names.contains(&"Referenced files"),
            "Full profile should emit referenced paths; got {names:?}"
        );
    }

    #[test]
    fn profile_diff_focused_fires_changed_files() {
        let dir = match setup_git_repo(11) {
            Some(d) => d,
            None => return,
        };
        // Create outlines for all files that changed beyond HEAD~10.
        // setup_git_repo creates file_00..file_10 in src/.
        let mut outlines = Vec::new();
        for i in 0..=10 {
            let rel = format!("src/file_{i:02}.rs");
            outlines.push(outline_for(&dir, &rel));
        }
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::DiffFocused,
        };
        let (sections, _skipped, diff_paths) = render_signal_sections(&ctx, &outlines);
        let names: Vec<&str> = sections.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"Changed files"),
            "DiffFocused profile must emit changed-files section; got {names:?}"
        );
        // diff_paths should be non-empty.
        assert!(!diff_paths.is_empty(), "diff_paths should be non-empty");
    }

    #[test]
    fn profile_diff_focused_scopes_symbol_map() {
        let dir = match setup_git_repo(11) {
            Some(d) => d,
            None => return,
        };
        // Outline only for files that appear in the diff.
        let mut outlines = Vec::new();
        for i in 0..=10 {
            let rel = format!("src/file_{i:02}.rs");
            outlines.push(outline_for(&dir, &rel));
        }
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::DiffFocused,
        };
        let (sections, _skipped, diff_paths) = render_signal_sections(&ctx, &outlines);

        // At least one section should be Symbol map (files in diff contain
        // `fn func_NN` identifiers).
        let symbol_section = sections
            .iter()
            .find(|(n, _)| n == "Symbol map")
            .map(|(_, body)| body);
        assert!(
            symbol_section.is_some(),
            "DiffFocused should produce symbol map when diff files contain identifiers"
        );
        let body = symbol_section.unwrap();
        // Symbols from diff files appear.
        // HEAD~10 with n=11 → diff covers file_01 through file_10 (not file_00).
        assert!(
            body.contains("func_01"),
            "symbol map should contain func_01 from diff file; got: {body}"
        );
        assert!(
            !body.contains("func_00"),
            "symbol map should NOT contain func_00 (outside diff); got: {body}"
        );
        // HEAD~10 with n=11 → diff covers 10 files.
        let diff_set: std::collections::HashSet<&std::path::Path> =
            diff_paths.iter().map(|p| p.as_path()).collect();
        assert_eq!(
            diff_set.len(),
            10,
            "diff should cover 10 files, got {}",
            diff_set.len()
        );
    }
}
