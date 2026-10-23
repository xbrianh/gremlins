use std::path::Path;

use crate::config;
use crate::definition::r#static::expand;
use crate::schemas::error::SchemaError;

/// Bundle a modular definition into a self-contained monolith YAML.
///
/// Expands all structural references (stage-definitions, prompt files,
/// includes) into inline values while preserving model configuration
/// and bootstrap parameters verbatim so the output remains
/// environment-tunable.
///
/// The returned [`serde_yaml::Value`] is suitable for serialization as
/// a standalone YAML file that `gremlins launch` can consume directly.
///
/// `project_root` is the directory containing `.gremlins/` — the CLI
/// caller resolves it via `config::project_root()`.
pub fn bundle_definition(
    yaml_path: &Path,
    project_root: &Path,
) -> Result<serde_yaml::Value, SchemaError> {
    let overlay_dir = config::project_overlay_dir(project_root);
    let expanded = expand::parse_definition_file(yaml_path, project_root, &overlay_dir)?;

    let mut mapping = match expanded {
        serde_yaml::Value::Mapping(m) => m,
        other => {
            return Err(SchemaError::Generic(format!(
                "expected YAML mapping after expansion, got {:?}",
                other
            )));
        }
    };

    // Strip structural keys that serve no purpose once references are inlined.
    mapping.remove("__gremlins_expanded__");
    mapping.remove("prompt_dir");
    mapping.remove("prompts");
    mapping.remove("stage-definitions");

    Ok(serde_yaml::Value::Mapping(mapping))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    use std::fs;

    /// Write a minimal definition to a Sandbox, bundle it, and verify
    /// structural keys are absent and model keys survive verbatim.
    #[test]
    fn bundle_strips_structural_keys_and_preserves_model_keys() {
        let s = Sandbox::new();
        let def_path = s.join("demo.yaml");

        // Use | for multi-line prompt strings so they are treated as inline
        // text rather than file paths.
        let yaml = r#"
name: demo
default_model: anthropic/claude-sonnet-4-20250514
bootstrap:
  source:
    goal:
      type: string
stages:
  - name: plan
    type: agent
    model: openai/gpt-5
    task-models:
      review: openai/gpt-4.1
    prompt: |
      hello
"#;
        fs::write(&def_path, yaml).unwrap();

        let bundled = bundle_definition(&def_path, s.path()).unwrap();
        let yaml_str = serde_yaml::to_string(&bundled).unwrap();

        // Forbidden keys
        assert!(!yaml_str.contains("__gremlins_expanded__"));
        assert!(!yaml_str.contains("prompt_dir"));
        assert!(!yaml_str.contains("prompts"));
        assert!(!yaml_str.contains("stage-definitions"));

        // Model keys pass through verbatim
        assert!(yaml_str.contains("default_model: anthropic/claude-sonnet-4-20250514"));
        assert!(yaml_str.contains("model: openai/gpt-5"));
        assert!(yaml_str.contains("task-models:"));
        assert!(yaml_str.contains("review: openai/gpt-4.1"));

        // Bootstrap preserved
        assert!(yaml_str.contains("bootstrap:"));
        assert!(yaml_str.contains("goal:"));
        assert!(yaml_str.contains("type: string"));

        // Stage expansion — prompt is inlined (no .md refs)
        assert!(yaml_str.contains("prompt:"));
        assert!(yaml_str.contains("hello"));
    }

    /// When the source has no `default_model`, the bundled output also omits it.
    #[test]
    fn bundle_omits_default_model_when_source_has_none() {
        let s = Sandbox::new();
        let def_path = s.join("no-model.yaml");

        let yaml = r#"
name: no-model
stages:
  - name: plan
    type: agent
    prompt: |
      hello
"#;
        fs::write(&def_path, yaml).unwrap();

        let bundled = bundle_definition(&def_path, s.path()).unwrap();
        let mapping = bundled.as_mapping().unwrap();

        // default_model absent
        assert!(
            !mapping.contains_key("default_model"),
            "bundled output must not inject a default_model when the source had none"
        );
    }

    /// A definition with stage-definitions reference resolves them inline.
    #[test]
    fn bundle_inlines_stage_definition_references() {
        let s = Sandbox::new();

        // Write a stage definition into the overlay
        let stages_dir = s.join(".gremlins/stages");
        fs::create_dir_all(&stages_dir).unwrap();
        fs::write(
            stages_dir.join("baseline.yaml"),
            r#"
stages:
  - type: agent
    prompt: |
      {{prompt}}
    options: "{{options}}"
"#,
        )
        .unwrap();

        let def_path = s.join("uses-def.yaml");
        let yaml = r#"
name: uses-def
stages:
  - name: first
    type: baseline
    prompt: |
      do the thing
    options:
      key: val
"#;
        fs::write(&def_path, yaml).unwrap();

        let bundled = bundle_definition(&def_path, s.path()).unwrap();
        let yaml_str = serde_yaml::to_string(&bundled).unwrap();

        // The stage ref "baseline" should be gone from type position
        assert!(
            !yaml_str.contains("type: baseline"),
            "type: baseline should be inlined; got:\n{yaml_str}"
        );

        // The expanded stage should carry the inlined prompt text
        assert!(
            yaml_str.contains("do the thing"),
            "prompt should be inlined; got:\n{yaml_str}"
        );

        // The options should be inlined
        assert!(
            yaml_str.contains("key: val"),
            "options should be inlined; got:\n{yaml_str}"
        );
    }

    /// A definition without an overlay directory still bundles correctly.
    #[test]
    fn bundle_without_overlay_directory() {
        let s = Sandbox::new();
        let def_path = s.join("standalone.yaml");

        let yaml = r#"
name: standalone
stages:
  - name: plan
    type: agent
    prompt: |
      hello
"#;
        fs::write(&def_path, yaml).unwrap();

        // No .gremlins/ directory exists — project_root is the sandbox
        let bundled = bundle_definition(&def_path, s.path()).unwrap();
        let mapping = bundled.as_mapping().unwrap();

        assert!(mapping.contains_key("name"));
        assert!(mapping.contains_key("stages"));
        assert!(!mapping.contains_key("__gremlins_expanded__"));
    }
}
