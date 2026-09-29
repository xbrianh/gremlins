//! The canonical [`GremlinDefinition`] implementation — a cursor-driven
//! definition that owns its data directly.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_yaml::{Mapping, Value};

use crate::schemas::bootstrap::Bootstrap;
use crate::stage_spec::node::StageSpec;

use super::{DefinitionError, ExecutorStage, GremlinDefinition, Sequence, UNLOADED_NAME};

pub(crate) mod expand;
pub(crate) mod loader;
pub(crate) mod prompts;
pub(crate) mod resolve;
pub(crate) mod yaml;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// StaticDefinition
// ---------------------------------------------------------------------------

/// A [`GremlinDefinition`] trait implementation that owns the definition
/// data directly.
///
/// All accessors read the struct fields. `next_stage()` walks the top-level
/// stage list one [`StageSpec`] at a time, converting each into an
/// [`ExecutorStage`] via a pure recursive projection.
#[derive(Debug, Clone)]
pub struct StaticDefinition {
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
    pub(crate) stages: Vec<StageSpec>,
    /// The optional `land` stage — always an exec stage named `land`.
    pub(crate) land: Option<StageSpec>,
    /// The fully expanded YAML tree, kept for round-tripping via
    /// [`to_expanded_yaml`](StaticDefinition::to_expanded_yaml).
    pub(crate) expanded_yaml: Value,
    cursor: usize,
}

impl StaticDefinition {
    /// Create a new cursor-driven definition starting at position 0.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        path: PathBuf,
        default_client: String,
        base_ref: String,
        bootstrap: Bootstrap,
        stages: Vec<StageSpec>,
        land: Option<StageSpec>,
        expanded_yaml: Value,
    ) -> Self {
        StaticDefinition {
            name,
            path,
            default_client,
            base_ref,
            bootstrap,
            stages,
            land,
            expanded_yaml,
            cursor: 0,
        }
    }

    /// A placeholder carrying no identity: the value a [`Gremlin`] holds until
    /// [`Gremlin::init_runtime`] reads the real YAML.
    ///
    /// [`Gremlin`]: crate::executor::gremlin::Gremlin
    /// [`Gremlin::init_runtime`]: crate::executor::gremlin::Gremlin::init_runtime
    pub fn stub() -> StaticDefinition {
        StaticDefinition {
            name: UNLOADED_NAME.to_string(),
            path: PathBuf::from("."),
            default_client: String::new(),
            base_ref: String::new(),
            bootstrap: Bootstrap::default(),
            stages: Vec::new(),
            land: None,
            expanded_yaml: Value::Null,
            cursor: 0,
        }
    }

    /// Whether this definition is the [`StaticDefinition::stub`] rather than a loaded one.
    pub fn is_stub(&self) -> bool {
        self.name.is_empty() || self.name == UNLOADED_NAME
    }

    /// Clone this definition, replacing its stage list with `stages`.
    ///
    /// Used by the parallel executor to give each child a definition that
    /// contains only the child's own stage(s), while inheriting every other
    /// field (name, path, default_client, base_ref, bootstrap) from the
    /// parent. `land` is cleared so parallel children never duplicate the
    /// parent's land side effects.
    pub(crate) fn clone_with_stages(&self, stages: Vec<StageSpec>) -> Self {
        StaticDefinition {
            stages,
            cursor: 0,
            land: None,
            ..self.clone()
        }
    }

    /// Serialize this definition to a [`serde_yaml::Value`] matching the
    /// canonical expanded-YAML shape.
    ///
    /// The output always includes `__gremlins_expanded__: true` so
    /// [`StaticDefinition::from_yaml_file`] recognizes it.
    pub(crate) fn to_expanded_yaml(&self) -> Value {
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
        let stages: Vec<Value> = self.stages.iter().map(StageSpec::to_yaml).collect();
        root.insert(Value::String("stages".to_string()), Value::Sequence(stages));

        Value::Mapping(root)
    }

    /// Deserialize a definition from bytes, returning an owned
    /// [`StaticDefinition`] so callers can call [`goto`](Self::goto) and
    /// extract fields before handing it to the executor.
    pub fn deserialize_owned(data: &[u8]) -> Result<Self, DefinitionError> {
        Self::from_expanded_bytes(data, None).map_err(|e| DefinitionError::Message(e.to_string()))
    }

    // -----------------------------------------------------------------------
    // Stage conversion
    // -----------------------------------------------------------------------

    /// Recursively convert one [`StageSpec`] into an [`ExecutorStage`].
    pub(crate) fn convert_stage(&self, stage: StageSpec) -> ExecutorStage {
        match stage {
            StageSpec::Agent { stage, client } => ExecutorStage::Agent { stage, client },
            StageSpec::Exec { stage, client } => ExecutorStage::Exec { stage, client },
            StageSpec::Sequence {
                attrs,
                max_iterations,
                interval,
                client: seq_client,
                body,
            } => {
                let stages: Vec<ExecutorStage> =
                    body.into_iter().map(|s| self.convert_stage(s)).collect();
                ExecutorStage::Sequence(Sequence {
                    name: attrs.name,
                    stages,
                    scope: None,
                    skip_if_exists: attrs.skip_if_exists,
                    client: seq_client,
                    max_iterations,
                    interval,
                })
            }
            StageSpec::Parallel {
                attrs,
                max_concurrent,
                cancel_on_error,
                error_policy,
                client,
                body,
            } => {
                let children: Vec<Box<dyn GremlinDefinition>> = body
                    .into_iter()
                    .map(|child| {
                        Box::new(self.clone_with_stages(vec![child])) as Box<dyn GremlinDefinition>
                    })
                    .collect();
                ExecutorStage::Parallel {
                    name: attrs.name,
                    max_concurrent,
                    cancel_on_error,
                    error_policy,
                    client,
                    children,
                    skip_if_exists: attrs.skip_if_exists,
                }
            }
        }
    }
}

