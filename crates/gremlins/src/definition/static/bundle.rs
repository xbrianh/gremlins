use std::path::{Path, PathBuf};

use crate::config;
use crate::definition::r#static::expand;
use crate::schemas::error::SchemaError;

/// Bundle a modular definition into a self-contained monolith JSON.
///
/// Expands all structural references (stage-definitions, prompt files,
/// includes) into inline values while preserving model configuration
/// and bootstrap parameters verbatim so the output remains
/// environment-tunable.
///
/// The returned [`serde_json::Value`] is suitable for serialization as
/// a standalone JSON file that `gremlins launch` can consume directly.
///
/// The expansion root is derived from the definition file path (walking
/// up to the nearest `.gremlins/` directory), not from the current
/// working directory. This ensures explicit paths to definitions in
/// other projects resolve stage definitions, prompts, and includes
/// relative to the target project.
pub fn bundle_definition(yaml_path: &Path) -> Result<serde_json::Value, SchemaError> {
    let project_root = project_root_for(yaml_path);
    let overlay_dir = config::project_overlay_dir(&project_root);
    let expanded = expand::parse_definition_file(yaml_path, &project_root, &overlay_dir)?;

    let mut mapping = match expanded {
        serde_json::Value::Object(m) => m,
        other => {
            return Err(SchemaError::Generic(format!(
                "expected JSON object after expansion, got {:?}",
                other
            )));
        }
    };

    // Strip structural keys that serve no purpose once references are
    // inlined. Keep __gremlins_expanded__ so launch skips re-expansion
    // and treats every prompt as inline text (round-trip safety).
    mapping.remove("prompt_dir");
    mapping.remove("prompts");
    mapping.remove("stage-definitions");

    Ok(serde_json::Value::Object(mapping))
}

