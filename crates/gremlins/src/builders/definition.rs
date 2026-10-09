//! Builder for [`StaticDefinition`], plus [`BootstrapBuilder`] and
//! [`LandBuilder`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use indexmap::IndexMap;

use crate::builders::artifacts::{InterpolationValue, OutputTarget};
use crate::definition::r#static::expand::key_referenced_in_text;
use crate::definition::r#static::loader::{self, StageEntry, StageNode};
use crate::definition::r#static::StaticDefinition;
use crate::schemas::bootstrap::{Bootstrap, InputSource, InputSources};
use crate::schemas::error::SchemaError;
use crate::stage_spec::constants::FRAMEWORK_KEYS;
use crate::stage_spec::node::StageSpec;

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
/// let (land, clean_cmds) = LandBuilder::new()
///     .land_cmd("gh pr merge --squash --delete-branch \"{pr_url}\"")
///     .clean_cmd("git worktree remove --force \"$GREMLIN_WORKDIR\" || true")
///     .interpolate("pr_url", content("artifact://pr-url.txt"))
///     .build()
///     .unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct LandBuilder {
    land_cmds: Vec<String>,
    clean_cmds: Vec<String>,
    options: HashMap<String, serde_json::Value>,
    interpolation_map: HashMap<String, String>,
    outputs_map: HashMap<String, String>,
    client: Option<crate::definition::ClientSpec>,
}

impl LandBuilder {
    /// Start building a land stage.
    pub fn new() -> Self {
        LandBuilder {
            land_cmds: Vec::new(),
            clean_cmds: Vec::new(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::new(),
            client: None,
        }
    }

    /// Append a land command.
    pub fn land_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.land_cmds.push(cmd.into());
        self
    }

    /// Append many land commands.
    pub fn land_cmds(mut self, cmds: Vec<String>) -> Self {
        self.land_cmds.extend(cmds);
        self
    }

    /// Append a clean command.
    pub fn clean_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.clean_cmds.push(cmd.into());
        self
    }

    /// Append many clean commands.
    pub fn clean_cmds(mut self, cmds: Vec<String>) -> Self {
        self.clean_cmds.extend(cmds);
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

    /// Add an output entry.
    pub fn output(mut self, key: impl Into<String>, target: impl Into<OutputTarget>) -> Self {
        self.outputs_map.insert(key.into(), target.into().into());
        self
    }

    /// Replace the entire outputs map.
    pub fn outputs_map(mut self, map: HashMap<String, String>) -> Self {
        self.outputs_map = map;
        self
    }

    /// Set an option value.
    pub fn option(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.options.insert(key.into(), value.into());
        self
    }

    /// Set the stage's own client spec.
    pub fn client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(crate::definition::ClientSpec(client.into()));
        self
    }

    /// Consume the builder and produce a [`StageSpec::Exec`] named `land`,
    /// plus the `clean_cmds` vec for storage on [`StaticDefinition`].
    pub fn build(self) -> Result<(StageSpec, Vec<String>), SchemaError> {
        let name = "land".to_string();

        crate::artifacts::resolve::validate_interpolation_map(&self.interpolation_map, &name)
            .map_err(|msg| SchemaError::Stage {
                name: name.clone(),
                msg,
            })?;

        // Populate options["cmds"] from land_cmds so prepare_exec works.
        let mut options = self.options;
        if !self.land_cmds.is_empty() {
            let cmds: Vec<serde_json::Value> = self
                .land_cmds
                .iter()
                .map(|c| serde_json::Value::String(c.clone()))
                .collect();
            options.insert("cmds".to_string(), serde_json::json!(cmds));
        }

        for key in options.keys() {
            if FRAMEWORK_KEYS.contains(key.as_str()) {
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!(
                        "option key {key:?} collides with framework substitution variable"
                    ),
                });
            }
        }

        // --- Collision check: keys in both outputs: and interpolation: ---
        {
            let output_keys: HashSet<String> = self
                .outputs_map
                .keys()
                .filter(|k| !k.contains('{'))
                .map(|k| k.strip_suffix('?').unwrap_or(k).to_string())
                .collect();
            for interp_key in self.interpolation_map.keys() {
                if interp_key.contains('{') {
                    continue;
                }
                if output_keys.contains(interp_key.as_str()) {
                    return Err(SchemaError::Stage {
                        name: name.clone(),
                        msg: format!(
                            "key {interp_key:?} declared in both outputs: and interpolation: — a stage cannot both produce and consume the same key"
                        ),
                    });
                }
            }
        }

        // --- Unused-key check ---
        {
            // Collect all text from land_cmds and clean_cmds
            let mut text = String::new();
            for cmd in &self.land_cmds {
                text.push_str(cmd);
                text.push('\n');
            }
            for cmd in &self.clean_cmds {
                text.push_str(cmd);
                text.push('\n');
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

            // Check output keys
            for key in self.outputs_map.keys() {
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
                        "key {key:?} declared in outputs: is not referenced in any prompt or command"
                    ),
                });
            }
        }

        let clean_cmds = self.clean_cmds;

        let stage = crate::stage_spec::exec::Exec {
            name,
            options,
            interpolation_map: self.interpolation_map,
            outputs_map: self.outputs_map,
        };
        Ok((
            StageSpec::Exec {
                stage,
                client: self.client,
                task_clients: None,
            },
            clean_cmds,
        ))
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

