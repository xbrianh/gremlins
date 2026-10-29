//! Heuristic signal extraction from file content.
//!
//! This module scans source files for structural signals that complement the
//! outline: identifiers in Rust's three case conventions, and a deduplicated
//! symbol index keyed by first occurrence. The per-line regex extraction lives
//! in [`heuristic`]; this module handles coordination, rendering, and sandbox
//! containment.

pub(crate) mod heuristic;

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::clients::tools::{canonicalize_or_ancestor, within_worktree};

/// One logical signal section with a stable machine key for logging and
/// the rendered text body.
#[derive(Debug, Clone)]
pub(crate) struct SignalSection {
    /// Stable key for log lines and tests (e.g. `"diff-stat"`).
    #[allow(dead_code)]
    pub key: String,
    /// Rendered section text, including any HTML-comment header.
    pub text: String,
}

impl SignalSection {
    pub(crate) fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
        }
    }
}

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
    /// Stage prompt/plan text used to derive symbol-map candidates.
    /// `None` (or an empty string) disables the symbol-map section.
    pub prompt: Option<String>,
    /// Emit a per-file added/deleted summary when the stage has a diff
    /// artifact.  Controlled by `landscape.diff-stat` / env var, wired
    /// by the executor so the signals module stays config-agnostic.
    pub diff_stat: bool,
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

            // Extract identifiers.
            let idents = heuristic::extract_identifiers(line, line_number, &outline.path);
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

/// Render a per-file added/deleted summary via `git diff --numstat`.
///
/// Complements [`build_changed_files`] by answering "how much changed" for
/// each path in the name-only list. Paths are resolved against the repo root,
/// sandbox-contained, and pruned exactly like the changed-files section;
/// binary entries (`-` added count) and files outside the worktree are
/// dropped. Entries are sorted by total magnitude descending and
/// capped at 30 to respect the shared token budget.
///
/// Returns `None` when git is unavailable, the directory is not a repo, or
/// no reported path passes containment.
fn build_diff_stat(ctx: &SignalContext) -> Option<String> {
    const MAX_ENTRIES: usize = 30;

    let Ok(repo_root_output) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&ctx.cwd)
        .output()
    else {
        return None;
    };
    if !repo_root_output.status.success() {
        return None;
    }
    let Ok(repo_root) = String::from_utf8(repo_root_output.stdout) else {
        return None;
    };
    let repo_root = PathBuf::from(repo_root.trim());
    let repo_root_canonical = canonicalize_or_ancestor(&repo_root);

    let Ok(output) = Command::new("git")
        .args(["diff", "--numstat", "--no-renames", "HEAD~10"])
        .current_dir(&ctx.cwd)
        .output()
    else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return None;
    };

    struct StatEntry {
        rel: String,
        added: u64,
        deleted: u64,
    }

    let mut entries: Vec<StatEntry> = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(raw_path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // Binary entries report "-" for both counts; drop them rather than
        // guessing at line counts.
        let (Ok(added), Ok(deleted)) = (added.parse::<u64>(), deleted.parse::<u64>()) else {
            continue;
        };
        // --no-renames guarantees every line is a plain path (no `=>` brace
        // form), but `core.quotePath` can still quote non-ASCII characters.
        // Trim to strip possible trailing newline artifacts.
        let clean = raw_path.trim();
        // If the path is quoted (starts with '"'), unquote it.
        let clean = if clean.starts_with('"') && clean.len() > 1 {
            // Simple unescaping: strip surrounding quotes and unescape
            // the limited set git uses (\a \b \f \n \r \t \v \\ \").
            let inner = &clean[1..clean.len() - 1];
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some('a') => out.push('\x07'),
                        Some('b') => out.push('\x08'),
                        Some('f') => out.push('\x0C'),
                        Some('n') => out.push('\n'),
                        Some('r') => out.push('\r'),
                        Some('t') => out.push('\t'),
                        Some('v') => out.push('\x0B'),
                        Some(other) => out.push(other),
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            out
        } else {
            clean.to_string()
        };
        let abs = repo_root.join(&clean);
        let Some(canonical) = contained_path(&ctx.cwd, &abs) else {
            continue;
        };
        // Don't gate on is_file() — fully-deleted files still show their
        // deletion magnitude. The path passes containment (it was inside the
        // sandbox) so it's safe to report.
        if let Some(ref pruned) = ctx.pruned_files {
            if pruned.contains(&canonical) {
                continue;
            }
        }
        let rel = canonical
            .strip_prefix(&repo_root_canonical)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| clean);
        entries.push(StatEntry {
            rel,
            added,
            deleted,
        });
    }

    if entries.is_empty() {
        return None;
    }

    entries.sort_by(|a, b| {
        let total_a = a.added + a.deleted;
        let total_b = b.added + b.deleted;
        total_b.cmp(&total_a).then_with(|| a.rel.cmp(&b.rel))
    });

    let lines: Vec<String> = entries
        .iter()
        .take(MAX_ENTRIES)
        .map(|e| format!("{}\t+{}/-{}", e.rel, e.added, e.deleted))
        .collect();
    Some(format!("<!-- Diff stat -->\n{}\n", lines.join("\n")))
}