#[async_trait]
impl GremlinDefinition for StaticDefinition {
    fn name(&self) -> &str {
        &self.name
    }

    fn default_client(&self) -> &str {
        &self.default_client
    }

    fn base_ref(&self) -> &str {
        &self.base_ref
    }

    fn bootstrap(&self) -> &Bootstrap {
        &self.bootstrap
    }

    fn land(&self) -> Option<ExecutorStage> {
        self.land.clone().map(|stage| self.convert_stage(stage))
    }

    fn is_at_start(&self) -> bool {
        self.cursor == 0
    }

    fn is_stub(&self) -> bool {
        self.name.is_empty() || self.name == UNLOADED_NAME
    }

    fn with_client(&mut self, client: &str) {
        self.default_client = client.to_string();
    }

    fn clone_box(&self) -> Box<dyn GremlinDefinition> {
        Box::new(self.clone())
    }

    fn first_stage_name(&self) -> &str {
        self.stages.first().map(|s| s.name()).unwrap_or(&self.name)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn goto(&mut self, stage: &str) {
        if let Some(pos) = self.stages.iter().position(|s| s.name() == stage) {
            self.cursor = pos;
        } else {
            self.cursor = 0;
        }
    }

    async fn next_stage(&mut self) -> Result<ExecutorStage, DefinitionError> {
        if self.cursor >= self.stages.len() {
            return Ok(ExecutorStage::Done);
        }
        let stage = self.stages[self.cursor].clone();
        self.cursor += 1;
        Ok(self.convert_stage(stage))
    }

    fn serialize(&self) -> Result<Vec<u8>, DefinitionError> {
        let yaml = self.to_expanded_yaml();
        serde_yaml::to_string(&yaml)
            .map(|s| s.into_bytes())
            .map_err(|e| DefinitionError::Message(format!("failed to serialize definition: {e}")))
    }

    fn deserialize(data: &[u8]) -> Result<Box<dyn GremlinDefinition>, DefinitionError>
    where
        Self: Sized,
    {
        StaticDefinition::deserialize_owned(data)
            .map(|sd| Box::new(sd) as Box<dyn GremlinDefinition>)
    }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::ClientSpec;
    use crate::stage_spec::agent::Agent;
    use crate::stage_spec::composite::StageAttrs;
    use crate::stage_spec::exec::Exec;
    use crate::stage_spec::parallel::ErrorPolicy;

    fn stub_definition() -> StaticDefinition {
        StaticDefinition::stub()
    }

    #[test]
    fn static_definition_delegates_name() {
        let def = stub_definition();
        // stub name is "unknown"
        assert_eq!(def.name(), "unknown");
    }

    #[test]
    fn static_definition_delegates_default_client() {
        let def = stub_definition();
        assert_eq!(def.default_client(), "");
    }

    #[test]
    fn static_definition_delegates_base_ref() {
        let def = stub_definition();
        assert_eq!(def.base_ref(), "");
    }

    #[test]
    fn static_definition_delegates_bootstrap() {
        let def = stub_definition();
        let bs = def.bootstrap();
        assert!(bs.source.is_none());
        assert!(bs.launch_cmds.is_empty());
    }

    #[test]
    fn static_definition_delegates_land_none() {
        let def = stub_definition();
        assert!(def.land().is_none());
    }

    #[test]
    fn static_definition_land_returns_some_when_populated() {
        let def = StaticDefinition {
            name: "test-gremlin".into(),
            path: "/tmp/test.yaml".into(),
            default_client: "openai:gpt-4".into(),
            base_ref: "main".into(),
            bootstrap: Bootstrap::default(),
            stages: vec![],
            land: Some(parsed_exec("land")),
            expanded_yaml: serde_yaml::Value::Null,
            cursor: 0,
        };
        let land = def.land().expect("land is populated");
        assert_eq!(land.name(), "land");
        assert_eq!(land.stage_type(), "exec");
    }

    #[test]
    fn boxed_definition_delegates_land_some() {
        let def = StaticDefinition {
            name: "test-gremlin".into(),
            path: "/tmp/test.yaml".into(),
            default_client: "openai:gpt-4".into(),
            base_ref: "main".into(),
            bootstrap: Bootstrap::default(),
            stages: vec![],
            land: Some(parsed_exec("land")),
            expanded_yaml: serde_yaml::Value::Null,
            cursor: 0,
        };
        let boxed: Box<dyn GremlinDefinition> = Box::new(def);
        let land = boxed.land().expect("land is populated");
        assert_eq!(land.name(), "land");
        assert_eq!(land.stage_type(), "exec");
    }

    #[tokio::test]
    async fn static_definition_next_stage_returns_done() {
        let mut def = stub_definition();
        let result = def.next_stage().await.unwrap();
        assert!(matches!(&result, ExecutorStage::Done));
        assert_eq!(result.stage_type(), "done");
    }

    #[test]
    fn static_definition_with_real_definition() {
        // Build a minimal but real StaticDefinition to exercise delegation
        // beyond the stub.
        let def = StaticDefinition {
            name: "test-gremlin".into(),
            path: "/tmp/test.yaml".into(),
            default_client: "openai:gpt-4".into(),
            base_ref: "main".into(),
            bootstrap: Bootstrap::default(),
            stages: vec![],
            land: None,
            expanded_yaml: serde_yaml::Value::Null,
            cursor: 0,
        };
        assert_eq!(def.name(), "test-gremlin");
        assert_eq!(def.default_client(), "openai:gpt-4");
        assert_eq!(def.base_ref(), "main");
        assert!(def.land().is_none());
    }

    // ---- ExecutorStage method tests ----

    fn make_agent(name: &str) -> ExecutorStage {
        ExecutorStage::Agent {
            stage: Agent {
                name: name.to_string(),
                prompts: vec![],
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            client: None,
        }
    }

    fn make_exec(name: &str) -> ExecutorStage {
        ExecutorStage::Exec {
            stage: Exec {
                name: name.to_string(),
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            client: None,
        }
    }

    #[test]
    fn executor_stage_agent_name() {
        let stage = make_agent("plan");
        assert_eq!(stage.name(), "plan");
        assert_eq!(stage.stage_type(), "agent");
        assert!(stage.client().is_none());
        assert_eq!(stage.skip_if_exists(), "");
    }

    #[test]
    fn executor_stage_exec_name() {
        let stage = make_exec("build");
        assert_eq!(stage.name(), "build");
        assert_eq!(stage.stage_type(), "exec");
        assert!(stage.client().is_none());
        assert_eq!(stage.skip_if_exists(), "");
    }

    #[test]
    fn executor_stage_sequence_name_returns_name_field() {
        let seq = ExecutorStage::Sequence(Sequence {
            name: "my-sequence".into(),
            stages: vec![make_agent("first"), make_exec("second")],
            scope: None,
            skip_if_exists: String::new(),
            client: None,
            max_iterations: 1,
            interval: None,
        });
        assert_eq!(seq.name(), "my-sequence");
        assert_eq!(seq.stage_type(), "sequence");
        assert!(seq.client().is_none());
    }

    #[test]
    fn executor_stage_sequence_name_empty_returns_empty() {
        let seq = ExecutorStage::Sequence(Sequence {
            name: String::new(),
            stages: vec![],
            scope: None,
            skip_if_exists: String::new(),
            client: None,
            max_iterations: 1,
            interval: None,
        });
        assert_eq!(seq.name(), "");
    }

    #[test]
    fn executor_stage_parallel_name() {
        let stage = ExecutorStage::Parallel {
            name: "reviews".into(),
            max_concurrent: None,
            cancel_on_error: false,
            error_policy: ErrorPolicy::Any,
            client: None,
            children: vec![],
            skip_if_exists: "artifact://reviews".into(),
        };
        assert_eq!(stage.name(), "reviews");
        assert_eq!(stage.stage_type(), "parallel");
        assert!(stage.client().is_none());
        assert_eq!(stage.skip_if_exists(), "artifact://reviews");
    }

    #[test]
    fn executor_stage_done() {
        assert_eq!(ExecutorStage::Done.name(), "");
        assert_eq!(ExecutorStage::Done.stage_type(), "done");
        assert!(ExecutorStage::Done.client().is_none());
        assert_eq!(ExecutorStage::Done.skip_if_exists(), "");
    }

    #[test]
    fn executor_stage_client_some() {
        let stage = ExecutorStage::Agent {
            stage: Agent {
                name: "plan".into(),
                prompts: vec![],
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            client: Some(ClientSpec("xai:grok-5".into())),
        };
        assert_eq!(stage.client(), Some(&ClientSpec("xai:grok-5".into())));
    }

    #[test]
    fn executor_stage_skip_if_exists_on_sequence() {
        let seq = ExecutorStage::Sequence(Sequence {
            name: String::new(),
            stages: vec![],
            scope: None,
            skip_if_exists: "artifact://guard".into(),
            client: None,
            max_iterations: 1,
            interval: None,
        });
        assert_eq!(seq.skip_if_exists(), "artifact://guard");
    }

    #[test]
    fn executor_stage_skip_if_exists_on_agent_is_always_empty() {
        let stage = make_agent("plan");
        assert_eq!(stage.skip_if_exists(), "");
    }

    #[test]
    fn executor_stage_skip_if_exists_on_exec_is_always_empty() {
        let stage = make_exec("build");
        assert_eq!(stage.skip_if_exists(), "");
    }

    // ---- StaticDefinition cursor / goto / next_stage tests ----

    /// Build a minimal Agent StageSpec for use in test definitions.
    fn parsed_agent(name: &str) -> StageSpec {
        StageSpec::Agent {
            stage: Agent {
                name: name.to_string(),
                prompts: vec![],
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            client: None,
        }
    }

    /// Build a minimal Exec StageSpec.
    fn parsed_exec(name: &str) -> StageSpec {
        StageSpec::Exec {
            stage: Exec {
                name: name.to_string(),
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            client: None,
        }
    }

    /// Build a multi-stage StaticDefinition from StageSpec entries.
    fn definition_with(stages: Vec<StageSpec>) -> StaticDefinition {
        StaticDefinition {
            name: "test-def".into(),
            path: "/tmp/test.yaml".into(),
            default_client: "openai:gpt-4".into(),
            base_ref: "main".into(),
            bootstrap: Bootstrap::default(),
            stages,
            land: None,
            expanded_yaml: serde_yaml::Value::Null,
            cursor: 0,
        }
    }

    #[tokio::test]
    async fn next_stage_yields_top_level_stages_in_order() {
        let def = definition_with(vec![
            parsed_agent("first"),
            parsed_exec("second"),
            parsed_agent("third"),
        ]);
        let mut sd = def;

        let s1 = sd.next_stage().await.unwrap();
        assert_eq!(s1.name(), "first");
        assert_eq!(s1.stage_type(), "agent");

        let s2 = sd.next_stage().await.unwrap();
        assert_eq!(s2.name(), "second");
        assert_eq!(s2.stage_type(), "exec");

        let s3 = sd.next_stage().await.unwrap();
        assert_eq!(s3.name(), "third");
        assert_eq!(s3.stage_type(), "agent");

        // Exhausted — Done forever.
        for _ in 0..3 {
            let s = sd.next_stage().await.unwrap();
            assert!(matches!(s, ExecutorStage::Done));
        }
    }

    #[tokio::test]
    async fn goto_jumps_to_existing_stage() {
        let def = definition_with(vec![
            parsed_agent("alpha"),
            parsed_agent("beta"),
            parsed_agent("gamma"),
        ]);
        let mut sd = def;

        sd.goto("gamma");
        let s = sd.next_stage().await.unwrap();
        assert_eq!(s.name(), "gamma");

        // After exhausting gamma, we get Done.
        let s = sd.next_stage().await.unwrap();
        assert!(matches!(s, ExecutorStage::Done));
    }

    #[tokio::test]
    async fn goto_unknown_resets_to_zero() {
        let def = definition_with(vec![parsed_agent("alpha"), parsed_agent("beta")]);
        let mut sd = def;

        // Advance past alpha.
        let _ = sd.next_stage().await.unwrap();
        assert_eq!(sd.next_stage().await.unwrap().name(), "beta");

        // Unknown name resets cursor to 0.
        sd.goto("no-such-stage");
        let s = sd.next_stage().await.unwrap();
        assert_eq!(s.name(), "alpha");
    }

    #[tokio::test]
    async fn boxed_goto_forwards_to_inner() {
        let def = definition_with(vec![parsed_agent("x"), parsed_agent("y")]);
        let mut bx: Box<dyn GremlinDefinition> = Box::new(def);

        bx.goto("y");
        let s = bx.next_stage().await.unwrap();
        assert_eq!(s.name(), "y");
    }

    // ---- convert_stage variant tests ----

    #[tokio::test]
    async fn convert_stage_agent() {
        let def = definition_with(vec![parsed_agent("plan")]);
        let mut sd = def;
        let s = sd.next_stage().await.unwrap();
        assert!(matches!(s, ExecutorStage::Agent { .. }));
        assert_eq!(s.name(), "plan");
        assert_eq!(s.stage_type(), "agent");
    }

    #[tokio::test]
    async fn convert_stage_sequence() {
        let seq = StageSpec::Sequence {
            attrs: StageAttrs {
                name: "outer".into(),
                skip_if_exists: "artifact://guard".into(),
                ..StageAttrs::new("outer".into())
            },
            max_iterations: 1,
            interval: None,
            client: None,
            body: vec![parsed_agent("inner-a"), parsed_exec("inner-b")],
        };
        let def = definition_with(vec![seq]);
        let mut sd = def;
        let s = sd.next_stage().await.unwrap();
        assert_eq!(s.name(), "outer");
        assert_eq!(s.stage_type(), "sequence");
        assert_eq!(s.skip_if_exists(), "artifact://guard");
        match s {
            ExecutorStage::Sequence(seq) => {
                assert_eq!(seq.stages.len(), 2);
                assert_eq!(seq.stages[0].name(), "inner-a");
                assert_eq!(seq.stages[1].name(), "inner-b");
                assert!(seq.scope.is_none());
            }
            _ => panic!("expected Sequence"),
        }
    }

    #[tokio::test]
    async fn convert_stage_sequence_with_max_iterations() {
        // A Sequence with max_iterations > 1 stays an ExecutorStage::Sequence.
        let seq = StageSpec::Sequence {
            attrs: StageAttrs {
                name: "retry".into(),
                skip_if_exists: "artifact://retry-guard".into(),
                ..StageAttrs::new("retry".into())
            },
            max_iterations: 5,
            interval: Some(20.0),
            client: Some(ClientSpec("xai:grok".into())),
            body: vec![parsed_agent("loop-child")],
        };
        let def = definition_with(vec![seq]);
        let mut sd = def;
        let s = sd.next_stage().await.unwrap();
        assert_eq!(s.name(), "retry");
        assert_eq!(s.stage_type(), "sequence");
        assert_eq!(s.skip_if_exists(), "artifact://retry-guard");
        match s {
            ExecutorStage::Sequence(seq) => {
                assert_eq!(seq.max_iterations, 5);
                assert_eq!(seq.interval, Some(20.0));
                assert_eq!(seq.client, Some(ClientSpec("xai:grok".into())));
                assert_eq!(seq.stages.len(), 1);
                assert_eq!(seq.stages[0].name(), "loop-child");
                assert!(seq.scope.is_none());
                assert_eq!(seq.skip_if_exists, "artifact://retry-guard");
            }
            _ => panic!("expected Sequence"),
        }
    }

    #[tokio::test]
    async fn convert_stage_parallel_children_inherit_metadata() {
        let par = StageSpec::Parallel {
            attrs: StageAttrs::new("reviews".into()),
            max_concurrent: Some(4),
            cancel_on_error: true,
            error_policy: ErrorPolicy::All,
            client: Some(ClientSpec("openai:gpt-5".into())),
            body: vec![parsed_agent("rev-a"), parsed_agent("rev-b")],
        };
        let def = definition_with(vec![par]);
        let mut sd = def;
        let s = sd.next_stage().await.unwrap();
        assert_eq!(s.name(), "reviews");
        assert_eq!(s.stage_type(), "parallel");
        match s {
            ExecutorStage::Parallel {
                max_concurrent,
                cancel_on_error,
                error_policy,
                client,
                children,
                ..
            } => {
                assert_eq!(max_concurrent, Some(4));
                assert!(cancel_on_error);
                assert_eq!(error_policy, ErrorPolicy::All);
                assert_eq!(client, Some(ClientSpec("openai:gpt-5".into())));
                assert_eq!(children.len(), 2);
                // Each child is a StaticDefinition that inherits parent metadata
                // (name, default_client, base_ref, bootstrap).
                for child in &children {
                    assert_eq!(child.name(), "test-def");
                    assert_eq!(child.default_client(), "openai:gpt-4");
                    assert_eq!(child.base_ref(), "main");
                }
            }
            _ => panic!("expected Parallel"),
        }
    }

    #[tokio::test]
    async fn parallel_child_yields_its_own_stage_then_done() {
        let par = StageSpec::Parallel {
            attrs: StageAttrs::new("group".into()),
            max_concurrent: None,
            cancel_on_error: false,
            error_policy: ErrorPolicy::Any,
            client: None,
            body: vec![parsed_agent("sole-child")],
        };
        let def = definition_with(vec![par]);
        let mut sd = def;
        let s = sd.next_stage().await.unwrap();
        let children = match s {
            ExecutorStage::Parallel { children, .. } => children,
            _ => panic!("expected Parallel"),
        };
        assert_eq!(children.len(), 1);
        let mut child = children.into_iter().next().unwrap();
        let cs = child.next_stage().await.unwrap();
        assert_eq!(cs.name(), "sole-child");
        assert!(matches!(
            child.next_stage().await.unwrap(),
            ExecutorStage::Done
        ));
    }

    #[tokio::test]
    async fn boxed_definition_delegates_name() {
        let def: Box<dyn GremlinDefinition> = Box::new(stub_definition());
        assert_eq!(def.name(), "unknown");
    }

    #[tokio::test]
    async fn boxed_definition_delegates_land() {
        let def: Box<dyn GremlinDefinition> = Box::new(stub_definition());
        assert!(def.land().is_none());
    }

    #[tokio::test]
    async fn boxed_definition_delegates_next_stage() {
        let mut def: Box<dyn GremlinDefinition> = Box::new(stub_definition());
        let result = def.next_stage().await.unwrap();
        assert!(matches!(result, ExecutorStage::Done));
    }

    #[tokio::test]
    async fn boxed_definition_serialize_roundtrips() {
        // Build a real definition with stages, bootstrap, and land so the
        // round-trip validates more than just scalar metadata.
        let def = definition_with(vec![
            parsed_agent("greet"),
            parsed_exec("build"),
            parsed_agent("farewell"),
        ]);
        let def: Box<dyn GremlinDefinition> = Box::new(def);
        let bytes = def.serialize().unwrap();
        assert!(!bytes.is_empty());
        // Round-trip: deserialize and verify it's a valid definition.
        let mut deserialized = StaticDefinition::deserialize(&bytes).unwrap();
        assert_eq!(deserialized.name(), "test-def");
        assert_eq!(deserialized.default_client(), "openai:gpt-4");
        assert_eq!(deserialized.base_ref(), "main");
        // Stage traversal: all three stages must survive the round-trip.
        let mut stage_names: Vec<String> = Vec::new();
        loop {
            match deserialized.next_stage().await.unwrap() {
                ExecutorStage::Agent { stage, .. } => stage_names.push(stage.name),
                ExecutorStage::Exec { stage, .. } => stage_names.push(stage.name),
                ExecutorStage::Done => break,
                _ => {}
            }
        }
        assert_eq!(stage_names, vec!["greet", "build", "farewell"]);
    }

    // ---- DefinitionError tests ----

    #[test]
    fn definition_error_registry_variant() {
        let err = DefinitionError::Registry("boom".into());
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn definition_error_message_variant() {
        let err = DefinitionError::Message("nope".into());
        assert_eq!(err.to_string(), "nope");
    }
}
