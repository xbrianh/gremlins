//! Builder for [`GremlinDefinition`], plus [`BootstrapBuilder`] and
//! [`LandBuilder`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::builders::agent::AgentBuilder;
use crate::builders::artifacts::{BindTarget, InterpolationValue};
use crate::builders::composite::{LoopBuilder, ParallelBuilder, SequenceBuilder};
use crate::builders::exec::ExecBuilder;
use crate::schemas::bootstrap::{Bootstrap, InputSource, InputSources};
use crate::schemas::error::SchemaError;
use crate::schemas::expand;
use crate::schemas::expand::key_referenced_in_text;
use crate::schemas::gremlin_definition::{
    base_ref_from_yaml, default_client_from_yaml, project_root_for, resolve_default_client,
    stages_from_yaml, GremlinDefinition,
};
use crate::schemas::loader::{self, StageEntry, StageNode};
use crate::stages::composite::ClientSpec;
use crate::stages::constants::FRAMEWORK_KEYS;
use crate::stages::node::RunnableStage;
use crate::stages::parallel::ErrorPolicy;

// ---------------------------------------------------------------------------
// BootstrapBuilder
// ---------------------------------------------------------------------------

/// Build a [`Bootstrap`] value.
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let bootstrap = BootstrapBuilder::new()
///     .launch_cmd("gremlins:bind_artifact(\"artifact://plan.md\", plan)")
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct BootstrapBuilder {
    source: HashMap<String, InputSource>,
    launch_cmds: Vec<String>,
    cmds: Vec<String>,
    cli_out: HashMap<String, String>,
    env: String,
}

impl BootstrapBuilder {
    /// Start with default (empty) bootstrap.
    pub fn new() -> Self {
        BootstrapBuilder::default()
    }

    /// Append a launch command.
    pub fn launch_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.launch_cmds.push(cmd.into());
        self
    }

    /// Replace all launch commands.
    pub fn launch_cmds(mut self, cmds: Vec<String>) -> Self {
        self.launch_cmds = cmds;
        self
    }

    /// Append a bootstrap command.
    pub fn cmd(mut self, cmd: impl Into<String>) -> Self {
        self.cmds.push(cmd.into());
        self
    }

    /// Replace all bootstrap commands.
    pub fn cmds(mut self, cmds: Vec<String>) -> Self {
        self.cmds = cmds;
        self
    }

    /// Add a CLI output binding.
    pub fn cli_out(mut self, key: impl Into<String>, uri: impl Into<String>) -> Self {
        self.cli_out.insert(key.into(), uri.into());
        self
    }

    /// Replace all CLI outputs.
    pub fn cli_out_map(mut self, map: HashMap<String, String>) -> Self {
        self.cli_out = map;
        self
    }

    /// Set the bootstrap environment script.
    pub fn env(mut self, env: impl Into<String>) -> Self {
        self.env = env.into();
        self
    }

    /// Add a single input source.
    ///
    /// Returns an error if the type list is empty or contains an unknown
    /// type (validated by [`InputSource::new`]).
    pub fn source(
        mut self,
        name: impl Into<String>,
        types: &[impl ToString],
        optional: bool,
    ) -> Result<Self, SchemaError> {
        let name = name.into();
        let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        let src = InputSource::new(name.clone(), types, optional)?;
        self.source.insert(name, src);
        Ok(self)
    }

    /// Replace the entire source map with pre-validated [`InputSource`]
    /// values.
    pub fn sources(mut self, map: HashMap<String, InputSource>) -> Self {
        self.source = map;
        self
    }

    /// Consume the builder and produce a [`Bootstrap`].
    pub fn build(self) -> Bootstrap {
        let source = if self.source.is_empty() {
            None
        } else {
            Some(InputSources::new(self.source))
        };
        Bootstrap {
            source,
            launch_cmds: self.launch_cmds,
            cmds: self.cmds,
            cli_out: self.cli_out,
            env: self.env,
        }
    }
}

impl From<BootstrapBuilder> for Bootstrap {
    fn from(b: BootstrapBuilder) -> Self {
        b.build()
    }
}

