//! YAML ingestion for StaticDefinition.
//!
//! All YAML-reading logic lives here: the public [`StaticDefinition::from_yaml_file`]
//! constructor, the private `from_expanded_value` dispatcher, per-stage YAML→builder
//! conversion helpers, and the small helper functions that extract fields from a YAML
//! mapping.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::builders::agent::AgentBuilder;
use crate::builders::artifacts::{BindTarget, InterpolationValue};
use crate::builders::composite::{ParallelBuilder, SequenceBuilder};
use crate::builders::definition::{fill_builder_names, DefinitionBuilder, LandBuilder};
use crate::builders::exec::ExecBuilder;
use crate::config;
use crate::definition::r#static::expand;
use crate::definition::ClientSpec;
use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
use crate::stages::node::BuilderStage;
use crate::stages::parallel::ErrorPolicy;

use super::StaticDefinition;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The message emitted when no layer supplied a default client.
const MISSING_DEFAULT_CLIENT: &str = "gremlin definition is missing 'default_client' — set a \
     'default_client' in the definition YAML, pass --client on the command line, or set \
     'default-client' in config.json";

// ---------------------------------------------------------------------------
// Public constructors
// ---------------------------------------------------------------------------

impl StaticDefinition {
    /// Load a definition from a YAML file, expanding includes, stage-definitions,
    /// and prompts. `client_override` is the CLI `--client` value; consulted only
    /// when the YAML declares none.
    pub fn from_yaml_file(path: &Path, client_override: Option<&str>) -> Result<Self, SchemaError> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path.exists() {
            return Err(SchemaError::DefinitionFileNotFound {
                path: path.display().to_string(),
            });
        }

        let project_root = project_root_for(&path);
        let expanded = expand::parse_definition_file(&path, &project_root)?;

        from_expanded_value(expanded, &path, client_override)
    }

    /// Parse already-expanded YAML bytes directly — no file I/O, no
    /// project-root walk.
    ///
    /// Strips the `__gremlins_expanded__` sentinel if present, but tolerates
    /// its absence.
    pub fn from_expanded_bytes(
        data: &[u8],
        client_override: Option<&str>,
    ) -> Result<Self, SchemaError> {
        let mut expanded: Value = serde_yaml::from_slice(data)
            .map_err(|e| SchemaError::Generic(format!("failed to parse definition YAML: {e}")))?;
        if let Some(mapping) = expanded.as_mapping_mut() {
            let sentinel = Value::from("__gremlins_expanded__");
            mapping.remove(&sentinel);
        }
        from_expanded_value(expanded, Path::new("definition.yaml"), client_override)
    }

    /// Read an already-expanded YAML file — no expansion, no project-root
    /// walk. Strips the `__gremlins_expanded__` sentinel before dispatching.
    pub fn from_expanded_yaml_file(
        path: &Path,
        client_override: Option<&str>,
    ) -> Result<Self, SchemaError> {
        let data = std::fs::read(path).map_err(|e| {
            SchemaError::Generic(format!("failed to read {}: {e}", path.display()))
        })?;
        Self::from_expanded_bytes(&data, client_override)
    }
}

// ---------------------------------------------------------------------------
// Private: from_expanded_value
// ---------------------------------------------------------------------------

