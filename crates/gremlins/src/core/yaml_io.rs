//! YAML file I/O and the bundled-prompt accessors layered on top of it.
//!
//! Definitions, gremlin state, and the persisting side of the launcher all move
//! YAML around; this module is the single place that knows how. Two invariants
//! are enforced here and nowhere else:
//!
//! * a *definition-ish* file the caller loads is a mapping — a top-level list or
//!   scalar is a mistake, not a document, so [`load_yaml_file`] rejects it with
//!   a named error rather than handing back a value the callers would then
//!   have to re-check; and
//! * a bundled prompt is non-empty — an empty `include_str!` means a missing
//!   asset, and reporting it as a load failure beats letting an empty prompt
//!   reach a model.
//!
//! Serialization is deliberately one-directional: [`dump_yaml_text`] renders a
//! [`Value`] the caller already built, so the full-fidelity number handling it
//! depends on lives in the pyext converter that builds that value
//! (`convert::pyval_to_serde` / `convert::serde_to_pyval`).
//!
//! The error type keeps every failure distinguishable so the pyext layer can
//! raise the exact Python exception the module this replaces raised —
//! `YamlLoadError` for file problems, `PromptLoadError` for prompt problems.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::assets;

/// Failure modes of loading, dumping, and rendering YAML and bundled prompts.
///
/// Each variant maps to one of the two Python exceptions the module this
/// replaces defined: the file-and-parse variants surface as `YamlLoadError`,
/// the prompt variants as `PromptLoadError` (see
/// `crates/pyext/src/python/utils/yaml_io.rs`).
#[derive(Debug, thiserror::Error)]
pub enum YamlIoError {
    /// The file does not exist.
    #[error("file not found: {path}")]
    FileNotFound { path: String },

    /// The file exists but could not be read or decoded as UTF-8.
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },

    /// The text is not well-formed YAML. `problem` describes what went wrong and
    /// `location` is the rendered source position, empty when serde_yaml
    /// reports none.
    #[error("YAML parse error in {label}: {problem}{location}")]
    Parse {
        label: String,
        problem: String,
        location: String,
    },

    /// The document parsed, but its top-level value is not a mapping.
    #[error("expected a YAML mapping in {label}, got {found}")]
    NotAMapping { label: String, found: String },

    /// The value could not be rendered back to YAML.
    #[error("could not serialize YAML: {detail}")]
    Serialize { detail: String },

    /// No bundled prompt is registered under `name`.
    #[error("bundled prompt not found: {name}")]
    PromptNotFound { name: String },

    /// The bundled prompt exists but holds only whitespace.
    #[error("bundled prompt is empty: {name}")]
    PromptEmpty { name: String },

    /// Placeholder substitution in a bundled prompt failed: a `{key}` had no
    /// matching keyword argument, or the template held a stray brace.
    #[error("render failed for bundled prompt {name}: {detail}")]
    PromptRender { name: String, detail: String },
}

/// Read `path` and parse it as a YAML mapping.
pub fn load_yaml_file(path: &Path) -> Result<serde_yaml::Value, YamlIoError> {
    let text = std::fs::read_to_string(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => YamlIoError::FileNotFound {
            path: path.display().to_string(),
        },
        _ => YamlIoError::Read {
            path: path.display().to_string(),
            source,
        },
    })?;
    parse_mapping(&text, &path.display().to_string())
}

/// Render `value` as a block-style YAML document.
pub fn dump_yaml_text(value: &serde_yaml::Value) -> Result<String, YamlIoError> {
    serde_yaml::to_string(value).map_err(|error| YamlIoError::Serialize {
        detail: error.to_string(),
    })
}

/// Look up a bundled prompt by name, rejecting a missing or empty asset.
pub fn load_bundled_prompt(name: &str) -> Result<String, YamlIoError> {
    let text = assets::PROMPTS
        .get(name)
        .ok_or_else(|| YamlIoError::PromptNotFound {
            name: name.to_string(),
        })?;
    if text.trim().is_empty() {
        return Err(YamlIoError::PromptEmpty {
            name: name.to_string(),
        });
    }
    Ok(text.to_string())
}