// ---------------------------------------------------------------------------
// LandBuilder
// ---------------------------------------------------------------------------

/// Build the `land` stage — always an exec stage named `land`.
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let land = LandBuilder::new()
///     .cmd("gh pr merge --squash --delete-branch \"{pr_url}\"")
///     .interpolate("pr_url", content("artifact://pr-url.txt"))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct LandBuilder {
    options: HashMap<String, serde_json::Value>,
    interpolation_map: HashMap<String, String>,
    bind_map: HashMap<String, String>,
    skip_if_exists: String,
    client: Option<crate::stages::composite::ClientSpec>,
}

impl LandBuilder {
    /// Start building a land stage.
    pub fn new() -> Self {
        LandBuilder {
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
            skip_if_exists: String::new(),
            client: None,
        }
    }

    /// Append a command.
    pub fn cmd(mut self, cmd: impl Into<String>) -> Self {
        let cmds = self
            .options
            .entry("cmds".to_string())
            .or_insert_with(|| serde_json::json!([]));
        if let Some(arr) = cmds.as_array_mut() {
            arr.push(serde_json::Value::String(cmd.into()));
        }
        self
    }

    /// Append many commands.
    pub fn cmds(mut self, cmds: Vec<String>) -> Self {
        let arr = self
            .options
            .entry("cmds".to_string())
            .or_insert_with(|| serde_json::json!([]));
        if let Some(existing) = arr.as_array_mut() {
            for c in cmds {
                existing.push(serde_json::Value::String(c));
            }
        }
        self
    }

    /// Set the timeout in seconds.
    pub fn timeout(mut self, seconds: f64) -> Self {
        self.options
            .insert("timeout".to_string(), serde_json::json!(seconds));
        self
    }

    /// Add an interpolation entry.
    pub fn interpolate(
        mut self,
        key: impl Into<String>,
        value: impl Into<InterpolationValue>,
    ) -> Self {
        self.interpolation_map
            .insert(key.into(), value.into().into());
        self
    }

    /// Replace the entire interpolation map.
    pub fn interpolation_map(mut self, map: HashMap<String, String>) -> Self {
        self.interpolation_map = map;
        self
    }

    /// Add a bind entry.
    pub fn bind(mut self, key: impl Into<String>, target: impl Into<BindTarget>) -> Self {
        self.bind_map.insert(key.into(), target.into().into());
        self
    }

    /// Replace the entire bind map.
    pub fn bind_map(mut self, map: HashMap<String, String>) -> Self {
        self.bind_map = map;
        self
    }

    /// Set an option value.
    pub fn option(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.options.insert(key.into(), value.into());
        self
    }

    /// Set the `skip_if_exists` artifact guard.
    pub fn skip_if_exists(mut self, uri: impl Into<String>) -> Self {
        self.skip_if_exists = uri.into();
        self
    }

