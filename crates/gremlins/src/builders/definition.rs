//! Builder for [`GremlinDefinition`], plus [`BootstrapBuilder`] and
//! [`LandBuilder`].

use std::collections::HashMap;
use std::path::PathBuf;

use crate::builders::artifacts::{BindTarget, InterpolationValue};
use crate::schemas::bootstrap::{Bootstrap, InputSource, InputSources};
use crate::schemas::error::SchemaError;
use crate::schemas::gremlin_definition::GremlinDefinition;
use crate::schemas::loader::{self, StageEntry, StageNode};
use crate::stages::node::RunnableStage;

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
    pub fn build(self) -> RunnableStage {
        let stage = crate::stages::exec::Exec {
            name: "land".to_string(),
            options: self.options,
            interpolation_map: self.interpolation_map,
            bind_map: self.bind_map,
        };
        RunnableStage::Exec {
            stage,
            skip_if_exists: self.skip_if_exists,
            client: self.client,
        }
    }
}

impl Default for LandBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl From<LandBuilder> for RunnableStage {
    fn from(b: LandBuilder) -> Self {
        b.build()
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
///             .build(),
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

        Ok(GremlinDefinition {
            name: self.name,
            path: self.prompt_dir.unwrap_or_else(|| PathBuf::from(".")),
            default_client: self.default_client,
            base_ref: self.base_ref,
            bootstrap: self.bootstrap,
            stages: self.stages,
            land: self.land,
            expanded_yaml: serde_yaml::Value::Null,
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

/// Run the name-filling pass over a flat list of stages (non-recursive).
///
/// Mirrors the YAML path: unnamed stages get auto-generated names based on
/// their stage type, and duplicate explicit names are disambiguated with
/// `-N` suffixes.
fn fill_builder_names(stages: &mut [RunnableStage]) {
    let mut entries: Vec<StageEntry> = stages.iter().map(|s| s.to_stage_entry()).collect();
    // fill_names is infallible for well-formed stages.
    if loader::fill_names(&mut entries).is_ok() {
        for (stage, entry) in stages.iter_mut().zip(&entries) {
            if let Some(name) = &entry.name {
                stage.set_name(name.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::agent::AgentBuilder;
    use crate::builders::artifacts::{artifact, content};
    use crate::builders::exec::ExecBuilder;

    #[test]
    fn definition_builder_basic() {
        let def = DefinitionBuilder::new("demo", "xai:grok-4")
            .base_ref("main")
            .stage(
                AgentBuilder::new("plan")
                    .prompt("write the plan to {plan}")
                    .bind("plan", artifact("artifact://plan.md"))
                    .build(),
            )
            .stage(
                ExecBuilder::new("run")
                    .cmd("cat {plan}")
                    .interpolate("plan", content("artifact://plan.md"))
                    .build(),
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
                    .build(),
            )
            .stage(
                AgentBuilder::new("second")
                    .prompt("two {out}")
                    .bind("out", artifact("artifact://shared.md"))
                    .build(),
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
                    .build(),
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
                    .prompt("write {plan}")
                    .bind("plan", artifact("artifact://plan.md"))
                    .bind("pr_url", artifact("artifact://pr-url.txt"))
                    .build(),
            )
            .land(
                LandBuilder::new()
                    .cmd("gh pr merge --squash \"{pr_url}\"")
                    .interpolate("pr_url", content("artifact://pr-url.txt"))
                    .build(),
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
                    .build(),
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
}
