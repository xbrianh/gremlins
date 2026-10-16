//! YAML ingestion for StaticDefinition.
//!
//! All YAML-reading logic lives here: the public [`StaticDefinition::from_yaml_file`]
//! constructor, the private `from_expanded_value` dispatcher, per-stage YAML→builder
//! conversion helpers, and the small helper functions that extract fields from a YAML
//! mapping.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_yaml::{Mapping, Value};

use crate::builders::agent::AgentBuilder;
use crate::builders::artifacts::{InterpolationValue, OutputTarget};
use crate::builders::composite::{ParallelBuilder, SequenceBuilder};
use crate::builders::definition::{fill_builder_names, DefinitionBuilder, LandBuilder};
use crate::builders::exec::ExecBuilder;
use crate::clients::client::ModelURI;
use crate::clients::task_kind::TaskKind;
use crate::config;
use crate::definition::r#static::expand;
use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
use crate::stage_spec::node::StageSpec;
use crate::stage_spec::parallel::ErrorPolicy;

use super::StaticDefinition;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The message emitted when no layer supplied a default model.
const MISSING_DEFAULT_MODEL: &str = "gremlin definition is missing 'default_model' — set a \
     'default_model' in the definition YAML, pass --model on the command line, or set \
     'default-model' in settings.yaml";

/// Legacy model-selection keys carried over from the `client` grammar.
///
/// These were renamed in #1928/#1931 with **no** backward-compatibility
/// aliases. A YAML file that still uses them is rejected rather than silently
/// ignoring the model selection and falling back to a default (or losing a
/// stage override).
const LEGACY_DEFAULT_MODEL_KEYS: [(&str, &str); 2] = [
    ("default_client", "default_model"),
    ("default_task_clients", "default_task_models"),
];
const LEGACY_STAGE_MODEL_KEYS: [(&str, &str); 2] =
    [("client", "model"), ("task-clients", "task-models")];

// ---------------------------------------------------------------------------
// Public constructors
// ---------------------------------------------------------------------------

impl StaticDefinition {
    /// Load a definition from a YAML file, expanding includes, stage-definitions,
    /// and prompts. `model_override` is the CLI `--model` value; consulted only
    /// when the YAML declares none.
    pub fn from_yaml_file(
        path: &Path,
        model_override: Option<&str>,
        config_default_model: Option<&str>,
    ) -> Result<Self, SchemaError> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path.exists() {
            return Err(SchemaError::DefinitionFileNotFound {
                path: path.display().to_string(),
            });
        }

        let project_root = project_root_for(&path);
        let overlay_dir = config::overlay_dir_without_env(&project_root);
        let expanded = expand::parse_definition_file(&path, &project_root, &overlay_dir)?;

        let settings = config::global_config()
            .map_err(|e| SchemaError::Generic(format!("failed to load settings.yaml: {e}")))?;
        from_expanded_value(
            expanded,
            &path,
            model_override,
            config_default_model,
            Some(&settings),
        )
    }

    /// Parse already-expanded YAML bytes directly — no file I/O, no
    /// project-root walk.
    ///
    /// Strips the `__gremlins_expanded__` sentinel if present, but tolerates
    /// its absence.
    pub fn from_expanded_bytes(
        data: &[u8],
        model_override: Option<&str>,
        config_default_model: Option<&str>,
    ) -> Result<Self, SchemaError> {
        let expanded = parse_expanded_yaml(data)?;
        from_expanded_value(
            expanded,
            Path::new("definition.yaml"),
            model_override,
            config_default_model,
            None,
        )
    }

    /// Read an already-expanded YAML file — no expansion, no project-root
    /// walk. Strips the `__gremlins_expanded__` sentinel before dispatching.
    pub fn from_expanded_yaml_file(
        path: &Path,
        model_override: Option<&str>,
        config_default_model: Option<&str>,
    ) -> Result<Self, SchemaError> {
        let data = std::fs::read(path)
            .map_err(|e| SchemaError::Generic(format!("failed to read {}: {e}", path.display())))?;
        let expanded = parse_expanded_yaml(&data)?;
        from_expanded_value(expanded, path, model_override, config_default_model, None)
    }
}

// ---------------------------------------------------------------------------
// Private: parse_expanded_yaml
// ---------------------------------------------------------------------------

/// Parse YAML bytes and strip the `__gremlins_expanded__` sentinel.
fn parse_expanded_yaml(data: &[u8]) -> Result<Value, SchemaError> {
    let mut expanded: Value = serde_yaml::from_slice(data)
        .map_err(|e| SchemaError::Generic(format!("failed to parse definition YAML: {e}")))?;
    if let Some(mapping) = expanded.as_mapping_mut() {
        let sentinel = Value::from("__gremlins_expanded__");
        mapping.remove(&sentinel);
    }
    Ok(expanded)
}

// ---------------------------------------------------------------------------
// Private: from_expanded_value
// ---------------------------------------------------------------------------