/// Walk up from `path` to find the nearest `.gremlins/` directory and
/// return its parent as the project root. Falls back to the file's
/// immediate parent when no `.gremlins/` ancestor exists.
fn project_root_for(path: &Path) -> PathBuf {
    let overlay_name = config::overlay_dirname();
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory
            .file_name()
            .is_some_and(|name| name == overlay_name)
        {
            if let Some(parent) = directory.parent() {
                return parent.to_path_buf();
            }
            break;
        }
        current = directory.parent();
    }
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    use std::fs;

    /// Write a minimal definition to a Sandbox, bundle it, and verify
    /// structural keys are absent, the sentinel survives, and model keys
    /// survive verbatim.
    #[test]
    fn bundle_strips_structural_keys_and_preserves_model_keys() {
        let s = Sandbox::new();
        let def_path = s.join("demo.json5");

        // Use plain string prompts so they are treated as inline
        // text rather than file paths.
        let json = r#"
{
  "name": "demo",
  "default_model": "anthropic/claude-sonnet-4-20250514",
  "bootstrap": {
    "source": {
      "goal": {
        "type": "string"
      }
    }
  },
  "stages": [
    {
      "name": "plan",
      "type": "agent",
      "model": "openai/gpt-5",
      "task-models": {
        "review": "openai/gpt-4.1"
      },
      "prompt": "hello\n"
    }
  ]
}
"#;
        fs::write(&def_path, json).unwrap();

        let bundled = bundle_definition(&def_path).unwrap();
        let json_str = serde_json::to_string(&bundled).unwrap();

        // __gremlins_expanded__ must survive so launch skips re-expansion.
        assert!(json_str.contains("__gremlins_expanded__"));

        // These structural keys are stripped — they serve no purpose.
        assert!(!json_str.contains("prompt_dir"));
        assert!(!json_str.contains("prompts"));
        assert!(!json_str.contains("stage-definitions"));

        // Model keys pass through verbatim
        assert!(json_str.contains("\"default_model\""));
        assert!(json_str.contains("\"anthropic/claude-sonnet-4-20250514\""));
        assert!(json_str.contains("\"model\""));
        assert!(json_str.contains("\"openai/gpt-5\""));
        assert!(json_str.contains("\"task-models\""));
        assert!(json_str.contains("\"review\""));
        assert!(json_str.contains("\"openai/gpt-4.1\""));

        // Bootstrap preserved
        assert!(json_str.contains("\"bootstrap\""));
        assert!(json_str.contains("\"goal\""));
        assert!(json_str.contains("\"type\""));
        assert!(json_str.contains("\"string\""));

        // Stage expansion — prompt is inlined (no .md refs)
        assert!(json_str.contains("\"prompt\""));
        assert!(json_str.contains("hello"));
    }

    /// When the source has no `default_model`, the bundled output also omits it.
    #[test]
    fn bundle_omits_default_model_when_source_has_none() {
        let s = Sandbox::new();
        let def_path = s.join("no-model.json5");

        let json = r#"
{
  "name": "no-model",
  "stages": [
    {
      "name": "plan",
      "type": "agent",
      "prompt": "hello\n"
    }
  ]
}
"#;
        fs::write(&def_path, json).unwrap();

        let bundled = bundle_definition(&def_path).unwrap();
        let mapping = bundled.as_object().unwrap();

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
            stages_dir.join("baseline.json5"),
            r#"
{
  "stages": [
    {
      "type": "agent",
      "prompt": "{{prompt}}",
      "options": "{{options}}"
    }
  ]
}
"#,
        )
        .unwrap();

        let def_path = s.join("uses-def.json5");
        let json = r#"
{
  "name": "uses-def",
  "stages": [
    {
      "name": "first",
      "type": "baseline",
      "prompt": "do the thing\n",
      "options": {
        "key": "val"
      }
    }
  ]
}
"#;
        fs::write(&def_path, json).unwrap();

        let bundled = bundle_definition(&def_path).unwrap();
        let json_str = serde_json::to_string(&bundled).unwrap();

        // The stage ref "baseline" should be gone from type position
        assert!(
            !json_str.contains("baseline"),
            "type: baseline should be inlined; got:\n{json_str}"
        );

        // The expanded stage should carry the inlined prompt text
        assert!(
            json_str.contains("do the thing"),
            "prompt should be inlined; got:\n{json_str}"
        );

        // The options should be inlined
        assert!(
            json_str.contains("\"key\"") && json_str.contains("\"val\""),
            "options should be inlined; got:\n{json_str}"
        );
    }

    /// A definition without an overlay directory still bundles correctly.
    #[test]
    fn bundle_without_overlay_directory() {
        let s = Sandbox::new();
        let def_path = s.join("standalone.json5");

        let json = r#"
{
  "name": "standalone",
  "stages": [
    {
      "name": "plan",
      "type": "agent",
      "prompt": "hello\n"
    }
  ]
}
"#;
        fs::write(&def_path, json).unwrap();

        // No .gremlins/ directory exists — project_root is the sandbox
        let bundled = bundle_definition(&def_path).unwrap();
        let mapping = bundled.as_object().unwrap();

        assert!(mapping.contains_key("name"));
        assert!(mapping.contains_key("stages"));
        assert!(mapping.contains_key("__gremlins_expanded__"));
    }

    /// Bundling an explicit path outside the current project still
    /// resolves stage definitions from the target project's overlay.
    #[test]
    fn bundle_explicit_path_uses_target_project_stage_defs() {
        let s = Sandbox::new();

        // Simulate "other" project with its own .gremlins/stages
        let other = s.join("other");
        let other_overlay = other.join(".gremlins");
        let other_stages = other_overlay.join("stages");
        fs::create_dir_all(&other_stages).unwrap();
        fs::write(
            other_stages.join("tool.json5"),
            r#"
{
  "stages": [
    {
      "type": "agent",
      "prompt": "{{prompt}}"
    }
  ]
}
"#,
        )
        .unwrap();

        let def_path = other.join("mydef.json5");
        fs::write(
            &def_path,
            r#"
{
  "name": "other-def",
  "stages": [
    {
      "name": "step",
      "type": "tool",
      "prompt": "build it\n"
    }
  ]
}
"#,
        )
        .unwrap();

        // Bundle using the explicit path. Even though the current project
        // is the sandbox root (with no .gremlins), stage "tool" must
        // resolve from `other/.gremlins/stages/tool.json5`.
        let bundled = bundle_definition(&def_path).unwrap();
        let json_str = serde_json::to_string(&bundled).unwrap();

        assert!(
            !json_str.contains("tool"),
            "stage-def ref must be inlined, got:\n{json_str}"
        );
        assert!(json_str.contains("build it"));
    }

    /// A bundled definition must survive a round-trip through serialize →
    /// deserialize without re-triggering prompt-file resolution. When a
    /// prompt file contains text without a trailing newline (e.g. a single
    /// line like "Write a plan"), the bundled output must carry the
    /// sentinel so the loader treats every prompt as already-inlined text.
    #[test]
    fn bundle_round_trip_preserves_inline_prompts() {
        let s = Sandbox::new();

        // Write a prompt file whose content has no trailing newline.
        // `read_prompts` treats strings without \n as filenames, so
        // without the sentinel the loader would try to find a file
        // called "Write a plan" and fail.
        let prompts_dir = s.join("prompts");
        fs::create_dir_all(&prompts_dir).unwrap();
        fs::write(prompts_dir.join("plan.md"), "Write a plan").unwrap();

        let def_path = s.join("roundtrip.json5");
        let json = r#"
{
  "name": "roundtrip",
  "default_model": "openai/gpt-4",
  "prompt_dir": "prompts",
  "stages": [
    {
      "name": "plan",
      "type": "agent",
      "prompt": "plan.md"
    }
  ]
}
"#;
        fs::write(&def_path, json).unwrap();

        // Bundle → JSON text (expansion inlines the prompt file content).
        let bundled = bundle_definition(&def_path).unwrap();
        let json_str = serde_json::to_string(&bundled).unwrap();

        // The sentinel must be present so the loader skips re-expansion.
        assert!(json_str.contains("__gremlins_expanded__"));

        // The inlined prompt text must be present (not a filename).
        assert!(json_str.contains("Write a plan"));

        // Deserialize the bundled JSON through the normal definition
        // loader. This must not fail with PromptFileNotFound.
        let bytes = json_str.as_bytes();
        let def = crate::definition::StaticDefinition::deserialize_owned(bytes)
            .expect("round-trip deserialize must succeed");

        assert_eq!(def.name, "roundtrip");
        assert_eq!(def.stages.len(), 1);
        assert_eq!(def.stages[0].name(), "plan");
    }
}
