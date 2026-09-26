//! The `GremlinStageProvider` trait — the abstraction boundary between the
//! executor run-loop and concrete definition sources (static YAML, Python
//! plugins, etc.).
//!
//! This module defines the trait and the `ExecutorStage` enum the trait
//! returns.  `StaticDefinition` is a cursor-driven implementation that wraps
//! the concrete [`GremlinDefinition`] and converts one top-level
//! [`ParsedStage`] into an [`ExecutorStage`] per `next_stage()` call.

use async_trait::async_trait;
use thiserror::Error;

use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
use crate::schemas::gremlin_definition::GremlinDefinition;
use crate::stages::agent::Agent;
use crate::stages::composite::ClientSpec;
use crate::stages::exec::Exec;
use crate::stages::node::ParsedStage;
use crate::stages::parallel::ErrorPolicy;

// ---------------------------------------------------------------------------
// Sequence — shared payload for Sequence and Loop variants
// ---------------------------------------------------------------------------

/// A sequence of stages with an optional scope and artifact guard.
///
/// Used as the payload of [`ExecutorStage::Sequence`] and as the body of
/// [`ExecutorStage::Loop`].
pub struct Sequence {
    pub name: String,
    pub stages: Vec<ExecutorStage>,
    pub scope: Option<String>,
    pub skip_if_exists: String,
}

// ---------------------------------------------------------------------------
// ExecutorStage — what next_stage() returns
// ---------------------------------------------------------------------------

/// The next stage (or stages) the executor should run.
///
/// Converted from [`ParsedStage`] by [`convert_stage`].
pub enum ExecutorStage {
    /// Run an agent stage.
    Agent {
        stage: Agent,
        skip_if_exists: String,
        client: Option<ClientSpec>,
    },
    /// Run an exec stage.
    Exec {
        stage: Exec,
        skip_if_exists: String,
        client: Option<ClientSpec>,
    },
    /// Run a sequence of stages in order.
    Sequence(Sequence),
    /// Run children in parallel.
    Parallel {
        name: String,
        max_concurrent: Option<u32>,
        cancel_on_error: bool,
        error_policy: ErrorPolicy,
        client: Option<ClientSpec>,
        children: Vec<Box<dyn GremlinStageProvider>>,
        skip_if_exists: String,
    },
    /// Run a body repeatedly.
    Loop {
        name: String,
        max_iterations: Option<u32>,
        stop_when_exists: Option<String>,
        loop_iter_template: String,
        client: Option<ClientSpec>,
        body: Sequence,
        skip_if_exists: String,
    },
    /// No more stages — the gremlin is done.
    Done,
}

impl ExecutorStage {
    /// The stage's name.
    pub fn name(&self) -> &str {
        match self {
            ExecutorStage::Agent { stage, .. } => &stage.name,
            ExecutorStage::Exec { stage, .. } => &stage.name,
            ExecutorStage::Sequence(seq) => &seq.name,
            ExecutorStage::Parallel { name, .. } => name,
            ExecutorStage::Loop { name, .. } => name,
            ExecutorStage::Done => "",
        }
    }

    /// The stage's type as a static string.
    pub fn stage_type(&self) -> &str {
        match self {
            ExecutorStage::Agent { .. } => "agent",
            ExecutorStage::Exec { .. } => "exec",
            ExecutorStage::Sequence(_) => "sequence",
            ExecutorStage::Parallel { .. } => "parallel",
            ExecutorStage::Loop { .. } => "loop",
            ExecutorStage::Done => "done",
        }
    }

    /// The stage's own client, if it declared one.
    pub fn client(&self) -> Option<&ClientSpec> {
        match self {
            ExecutorStage::Agent { client, .. }
            | ExecutorStage::Exec { client, .. }
            | ExecutorStage::Parallel { client, .. }
            | ExecutorStage::Loop { client, .. } => client.as_ref(),
            ExecutorStage::Sequence(_) | ExecutorStage::Done => None,
        }
    }