/// Extract candidate identifiers from a prompt/plan string using the same
/// three case-convention regexes used for source-file scanning.
pub(crate) fn extract_prompt_identifiers(prompt: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    for re in [
        heuristic::snake_case_regex(),
        heuristic::camel_case_regex(),
        heuristic::shouty_snake_regex(),
    ] {
        for m in re.find_iter(prompt) {
            names.insert(m.as_str().to_string());
        }
    }
    names
}

/// Render the symbol map from prompt-derived candidates resolved against the
/// full symbol index. Entries are sorted by name and capped at 30. When the
/// prompt is absent or empty the section is omitted entirely.
pub(crate) fn build_symbol_map(ctx: &SignalContext, symbol_index: &HashMap<String, SymbolEntry>) -> String {
    let candidates = match ctx.prompt.as_deref() {
        Some(p) if !p.trim().is_empty() => extract_prompt_identifiers(p),
        _ => return String::new(),
    };
    if candidates.is_empty() {
        return String::new();
    }
    let mut entries: Vec<&SymbolEntry> = symbol_index
        .values()
        .filter(|e| candidates.contains(e.name.as_str()))
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if entries.is_empty() {
        return String::new();
    }
    let root_canonical = canonicalize_or_ancestor(&ctx.cwd);
    let max_show = 30usize;
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
        "<!-- Symbol map (prompt-derived, first occurrence) -->\n{}\n",
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
) -> (Vec<SignalSection>, usize, Vec<PathBuf>, HashMap<String, SymbolEntry>) {
    let signals = build_signals(ctx, outlines);
    let mut sections = Vec::new();
    let mut total_skipped = 0usize;

    // Changed files (via git) — always attempt regardless of profile.
    let (changed, diff_paths, n) = build_changed_files(ctx);
    total_skipped += n;
    if let Some(changed) = changed {
        sections.push(SignalSection::new("changed-files", changed));
    }

    // Diff stat summary — prepend when enabled and changed files exist.
    if ctx.diff_stat && !diff_paths.is_empty() {
        if let Some(stat) = build_diff_stat(ctx) {
            sections.insert(0, SignalSection::new("diff-stat", stat));
        }
    }

    // When DiffFocused with non-empty diff, scope the symbol map to the
    // diff files; otherwise emit the full (unscoped) section.
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
                let body = build_symbol_map(ctx, &filtered);
                if !body.is_empty() {
                    sections.push(SignalSection::new("symbol-map", body));
                }
            }
        }

        (sections, total_skipped, diff_paths, signals.symbol_index)
    } else {
        // Full profile (or DiffFocused with empty diff): emit the unscoped
        // symbol map section.
        if !signals.symbol_index.is_empty() {
            let body = build_symbol_map(ctx, &signals.symbol_index);
            if !body.is_empty() {
                sections.push(SignalSection::new("symbol-map", body));
            }
        }

        (sections, total_skipped, Vec::new(), signals.symbol_index)
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
            prompt: None,
            diff_stat: false,
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
            prompt: None,
            diff_stat: false,
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
            prompt: None,
            diff_stat: false,
        };
        let result = build_signals(&ctx, &[outline_main, outline_bar]);

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
            prompt: None,
            diff_stat: false,
        };
        let result = build_signals(&ctx, &[outline]);

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
            prompt: None,
            diff_stat: false,
        };
        let result = build_signals(&ctx, &[outline]);

        assert!(result.identifiers.is_empty());
        assert!(result.symbol_index.is_empty());
    }

    // ── negative tests: patterns that should NOT be extracted ─────────

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
            prompt: None,
            diff_stat: false,
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

    // ── pruning ────────────────────────────────────────────────────────

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
            prompt: None,
            diff_stat: false,
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

    /// Commit every tracked/untracked change in one shot.
    fn git_commit_all(dir: &tempfile::TempDir, msg: &str) {
        crate::test_support::git(dir.path(), &["add", "-A"]);
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
                msg,
            ],
        );
    }

    fn outline_for(dir: &tempfile::TempDir, rel: &str) -> FileOutline {
        let path = dir.path().join(rel);
        let canonical = contained_path(dir.path(), &path).expect("must be inside tempdir");
        make_outline(&canonical, false, vec![])
    }

    #[test]
    fn profile_full_emits_symbol_map() {
        // Full profile fires the symbol-map section.  Use a non-git cwd so
        // that the changed-files section is absent, but the symbol map still
        // fires when outlines contain identifiers matched by the prompt.
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "src/lib.rs", "fn helper_fn() {}\nuse ./util.rs;\n");
        let outline = make_outline(&path, false, vec![]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: Some("helper_fn".to_string()),
            diff_stat: false,
        };
        let (sections, _skipped, _diff, _) = render_signal_sections(&ctx, &[outline]);
        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        assert!(
            keys.contains(&"symbol-map"),
            "Full profile should emit symbol map; got {keys:?}"
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
            prompt: None,
            diff_stat: false,
        };
        let (sections, _skipped, diff_paths, _) = render_signal_sections(&ctx, &outlines);
        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        assert!(
            keys.contains(&"changed-files"),
            "DiffFocused profile must emit changed-files section; got {keys:?}"
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
            // Prompt-derived resolution: only identifiers mentioned in the
            // prompt appear in the symbol map. func_01 is in the diff and
            // the prompt; func_02 is in the prompt but not in the codebase
            // (no-op); func_00 is outside the diff range AND not in the
            // prompt, so it must be excluded from the symbol map.
            prompt: Some("func_01 func_02".to_string()),
            diff_stat: false,
        };
        let (sections, _skipped, diff_paths, _) = render_signal_sections(&ctx, &outlines);

        // At least one section should be Symbol map (files in diff contain
        // `fn func_NN` identifiers).
        let symbol_section = sections
            .iter()
            .find(|s| s.key == "symbol-map")
            .map(|s| &s.text);
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

    // ── diff stat ──────────────────────────────────────────────────

    #[test]
    fn diff_stat_parses_numstat_and_sorts_desc() {
        let dir = match setup_git_repo(11) {
            Some(d) => d,
            None => return,
        };
        // Add uncommitted lines to two files so they dwarf the others in the
        // working-tree diff that `git diff --numstat HEAD~10` reports.
        let big = dir.path().join("src/file_01.rs");
        let mut big_content = std::fs::read_to_string(&big).unwrap();
        for i in 0..100 {
            big_content.push_str(&format!("fn extra_{i}() {{}}\n"));
        }
        std::fs::write(&big, big_content).unwrap();

        let small = dir.path().join("src/file_02.rs");
        let mut small_content = std::fs::read_to_string(&small).unwrap();
        for i in 0..5 {
            small_content.push_str(&format!("fn extra_{i}() {{}}\n"));
        }
        std::fs::write(&small, small_content).unwrap();

        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: false,
        };
        let (_, diff_paths, _) = build_changed_files(&ctx);
        assert!(!diff_paths.is_empty(), "expected changed files");
        let stat = build_diff_stat(&ctx).expect("diff stat should exist");
        let lines: Vec<&str> = stat.lines().collect();
        assert_eq!(lines[0], "<!-- Diff stat -->");
        // file_01 gained 100 lines on top of its 2 committed lines → +102.
        assert!(
            lines[1].starts_with("src/file_01.rs\t+102/-0"),
            "largest file must sort first; got {:?}",
            lines[1]
        );
        // file_02 gained 5 lines → +7; sorts below file_01.
        assert!(
            lines[2].starts_with("src/file_02.rs\t+7/-0"),
            "second entry must be file_02; got {:?}",
            lines[2]
        );
    }

    #[test]
    fn diff_stat_drops_binary_and_pruned_and_escaped() {
        let dir = match setup_git_repo(11) {
            Some(d) => d,
            None => return,
        };
        // Binary file: non-UTF8 bytes → numstat reports "-" and we drop it.
        std::fs::write(dir.path().join("src/binary.bin"), [0u8, 1, 2, 0xff]).unwrap();
        git_commit_all(&dir, "binary");
        // Text file we will prune via the context.
        std::fs::write(dir.path().join("src/pruned.rs"), "fn pruned() {}\n").unwrap();
        git_commit_all(&dir, "pruned");
        // Symlink whose target escapes the worktree.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("src/escape.rs"),
        )
        .unwrap();
        git_commit_all(&dir, "escape");

        let pruned_canon = contained_path(dir.path(), &dir.path().join("src/pruned.rs")).unwrap();
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: Some(std::collections::HashSet::from([pruned_canon])),
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: false,
        };
        let (_, _diff_paths, _) = build_changed_files(&ctx);
        let stat = build_diff_stat(&ctx).expect("stat should exist");
        assert!(
            !stat.contains("binary.bin"),
            "binary entry must be dropped; got: {stat}"
        );
        assert!(
            !stat.contains("pruned.rs"),
            "pruned entry must be dropped; got: {stat}"
        );
        assert!(
            !stat.contains("escape.rs"),
            "escaping entry must be dropped; got: {stat}"
        );
        // A normal text file still appears.
        assert!(
            stat.contains("file_10.rs"),
            "normal file must still appear; got: {stat}"
        );
    }

    #[test]
    fn diff_stat_caps_at_30() {
        let dir = match setup_git_repo(11) {
            Some(d) => d,
            None => return,
        };
        // Add 40 files in a single commit so the HEAD~10 diff exceeds the cap.
        for i in 0..40 {
            let rel = format!("src/extra_{i:02}.rs");
            std::fs::write(dir.path().join(&rel), format!("fn extra_{i}() {{}}\n")).unwrap();
        }
        git_commit_all(&dir, "bulk");
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: false,
        };
        let (_, _diff_paths, _) = build_changed_files(&ctx);
        let stat = build_diff_stat(&ctx).expect("stat should exist");
        let lines: Vec<&str> = stat.lines().collect();
        // Header line + at most 30 entry lines.
        assert!(
            lines.len() == 31,
            "diff stat must cap at 30 entries; got {} entries",
            lines.len() - 1
        );
    }

    #[test]
    fn diff_stat_empty_when_no_git() {
        // Non-git directory: build_diff_stat returns None.
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "src/lib.rs", "fn f() {}\n");
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: false,
        };
        assert!(
            build_diff_stat(&ctx).is_none(),
            "non-git cwd must yield no diff stat"
        );
    }

    // ── diff-stat dispatch (render_signal_sections) ─────────────────

    #[test]
    fn diff_stat_section_prepended_when_enabled() {
        let dir = match setup_git_repo(11) {
            Some(d) => d,
            None => return,
        };
        // Add uncommitted changes so the diff is non-empty.
        std::fs::write(dir.path().join("src/file_01.rs"), "fn extra() {}\n").unwrap();
        let outlines: Vec<crate::landscape::FileOutline> = vec![];
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: true,
        };
        let (sections, _skipped, _paths, _) = render_signal_sections(&ctx, &outlines);
        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        assert!(
            keys.first() == Some(&"diff-stat"),
            "diff-stat must be first section; got {keys:?}"
        );
        assert!(
            keys.contains(&"changed-files"),
            "changed-files must follow diff-stat; got {keys:?}"
        );
        // The diff-stat section comes before changed-files.
        let diff_pos = keys.iter().position(|&k| k == "diff-stat").unwrap();
        let changed_pos = keys.iter().position(|&k| k == "changed-files").unwrap();
        assert!(
            diff_pos < changed_pos,
            "diff-stat must precede changed-files"
        );
    }

    #[test]
    fn diff_stat_section_absent_when_disabled() {
        let dir = match setup_git_repo(11) {
            Some(d) => d,
            None => return,
        };
        std::fs::write(dir.path().join("src/file_01.rs"), "fn extra() {}\n").unwrap();
        let outlines: Vec<crate::landscape::FileOutline> = vec![];

        // diff_stat: false → section must be absent.
        let ctx_off = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: false,
        };
        let (sections_off, _, _, _) = render_signal_sections(&ctx_off, &outlines);
        assert!(
            !sections_off.iter().any(|s| s.key == "diff-stat"),
            "diff-stat must be absent when flag is off"
        );

        // diff_stat: true → section must appear.
        let ctx_on = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: true,
        };
        let (sections_on, _, _, _) = render_signal_sections(&ctx_on, &outlines);
        let has_diff_stat = sections_on.iter().any(|s| s.key == "diff-stat");
        assert!(has_diff_stat, "diff-stat must appear when flag is on");
    }

    // ── prompt-derived symbol resolution ──────────────────────────────

    #[test]
    fn prompt_derived_symbol_map_includes_matching_candidates() {
        let dir = tempfile::tempdir().unwrap();
        // Create files with identifiers in all three casing forms.
        write_file(
            &dir,
            "src/lib.rs",
            "fn helper_fn() {}\nstruct MyStruct {}\nconst MAX_ITEMS: usize = 10;\n",
        );
        let outline = make_outline(&dir.path().join("src/lib.rs"), false, vec![]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: Some("helper_fn MyStruct MAX_ITEMS".to_string()),
            diff_stat: false,
        };
        let (sections, _skipped, _diff, _) = render_signal_sections(&ctx, &[outline]);
        let body = sections
            .iter()
            .find(|s| s.key == "symbol-map")
            .map(|s| &s.text)
            .expect("symbol map should be emitted");
        assert!(
            body.contains("helper_fn"),
            "symbol map should contain helper_fn; got: {body}"
        );
        assert!(
            body.contains("MyStruct"),
            "symbol map should contain MyStruct; got: {body}"
        );
        assert!(
            body.contains("MAX_ITEMS"),
            "symbol map should contain MAX_ITEMS; got: {body}"
        );
    }

    #[test]
    fn prompt_derived_symbol_map_omitted_for_empty_prompt() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "src/lib.rs", "fn helper_fn() {}\n");
        let outline = make_outline(&dir.path().join("src/lib.rs"), false, vec![]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: None,
            diff_stat: false,
        };
        let (sections, _skipped, _diff, _) = render_signal_sections(&ctx, &[outline]);
        assert!(
            !sections.iter().any(|s| s.key == "symbol-map"),
            "empty prompt must not produce symbol map; got sections: {sections:?}"
        );
    }

    #[test]
    fn prompt_derived_symbol_map_omitted_when_no_candidates_match() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "src/lib.rs", "fn helper_fn() {}\n");
        let outline = make_outline(&dir.path().join("src/lib.rs"), false, vec![]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            // "Config" is a single-word capital — rejected by camel_case_regex.
            // "use" is bare lowercase — rejected by snake_case_regex.
            // Neither matches helper_fn.
            prompt: Some("Config use something_else".to_string()),
            diff_stat: false,
        };
        let (sections, _skipped, _diff, _) = render_signal_sections(&ctx, &[outline]);
        assert!(
            !sections.iter().any(|s| s.key == "symbol-map"),
            "no-match prompt must not produce symbol map; got sections: {sections:?}"
        );
    }

    #[test]
    fn prompt_derived_symbol_map_rejects_bare_words_and_single_word_caps() {
        // The regexes require: an underscore for snake_case/SHOUTY_SNAKE, and
        // an internal case transition for CamelCase.  Bare lowercase words
        // ("use", "let") and single-word capitals ("Config") must not
        // become candidates.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            &dir,
            "src/lib.rs",
            "fn use_it() {}\nstruct Config {}\nfn LET_GO() {}\n",
        );
        let outline = make_outline(&dir.path().join("src/lib.rs"), false, vec![]);
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: Some("use let Config".to_string()),
            diff_stat: false,
        };
        let (sections, _skipped, _diff, _) = render_signal_sections(&ctx, &[outline]);
        assert!(
            !sections.iter().any(|s| s.key == "symbol-map"),
            "bare words must not produce symbol map; got sections: {sections:?}"
        );
    }

    #[test]
    fn prompt_derived_symbol_map_respects_thirty_entry_cap() {
        let dir = tempfile::tempdir().unwrap();
        // Create 35 files, each with one identifier, referenced by a single prompt.
        let mut prompt_parts = Vec::new();
        for i in 0..35 {
            let name = format!("src/f{i:02}.rs");
            prompt_parts.push(format!("fn_{i:02}"));
            write_file(&dir, &name, &format!("fn fn_{i:02}() {{}}\n"));
            // Touch so the file exists for make_outline.
        }
        let mut outlines: Vec<FileOutline> = Vec::new();
        for i in 0..35 {
            let path = dir.path().join(format!("src/f{i:02}.rs"));
            outlines.push(make_outline(&path, false, vec![]));
        }
        let ctx = SignalContext {
            cwd: dir.path().to_path_buf(),
            pruned_files: None,
            profile: SignalProfile::Full,
            prompt: Some(prompt_parts.join(" ")),
            diff_stat: false,
        };
        let (sections, _skipped, _diff, _) = render_signal_sections(&ctx, &outlines);
        let body = sections
            .iter()
            .find(|s| s.key == "symbol-map")
            .map(|s| &s.text)
            .expect("symbol map should be emitted");
        // Count how many fn_NN entries appear.
        let count = body.lines().filter(|l| l.starts_with("fn_")).count();
        assert_eq!(
            count, 30,
            "symbol map should cap at 30 entries, got {count}"
        );
        assert!(
            body.contains("... and 5 more"),
            "truncation notice should mention 5 more; got: {body}"
        );
    }
}