/// Load a bundled prompt and substitute `{key}` placeholders from `kwargs`.
///
/// Substitution is a single left-to-right pass, so a value that happens to
/// contain braces is copied through verbatim and never re-scanned as template
/// syntax — the result does not depend on the iteration order of `kwargs`.
/// This mirrors the `str.format(**kwargs)` the Python module used: a
/// placeholder with no matching key is a [`YamlIoError::PromptRender`] failure
/// rather than a silently-kept brace, and `{{` / `}}` are the literal-brace
/// escapes.
pub fn render_bundled_prompt(
    name: &str,
    kwargs: &HashMap<String, String>,
) -> Result<String, YamlIoError> {
    let text = load_bundled_prompt(name)?;
    render_placeholders(&text, kwargs).map_err(|detail| YamlIoError::PromptRender {
        name: name.to_string(),
        detail,
    })
}

/// Substitute `{key}` placeholders in a single pass over `text`.
///
/// Returns a description of the offending construct on failure so the caller
/// can name it in the [`YamlIoError::PromptRender`] message.
fn render_placeholders(text: &str, kwargs: &HashMap<String, String>) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                out.push('{');
                i += 2;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                out.push('}');
                i += 2;
            }
            b'{' => {
                let close = text[i + 1..]
                    .find('}')
                    .ok_or_else(|| "unmatched '{' in prompt".to_string())?;
                let key = &text[i + 1..i + 1 + close];
                let value = kwargs
                    .get(key)
                    .ok_or_else(|| format!("missing placeholder {{{key}}}"))?;
                out.push_str(value);
                i += close + 2;
            }
            b'}' => return Err("single '}' in prompt".to_string()),
            _ => {
                let ch = text[i..].chars().next().expect("index is a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Ok(out)
}

fn parse_mapping(text: &str, label: &str) -> Result<serde_yaml::Value, YamlIoError> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|error| parse_error(label, &error))?;
    if !value.is_mapping() {
        return Err(YamlIoError::NotAMapping {
            label: label.to_string(),
            found: yaml_type_name(&value).to_string(),
        });
    }
    Ok(value)
}

/// Split a serde_yaml error into the problem text and the rendered position.
///
/// serde_yaml embeds " at line N column M" inside its message (sometimes with
/// trailing context). Lifting it out into [`YamlIoError::Parse::location`] lets
/// the [`Display`][std::fmt::Display] output carry the position once, in the
/// `problem (line N, column M)` shape the Python module produced.
fn parse_error(label: &str, error: &serde_yaml::Error) -> YamlIoError {
    let raw = error.to_string();
    let Some(loc) = error.location() else {
        return YamlIoError::Parse {
            label: label.to_string(),
            problem: raw,
            location: String::new(),
        };
    };
    let (line, column) = (loc.line(), loc.column());
    let embedded = format!(" at line {line} column {column}");
    if raw.contains(&embedded) {
        YamlIoError::Parse {
            label: label.to_string(),
            problem: raw.replacen(&embedded, "", 1),
            location: format!(" (line {line}, column {column})"),
        }
    } else {
        // The position was not in the message; leave the problem untouched
        // rather than appending a position that may already read clearly.
        YamlIoError::Parse {
            label: label.to_string(),
            problem: raw,
            location: String::new(),
        }
    }
}