    /// The artifact guard that makes the stage a conditional producer.
    pub fn skip_if_exists(&self) -> &str {
        match self {
            ExecutorStage::Agent { skip_if_exists, .. }
            | ExecutorStage::Exec { skip_if_exists, .. } => skip_if_exists,
            ExecutorStage::Sequence(seq) => &seq.skip_if_exists,
            ExecutorStage::Parallel { skip_if_exists, .. }
            | ExecutorStage::Loop { skip_if_exists, .. } => skip_if_exists,
            ExecutorStage::Done => "",
        }
    }
}

// ---------------------------------------------------------------------------
// GremlinStageProvider trait
// ---------------------------------------------------------------------------

/// The interface every gremlin definition must satisfy.
///
/// `Send` is required because the executor holds one definition per gremlin
/// and may move it across threads. `Sync` is not required — no concurrent
/// access.
#[async_trait]
pub trait GremlinStageProvider: Send {
    /// The definition's identity (the YAML file stem, or equivalent).
    fn name(&self) -> &str;

    /// The client every stage uses unless it declares its own.
    fn default_client(&self) -> &str;

    /// The git ref the worktree branches from.
    fn base_ref(&self) -> &str;

    /// Bootstrap commands and input sources.
    fn bootstrap(&self) -> &Bootstrap;

    /// The optional `land` stage — always an exec stage named `land`.
    fn land(&self) -> Option<&ExecutorStage>;

    /// Jump the cursor to a named top-level stage.
    ///
    /// If the name is not found, the cursor resets to position 0 (the start).
    fn goto(&mut self, stage: &str);

    /// Return the next stage(s) to run.
    ///
    /// Returns `Ok(ExecutorStage::Done)` when the definition has no more stages.
    async fn next_stage(&mut self) -> Result<ExecutorStage, DefinitionError>;

    /// Serialize this definition to bytes.
    ///
    /// Stub — returns `Err(DefinitionError::Message("not implemented"))`.
    fn serialize(&self) -> Result<Vec<u8>, DefinitionError> {
        Err(DefinitionError::Message("not implemented".into()))
    }

    /// Deserialize a definition from bytes.
    ///
    /// Stub — returns `Err(DefinitionError::Message("not implemented"))`.
    fn deserialize(_data: &[u8]) -> Result<Box<dyn GremlinStageProvider>, DefinitionError>
    where
        Self: Sized,
    {
        Err(DefinitionError::Message("not implemented".into()))
    }
}

// ---------------------------------------------------------------------------
// Blanket impl for Box<dyn GremlinStageProvider>
// ---------------------------------------------------------------------------

#[async_trait]
impl GremlinStageProvider for Box<dyn GremlinStageProvider> {
    fn name(&self) -> &str {
        self.as_ref().name()
    }

    fn default_client(&self) -> &str {
        self.as_ref().default_client()
    }

    fn base_ref(&self) -> &str {
        self.as_ref().base_ref()
    }

    fn bootstrap(&self) -> &Bootstrap {
        self.as_ref().bootstrap()
    }

    fn land(&self) -> Option<&ExecutorStage> {
        self.as_ref().land()
    }

    fn goto(&mut self, stage: &str) {
        self.as_mut().goto(stage)
    }

    async fn next_stage(&mut self) -> Result<ExecutorStage, DefinitionError> {
        self.as_mut().next_stage().await
    }

    fn serialize(&self) -> Result<Vec<u8>, DefinitionError> {
        self.as_ref().serialize()
    }

    fn deserialize(_data: &[u8]) -> Result<Box<dyn GremlinStageProvider>, DefinitionError>
    where
        Self: Sized,
    {
        Err(DefinitionError::Message("not implemented".into()))
    }
}

// ---------------------------------------------------------------------------
// DefinitionError
// ---------------------------------------------------------------------------

