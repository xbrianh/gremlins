//! Heuristic identifier and path extraction.
//!
//! Regex-driven source-line scanners that find file-path references and
//! identifiers in Rust's three case conventions. This module is deliberately
//! language-agnostic — it never parses a language, only scans textual
//! patterns. It is the swap surface for a future tree-sitter backend: replace
//! the bodies of the two extraction functions and drop the regex helpers, and
//! the coordination code in [`super`] continues unchanged.
//!
//! See [`landscape::heuristic`](crate::landscape::heuristic) for the same
//! pattern applied to the outline scanner.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::{Identifier, IdentifierForm};

// ── Regex patterns (compiled once) ────────────────────────────────────────

pub(crate) fn snake_case_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Lowercase word(s) separated by underscores: `foo_bar`, `foo_bar_baz`.
        // Must contain at least one underscore to distinguish from plain lowercase words.
        Regex::new(r"\b([a-z][a-z0-9]*_[a-z0-9_]*[a-z0-9])\b").expect("valid snake_case regex")
    })
}

pub(crate) fn camel_case_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Capitalised words with an internal case transition: `FooBar`, `MyStruct`.
        // Requires an initial uppercase, at least one lowercase‑then‑uppercase
        // transition, so single‑word capitals (`Config`, `Path`) are excluded.
        Regex::new(r"\b([A-Z][a-zA-Z0-9]*[a-z][A-Z][a-zA-Z0-9]*)\b").expect("valid CamelCase regex")
    })
}

pub(crate) fn shouty_snake_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // All-uppercase words separated by underscores: `MAX_SIZE`, `DEFAULT_TIMEOUT_MS`.
        // Must contain at least one underscore to distinguish from plain acronyms.
        Regex::new(r"\b([A-Z][A-Z0-9]*_[A-Z0-9_]*[A-Z0-9])\b").expect("valid SHOUTY_SNAKE regex")
    })
}

// ── Extraction helpers ────────────────────────────────────────────────────

/// Scan a single line of source text for identifiers in all three forms.
pub(crate) fn extract_identifiers(
    line: &str,
    line_number: usize,
    source_file: &Path,
) -> Vec<Identifier> {
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