/// Shared extraction: turn an already-expanded YAML [`Value`] into a
/// [`StaticDefinition`] via the builder path.
fn from_expanded_value(
    expanded: Value,
    path: &Path,
    default_client_override: Option<&str>,
) -> Result<StaticDefinition, SchemaError> {
    let root = expanded
        .as_mapping()
        .ok_or_else(|| SchemaError::YamlNotMapping {
            label: path.display().to_string(),
            got: format!("{expanded:?}"),
        })?;

    let name = root
        .get("name")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(String::from))
        .unwrap_or_default();

    let yaml_default_client = default_client_from_yaml(root)?;
    let base_ref = base_ref_from_yaml(root)?;

    let raw_stages = stages_from_yaml(root)?;

    // Parse stages through the per-type YAML→builder dispatch.
    let mut stages: Vec<BuilderStage> = Vec::new();
    for raw in &raw_stages {
        let mapping = raw
            .as_mapping()
            .ok_or_else(|| SchemaError::Generic("each stage must be a mapping".to_string()))?;
        stages.push(stage_from_yaml(mapping)?);
    }

    // Bootstrap.
    let bootstrap = match root.get("bootstrap") {
        None | Some(Value::Null) => Bootstrap::default(),
        Some(value) => Bootstrap::from_yaml(Some(value))?,
    };

    // Land.
    let land = if let Some(land_val) = root.get("land").filter(|v| !v.is_null()) {
        let land_mapping = land_val
            .as_mapping()
            .ok_or_else(|| SchemaError::Generic("'land' must be a mapping".to_string()))?;
        Some(land_from_yaml_builder(land_mapping)?)
    } else {
        None
    };

    let default_client = resolve_default_client(yaml_default_client, default_client_override)?;

    let builder = DefinitionBuilder {
        name,
        base_ref,
        default_client,
        prompt_dir: path.parent().map(Path::to_path_buf),
        bootstrap,
        stages,
        land,
    };

    builder.build()
}

// ---------------------------------------------------------------------------
// Per-stage YAML → builder conversion
// ---------------------------------------------------------------------------

/// Dispatch a single stage mapping to the appropriate per-type builder.
fn stage_from_yaml(mapping: &Mapping) -> Result<BuilderStage, SchemaError> {
    let name = mapping
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let stage_type = mapping.get("type").and_then(Value::as_str).unwrap_or("");
    if stage_type.is_empty() {
        return Err(SchemaError::Generic(format!(
            "stage {name:?}: must have a 'type' field"
        )));
    }

    match stage_type {
        "agent" => agent_from_yaml(mapping, &name),
        "exec" => exec_from_yaml(mapping, &name),
        "sequence" => sequence_from_yaml(mapping, &name),
        "parallel" => parallel_from_yaml(mapping, &name),
        other => Err(SchemaError::Generic(format!(
            "stage {name:?}: unknown type {other:?}"
        ))),
    }
}

/// Read a YAML string key, returning `None` when absent or null.
fn yaml_str(mapping: &Mapping, key: &str) -> Option<String> {
    mapping
        .get(key)
        .filter(|v| !v.is_null())
        .and_then(Value::as_str)
        .map(String::from)
}

/// Read a YAML string→string mapping, returning an empty map when absent.
fn yaml_string_map(mapping: &Mapping, key: &str) -> Result<HashMap<String, String>, SchemaError> {
    let Some(raw) = mapping.get(key).filter(|v| !v.is_null()) else {
        return Ok(HashMap::new());
    };
    let map = raw
        .as_mapping()
        .ok_or_else(|| SchemaError::Generic(format!("'{key}' must be a mapping")))?;
    let mut result = HashMap::new();
    for (k, v) in map {
        let ks = k
            .as_str()
            .ok_or_else(|| SchemaError::Generic(format!("'{key}' keys must be strings")))?;
        let vs = v
            .as_str()
            .ok_or_else(|| SchemaError::Generic(format!("'{key}' values must be strings")))?;
        result.insert(ks.to_string(), vs.to_string());
    }
    Ok(result)
}

/// Read a YAML sequence of strings.
fn yaml_string_list(mapping: &Mapping, key: &str) -> Result<Vec<String>, SchemaError> {
    let Some(raw) = mapping.get(key).filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let seq = raw
        .as_sequence()
        .ok_or_else(|| SchemaError::Generic(format!("'{key}' must be a sequence")))?;
    let mut result = Vec::new();
    for (i, v) in seq.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| SchemaError::Generic(format!("'{key}'[{i}] must be a string")))?;
        result.push(s.to_string());
    }
    Ok(result)
}

/// Read `options` as a `HashMap<String, serde_json::Value>`.
fn yaml_options(mapping: &Mapping) -> Result<HashMap<String, serde_json::Value>, SchemaError> {
    let Some(raw) = mapping.get("options").filter(|v| !v.is_null()) else {
        return Ok(HashMap::new());
    };
    let opts = raw
        .as_mapping()
        .ok_or_else(|| SchemaError::Generic("'options' must be a mapping".to_string()))?;
    let mut result = HashMap::new();
    for (k, v) in opts {
        let ks = k
            .as_str()
            .ok_or_else(|| SchemaError::Generic("'options' keys must be strings".to_string()))?;
        let jv = serde_json::to_value(v).map_err(|e| {
            SchemaError::Generic(format!("'options' value for '{ks}' is not valid: {e}"))
        })?;
        result.insert(ks.to_string(), jv);
    }
    Ok(result)
}