/// Errors that can arise from [`GremlinStageProvider::next_stage`].
#[derive(Error, Debug)]
pub enum DefinitionError {
    /// A schema-level problem (missing keys, type mismatches, etc.).
    #[error(transparent)]
    Schema(#[from] SchemaError),

    /// A registry-level problem.
    #[error("{0}")]
    Registry(String),

    /// A free-form message for definition-specific failures.
    #[error("{0}")]
    Message(String),
}

// ---------------------------------------------------------------------------
// StaticDefinition — newtype bridge over the concrete GremlinDefinition
// ---------------------------------------------------------------------------

/// A [`GremlinStageProvider`] trait implementation that wraps the existing
/// concrete [`GremlinDefinition`] struct.
///
/// All accessors delegate to the inner struct. `next_stage()` walks the
/// top-level stage list one [`ParsedStage`] at a time, converting each into
/// an [`ExecutorStage`] via a pure recursive projection.
#[derive(Debug)]
pub struct StaticDefinition {
    pub inner: GremlinDefinition,
    cursor: usize,
}

impl StaticDefinition {
    /// Create a new cursor-driven definition starting at position 0.
    pub fn new(inner: GremlinDefinition) -> Self {
        StaticDefinition { inner, cursor: 0 }
    }

    /// Deserialize a definition from bytes, returning an owned
    /// [`StaticDefinition`] so callers can call [`goto`](Self::goto) and
    /// extract [`inner`](Self::inner) before handing it to the executor.
    pub fn deserialize_owned(data: &[u8]) -> Result<Self, DefinitionError> {
        let definition =
            crate::builders::definition::DefinitionBuilder::from_expanded_bytes(data, None)
                .map_err(|e| DefinitionError::Message(e.to_string()))?;
        Ok(StaticDefinition::new(definition))
    }
}

#[async_trait]
impl GremlinStageProvider for StaticDefinition {
    fn name(&self) -> &str {
        &self.inner.name
    }

    fn default_client(&self) -> &str {
        &self.inner.default_client
    }

    fn base_ref(&self) -> &str {
        &self.inner.base_ref
    }

    fn bootstrap(&self) -> &Bootstrap {
        &self.inner.bootstrap
    }

    fn land(&self) -> Option<&ExecutorStage> {
        None
    }

    fn goto(&mut self, stage: &str) {
        if let Some(pos) = self.inner.stages.iter().position(|s| s.name() == stage) {
            self.cursor = pos;
        } else {
            self.cursor = 0;
        }
    }

    async fn next_stage(&mut self) -> Result<ExecutorStage, DefinitionError> {
        if self.cursor >= self.inner.stages.len() {
            return Ok(ExecutorStage::Done);
        }
        let stage = self.inner.stages[self.cursor].clone();
        self.cursor += 1;
        Ok(convert_stage(stage, &self.inner))
    }

    fn serialize(&self) -> Result<Vec<u8>, DefinitionError> {
        let yaml = self.inner.to_expanded_yaml();
        serde_yaml::to_string(&yaml)
            .map(|s| s.into_bytes())
            .map_err(|e| DefinitionError::Message(format!("failed to serialize definition: {e}")))
    }

