//! The resolved `GremlinDefinition` — an expanded gremlin definition YAML
//! deserialized to typed data.
//!
//! This is the output of [`DefinitionBuilder::from_yaml`] (or
//! [`DefinitionBuilder::from_expanded_yaml`]), which handles expansion,
//! validation, and resolution. The struct is a plain data holder: identity,
//! stage tree, bootstrap, and the serialized expanded YAML tree for
//! round-tripping.
//!
//! [`DefinitionBuilder::from_yaml`]: crate::builders::DefinitionBuilder::from_yaml
//! [`DefinitionBuilder::from_expanded_yaml`]: crate::builders::DefinitionBuilder::from_expanded_yaml

use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::config;
use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;

use crate::stages::node::RunnableStage;

/// The message emitted when no layer supplied a default client.
const MISSING_DEFAULT_CLIENT: &str = "gremlin definition is missing 'default_client' — set a \
     'default_client' in the definition YAML, pass --client on the command line, or set \
     'default-client' in config.json";

/// A gremlin definition resolved from an expanded YAML file.
#[derive(Debug, Clone)]
pub struct GremlinDefinition {
    /// The definition's identity: the YAML file stem.
    pub name: String,
    /// Where the definition was loaded from, canonicalised where possible.
    pub path: PathBuf,
    /// Every stage runs with this client unless it declares its own.
    pub default_client: String,
    /// The git ref the worktree branches from.
    pub base_ref: String,
    /// Bootstrap commands and input sources.
    pub bootstrap: Bootstrap,
    /// The stage tree, in declaration order.
    pub stages: Vec<RunnableStage>,
    /// The optional `land` stage — always an exec stage named `land`.
    pub land: Option<RunnableStage>,
    /// The fully expanded YAML tree, kept for round-tripping via
    /// [`to_expanded_yaml`](GremlinDefinition::to_expanded_yaml).
    pub expanded_yaml: Value,
}

/// The name a not-yet-loaded definition carries. [`Gremlin::init_runtime`]
/// treats it as "nothing loaded yet", so it must never be a real definition's
/// name — the builder derives that from the YAML file stem.
pub const UNLOADED_NAME: &str = "unknown";

impl GremlinDefinition {
    /// A placeholder carrying no identity: the value a [`Gremlin`] holds until
    /// [`Gremlin::init_runtime`] reads the real YAML.
    ///
    /// [`Gremlin`]: crate::executor::gremlin::Gremlin
    /// [`Gremlin::init_runtime`]: crate::executor::gremlin::Gremlin::init_runtime
    pub fn stub() -> GremlinDefinition {
        GremlinDefinition {
            name: UNLOADED_NAME.to_string(),
            path: PathBuf::from("."),
            default_client: String::new(),
            base_ref: String::new(),
            bootstrap: Bootstrap::default(),
            stages: Vec::new(),
            land: None,
            expanded_yaml: Value::Null,
        }
    }

    /// Whether this definition is the [`GremlinDefinition::stub`] rather than a loaded one.
    pub fn is_stub(&self) -> bool {
        self.name.is_empty() || self.name == UNLOADED_NAME
    }

    /// Clone this definition, replacing its stage list with `stages`.
    ///
    /// Used by the parallel executor to give each child a definition that
    /// contains only the child's own stage(s), while inheriting every other
    /// field (name, path, default_client, base_ref, bootstrap, land) from
    /// the parent.
    pub fn clone_with_stages(&self, stages: Vec<RunnableStage>) -> Self {
        GremlinDefinition {
            stages,
            ..self.clone()
        }
    }