    /// Set the stage's own client spec.
    pub fn client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(crate::stages::composite::ClientSpec(client.into()));
        self
    }

    /// Consume the builder and produce a [`RunnableStage::Exec`] named `land`.
    pub fn build(self) -> Result<RunnableStage, SchemaError> {
        let name = "land".to_string();

        crate::artifacts::resolve::validate_interpolation_map(&self.interpolation_map, &name)
            .map_err(|msg| SchemaError::Stage {
                name: name.clone(),
                msg,
            })?;

        for key in self.options.keys() {
            if FRAMEWORK_KEYS.contains(key.as_str()) {
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!(
                        "option key {key:?} collides with framework substitution variable"
                    ),
                });
            }
        }

        // --- Collision check: keys in both bind: and interpolation: ---
        {
            let bind_keys: HashSet<String> = self
                .bind_map
                .keys()
                .filter(|k| !k.contains('{'))
                .map(|k| k.strip_suffix('?').unwrap_or(k).to_string())
                .collect();
            for interp_key in self.interpolation_map.keys() {
                if interp_key.contains('{') {
                    continue;
                }
                if bind_keys.contains(interp_key.as_str()) {
                    return Err(SchemaError::Stage {
                        name: name.clone(),
                        msg: format!(
                            "key {interp_key:?} declared in both bind: and interpolation: — a stage cannot both produce and consume the same key"
                        ),
                    });
                }
            }
        }

        // --- Unused-key check ---
        {
            // Collect all text from cmds
            let mut text = String::new();
            if let Some(cmds) = self.options.get("cmds").and_then(|v| v.as_array()) {
                for cmd in cmds {
                    if let Some(s) = cmd.as_str() {
                        text.push_str(s);
                        text.push('\n');
                    }
                }
            }

            // Check interpolation keys
            for key in self.interpolation_map.keys() {
                if key.contains('{') {
                    continue;
                }
                if !key_referenced_in_text(key, &text) {
                    return Err(SchemaError::Stage {
                        name: name.clone(),
                        msg: format!(
                            "key {key:?} declared in interpolation: is not referenced in any prompt or command"
                        ),
                    });
                }
            }

            // Check bind keys
            for key in self.bind_map.keys() {
                if key.contains('{') {
                    continue;
                }
                let stripped = key.strip_suffix('?').unwrap_or(key);
                if key_referenced_in_text(stripped, &text) {
                    continue;
                }
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!(
                        "key {key:?} declared in bind: is not referenced in any prompt or command"
                    ),
                });
            }
        }

        let stage = crate::stages::exec::Exec {
            name,
            options: self.options,
            interpolation_map: self.interpolation_map,
            bind_map: self.bind_map,
        };
        Ok(RunnableStage::Exec {
            stage,
            skip_if_exists: self.skip_if_exists,
            client: self.client,
        })
    }
}

impl Default for LandBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// DefinitionBuilder
// ---------------------------------------------------------------------------

/// Build a [`GremlinDefinition`].
///
/// `build()` runs the existing validators (`check_duplicate_producers`,
/// `check_unresolved_consumers`) and returns `Result<GremlinDefinition,
/// SchemaError>`.
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let def = DefinitionBuilder::new("demo", "xai:grok-4")
///     .stage(
///         AgentBuilder::new("plan")
///             .prompt("write the plan to {plan}")
///             .bind("plan", artifact("artifact://plan.md"))
///             .build()
///             .unwrap(),
///     )
///     .build()
///     .unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct DefinitionBuilder {
    name: String,
    base_ref: String,
    default_client: String,
    prompt_dir: Option<PathBuf>,
    bootstrap: Bootstrap,
    stages: Vec<RunnableStage>,
    land: Option<RunnableStage>,
}

impl DefinitionBuilder {
    /// Start building a definition.
    ///
    /// `name` is the definition identity.  `default_client` is required.
    pub fn new(name: impl Into<String>, default_client: impl Into<String>) -> Self {
        DefinitionBuilder {
            name: name.into(),
            base_ref: "current".to_string(),
            default_client: default_client.into(),
            prompt_dir: None,
            bootstrap: Bootstrap::default(),
            stages: Vec::new(),
            land: None,
        }
    }

    /// Set the definition name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set the base git ref.
    pub fn base_ref(mut self, base_ref: impl Into<String>) -> Self {
        self.base_ref = base_ref.into();
        self
    }

    /// Set the default client.
    pub fn default_client(mut self, client: impl Into<String>) -> Self {
        self.default_client = client.into();
        self
    }