    fn deserialize(data: &[u8]) -> Result<Box<dyn GremlinStageProvider>, DefinitionError>
    where
        Self: Sized,
    {
        StaticDefinition::deserialize_owned(data)
            .map(|sd| Box::new(sd) as Box<dyn GremlinStageProvider>)
    }
}

// ---------------------------------------------------------------------------
// ParsedStage → ExecutorStage conversion
// ---------------------------------------------------------------------------

/// Recursively convert one [`ParsedStage`] into an [`ExecutorStage`].
fn convert_stage(stage: ParsedStage, def: &GremlinDefinition) -> ExecutorStage {
    match stage {
        ParsedStage::Agent {
            stage,
            skip_if_exists,
            client,
        } => ExecutorStage::Agent {
            stage,
            skip_if_exists,
            client,
        },
        ParsedStage::Exec {
            stage,
            skip_if_exists,
            client,
        } => ExecutorStage::Exec {
            stage,
            skip_if_exists,
            client,
        },
        ParsedStage::Sequence {
            attrs,
            client: _seq_client,
            body,
        } => {
            let stages: Vec<ExecutorStage> =
                body.into_iter().map(|s| convert_stage(s, def)).collect();
            ExecutorStage::Sequence(Sequence {
                name: attrs.name,
                stages,
                scope: None,
                skip_if_exists: attrs.skip_if_exists,
            })
        }
        ParsedStage::Loop {
            attrs,
            max_iterations,
            stop_when_exists,
            client,
            body,
            ..
        } => {
            let stages: Vec<ExecutorStage> =
                body.into_iter().map(|s| convert_stage(s, def)).collect();
            ExecutorStage::Loop {
                name: attrs.name,
                max_iterations: Some(max_iterations),
                stop_when_exists,
                loop_iter_template: "{n}".to_string(),
                client,
                body: Sequence {
                    name: String::new(),
                    stages,
                    scope: None,
                    skip_if_exists: String::new(),
                },
                skip_if_exists: attrs.skip_if_exists,
            }
        }
        ParsedStage::Parallel {
            attrs,
            max_concurrent,
            cancel_on_error,
            error_policy,
            client,
            body,
        } => {
            let children: Vec<Box<dyn GremlinStageProvider>> = body
                .into_iter()
                .map(|child| {
                    Box::new(StaticDefinition::new(def.clone_with_stages(vec![child])))
                        as Box<dyn GremlinStageProvider>
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::composite::StageAttrs;

    fn stub_definition() -> GremlinDefinition {
        GremlinDefinition::stub()
    }

    #[test]
    fn static_definition_delegates_name() {
        let def = StaticDefinition::new(stub_definition());
        // stub name is "unknown"
        assert_eq!(def.name(), "unknown");
    }

    #[test]
    fn static_definition_delegates_default_client() {
        let def = StaticDefinition::new(stub_definition());
        assert_eq!(def.default_client(), "");
    }

    #[test]
    fn static_definition_delegates_base_ref() {
        let def = StaticDefinition::new(stub_definition());
        assert_eq!(def.base_ref(), "");
    }

    #[test]
    fn static_definition_delegates_bootstrap() {
        let def = StaticDefinition::new(stub_definition());
        let bs = def.bootstrap();
        assert!(bs.source.is_none());
        assert!(bs.launch_cmds.is_empty());
    }

    #[test]
    fn static_definition_delegates_land_none() {
        let def = StaticDefinition::new(stub_definition());
        assert!(def.land().is_none());
    }

    #[tokio::test]
    async fn static_definition_next_stage_returns_done() {
        let mut def = StaticDefinition::new(stub_definition());
        let result = def.next_stage().await.unwrap();
        assert!(matches!(&result, ExecutorStage::Done));
        assert_eq!(result.stage_type(), "done");
    }

    #[test]
    fn static_definition_with_real_definition() {
        // Build a minimal but real GremlinDefinition to exercise delegation
        // beyond the stub.
        let inner = GremlinDefinition {
            name: "test-gremlin".into(),
            path: "/tmp/test.yaml".into(),
            default_client: "openai:gpt-4".into(),
            base_ref: "main".into(),
            bootstrap: Bootstrap::default(),
            stages: vec![],
            land: None,
            expanded_yaml: serde_yaml::Value::Null,
        };
        let def = StaticDefinition::new(inner);
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
            skip_if_exists: String::new(),
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
            skip_if_exists: String::new(),
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
    fn executor_stage_loop_name() {
        let stage = ExecutorStage::Loop {
            name: "retry".into(),
            max_iterations: Some(3),
            stop_when_exists: None,
            loop_iter_template: "retry-{n}".into(),
            client: None,
            body: Sequence {
                name: String::new(),
                stages: vec![],
                scope: None,
                skip_if_exists: String::new(),
            },
            skip_if_exists: "artifact://retry".into(),
        };
        assert_eq!(stage.name(), "retry");
        assert_eq!(stage.stage_type(), "loop");
        assert!(stage.client().is_none());
        assert_eq!(stage.skip_if_exists(), "artifact://retry");
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
            skip_if_exists: String::new(),
            client: Some(ClientSpec("xai:grok-5".into())),
        };
        assert_eq!(stage.client(), Some(&ClientSpec("xai:grok-5".into())));
    }

    #[test]
    fn executor_stage_skip_if_exists() {
        let stage = ExecutorStage::Exec {
            stage: Exec {
                name: "build".into(),
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            skip_if_exists: "artifact://done".into(),
            client: None,
        };
        assert_eq!(stage.skip_if_exists(), "artifact://done");
    }

    #[test]
    fn executor_stage_sequence_skip_if_exists() {
        let seq = ExecutorStage::Sequence(Sequence {
            name: String::new(),
            stages: vec![],
            scope: None,
            skip_if_exists: "artifact://guard".into(),
        });
        assert_eq!(seq.skip_if_exists(), "artifact://guard");
    }

    // ---- StaticDefinition cursor / goto / next_stage tests ----

    /// Build a minimal Agent ParsedStage for use in test definitions.
    fn parsed_agent(name: &str) -> ParsedStage {
        ParsedStage::Agent {
            stage: Agent {
                name: name.to_string(),
                prompts: vec![],
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            skip_if_exists: String::new(),
            client: None,
        }
    }

    /// Build a minimal Exec ParsedStage.
    fn parsed_exec(name: &str) -> ParsedStage {
        ParsedStage::Exec {
            stage: Exec {
                name: name.to_string(),
                options: std::collections::HashMap::new(),
                interpolation_map: std::collections::HashMap::new(),
                bind_map: std::collections::HashMap::new(),
            },
            skip_if_exists: String::new(),
            client: None,
        }
    }

    /// Build a multi-stage GremlinDefinition from ParsedStage entries.
    fn definition_with(stages: Vec<ParsedStage>) -> GremlinDefinition {
        GremlinDefinition {
            name: "test-def".into(),
            path: "/tmp/test.yaml".into(),
            default_client: "openai:gpt-4".into(),
            base_ref: "main".into(),
            bootstrap: Bootstrap::default(),
            stages,
            land: None,
            expanded_yaml: serde_yaml::Value::Null,
        }
    }

    #[tokio::test]
    async fn next_stage_yields_top_level_stages_in_order() {
        let def = definition_with(vec![
            parsed_agent("first"),
            parsed_exec("second"),
            parsed_agent("third"),
        ]);
        let mut sd = StaticDefinition::new(def);

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
        let mut sd = StaticDefinition::new(def);

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
        let mut sd = StaticDefinition::new(def);

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
        let mut bx: Box<dyn GremlinStageProvider> = Box::new(StaticDefinition::new(def));

        bx.goto("y");
        let s = bx.next_stage().await.unwrap();
        assert_eq!(s.name(), "y");
    }

    // ---- convert_stage variant tests ----

    #[tokio::test]
    async fn convert_stage_agent() {
        let def = definition_with(vec![parsed_agent("plan")]);
        let mut sd = StaticDefinition::new(def);
        let s = sd.next_stage().await.unwrap();
        assert!(matches!(s, ExecutorStage::Agent { .. }));
        assert_eq!(s.name(), "plan");
        assert_eq!(s.stage_type(), "agent");
    }

    #[tokio::test]
    async fn convert_stage_sequence() {
        let seq = ParsedStage::Sequence {
            attrs: StageAttrs {
                name: "outer".into(),
                skip_if_exists: "artifact://guard".into(),
                ..StageAttrs::new("outer".into())
            },
            client: None,
            body: vec![parsed_agent("inner-a"), parsed_exec("inner-b")],
        };
        let def = definition_with(vec![seq]);
        let mut sd = StaticDefinition::new(def);
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
    async fn convert_stage_loop() {
        let lp = ParsedStage::Loop {
            attrs: StageAttrs {
                name: "retry".into(),
                skip_if_exists: "artifact://retry-guard".into(),
                ..StageAttrs::new("retry".into())
            },
            max_iterations: 5,
            stop_when_exists: Some("artifact://done".into()),
            interval: None,
            client: Some(ClientSpec("xai:grok".into())),
            body: vec![parsed_agent("loop-child")],
        };
        let def = definition_with(vec![lp]);
        let mut sd = StaticDefinition::new(def);
        let s = sd.next_stage().await.unwrap();
        assert_eq!(s.name(), "retry");
        assert_eq!(s.stage_type(), "loop");
        assert_eq!(s.skip_if_exists(), "artifact://retry-guard");
        match s {
            ExecutorStage::Loop {
                max_iterations,
                stop_when_exists,
                loop_iter_template,
                client,
                body,
                ..
            } => {
                assert_eq!(max_iterations, Some(5));
                assert_eq!(stop_when_exists.as_deref(), Some("artifact://done"));
                assert_eq!(loop_iter_template, "{n}");
                assert_eq!(client, Some(ClientSpec("xai:grok".into())));
                assert_eq!(body.stages.len(), 1);
                assert_eq!(body.stages[0].name(), "loop-child");
                assert!(body.scope.is_none());
                assert_eq!(body.skip_if_exists, "");
            }
            _ => panic!("expected Loop"),
        }
    }

    #[tokio::test]
    async fn convert_stage_parallel_children_inherit_metadata() {
        let par = ParsedStage::Parallel {
            attrs: StageAttrs::new("reviews".into()),
            max_concurrent: Some(4),
            cancel_on_error: true,
            error_policy: ErrorPolicy::All,
            client: Some(ClientSpec("openai:gpt-5".into())),
            body: vec![parsed_agent("rev-a"), parsed_agent("rev-b")],
        };
        let def = definition_with(vec![par]);
        let mut sd = StaticDefinition::new(def);
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
                // Each child is a StaticDefinition wrapping a GremlinDefinition
                // that inherits parent metadata (name, default_client, base_ref, bootstrap).
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
        let par = ParsedStage::Parallel {
            attrs: StageAttrs::new("group".into()),
            max_concurrent: None,
            cancel_on_error: false,
            error_policy: ErrorPolicy::Any,
            client: None,
            body: vec![parsed_agent("sole-child")],
        };
        let def = definition_with(vec![par]);
        let mut sd = StaticDefinition::new(def);
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
        let def: Box<dyn GremlinStageProvider> = Box::new(StaticDefinition::new(stub_definition()));
        assert_eq!(def.name(), "unknown");
    }

    #[tokio::test]
    async fn boxed_definition_delegates_land() {
        let def: Box<dyn GremlinStageProvider> = Box::new(StaticDefinition::new(stub_definition()));
        assert!(def.land().is_none());
    }

    #[tokio::test]
    async fn boxed_definition_delegates_next_stage() {
        let mut def: Box<dyn GremlinStageProvider> =
            Box::new(StaticDefinition::new(stub_definition()));
        let result = def.next_stage().await.unwrap();
        assert!(matches!(result, ExecutorStage::Done));
    }

    #[tokio::test]
    async fn boxed_definition_serialize_roundtrips() {
        // Build a real definition with stages, bootstrap, and land so the
        // round-trip validates more than just scalar metadata.
        let inner = definition_with(vec![
            parsed_agent("greet"),
            parsed_exec("build"),
            parsed_agent("farewell"),
        ]);
        let def: Box<dyn GremlinStageProvider> = Box::new(StaticDefinition::new(inner));
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