    /// Serialize this definition to a [`serde_yaml::Value`] matching the
    /// canonical expanded-YAML shape.
    ///
    /// The output always includes `__gremlins_expanded__: true` so
    /// [`DefinitionBuilder::from_expanded_yaml`] recognizes it.
    ///
    /// [`DefinitionBuilder::from_expanded_yaml`]: crate::builders::DefinitionBuilder::from_expanded_yaml
    pub fn to_expanded_yaml(&self) -> Value {
        let mut root = Mapping::new();

        // Sentinel — always emitted.
        root.insert(
            Value::String("__gremlins_expanded__".to_string()),
            Value::Bool(true),
        );

        // name — always present, so round-trips preserve definition identity.
        root.insert(
            Value::String("name".to_string()),
            Value::String(self.name.clone()),
        );

        // default_client — always present.
        root.insert(
            Value::String("default_client".to_string()),
            Value::String(self.default_client.clone()),
        );

        // base_ref — omit if "current".
        if self.base_ref != "current" {
            root.insert(
                Value::String("base_ref".to_string()),
                Value::String(self.base_ref.clone()),
            );
        }

        // bootstrap — omit entirely if all fields are default/empty.
        let bootstrap_yaml = bootstrap_to_yaml(&self.bootstrap);
        if !is_empty_mapping(&bootstrap_yaml) {
            root.insert(Value::String("bootstrap".to_string()), bootstrap_yaml);
        }

        // land — omit if None.
        if let Some(ref land) = self.land {
            let mut land_val = land.to_yaml();
            // Ensure name and type are forced to "land"/"exec" for round-trip
            // safety, matching what the builder does on parse.
            if let Value::Mapping(ref mut land_map) = land_val {
                land_map.insert(
                    Value::String("name".to_string()),
                    Value::String("land".to_string()),
                );
                land_map.insert(
                    Value::String("type".to_string()),
                    Value::String("exec".to_string()),
                );
            }
            root.insert(Value::String("land".to_string()), land_val);
        }

        // stages
        let stages: Vec<Value> = self.stages.iter().map(RunnableStage::to_yaml).collect();
        root.insert(Value::String("stages".to_string()), Value::Sequence(stages));

        Value::Mapping(root)
    }
}

/// The project root: the parent of the nearest ancestor `.gremlins` directory,
/// falling back to the definition's own directory.
pub(crate) fn project_root_for(path: &Path) -> PathBuf {
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

pub(crate) fn default_client_from_yaml(root: &Mapping) -> Result<Option<String>, SchemaError> {
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

pub(crate) fn base_ref_from_yaml(root: &Mapping) -> Result<String, SchemaError> {
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

pub(crate) fn stages_from_yaml(root: &Mapping) -> Result<Vec<Value>, SchemaError> {
    match root.get("stages") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(stages)) => Ok(stages.clone()),
        Some(_) => Err(SchemaError::Generic("'stages' must be a list".to_string())),
    }
}

pub(crate) fn resolve_default_client(
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
// Serialization helpers for to_expanded_yaml()
// ---------------------------------------------------------------------------

fn is_empty_mapping(value: &Value) -> bool {
    match value {
        Value::Mapping(m) => m.is_empty(),
        _ => false,
    }
}

fn bootstrap_to_yaml(bootstrap: &Bootstrap) -> Value {
    let mut m = Mapping::new();

    if let Some(ref source) = bootstrap.source {
        let mut src_map = Mapping::with_capacity(source.sources.len());
        for (name, input_src) in &source.sources {
            let mut entry = Mapping::new();
            if input_src.types.len() == 1 {
                entry.insert(
                    Value::String("type".to_string()),
                    Value::String(input_src.types[0].clone()),
                );
            } else {
                let types: Vec<Value> = input_src
                    .types
                    .iter()
                    .map(|t| Value::String(t.clone()))
                    .collect();
                entry.insert(Value::String("type".to_string()), Value::Sequence(types));
            }
            if input_src.optional {
                entry.insert(Value::String("optional".to_string()), Value::Bool(true));
            }
            src_map.insert(Value::String(name.clone()), Value::Mapping(entry));
        }
        m.insert(Value::String("source".to_string()), Value::Mapping(src_map));
    }

    if !bootstrap.launch_cmds.is_empty() {
        let cmds: Vec<Value> = bootstrap
            .launch_cmds
            .iter()
            .map(|c| Value::String(c.clone()))
            .collect();
        m.insert(
            Value::String("launch_cmds".to_string()),
            Value::Sequence(cmds),
        );
    }

    if !bootstrap.cmds.is_empty() {
        let cmds: Vec<Value> = bootstrap
            .cmds
            .iter()
            .map(|c| Value::String(c.clone()))
            .collect();
        m.insert(Value::String("cmds".to_string()), Value::Sequence(cmds));
    }

    if !bootstrap.cli_out.is_empty() {
        let mut cli = Mapping::with_capacity(bootstrap.cli_out.len());
        for (k, v) in &bootstrap.cli_out {
            cli.insert(Value::String(k.clone()), Value::String(v.clone()));
        }
        m.insert(Value::String("cli_out".to_string()), Value::Mapping(cli));
    }

    if !bootstrap.env.is_empty() {
        m.insert(
            Value::String("env".to_string()),
            Value::String(bootstrap.env.clone()),
        );
    }

    Value::Mapping(m)
}