    /// Set the prompt directory.
    pub fn prompt_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.prompt_dir = Some(dir.into());
        self
    }

    /// Set the bootstrap.
    pub fn bootstrap(mut self, bootstrap: Bootstrap) -> Self {
        self.bootstrap = bootstrap;
        self
    }

    /// Append a stage.
    pub fn stage(mut self, stage: RunnableStage) -> Self {
        self.stages.push(stage);
        self
    }

    /// Append many stages.
    pub fn stages(mut self, stages: Vec<RunnableStage>) -> Self {
        self.stages.extend(stages);
        self
    }

    /// Set the land stage.
    pub fn land(mut self, land: RunnableStage) -> Self {
        self.land = Some(land);
        self
    }

    /// Consume the builder, run validators, and produce a
    /// [`GremlinDefinition`].
    pub fn build(mut self) -> Result<GremlinDefinition, SchemaError> {
        let name = self.name.clone();

        // Reject blank required fields.
        if self.default_client.is_empty() {
            return Err(SchemaError::Stage {
                name: name.clone(),
                msg: "'default_client' must not be blank".to_string(),
            });
        }
        if self.base_ref.is_empty() {
            return Err(SchemaError::Stage {
                name: name.clone(),
                msg: "'base_ref' must not be blank".to_string(),
            });
        }

        // Validate land stage: must be an exec stage named "land".
        if let Some(ref land) = self.land {
            if land.stage_type() != "exec" {
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!(
                        "land stage must be an exec stage, got {}",
                        land.stage_type()
                    ),
                });
            }
            if land.name() != "land" {
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!("land stage must be named 'land', got {:?}", land.name()),
                });
            }
        }

        // Run name-filling pass first (same as the YAML path).
        fill_builder_names(&mut self.stages);

        // Build the node list for validation, including land.
        let mut nodes: Vec<StageNode> = self
            .stages
            .iter()
            .map(RunnableStage::to_stage_node)
            .collect();
        if let Some(ref land) = self.land {
            nodes.push(land.to_stage_node());
        }

        loader::check_duplicate_producers(&nodes, &self.bootstrap.cli_out)?;
        loader::check_unresolved_consumers(
            &nodes,
            &self.bootstrap.launch_cmds,
            &self.bootstrap.cli_out,
        )?;

        let definition = GremlinDefinition {
            name: self.name,
            path: self.prompt_dir.unwrap_or_else(|| PathBuf::from(".")),
            default_client: self.default_client,
            base_ref: self.base_ref,
            bootstrap: self.bootstrap,
            stages: self.stages,
            land: self.land,
            expanded_yaml: serde_yaml::Value::Null,
        };

        // Populate expanded_yaml from the typed tree.
        let expanded_yaml = definition.to_expanded_yaml();

        Ok(GremlinDefinition {
            expanded_yaml,
            ..definition
        })
    }
}

// ---------------------------------------------------------------------------
// GremlinDefinition::from_builder
// ---------------------------------------------------------------------------

impl GremlinDefinition {
    /// Construct a [`GremlinDefinition`] from a [`DefinitionBuilder`].
    ///
    /// This is the entry point called by [`DefinitionBuilder::build`].
    pub fn from_builder(builder: DefinitionBuilder) -> Result<Self, SchemaError> {
        builder.build()
    }
}