/// Read `skip_if_exists` — empty string when absent.
fn yaml_skip_if_exists(mapping: &Mapping) -> String {
    yaml_str(mapping, "skip_if_exists").unwrap_or_default()
}

/// Read `client` — None when absent.
fn yaml_client(mapping: &Mapping) -> Option<ClientSpec> {
    yaml_str(mapping, "client").map(ClientSpec)
}

/// Build an [`AgentBuilder`] from a YAML stage mapping.
fn agent_from_yaml(mapping: &Mapping, name: &str) -> Result<BuilderStage, SchemaError> {
    let prompts = yaml_string_list(mapping, "prompt")?;
    let interpolation_map = yaml_string_map(mapping, "interpolation")?;
    let bind_map = yaml_string_map(mapping, "bind")?;
    let options = yaml_options(mapping)?;
    let client = yaml_client(mapping);

    let mut builder = AgentBuilder::new(name);
    for p in prompts {
        builder = builder.prompt(p);
    }
    for (k, v) in interpolation_map {
        builder = builder.interpolate(k, InterpolationValue(v));
    }
    for (k, v) in bind_map {
        builder = builder.bind(k, BindTarget(v));
    }
    for (k, v) in options {
        builder = builder.option(k, v);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

/// Build an [`ExecBuilder`] from a YAML stage mapping.
fn exec_from_yaml(mapping: &Mapping, name: &str) -> Result<BuilderStage, SchemaError> {
    let interpolation_map = yaml_string_map(mapping, "interpolation")?;
    let bind_map = yaml_string_map(mapping, "bind")?;
    let options = yaml_options(mapping)?;
    let client = yaml_client(mapping);

    let mut builder = ExecBuilder::new(name);
    for (k, v) in interpolation_map {
        builder = builder.interpolate(k, InterpolationValue(v));
    }
    for (k, v) in bind_map {
        builder = builder.bind(k, BindTarget(v));
    }
    for (k, v) in options {
        builder = builder.option(k, v);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

/// Build a [`SequenceBuilder`] from a YAML stage mapping.
fn sequence_from_yaml(mapping: &Mapping, name: &str) -> Result<BuilderStage, SchemaError> {
    let max_iterations = match mapping.get("max-iterations").filter(|v| !v.is_null()) {
        None => 1u32,
        Some(v) => {
            if let Some(n) = v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                n
            } else if let Some(s) = v.as_str() {
                s.parse::<u32>().map_err(|_| {
                    SchemaError::Generic(format!(
                        "'max-iterations' must be a positive integer, got {s:?}"
                    ))
                })?
            } else {
                return Err(SchemaError::Generic(format!(
                    "'max-iterations' must be a positive integer, got {v:?}"
                )));
            }
        }
    };
    let skip_if_exists = yaml_skip_if_exists(mapping);
    let client = yaml_client(mapping);

    // Interval from top-level key.
    let interval = mapping.get("interval").and_then(|v| v.as_f64());

    let body = yaml_children(mapping, "body")?;

    let mut builder = SequenceBuilder::new(name)
        .max_iterations(max_iterations)
        .stages(body);
    if let Some(secs) = interval {
        builder = builder.interval(secs);
    }
    if !skip_if_exists.is_empty() {
        builder = builder.skip_if_exists(skip_if_exists);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

/// Build a [`ParallelBuilder`] from a YAML stage mapping.
fn parallel_from_yaml(mapping: &Mapping, name: &str) -> Result<BuilderStage, SchemaError> {
    let max_concurrent = match mapping.get("max_concurrent").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let n = v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    SchemaError::Generic(format!(
                        "'max_concurrent' must be a positive integer, got {v:?}"
                    ))
                })?;
            Some(n)
        }
    };
    let cancel_on_error = match mapping.get("cancel_on_error").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| {
            SchemaError::Generic(format!("'cancel_on_error' must be a boolean, got {v:?}"))
        })?,
    };
    let error_policy = match mapping.get("error_policy").filter(|v| !v.is_null()) {
        None => ErrorPolicy::Any,
        Some(v) => {
            let raw = v.as_str().ok_or_else(|| {
                SchemaError::Generic(format!(
                    "'error_policy' must be a string (\"any\" or \"all\"), got {v:?}"
                ))
            })?;
            ErrorPolicy::parse(raw).ok_or_else(|| {
                SchemaError::Generic(format!(
                    "'error_policy' must be \"any\" or \"all\", got {raw:?}"
                ))
            })?
        }
    };
    let skip_if_exists = yaml_skip_if_exists(mapping);
    let client = yaml_client(mapping);

    let body = yaml_children(mapping, "body")?;

    let mut builder = ParallelBuilder::new(name)
        .stages(body)
        .cancel_on_error(cancel_on_error)
        .error_policy(error_policy);
    if let Some(mc) = max_concurrent {
        builder = builder.max_concurrent(mc);
    }
    if !skip_if_exists.is_empty() {
        builder = builder.skip_if_exists(skip_if_exists);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

/// Parse children from a composite's `key` ("body") through
/// the same per-type dispatch.
fn yaml_children(mapping: &Mapping, key: &str) -> Result<Vec<BuilderStage>, SchemaError> {
    let Some(raw) = mapping.get(key).filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let seq = raw
        .as_sequence()
        .ok_or_else(|| SchemaError::Generic(format!("'{key}' must be a sequence")))?;
    let mut children = Vec::new();
    for entry in seq {
        let child_map = entry.as_mapping().ok_or_else(|| {
            SchemaError::Generic("each child stage must be a mapping".to_string())
        })?;
        children.push(stage_from_yaml(child_map)?);
    }
    fill_builder_names(&mut children);
    Ok(children)
}

/// Build the land stage from its YAML mapping, forcing name=land and
/// type=exec through [`LandBuilder`].
fn land_from_yaml_builder(mapping: &Mapping) -> Result<BuilderStage, SchemaError> {
    let interpolation_map = yaml_string_map(mapping, "interpolation")?;
    let bind_map = yaml_string_map(mapping, "bind")?;
    let options = yaml_options(mapping)?;
    let client = yaml_client(mapping);

    let mut builder = LandBuilder::new();
    for (k, v) in interpolation_map {
        builder = builder.interpolate(k, InterpolationValue(v));
    }
    for (k, v) in bind_map {
        builder = builder.bind(k, BindTarget(v));
    }
    for (k, v) in options {
        builder = builder.option(k, v);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

// ---------------------------------------------------------------------------
// Helper functions (moved from definition/static/mod.rs)
// ---------------------------------------------------------------------------

/// The project root: the parent of the nearest ancestor `.gremlins` directory,
/// falling back to the definition's own directory.
fn project_root_for(path: &Path) -> PathBuf {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory
            .file_name()
            .is_some_and(|name| name == config::overlay_dirname())
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

fn default_client_from_yaml(root: &Mapping) -> Result<Option<String>, SchemaError> {
    let Some(value) = root.get("default_client").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let client = value
        .as_str()
        .ok_or_else(|| SchemaError::Generic("default_client must be a string".to_string()))?;
    if client.trim().is_empty() {
        return Err(SchemaError::Generic(
            "default_client must be a non-empty string".to_string(),
        ));
    }
    Ok(Some(client.to_string()))
}

fn base_ref_from_yaml(root: &Mapping) -> Result<String, SchemaError> {
    let value = match root.get("base_ref") {
        None | Some(Value::Null) => return Ok("current".to_string()),
        Some(value) => value,
    };
    let base_ref = value
        .as_str()
        .ok_or_else(|| SchemaError::Generic("base_ref must be a string".to_string()))?;
    let trimmed = base_ref.trim();
    if trimmed.is_empty() {
        return Err(SchemaError::Generic(
            "base_ref must be a non-empty string".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn stages_from_yaml(root: &Mapping) -> Result<Vec<Value>, SchemaError> {
    match root.get("stages") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(stages)) => Ok(stages.clone()),
        Some(_) => Err(SchemaError::Generic("'stages' must be a list".to_string())),
    }
}

fn resolve_default_client(
    yaml_default: Option<String>,
    override_client: Option<&str>,
) -> Result<String, SchemaError> {
    if let Some(client) = yaml_default {
        return Ok(client);
    }
    // A blank `--client` is no client at all: skip it rather than resolve to an
    // unusable empty spec.
    if let Some(client) = override_client
        .map(str::trim)
        .filter(|client| !client.is_empty())
    {
        return Ok(client.to_string());
    }
    config::global_config()
        .ok()
        .and_then(|cfg| cfg.default_client().map(String::from))
        .ok_or_else(|| SchemaError::Generic(MISSING_DEFAULT_CLIENT.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::agent::AgentBuilder;
    use crate::builders::artifacts::artifact;
    use crate::builders::composite::{ParallelBuilder, SequenceBuilder};
    use crate::builders::exec::ExecBuilder;
    use crate::stages::parallel::ErrorPolicy;

    // ------------------------------------------------------------------
    // to_yaml / from_yaml round-trip symmetry
    // ------------------------------------------------------------------

    /// Helper: serialize a BuilderStage to a YAML Mapping and parse it back
    /// through stage_from_yaml, asserting the two are equal.
    fn assert_round_trip(stage: &BuilderStage) {
        let yaml_val = stage.to_yaml();
        let mapping = yaml_val
            .as_mapping()
            .expect("to_yaml must produce a mapping");
        let round_tripped =
            stage_from_yaml(mapping).expect("stage_from_yaml must accept to_yaml output");
        assert_eq!(
            stage,
            &round_tripped,
            "round-trip mismatch for stage {} (type {})",
            stage.name(),
            stage.stage_type()
        );
    }

    #[test]
    fn agent_to_yaml_round_trip() {
        let stage = AgentBuilder::new("plan")
            .prompt("write the plan using {input} to {plan}")
            .interpolate(
                "input",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://input.md\")",
                ),
            )
            .bind("plan", artifact("artifact://plan.md"))
            .option("model", "xai:grok-4")
            .client("xai:grok-4")
            .build()
            .unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn agent_to_yaml_round_trip_minimal() {
        let stage = AgentBuilder::new("min").prompt("hi").build().unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn exec_to_yaml_round_trip() {
        let stage = ExecBuilder::new("run")
            .cmd("echo {input} > {out}")
            .interpolate(
                "input",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://in.txt\")",
                ),
            )
            .bind("out", artifact("artifact://out.txt"))
            .option("timeout", "30")
            .client("local")
            .build()
            .unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn exec_to_yaml_round_trip_minimal() {
        let stage = ExecBuilder::new("cmd").cmd("true").build().unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn sequence_to_yaml_round_trip() {
        let stage = SequenceBuilder::new("workflow")
            .stage(ExecBuilder::new("step-a").cmd("echo a").build().unwrap())
            .stage(ExecBuilder::new("step-b").cmd("echo b").build().unwrap())
            .max_iterations(3)
            .interval(1.5)
            .skip_if_exists("artifact://done.txt")
            .client("xai:grok-4")
            .build()
            .unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn sequence_to_yaml_round_trip_minimal() {
        let stage = SequenceBuilder::new("seq")
            .stage(ExecBuilder::new("x").cmd("true").build().unwrap())
            .build()
            .unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn parallel_to_yaml_round_trip() {
        let stage = ParallelBuilder::new("reviews")
            .stage(
                AgentBuilder::new("review-a")
                    .prompt("review")
                    .build()
                    .unwrap(),
            )
            .stage(
                AgentBuilder::new("review-b")
                    .prompt("review")
                    .build()
                    .unwrap(),
            )
            .max_concurrent(2)
            .cancel_on_error(true)
            .error_policy(ErrorPolicy::All)
            .skip_if_exists("artifact://reviews-done")
            .client("xai:grok-4")
            .build()
            .unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn parallel_to_yaml_round_trip_minimal() {
        let stage = ParallelBuilder::new("par")
            .stage(ExecBuilder::new("a").cmd("true").build().unwrap())
            .stage(ExecBuilder::new("b").cmd("true").build().unwrap())
            .build()
            .unwrap();
        assert_round_trip(&stage);
    }
}