/// Shared extraction: turn an already-expanded YAML [`Value`] into a
/// [`StaticDefinition`] via the builder path.
fn from_expanded_value(
    expanded: Value,
    path: &Path,
    default_model_override: Option<&str>,
    config_default_model: Option<&str>,
    settings: Option<&config::Config>,
) -> Result<StaticDefinition, SchemaError> {
    let root = expanded
        .as_mapping()
        .ok_or_else(|| SchemaError::YamlNotMapping {
            label: path.display().to_string(),
            got: format!("{expanded:?}"),
        })?;

    reject_legacy_default_model_keys(root)?;

    let name = root
        .get("name")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(String::from))
        .unwrap_or_default();

    let raw_yaml_default_model = default_model_from_yaml(root)?;
    base_ref_from_yaml(root)?;

    let raw_stages = stages_from_yaml(root)?;

    // Capture profile names from raw default-model sources before
    // resolution. These determine which profile's `task-models`
    // stages inherit when they don't supply their own model.
    let yaml_default_profile: Option<String> = raw_yaml_default_model
        .as_ref()
        .and_then(|raw| raw.strip_prefix("profile:").map(|s| s.to_string()));
    let override_default_profile: Option<String> =
        default_model_override.and_then(|raw| raw.strip_prefix("profile:").map(|s| s.to_string()));
    let config_default_profile: Option<String> =
        config_default_model.and_then(|raw| raw.strip_prefix("profile:").map(|s| s.to_string()));

    // Bake every stage and default model with the current settings profile
    // (source-definition path only). Expanded definitions from
    // `from_expanded_bytes` / `from_expanded_yaml_file` pass `None` here:
    // their values are already concrete `provider/model[?k=v]` strings, so
    // no profile resolution is needed and a missing/malformed settings.yaml
    // must not block resuming from a baked definition.
    let yaml_default_model = match settings {
        Some(settings) => raw_yaml_default_model
            .map(|c| resolve_model(&c, settings).map_err(SchemaError::Generic))
            .transpose()?,
        None => raw_yaml_default_model,
    };
    let config_default_model = match settings {
        Some(settings) => config_default_model
            .map(|c| resolve_model(c, settings).map_err(SchemaError::Generic))
            .transpose()?,
        None => config_default_model.map(String::from),
    };
    let default_model_override = match settings {
        Some(settings) => default_model_override
            .map(|c| resolve_model(c, settings).map_err(SchemaError::Generic))
            .transpose()?,
        None => default_model_override.map(String::from),
    };

    // Determine which profile (if any) contributes the effective default
    // model.  This mirrors resolve_default_model's precedence:
    // YAML > CLI override > settings.yaml.
    let effective_profile: Option<&str> = if yaml_default_model.is_some() {
        yaml_default_profile.as_deref()
    } else if default_model_override
        .as_ref()
        .is_some_and(|s| !s.is_empty())
    {
        override_default_profile.as_deref()
    } else {
        config_default_profile.as_deref()
    };

    // Parse stages through the per-type YAML→builder dispatch.
    let mut stages: Vec<StageSpec> = Vec::new();
    for raw in &raw_stages {
        let mapping = raw
            .as_mapping()
            .ok_or_else(|| SchemaError::Generic("each stage must be a mapping".to_string()))?;
        stages.push(stage_from_yaml(mapping, settings, effective_profile)?);
    }

    // Bootstrap.
    let bootstrap = match root.get("bootstrap") {
        None | Some(Value::Null) => Bootstrap::default(),
        Some(value) => Bootstrap::from_yaml(Some(value))?,
    };

    // Land.
    let (land, clean_cmds) = if let Some(land_val) = root.get("land").filter(|v| !v.is_null()) {
        let land_mapping = land_val
            .as_mapping()
            .ok_or_else(|| SchemaError::Generic("'land' must be a mapping".to_string()))?;
        let (land_stage, clean) = land_from_yaml_builder(land_mapping, settings)?;
        (Some(land_stage), clean)
    } else {
        (None, Vec::new())
    };

    let default_model = resolve_default_model(
        yaml_default_model,
        default_model_override.as_deref(),
        config_default_model.as_deref(),
    )?;

    // Read or resolve default_task_models.
    // If the YAML already contains a `default_task_models` key (from a
    // previously expanded definition), use it directly — values are
    // already resolved.  Otherwise resolve the global
    // `default-task-models` through profiles so the expanded output
    // includes only concrete `provider/model[?k=v]` strings.
    let default_task_models = yaml_default_task_models(root, settings)?;

    let builder = DefinitionBuilder {
        name,
        default_model,
        prompt_dir: path.parent().map(Path::to_path_buf),
        bootstrap,
        stages,
        land,
        clean_cmds,
        default_task_models,
    };

    builder.build()
}

// ---------------------------------------------------------------------------
// Per-stage YAML → builder conversion
// ---------------------------------------------------------------------------