/// Build a [`StaticDefinition`].
///
/// `build()` runs the existing validators (`check_duplicate_producers`,
/// `check_unresolved_consumers`) and returns `Result<StaticDefinition,
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
///             .output("plan", output("artifact://plan.md"))
///             .build()
///             .unwrap(),
///     )
///     .build()
///     .unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct DefinitionBuilder {
    pub(crate) name: String,
    pub(crate) default_client: String,
    pub(crate) prompt_dir: Option<PathBuf>,
    pub(crate) bootstrap: Bootstrap,
    pub(crate) stages: Vec<StageSpec>,
    pub(crate) land: Option<StageSpec>,
    pub(crate) clean_cmds: Vec<String>,
    pub(crate) default_task_clients: Option<IndexMap<String, String>>,
}

impl DefinitionBuilder {
    /// Start building a definition.
    ///
    /// `name` is the definition identity.  `default_client` is required.
    pub fn new(name: impl Into<String>, default_client: impl Into<String>) -> Self {
        DefinitionBuilder {
            name: name.into(),
            default_client: default_client.into(),
            prompt_dir: None,
            bootstrap: Bootstrap::default(),
            stages: Vec::new(),
            land: None,
            clean_cmds: Vec::new(),
            default_task_clients: None,
        }
    }

    /// Set the definition name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
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
    pub fn stage(mut self, stage: StageSpec) -> Self {
        self.stages.push(stage);
        self
    }

    /// Append many stages.
    pub fn stages(mut self, stages: Vec<StageSpec>) -> Self {
        self.stages.extend(stages);
        self
    }

    /// Set the land stage.
    pub fn land(mut self, land: StageSpec) -> Self {
        self.land = Some(land);
        self
    }

    /// Set the clean commands.
    pub fn clean_cmds(mut self, cmds: Vec<String>) -> Self {
        self.clean_cmds = cmds;
        self
    }

    /// Set the global default-task-clients (resolved through profiles).
    pub fn default_task_clients(mut self, dtc: Option<IndexMap<String, String>>) -> Self {
        self.default_task_clients = dtc;
        self
    }