/// The Python type name for a YAML scalar, matching the message the Python
/// module produced (`NoneType`, `list`, `str`, …).
fn yaml_type_name(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "NoneType",
        serde_yaml::Value::Bool(_) => "bool",
        serde_yaml::Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        serde_yaml::Value::Number(_) => "float",
        serde_yaml::Value::String(_) => "str",
        serde_yaml::Value::Sequence(_) => "list",
        serde_yaml::Value::Mapping(_) => "dict",
        serde_yaml::Value::Tagged(_) => "tagged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn yaml(text: &str) -> serde_yaml::Value {
        serde_yaml::from_str(text).unwrap()
    }

    #[test]
    fn load_reads_a_mapping() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("definition.yaml");
        fs::write(&path, "stages: []\n").unwrap();

        let value = load_yaml_file(&path).unwrap();
        assert_eq!(value["stages"], serde_yaml::Value::Sequence(vec![]));
    }

    #[test]
    fn load_missing_file_names_the_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.yaml");

        let error = load_yaml_file(&path).unwrap_err();
        assert!(matches!(error, YamlIoError::FileNotFound { .. }));
        assert!(error.to_string().contains("absent.yaml"));
    }

    #[test]
    fn load_rejects_a_non_mapping_document() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("list.yaml");
        fs::write(&path, "- one\n- two\n").unwrap();

        let error = load_yaml_file(&path).unwrap_err();
        assert!(matches!(
            error,
            YamlIoError::NotAMapping { ref found, .. } if found == "list"
        ));
        assert!(error.to_string().contains("expected a YAML mapping"));
    }

    #[test]
    fn parse_error_carries_a_single_position() {
        let error = load_labeled("a: [\n");

        let message = error.to_string();
        assert!(
            message.starts_with("YAML parse error in label.yaml: "),
            "{message}"
        );
        assert_eq!(message.matches("line ").count(), 1, "{message}");
    }

    #[test]
    fn dump_round_trips_a_mapping() {
        let value = yaml("a: 1\nnested:\n  b: true\nitems:\n  - x\n");
        let text = dump_yaml_text(&value).unwrap();

        assert_eq!(yaml(&text), value);
    }

    #[test]
    fn prompts_load_and_must_not_be_empty() {
        assert!(!load_bundled_prompt("analyze.md").unwrap().is_empty());
        assert!(matches!(
            load_bundled_prompt("does-not-exist.md").unwrap_err(),
            YamlIoError::PromptNotFound { .. }
        ));
    }

    #[test]
    fn render_substitutes_each_placeholder() {
        let kwargs = HashMap::from([
            ("state_json".to_string(), "{\"a\": 1}".to_string()),
            ("log_text".to_string(), "hello".to_string()),
        ]);
        let rendered = render_bundled_prompt("analyze.md", &kwargs).unwrap();

        assert!(rendered.contains("{\"a\": 1}"));
        assert!(rendered.contains("hello"));
        assert!(!rendered.contains("{state_json}"));
        assert!(!rendered.contains("{log_text}"));
    }

    #[test]
    fn render_does_not_rescan_substituted_values() {
        // A value that itself looks like a placeholder must survive verbatim,
        // regardless of the order the map yields its entries.
        let kwargs = HashMap::from([
            ("a".to_string(), "{b}".to_string()),
            ("b".to_string(), "clobbered".to_string()),
        ]);

        assert_eq!(render_placeholders("{a}", &kwargs).unwrap(), "{b}");
    }

    #[test]
    fn render_rejects_a_missing_placeholder() {
        let error = render_bundled_prompt("analyze.md", &HashMap::new()).unwrap_err();

        assert!(matches!(error, YamlIoError::PromptRender { .. }));
        assert!(error.to_string().contains("missing placeholder"));
    }

    #[test]
    fn render_unescapes_doubled_braces() {
        let rendered = render_placeholders("a {{b}} c", &HashMap::new()).unwrap();

        assert_eq!(rendered, "a {b} c");
    }

    #[test]
    fn render_rejects_a_stray_brace() {
        assert!(render_placeholders("a } b", &HashMap::new()).is_err());
        assert!(render_placeholders("a { b", &HashMap::new()).is_err());
    }

    fn load_labeled(text: &str) -> YamlIoError {
        parse_mapping(text, "label.yaml").unwrap_err()
    }
}