/// Dispatch a single stage mapping to the appropriate per-type builder.
fn stage_from_yaml(
    mapping: &Mapping,
    settings: Option<&config::Config>,
    effective_profile: Option<&str>,
) -> Result<StageSpec, SchemaError> {
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
    reject_legacy_stage_model_keys(mapping, &name)?;

    match stage_type {
        "agent" => agent_from_yaml(mapping, &name, settings, effective_profile),
        "exec" => exec_from_yaml(mapping, &name, settings, effective_profile),
        "sequence" => sequence_from_yaml(mapping, &name, settings, effective_profile),
        "parallel" => parallel_from_yaml(mapping, &name, settings, effective_profile),
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

/// Reject legacy top-level model-selection keys on a definition mapping.
fn reject_legacy_default_model_keys(root: &Mapping) -> Result<(), SchemaError> {
    for (legacy, replacement) in LEGACY_DEFAULT_MODEL_KEYS {
        if root.contains_key(legacy) {
            return Err(SchemaError::Generic(format!(
                "top-level '{legacy}' is no longer supported; use '{replacement}' instead"
            )));
        }
    }
    Ok(())
}

/// Reject legacy stage-level model-selection keys on a stage mapping.
fn reject_legacy_stage_model_keys(mapping: &Mapping, label: &str) -> Result<(), SchemaError> {
    for (legacy, replacement) in LEGACY_STAGE_MODEL_KEYS {
        if mapping.contains_key(legacy) {
            return Err(SchemaError::Generic(format!(
                "{label}: '{legacy}' is no longer supported; use '{replacement}' instead"
            )));
        }
    }
    Ok(())
}

/// Read the nested `interpolation:` structure: `{inputs: {…}, outputs: {…}}`.
/// Returns (inputs_map, outputs_map). If `interpolation` is absent, returns
/// empty maps.
///
/// Rejects legacy flat `interpolation:` (keys other than `inputs`/`outputs`),
/// non-mapping values, and the deprecated top-level `bind:` key.
#[allow(clippy::type_complexity)]
fn yaml_interpolation_nested(
    mapping: &Mapping,
) -> Result<(HashMap<String, String>, HashMap<String, String>), SchemaError> {
    // Reject legacy top-level `bind:` key
    if mapping.contains_key("bind") {
        return Err(SchemaError::Generic(
            "top-level 'bind:' is no longer supported; use 'interpolation: {outputs: {\u{2026}}}' instead"
                .to_string(),
        ));
    }
    if let Some(interp_val) = mapping.get("interpolation").filter(|v| !v.is_null()) {
        let interp_map = interp_val
            .as_mapping()
            .ok_or_else(|| SchemaError::Generic("'interpolation' must be a mapping".to_string()))?;
        // Reject unknown or non-string sub-keys — only "inputs" and "outputs" are valid
        for key in interp_map.keys() {
            match key.as_str() {
                Some("inputs") | Some("outputs") => {}
                Some(other) => {
                    return Err(SchemaError::Generic(format!(
                        "unknown key {other:?} in 'interpolation'; expected 'inputs' or 'outputs'"
                    )));
                }
                None => {
                    return Err(SchemaError::Generic(
                        "'interpolation' keys must be strings".to_string(),
                    ));
                }
            }
        }
        let inputs = yaml_string_map(interp_map, "inputs")?;
        let outputs = yaml_string_map(interp_map, "outputs")?;
        return Ok((inputs, outputs));
    }
    Ok((HashMap::new(), HashMap::new()))
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

/// Read a YAML string→string mapping, preserving declaration order.
fn yaml_string_map_ordered(
    mapping: &Mapping,
    key: &str,
) -> Result<Option<IndexMap<String, String>>, SchemaError> {
    let Some(raw) = mapping.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let map = raw
        .as_mapping()
        .ok_or_else(|| SchemaError::Generic("'task-models' must be a mapping".to_string()))?;
    let mut result = IndexMap::new();
    for (k, v) in map {
        let ks = k.as_str().ok_or_else(|| {
            SchemaError::Generic("'task-models' keys must be strings".to_string())
        })?;
        let vs = v.as_str().ok_or_else(|| {
            SchemaError::Generic("'task-models' values must be strings".to_string())
        })?;
        if TaskKind::parse(ks) == Some(TaskKind::Fork) {
            return Err(SchemaError::Generic(
                "Fork cannot be configured — it always inherits the parent model".to_string(),
            ));
        }
        if TaskKind::parse(ks).is_none() {
            return Err(SchemaError::Generic(format!(
                "unknown task-models key {ks:?}: expected one of {}",
                TaskKind::CONFIGURABLE
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        result.insert(ks.to_string(), vs.to_string());
    }
    Ok(Some(result))
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

/// Read `model` — None when absent. Values are resolved through profiles
/// when settings are supplied; otherwise they are assumed to already be
/// concrete `provider/model[?k=v]` strings.
fn yaml_model(
    mapping: &Mapping,
    settings: Option<&config::Config>,
) -> Result<Option<ModelURI>, SchemaError> {
    let Some(raw) = yaml_str(mapping, "model") else {
        return Ok(None);
    };
    let resolved = match settings {
        Some(settings) => resolve_model(&raw, settings).map_err(SchemaError::Generic)?,
        None => raw,
    };
    Ok(Some(
        ModelURI::parse(&resolved).map_err(SchemaError::Generic)?,
    ))
}

/// Read `task-models` — an ordered string→string map, resolved through
/// profiles when settings are supplied. Returns `None` when the stage has no
/// `task-models` key and no profile contributes task-models.
/// Global `default-task-models` are NOT merged here; that happens at
/// runtime so composite-stage inheritance works correctly.
///
/// When the stage's `model` references a profile (e.g. `profile:foo`),
/// that profile's `task_models` are used as a base layer; the stage's own
/// `task-models` entries win on key conflict.  When the stage has no
/// `model` of its own but `effective_profile` names a profile (because
/// the definition default-model, CLI --model, or settings.yaml
/// default-model selected a profile), that profile's `task_models` also
/// serve as the base layer.
fn yaml_task_models(
    mapping: &Mapping,
    settings: Option<&config::Config>,
    effective_profile: Option<&str>,
) -> Result<Option<IndexMap<String, String>>, SchemaError> {
    let stage = yaml_string_map_ordered(mapping, "task-models")?;

    // Resolve which profile (if any) contributes task-models as a base.
    // Priority: the stage's own `model: profile:…` wins; otherwise the
    // effective default-profile from the definition.
    let profile_name: Option<String> = yaml_str(mapping, "model")
        .and_then(|raw| raw.strip_prefix("profile:").map(|name| name.to_string()))
        .or_else(|| effective_profile.map(|s| s.to_string()));

    let profile_task_models: Option<IndexMap<String, String>> = settings.and_then(|settings| {
        profile_name
            .as_ref()
            .and_then(|name| settings.model_profiles().get(name))
            .and_then(|profile| profile.task_models.clone())
    });

    if stage.is_none() && profile_task_models.is_none() {
        return Ok(None);
    }

    // Merge: profile base + stage overrides (stage wins on conflict).
    let mut resolved = IndexMap::new();
    if let Some(settings) = settings {
        if let Some(ref profile_tc) = profile_task_models {
            for (key, value) in profile_tc {
                resolved.insert(
                    key.clone(),
                    config::resolve_model_reference(value, settings)
                        .map_err(SchemaError::Generic)?,
                );
            }
        }
        if let Some(ref stage_tc) = stage {
            for (key, value) in stage_tc {
                resolved.insert(
                    key.clone(),
                    config::resolve_model_reference(value, settings)
                        .map_err(SchemaError::Generic)?,
                );
            }
        }
    } else if let Some(ref stage_tc) = stage {
        for (key, value) in stage_tc {
            resolved.insert(key.clone(), value.clone());
        }
    }
    Ok(Some(resolved))
}

fn resolve_model(raw: &str, settings: &config::Config) -> Result<String, String> {
    config::resolve_model_reference(raw, settings)
}

/// Build an [`AgentBuilder`] from a YAML stage mapping.
fn agent_from_yaml(
    mapping: &Mapping,
    name: &str,
    settings: Option<&config::Config>,
    effective_profile: Option<&str>,
) -> Result<StageSpec, SchemaError> {
    let prompts = yaml_string_list(mapping, "prompt")?;
    let (interpolation_map, bind_map) = yaml_interpolation_nested(mapping)?;
    let options = yaml_options(mapping)?;
    let model = yaml_model(mapping, settings)?;
    let task_models = yaml_task_models(mapping, settings, effective_profile)?;

    let mut builder = AgentBuilder::new(name);
    for p in prompts {
        builder = builder.prompt(p);
    }
    for (k, v) in interpolation_map {
        builder = builder.interpolate(k, InterpolationValue(v));
    }
    for (k, v) in bind_map {
        builder = builder.output(k, OutputTarget(v));
    }
    for (k, v) in options {
        builder = builder.option(k, v);
    }
    if let Some(c) = model {
        builder = builder.model(c);
    }
    if let Some(tc) = task_models {
        builder = builder.task_models(tc);
    }

    builder.build()
}

/// Build an [`ExecBuilder`] from a YAML stage mapping.
fn exec_from_yaml(
    mapping: &Mapping,
    name: &str,
    settings: Option<&config::Config>,
    effective_profile: Option<&str>,
) -> Result<StageSpec, SchemaError> {
    let (interpolation_map, bind_map) = yaml_interpolation_nested(mapping)?;
    let options = yaml_options(mapping)?;
    let model = yaml_model(mapping, settings)?;
    let task_models = yaml_task_models(mapping, settings, effective_profile)?;

    let mut builder = ExecBuilder::new(name);
    for (k, v) in interpolation_map {
        builder = builder.interpolate(k, InterpolationValue(v));
    }
    for (k, v) in bind_map {
        builder = builder.output(k, OutputTarget(v));
    }
    for (k, v) in options {
        builder = builder.option(k, v);
    }
    if let Some(c) = model {
        builder = builder.model(c);
    }
    if let Some(tc) = task_models {
        builder = builder.task_models(tc);
    }

    builder.build()
}

/// Build a [`SequenceBuilder`] from a YAML stage mapping.
fn sequence_from_yaml(
    mapping: &Mapping,
    name: &str,
    settings: Option<&config::Config>,
    effective_profile: Option<&str>,
) -> Result<StageSpec, SchemaError> {
    let max_iterations = match mapping.get("max-iterations").filter(|v| !v.is_null()) {
        None => 1u32,
        Some(v) => {
            if let Some(n) = v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                n
            } else {
                return Err(SchemaError::Generic(format!(
                    "'max-iterations' must be a positive integer, got {v:?}"
                )));
            }
        }
    };
    let skip_if_exists = yaml_skip_if_exists(mapping);
    let model = yaml_model(mapping, settings)?;
    let task_models = yaml_task_models(mapping, settings, effective_profile)?;

    // Interval from top-level key.  Only accepts numbers (YAML integers
    // coerce to f64 via as_f64).  Reject non-numeric values with a
    // SchemaError.
    let interval = match mapping.get("interval") {
        None | Some(serde_yaml::Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(f) => Some(f),
            None => {
                return Err(SchemaError::Generic(format!(
                    "'interval' must be a number, got {v:?}"
                )));
            }
        },
    };

    let body = yaml_children(mapping, "body", settings, effective_profile)?;

    let mut builder = SequenceBuilder::new(name)
        .max_iterations(max_iterations)
        .stages(body);
    if let Some(secs) = interval {
        builder = builder.interval(secs);
    }
    if !skip_if_exists.is_empty() {
        builder = builder.skip_if_exists(skip_if_exists);
    }
    if let Some(c) = model {
        builder = builder.model(c);
    }
    if let Some(tc) = task_models {
        builder = builder.task_models(tc);
    }

    builder.build()
}

/// Build a [`ParallelBuilder`] from a YAML stage mapping.
fn parallel_from_yaml(
    mapping: &Mapping,
    name: &str,
    settings: Option<&config::Config>,
    effective_profile: Option<&str>,
) -> Result<StageSpec, SchemaError> {
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
    let model = yaml_model(mapping, settings)?;
    let task_models = yaml_task_models(mapping, settings, effective_profile)?;

    let body = yaml_children(mapping, "body", settings, effective_profile)?;

    // Parse fork and join specs
    let fork = yaml_fork_join(mapping, "fork")?;
    let join = yaml_fork_join(mapping, "join")?;

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
    if let Some(c) = model {
        builder = builder.model(c);
    }
    if let Some(tc) = task_models {
        builder = builder.task_models(tc);
    }
    if let Some(fork_cmds) = fork {
        builder = builder.fork(fork_cmds);
    }
    if let Some(join_cmds) = join {
        builder = builder.join(join_cmds);
    }

    builder.build()
}

/// Parse a `fork` or `join` key from a parallel stage mapping.
/// Each is a mapping with a `cmds:` sequence of strings.
fn yaml_fork_join(mapping: &Mapping, key: &str) -> Result<Option<Vec<String>>, SchemaError> {
    let Some(raw) = mapping.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let fmap = raw
        .as_mapping()
        .ok_or_else(|| SchemaError::Generic(format!("'{key}' must be a mapping")))?;
    let cmds = yaml_string_list(fmap, "cmds")?;
    Ok(Some(cmds))
}

/// Parse children from a composite's `key` ("body") through
/// the same per-type dispatch.
fn yaml_children(
    mapping: &Mapping,
    key: &str,
    settings: Option<&config::Config>,
    effective_profile: Option<&str>,
) -> Result<Vec<StageSpec>, SchemaError> {
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
        children.push(stage_from_yaml(child_map, settings, effective_profile)?);
    }
    fill_builder_names(&mut children);
    Ok(children)
}

/// Build the land stage from its YAML mapping, forcing name=land and
/// type=exec through [`LandBuilder`]. Returns the land stage and clean_cmds.
fn land_from_yaml_builder(
    mapping: &Mapping,
    settings: Option<&config::Config>,
) -> Result<(StageSpec, Vec<String>), SchemaError> {
    reject_legacy_stage_model_keys(mapping, "land")?;

    let (interpolation_map, bind_map) = yaml_interpolation_nested(mapping)?;
    let options = yaml_options(mapping)?;
    // Reject legacy options.cmds — land_cmds / clean_cmds are the only
    // supported command keys in the land mapping.
    if options.contains_key("cmds") {
        return Err(SchemaError::Generic(
            "land: options.cmds is no longer supported — use land_cmds and clean_cmds instead"
                .to_string(),
        ));
    }
    let model = yaml_model(mapping, settings)?;

    let land_cmds = yaml_string_list(mapping, "land_cmds")?;
    let clean_cmds = yaml_string_list(mapping, "clean_cmds")?;

    let mut builder = LandBuilder::new();
    for (k, v) in interpolation_map {
        builder = builder.interpolate(k, InterpolationValue(v));
    }
    for (k, v) in bind_map {
        builder = builder.output(k, OutputTarget(v));
    }
    for (k, v) in options {
        builder = builder.option(k, v);
    }
    if let Some(c) = model {
        builder = builder.model(c);
    }
    if !land_cmds.is_empty() {
        builder = builder.land_cmds(land_cmds);
    }
    if !clean_cmds.is_empty() {
        builder = builder.clean_cmds(clean_cmds.clone());
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

fn default_model_from_yaml(root: &Mapping) -> Result<Option<String>, SchemaError> {
    let Some(value) = root.get("default_model").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let model = value
        .as_str()
        .ok_or_else(|| SchemaError::Generic("default_model must be a string".to_string()))?;
    if model.trim().is_empty() {
        return Err(SchemaError::Generic(
            "default_model must be a non-empty string".to_string(),
        ));
    }
    Ok(Some(model.to_string()))
}

fn base_ref_from_yaml(root: &Mapping) -> Result<(), SchemaError> {
    if root.contains_key("base_ref") {
        return Err(SchemaError::Generic(
            "top-level 'base_ref:' is no longer supported. Use bootstrap.source to declare a base_ref input and set up the worktree in launch_cmds with: git worktree add --detach \"$GREMLIN_WORKDIR\" \"$SHA\"".to_string(),
        ));
    }
    Ok(())
}

fn stages_from_yaml(root: &Mapping) -> Result<Vec<Value>, SchemaError> {
    match root.get("stages") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(stages)) => Ok(stages.clone()),
        Some(_) => Err(SchemaError::Generic("'stages' must be a list".to_string())),
    }
}

/// Read `default_task_models` from the already-expanded YAML, or resolve
/// the global `default-task-models` through profiles.
///
/// When the YAML already carries a `default_task_models` key (round-tripped
/// from a previous expansion), values are treated as already-resolved
/// `provider/model[?k=v]` strings and returned as-is.  Otherwise the global
/// `default-task-models` map is read from settings.yaml and every value is
/// resolved through the profile table so the expanded output only contains
/// concrete model specs.
fn yaml_default_task_models(
    root: &Mapping,
    settings: Option<&config::Config>,
) -> Result<Option<IndexMap<String, String>>, SchemaError> {
    // If the expanded YAML already carries default_task_models, use them
    // directly — they were resolved during a previous expansion.
    if let Some(raw) = root.get("default_task_models").filter(|v| !v.is_null()) {
        let map = raw.as_mapping().ok_or_else(|| {
            SchemaError::Generic("'default_task_models' must be a mapping".to_string())
        })?;
        let mut result = IndexMap::new();
        for (k, v) in map {
            let key = k.as_str().ok_or_else(|| {
                SchemaError::Generic("'default_task_models' keys must be strings".to_string())
            })?;
            let value = v.as_str().ok_or_else(|| {
                SchemaError::Generic("'default_task_models' values must be strings".to_string())
            })?;
            if TaskKind::parse(key) == Some(TaskKind::Fork) {
                return Err(SchemaError::Generic(
                    "Fork cannot be configured — it always inherits the parent model".to_string(),
                ));
            }
            if TaskKind::parse(key).is_none() {
                return Err(SchemaError::Generic(format!(
                    "unknown default_task_models key {key:?}: expected one of {}",
                    TaskKind::CONFIGURABLE
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            result.insert(key.to_string(), value.to_string());
        }
        if result.is_empty() {
            return Ok(None);
        }
        return Ok(Some(result));
    }

    // Otherwise, resolve the global default-task-models through profiles.
    let settings = match settings {
        Some(settings) => settings,
        None => return Ok(None),
    };
    let global = match settings.default_task_models() {
        Some(g) => g,
        None => return Ok(None),
    };
    let mut resolved = IndexMap::new();
    for (key, value) in global {
        resolved.insert(
            key.clone(),
            resolve_model(value, settings).map_err(SchemaError::Generic)?,
        );
    }
    if resolved.is_empty() {
        Ok(None)
    } else {
        Ok(Some(resolved))
    }
}

fn resolve_default_model(
    yaml_default: Option<String>,
    override_model: Option<&str>,
    config_default_model: Option<&str>,
) -> Result<ModelURI, SchemaError> {
    let raw = if let Some(model) = yaml_default {
        model
    } else if let Some(model) = override_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        model.to_string()
    } else {
        config_default_model
            .map(String::from)
            .ok_or_else(|| SchemaError::Generic(MISSING_DEFAULT_MODEL.to_string()))?
    };
    ModelURI::parse(&raw).map_err(|message| {
        SchemaError::Generic(format!("invalid default model {raw:?}: {message}"))
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::agent::AgentBuilder;
    use crate::builders::artifacts::output;
    use crate::builders::composite::{ParallelBuilder, SequenceBuilder};
    use crate::builders::exec::ExecBuilder;
    use crate::clients::client::ModelURI;
    use crate::definition::GremlinDefinition;
    use crate::stage_spec::parallel::ErrorPolicy;
    use std::fs;

    fn model(spec: &str) -> ModelURI {
        ModelURI::parse(spec).unwrap()
    }

    // ------------------------------------------------------------------
    // to_yaml / from_yaml round-trip symmetry
    // ------------------------------------------------------------------

    /// Helper: serialize a StageSpec to a YAML Mapping and parse it back
    /// through stage_from_yaml, asserting the two are equal.
    fn assert_round_trip(stage: &StageSpec) {
        let settings = config::Config::default();
        let yaml_val = stage.to_yaml();
        let mapping = yaml_val
            .as_mapping()
            .expect("to_yaml must produce a mapping");
        let round_tripped = stage_from_yaml(mapping, Some(&settings), None)
            .expect("stage_from_yaml must accept to_yaml output");
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
            .output("plan", output("artifact://plan.md"))
            .option("model", "xai/grok-4")
            .model(model("xai/grok-4"))
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
            .output("out", output("artifact://out.txt"))
            .option("timeout", "30")
            .model(model("cmd/true"))
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
            .model(model("xai/grok-4"))
            .build()
            .unwrap();
        assert_round_trip(&stage);
    }

    #[test]
    fn sequence_interval_from_string_is_rejected() {
        // String-valued interval must be rejected — only YAML numbers accepted.
        use serde_yaml::Mapping;
        let mut m = Mapping::new();
        m.insert(
            serde_yaml::Value::String("name".into()),
            serde_yaml::Value::String("poll".into()),
        );
        m.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String("sequence".into()),
        );
        m.insert(
            serde_yaml::Value::String("max-iterations".into()),
            serde_yaml::Value::Number(serde_yaml::Number::from(20)),
        );
        m.insert(
            serde_yaml::Value::String("interval".into()),
            serde_yaml::Value::String("60".into()),
        );
        // body: one minimal exec child
        let mut child = Mapping::new();
        child.insert(
            serde_yaml::Value::String("name".into()),
            serde_yaml::Value::String("poll".into()),
        );
        child.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String("exec".into()),
        );
        child.insert(
            serde_yaml::Value::String("options".into()),
            serde_yaml::Value::Mapping({
                let mut opts = Mapping::new();
                opts.insert(
                    serde_yaml::Value::String("cmds".into()),
                    serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("true".into())]),
                );
                opts
            }),
        );
        m.insert(
            serde_yaml::Value::String("body".into()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(child)]),
        );
        // String interval must produce a SchemaError.
        let err = stage_from_yaml(&m, Some(&config::Config::default()), None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("interval"),
            "error must mention 'interval', got: {msg}"
        );
    }

    #[test]
    fn from_expanded_bytes_does_not_load_settings() {
        // Expanded definitions are already concrete and must deserialize even
        // when settings.yaml is malformed / unavailable.
        let _sandbox = crate::test_support::Sandbox::with_config(Some("default-model: [\n"));

        let yaml = "__gremlins_expanded__: true\n\
                     name: resume-me\n\
                     default_model: openai/gpt-4\n\
                     stages: []\n";
        let def = StaticDefinition::from_expanded_bytes(yaml.as_bytes(), None, None).unwrap();
        assert_eq!(def.default_model(), &model("openai/gpt-4"));
    }

    #[test]
    fn sequence_interval_from_number() {
        // YAML float interval must be accepted.
        use serde_yaml::Mapping;
        let mut m = Mapping::new();
        m.insert(
            serde_yaml::Value::String("name".into()),
            serde_yaml::Value::String("poll".into()),
        );
        m.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String("sequence".into()),
        );
        m.insert(
            serde_yaml::Value::String("max-iterations".into()),
            serde_yaml::Value::Number(serde_yaml::Number::from(20)),
        );
        m.insert(
            serde_yaml::Value::String("interval".into()),
            serde_yaml::Value::Number(serde_yaml::Number::from(60)),
        );
        let mut child = Mapping::new();
        child.insert(
            serde_yaml::Value::String("name".into()),
            serde_yaml::Value::String("poll".into()),
        );
        child.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String("exec".into()),
        );
        child.insert(
            serde_yaml::Value::String("options".into()),
            serde_yaml::Value::Mapping({
                let mut opts = Mapping::new();
                opts.insert(
                    serde_yaml::Value::String("cmds".into()),
                    serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("true".into())]),
                );
                opts
            }),
        );
        m.insert(
            serde_yaml::Value::String("body".into()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(child)]),
        );
        let stage = stage_from_yaml(&m, Some(&config::Config::default()), None)
            .expect("numeric interval should parse");
        let yaml = stage.to_yaml();
        let mapping = yaml.as_mapping().unwrap();
        let interval = mapping.get("interval").and_then(|v| v.as_f64());
        assert_eq!(
            interval,
            Some(60.0),
            "numeric interval 60 must round-trip as float 60.0"
        );
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
            .model(model("xai/grok-4"))
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

    #[test]
    fn task_models_unknown_key_is_a_schema_error() {
        use serde_yaml::Mapping;

        // A minimal agent stage carrying a `task-models` map with a
        // non-`TaskKind` key must be rejected at load time.
        for bad_key in ["", "scout", "Scout*"] {
            let mut m = Mapping::new();
            m.insert(
                serde_yaml::Value::String("name".into()),
                serde_yaml::Value::String("plan".into()),
            );
            m.insert(
                serde_yaml::Value::String("type".into()),
                serde_yaml::Value::String("agent".into()),
            );
            m.insert(
                serde_yaml::Value::String("prompt".into()),
                serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("hi".into())]),
            );
            let mut tc = Mapping::new();
            tc.insert(
                serde_yaml::Value::String(bad_key.into()),
                serde_yaml::Value::String("openai/mini".into()),
            );
            m.insert(
                serde_yaml::Value::String("task-models".into()),
                serde_yaml::Value::Mapping(tc),
            );

            let err = stage_from_yaml(&m, Some(&config::Config::default()), None)
                .expect_err("an unknown task-models key must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("unknown task-models key"),
                "error must name the bad key, got: {msg}"
            );
            assert!(
                msg.contains("Scout"),
                "error must list the allowed kinds, got: {msg}"
            );
        }
    }

    #[test]
    fn from_yaml_file_rejects_legacy_top_level_model_keys() {
        let sandbox = crate::test_support::Sandbox::new();
        let project = sandbox.path().join("project");
        let overlay = project.join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();

        for (legacy, replacement) in [
            ("default_client", "default_model"),
            ("default_task_clients", "default_task_models"),
        ] {
            let definition_path = overlay.join(format!("legacy-{legacy}.yaml"));
            fs::write(
                &definition_path,
                format!("{legacy}: openai/gpt-4\nstages: []\n"),
            )
            .unwrap();

            let err = StaticDefinition::from_yaml_file(&definition_path, None, None)
                .expect_err("legacy top-level model keys must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains(legacy) && msg.contains(replacement),
                "error must name {legacy:?} and its replacement {replacement:?}, got: {msg}"
            );
        }
    }

    #[test]
    fn from_yaml_file_rejects_legacy_stage_model_keys() {
        let sandbox = crate::test_support::Sandbox::new();
        let project = sandbox.path().join("project");
        let overlay = project.join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();

        for (legacy, replacement) in [("client", "model"), ("task-clients", "task-models")] {
            let definition_path = overlay.join(format!("legacy-stage-{legacy}.yaml"));
            let value = match legacy {
                "client" => "openai/gpt-4",
                _ => "{Scout: openai/gpt-4}",
            };
            fs::write(
                &definition_path,
                format!(
                    "default_model: openai/gpt-4\nstages:\n  - name: plan\n    type: agent\n    {legacy}: {value}\n"
                ),
            )
            .unwrap();

            let err = StaticDefinition::from_yaml_file(&definition_path, None, None)
                .expect_err("legacy stage model keys must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains(legacy) && msg.contains(replacement),
                "error must name {legacy:?} and its replacement {replacement:?}, got: {msg}"
            );
        }
    }

    #[test]
    fn from_expanded_bytes_rejects_legacy_model_keys() {
        let _sandbox = crate::test_support::Sandbox::new();

        for (kind, yaml) in [
            (
                "default_client",
                "__gremlins_expanded__: true\nname: old\ndefault_client: openai/gpt-4\nstages: []\n",
            ),
            (
                "default_task_clients",
                "__gremlins_expanded__: true\nname: old\ndefault_model: openai/gpt-4\ndefault_task_clients:\n  Scout: openai/gpt-4\nstages: []\n",
            ),
            (
                "stage-client",
                "__gremlins_expanded__: true\nname: old\ndefault_model: openai/gpt-4\nstages:\n  - name: plan\n    type: agent\n    client: openai/gpt-4\n",
            ),
            (
                "stage-task-clients",
                "__gremlins_expanded__: true\nname: old\ndefault_model: openai/gpt-4\nstages:\n  - name: plan\n    type: agent\n    task-clients:\n      Scout: openai/gpt-4\n",
            ),
        ] {
            let err = StaticDefinition::from_expanded_bytes(yaml.as_bytes(), None, None)
                .expect_err("expanded definitions with legacy keys must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("no longer supported"),
                "{kind}: expected a legacy-key rejection, got: {msg}"
            );
        }
    }
}