/// Run the name-filling pass over a flat list of stages, recursing into
/// composite bodies so every unnamed stage at every depth gets a name.
///
/// Mirrors the YAML path: unnamed stages get auto-generated names based on
/// their stage type, and duplicate explicit names are disambiguated with
/// `-N` suffixes.
pub(crate) fn fill_builder_names(stages: &mut [RunnableStage]) {
    let mut entries: Vec<StageEntry> = stages.iter().map(|s| s.to_stage_entry()).collect();
    // fill_names is infallible for well-formed stages.
    if loader::fill_names(&mut entries).is_ok() {
        for (stage, entry) in stages.iter_mut().zip(&entries) {
            if let Some(name) = &entry.name {
                stage.set_name(name.clone());
            }
        }
    }

    // Recurse into composite bodies.
    for stage in stages.iter_mut() {
        match stage {
            RunnableStage::Loop { body, .. }
            | RunnableStage::Sequence { body, .. }
            | RunnableStage::Parallel { body, .. } => {
                fill_builder_names(body);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// DefinitionBuilder::from_yaml — YAML ingestion through builders
// ---------------------------------------------------------------------------

impl DefinitionBuilder {
    /// Load a definition from an expanded YAML file, routing every stage
    /// through the typed builder constructors so builder-level validation
    /// fires.
    ///
    /// `default_client_override` is the CLI `--client` value; consulted only
    /// when the YAML declares none.
    pub fn from_yaml(
        path: impl AsRef<Path>,
        default_client_override: Option<&str>,
    ) -> Result<GremlinDefinition, SchemaError> {
        let path = path.as_ref();
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path.exists() {
            return Err(SchemaError::DefinitionFileNotFound {
                path: path.display().to_string(),
            });
        }

        let project_root = project_root_for(&path);
        let expanded = expand::parse_definition_file(&path, &project_root)?;

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
        let mut stages: Vec<RunnableStage> = Vec::new();
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
}

// ---------------------------------------------------------------------------
// Per-stage YAML → builder conversion
// ---------------------------------------------------------------------------

/// Dispatch a single stage mapping to the appropriate per-type builder.
fn stage_from_yaml(mapping: &Mapping) -> Result<RunnableStage, SchemaError> {
    let is_parallel = mapping.contains_key("parallel");
    let name = mapping
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let stage_type = if is_parallel {
        "parallel"
    } else {
        mapping.get("type").and_then(Value::as_str).unwrap_or("")
    };

    match stage_type {
        "agent" => agent_from_yaml(mapping, &name),
        "exec" => exec_from_yaml(mapping, &name),
        "loop" => loop_from_yaml(mapping, &name),
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
fn agent_from_yaml(mapping: &Mapping, name: &str) -> Result<RunnableStage, SchemaError> {
    let prompts = yaml_string_list(mapping, "prompt")?;
    let interpolation_map = yaml_string_map(mapping, "interpolation")?;
    let bind_map = yaml_string_map(mapping, "bind")?;
    let options = yaml_options(mapping)?;
    let skip_if_exists = yaml_skip_if_exists(mapping);
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
    if !skip_if_exists.is_empty() {
        builder = builder.skip_if_exists(skip_if_exists);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

/// Build an [`ExecBuilder`] from a YAML stage mapping.
fn exec_from_yaml(mapping: &Mapping, name: &str) -> Result<RunnableStage, SchemaError> {
    let interpolation_map = yaml_string_map(mapping, "interpolation")?;
    let bind_map = yaml_string_map(mapping, "bind")?;
    let options = yaml_options(mapping)?;
    let skip_if_exists = yaml_skip_if_exists(mapping);
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
    if !skip_if_exists.is_empty() {
        builder = builder.skip_if_exists(skip_if_exists);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

/// Build a [`LoopBuilder`] from a YAML stage mapping.
fn loop_from_yaml(mapping: &Mapping, name: &str) -> Result<RunnableStage, SchemaError> {
    let max_iterations = match mapping.get("max-iterations").filter(|v| !v.is_null()) {
        None => 3u32,
        Some(v) => {
            // Try as integer first, then as string.
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
    let stop_when_exists = yaml_str(mapping, "stop_when_exists");
    let skip_if_exists = yaml_skip_if_exists(mapping);
    let client = yaml_client(mapping);

    // Interval from options.interval.
    let interval = mapping
        .get("options")
        .and_then(|v| v.get("interval"))
        .and_then(|v| v.as_f64());

    // Parse children.
    let body = yaml_children(mapping, "body")?;

    let mut builder = LoopBuilder::new(name)
        .max_iterations(max_iterations)
        .stages(body);
    if let Some(uri) = stop_when_exists {
        builder = builder.stop_when_exists(uri);
    }
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

/// Build a [`SequenceBuilder`] from a YAML stage mapping.
fn sequence_from_yaml(mapping: &Mapping, name: &str) -> Result<RunnableStage, SchemaError> {
    let skip_if_exists = yaml_skip_if_exists(mapping);
    let client = yaml_client(mapping);
    let body = yaml_children(mapping, "body")?;

    let mut builder = SequenceBuilder::new(name).stages(body);
    if !skip_if_exists.is_empty() {
        builder = builder.skip_if_exists(skip_if_exists);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

/// Build a [`ParallelBuilder`] from a YAML stage mapping.
fn parallel_from_yaml(mapping: &Mapping, name: &str) -> Result<RunnableStage, SchemaError> {
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

    // Parallel children live under the `parallel` key, not `body`.
    let body = yaml_children(mapping, "parallel")?;

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

/// Parse children from a composite's `key` ("body" or "parallel") through
/// the same per-type dispatch.
fn yaml_children(mapping: &Mapping, key: &str) -> Result<Vec<RunnableStage>, SchemaError> {
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
fn land_from_yaml_builder(mapping: &Mapping) -> Result<RunnableStage, SchemaError> {
    let interpolation_map = yaml_string_map(mapping, "interpolation")?;
    let bind_map = yaml_string_map(mapping, "bind")?;
    let options = yaml_options(mapping)?;
    let skip_if_exists = yaml_skip_if_exists(mapping);
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
    if !skip_if_exists.is_empty() {
        builder = builder.skip_if_exists(skip_if_exists);
    }
    if let Some(c) = client {
        builder = builder.client(c.0);
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::agent::AgentBuilder;
    use crate::builders::artifacts::{artifact, content};
    use crate::builders::composite::{LoopBuilder, ParallelBuilder, SequenceBuilder};
    use crate::builders::exec::ExecBuilder;
    use serde_yaml::Value;

    #[test]
    fn definition_builder_basic() {
        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .base_ref("main")
            .stage(
                AgentBuilder::new("plan")
                    .prompt("write the plan to {plan}")
                    .bind("plan", artifact("artifact://plan.md"))
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("run")
                    .cmd("cat {plan}")
                    .interpolate("plan", content("artifact://plan.md"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        assert_eq!(def.name, "demo");
        assert_eq!(def.default_client, "xai:grok-4");
        assert_eq!(def.base_ref, "main");
        assert_eq!(def.stages.len(), 2);
        assert_eq!(def.stages[0].name(), "plan");
        assert_eq!(def.stages[0].stage_type(), "agent");
        assert_eq!(def.stages[1].name(), "run");
        assert_eq!(def.stages[1].stage_type(), "exec");
        assert!(def.land.is_none());
    }

    #[test]
    fn definition_builder_rejects_duplicate_producers() {
        let err = DefinitionBuilder::new("demo", "xai:grok-4")
            .stage(
                AgentBuilder::new("first")
                    .prompt("one {out}")
                    .bind("out", artifact("artifact://shared.md"))
                    .build()
                    .unwrap(),
            )
            .stage(
                AgentBuilder::new("second")
                    .prompt("two {out}")
                    .bind("out", artifact("artifact://shared.md"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap_err();

        assert!(
            err.to_string().contains("duplicate artifact producer"),
            "{err}"
        );
    }

    #[test]
    fn definition_builder_rejects_unresolved_consumers() {
        let err = DefinitionBuilder::new("demo", "xai:grok-4")
            .stage(
                ExecBuilder::new("consumer")
                    .cmd("cat {missing}")
                    .interpolate("missing", content("artifact://never-produced.md"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap_err();

        assert!(
            err.to_string().contains("artifact://never-produced.md"),
            "{err}"
        );
    }

    #[test]
    fn definition_builder_with_land() {
        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .stage(
                AgentBuilder::new("plan")
                    .prompt("write {plan} and create {pr_url}")
                    .bind("plan", artifact("artifact://plan.md"))
                    .bind("pr_url", artifact("artifact://pr-url.txt"))
                    .build()
                    .unwrap(),
            )
            .land(
                LandBuilder::new()
                    .cmd("gh pr merge --squash \"{pr_url}\"")
                    .interpolate("pr_url", content("artifact://pr-url.txt"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let land = def.land.as_ref().unwrap();
        assert_eq!(land.name(), "land");
        assert_eq!(land.stage_type(), "exec");
    }

    #[test]
    fn definition_builder_with_bootstrap() {
        let bootstrap = BootstrapBuilder::new()
            .launch_cmd("gremlins:bind_artifact(\"artifact://plan.md\", plan)")
            .build();

        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .bootstrap(bootstrap)
            .stage(
                ExecBuilder::new("consumer")
                    .cmd("cat {plan}")
                    .interpolate("plan", content("artifact://plan.md"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        assert_eq!(def.bootstrap.launch_cmds.len(), 1);
    }

    #[test]
    fn from_builder_entry_point() {
        let builder = DefinitionBuilder::new("demo", "xai:grok-4");
        let def = GremlinDefinition::from_builder(builder).unwrap();
        assert_eq!(def.name, "demo");
    }

    #[test]
    fn builder_populates_expanded_yaml() {
        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .stage(
                AgentBuilder::new("plan")
                    .prompt("write the plan to {plan}")
                    .bind("plan", artifact("artifact://plan.md"))
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("run")
                    .cmd("cat {plan}")
                    .interpolate("plan", content("artifact://plan.md"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        // expanded_yaml must be populated, not Null.
        assert!(
            !def.expanded_yaml.is_null(),
            "expanded_yaml must be populated by the builder"
        );

        // It must be a mapping with the sentinel.
        let mapping = def.expanded_yaml.as_mapping().unwrap();
        assert_eq!(
            mapping.get(Value::String("__gremlins_expanded__".to_string())),
            Some(&Value::Bool(true))
        );

        // It must contain the stages.
        let stages = mapping
            .get(Value::String("stages".to_string()))
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(stages.len(), 2);
    }

    // ---- Per-stage builder validation tests ----

    #[test]
    fn agent_builder_rejects_content_question_before_paren() {
        let err = AgentBuilder::new("test")
            .prompt("hi")
            .interpolate(
                "out",
                crate::builders::artifacts::InterpolationValue::from(
                    r#"content?("artifact://x.txt")"#,
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("content?(...)"), "{err}");
    }

    #[test]
    fn agent_builder_rejects_framework_option_key() {
        let err = AgentBuilder::new("test")
            .prompt("hi")
            .option("cwd", "/tmp")
            .build()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("collides with framework substitution variable"),
            "{err}"
        );
    }

    #[test]
    fn agent_builder_allows_model_option_key() {
        // Agent exempts "model" from framework-key collision.
        AgentBuilder::new("test")
            .prompt("hi")
            .option("model", "openai:gpt-5")
            .build()
            .unwrap();
    }

    #[test]
    fn exec_builder_rejects_content_question_before_paren() {
        let err = ExecBuilder::new("test")
            .cmd("echo hi")
            .interpolate(
                "out",
                crate::builders::artifacts::InterpolationValue::from(
                    r#"content?("artifact://x.txt")"#,
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("content?(...)"), "{err}");
    }

    #[test]
    fn exec_builder_rejects_framework_option_key() {
        for key in ["name", "model", "cwd", "base_ref"] {
            let err = ExecBuilder::new("test")
                .cmd("echo hi")
                .option(key, "x")
                .build()
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("collides with framework substitution variable"),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn loop_builder_rejects_max_iterations_zero() {
        let err = LoopBuilder::new("test")
            .max_iterations(0)
            .build()
            .unwrap_err();
        assert!(
            err.to_string().contains("max_iterations must be >= 1"),
            "{err}"
        );
    }

    #[test]
    fn loop_builder_accepts_max_iterations_one() {
        LoopBuilder::new("test").max_iterations(1).build().unwrap();
    }

    #[test]
    fn sequence_builder_rejects_empty_body() {
        let err = SequenceBuilder::new("test").build().unwrap_err();
        assert!(
            err.to_string().contains("'body' must not be empty"),
            "{err}"
        );
    }

    #[test]
    fn parallel_builder_rejects_nested_parallel() {
        let inner = ParallelBuilder::new("inner")
            .stage(ExecBuilder::new("cmd").cmd("echo hi").build().unwrap())
            .build()
            .unwrap();

        let err = ParallelBuilder::new("outer")
            .stage(inner)
            .build()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("nested parallel groups are not allowed"),
            "{err}"
        );
    }

    #[test]
    fn parallel_builder_disambiguates_duplicate_child_names() {
        // Name-filling runs before child-name validation, so duplicates
        // are auto-disambiguated (matching the YAML path).
        let group = ParallelBuilder::new("group")
            .stage(ExecBuilder::new("shard").cmd("echo hi").build().unwrap())
            .stage(ExecBuilder::new("shard").cmd("echo hi").build().unwrap())
            .build()
            .unwrap();
        let body = group.body();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0].name(), "shard");
        assert_eq!(body[1].name(), "shard-2");
    }

    #[test]
    fn parallel_builder_rejects_invalid_child_name() {
        let err = ParallelBuilder::new("group")
            .stage(ExecBuilder::new("bad/name").cmd("echo hi").build().unwrap())
            .build()
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid characters for child_id"),
            "{err}"
        );
    }

    #[test]
    fn land_builder_rejects_framework_option_key() {
        let err = LandBuilder::new()
            .cmd("echo hi")
            .option("cwd", "/tmp")
            .build()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("collides with framework substitution variable"),
            "{err}"
        );
    }

    #[test]
    fn land_builder_rejects_content_question_before_paren() {
        let err = LandBuilder::new()
            .cmd("echo hi")
            .interpolate(
                "out",
                crate::builders::artifacts::InterpolationValue::from(
                    r#"content?("artifact://x.txt")"#,
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("content?(...)"), "{err}");
    }

    // --- LandBuilder stage-key validation ---

    #[test]
    fn land_builder_all_keys_referenced_ok() {
        LandBuilder::new()
            .cmd("cat {foo} {bar}")
            .bind("foo", artifact("artifact://foo.txt"))
            .interpolate(
                "bar",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://bar.txt\")",
                ),
            )
            .build()
            .unwrap();
    }

    #[test]
    fn land_builder_unused_bind_key_error() {
        let err = LandBuilder::new()
            .cmd("cat {foo}")
            .bind("foo", artifact("artifact://foo.txt"))
            .bind("unused", artifact("artifact://unused.txt"))
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("unused"), "{err}");
        assert!(err.to_string().contains("bind:"), "{err}");
    }

    #[test]
    fn land_builder_unused_interpolation_key_error() {
        let err = LandBuilder::new()
            .cmd("cat {foo}")
            .interpolate(
                "foo",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://foo.txt\")",
                ),
            )
            .interpolate(
                "unused",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://unused.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("unused"), "{err}");
        assert!(err.to_string().contains("interpolation:"), "{err}");
    }

    #[test]
    fn land_builder_bind_interp_collision_error() {
        let err = LandBuilder::new()
            .cmd("cat {shared}")
            .bind("shared", artifact("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both bind:"), "{err}");
    }

    #[test]
    fn land_builder_optional_bind_collides_with_interp() {
        let err = LandBuilder::new()
            .cmd("cat {shared}")
            .bind("shared?", artifact("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both bind:"), "{err}");
    }

    #[test]
    fn land_builder_optional_bind_referenced_ok() {
        LandBuilder::new()
            .cmd("cat {foo}")
            .bind("foo?", artifact("artifact://foo.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn land_builder_hyphen_underscore_normalization_ok() {
        LandBuilder::new()
            .cmd("cat {child-plan}")
            .bind("child_plan", artifact("artifact://plan.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn land_builder_framework_template_key_skipped() {
        LandBuilder::new()
            .cmd("echo {model}")
            .interpolate(
                "{name}",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://name.txt\")",
                ),
            )
            .build()
            .unwrap();
    }

    #[test]
    fn fill_builder_names_recurses_into_composites() {
        // Build a sequence with unnamed children — name-filling should
        // assign names at every depth.
        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .stage(
                SequenceBuilder::new("workflow")
                    .stage(ExecBuilder::new("").cmd("echo one").build().unwrap())
                    .stage(ExecBuilder::new("").cmd("echo two").build().unwrap())
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let seq = &def.stages[0];
        assert_eq!(seq.name(), "workflow");
        assert_eq!(seq.stage_type(), "sequence");
        let body = seq.body();
        assert_eq!(body.len(), 2);
        // Both unnamed exec children should get auto-filled names.
        assert_eq!(body[0].name(), "exec");
        assert_eq!(body[1].name(), "exec-2");
    }
}