    /// Consume the builder, run validators, and produce a
    /// [`StaticDefinition`].
    pub fn build(mut self) -> Result<StaticDefinition, SchemaError> {
        let name = self.name.clone();

        // Reject blank required fields.
        if self.default_client.is_empty() {
            return Err(SchemaError::Stage {
                name: name.clone(),
                msg: "'default_client' must not be blank".to_string(),
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
        let mut nodes: Vec<StageNode> = self.stages.iter().map(StageSpec::to_stage_node).collect();
        if let Some(ref land) = self.land {
            nodes.push(land.to_stage_node());
        }

        loader::check_duplicate_producers(&nodes, &self.bootstrap.cli_out)?;
        loader::check_unresolved_consumers(
            &nodes,
            &self.bootstrap.launch_cmds,
            &self.bootstrap.cli_out,
        )?;

        let path = self.prompt_dir.unwrap_or_else(|| PathBuf::from("."));

        let mut definition = StaticDefinition::new(
            self.name,
            path,
            self.default_client,
            self.bootstrap,
            self.stages,
            self.land,
            self.clean_cmds,
            serde_yaml::Value::Null,
            self.default_task_clients,
        );

        // Populate expanded_yaml from the typed tree.
        let expanded_yaml = definition.to_expanded_yaml();
        definition.expanded_yaml = expanded_yaml;

        Ok(definition)
    }
}

// ---------------------------------------------------------------------------
// StaticDefinition::from_builder
// ---------------------------------------------------------------------------

impl StaticDefinition {
    /// Construct a [`StaticDefinition`] from a [`DefinitionBuilder`].
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
pub(crate) fn fill_builder_names(stages: &mut [StageSpec]) {
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
            StageSpec::Sequence { body, .. } | StageSpec::Parallel { body, .. } => {
                fill_builder_names(body);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::agent::AgentBuilder;
    use crate::builders::artifacts::{content, output};
    use crate::builders::composite::{ParallelBuilder, SequenceBuilder};
    use crate::builders::exec::ExecBuilder;
    use serde_yaml::Value;

    #[test]
    fn definition_builder_basic() {
        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .stage(
                AgentBuilder::new("plan")
                    .prompt("write the plan to {plan}")
                    .output("plan", output("artifact://plan.md"))
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
                    .output("out", output("artifact://shared.md"))
                    .build()
                    .unwrap(),
            )
            .stage(
                AgentBuilder::new("second")
                    .prompt("two {out}")
                    .output("out", output("artifact://shared.md"))
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
                    .output("plan", output("artifact://plan.md"))
                    .output("pr_url", output("artifact://pr-url.txt"))
                    .build()
                    .unwrap(),
            )
            .land(
                LandBuilder::new()
                    .land_cmd("gh pr merge --squash \"{pr_url}\"")
                    .interpolate("pr_url", content("artifact://pr-url.txt"))
                    .build()
                    .unwrap()
                    .0,
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
        let def = StaticDefinition::from_builder(builder).unwrap();
        assert_eq!(def.name, "demo");
    }

    #[test]
    fn builder_populates_expanded_yaml() {
        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .stage(
                AgentBuilder::new("plan")
                    .prompt("write the plan to {plan}")
                    .output("plan", output("artifact://plan.md"))
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
        for key in ["name", "model", "cwd"] {
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
    fn sequence_builder_rejects_max_iterations_zero() {
        let err = SequenceBuilder::new("test")
            .stage(ExecBuilder::new("cmd").cmd("echo hi").build().unwrap())
            .max_iterations(0)
            .build()
            .unwrap_err();
        assert!(
            err.to_string().contains("max_iterations must be >= 1"),
            "{err}"
        );
    }

    #[test]
    fn sequence_builder_accepts_max_iterations_one() {
        SequenceBuilder::new("test")
            .stage(ExecBuilder::new("cmd").cmd("echo hi").build().unwrap())
            .max_iterations(1)
            .build()
            .unwrap();
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
            .land_cmd("echo hi")
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
            .land_cmd("echo hi")
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
            .land_cmd("cat {foo} {bar}")
            .output("foo", output("artifact://foo.txt"))
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
    fn land_builder_unused_output_key_error() {
        let err = LandBuilder::new()
            .land_cmd("cat {foo}")
            .output("foo", output("artifact://foo.txt"))
            .output("unused", output("artifact://unused.txt"))
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("unused"), "{err}");
        assert!(err.to_string().contains("outputs:"), "{err}");
    }

    #[test]
    fn land_builder_unused_interpolation_key_error() {
        let err = LandBuilder::new()
            .land_cmd("cat {foo}")
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
    fn land_builder_output_interp_collision_error() {
        let err = LandBuilder::new()
            .land_cmd("cat {shared}")
            .output("shared", output("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both outputs:"), "{err}");
    }

    #[test]
    fn land_builder_optional_output_collides_with_interp() {
        let err = LandBuilder::new()
            .land_cmd("cat {shared}")
            .output("shared?", output("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both outputs:"), "{err}");
    }

    #[test]
    fn land_builder_optional_output_referenced_ok() {
        LandBuilder::new()
            .land_cmd("cat {foo}")
            .output("foo?", output("artifact://foo.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn land_builder_hyphen_underscore_normalization_ok() {
        LandBuilder::new()
            .land_cmd("cat {child-plan}")
            .output("child_plan", output("artifact://plan.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn land_builder_framework_template_key_skipped() {
        LandBuilder::new()
            .land_cmd("echo {model}")
            .interpolate(
                "{name}",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://name.txt\")",
                ),
            )
            .build()
            .unwrap();
    }

    // --- LandBuilder clean_cmd / clean_cmds ---

    #[test]
    fn land_builder_clean_cmd() {
        let (stage, clean_cmds) = LandBuilder::new()
            .land_cmd("echo hi")
            .clean_cmd("git worktree remove --force \"$GREMLIN_WORKDIR\" || true")
            .build()
            .unwrap();
        assert_eq!(stage.name(), "land");
        assert_eq!(clean_cmds.len(), 1);
        assert!(clean_cmds[0].contains("git worktree remove"));
    }

    #[test]
    fn land_builder_clean_cmds() {
        let (stage, clean_cmds) = LandBuilder::new()
            .land_cmd("echo hi")
            .clean_cmds(vec![
                "git worktree remove --force \"$GREMLIN_WORKDIR\" || true".to_string(),
                "git worktree prune".to_string(),
            ])
            .build()
            .unwrap();
        assert_eq!(stage.name(), "land");
        assert_eq!(clean_cmds.len(), 2);
    }

    #[test]
    fn land_builder_clean_cmd_only_no_land_cmds() {
        // clean_cmds without land_cmds is valid — the land stage has no
        // commands but clean_cmds are still returned.
        let (stage, clean_cmds) = LandBuilder::new()
            .clean_cmd("git worktree prune")
            .build()
            .unwrap();
        assert_eq!(stage.name(), "land");
        assert_eq!(clean_cmds.len(), 1);
    }

    #[test]
    fn land_builder_unused_key_in_clean_cmds_is_ok() {
        // Keys referenced only in clean_cmds must not be reported as unused.
        LandBuilder::new()
            .land_cmd("echo hi")
            .clean_cmd("cat {clean_input}")
            .interpolate(
                "clean_input",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://clean.txt\")",
                ),
            )
            .build()
            .unwrap();
    }

    #[test]
    fn land_builder_unused_output_in_clean_cmds_is_ok() {
        // Output keys referenced only in clean_cmds must not be reported as unused.
        LandBuilder::new()
            .land_cmd("echo hi")
            .clean_cmd("cat {clean_out}")
            .output("clean_out", output("artifact://clean-out.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn land_builder_truly_unused_key_still_errors() {
        // A key that is in neither land_cmds nor clean_cmds must still error.
        let err = LandBuilder::new()
            .land_cmd("echo hi")
            .clean_cmd("echo bye")
            .interpolate(
                "unused",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://unused.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("unused"), "{err}");
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
