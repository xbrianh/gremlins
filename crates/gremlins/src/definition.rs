//! The `GremlinDefinition` trait — the abstraction boundary between the
//! executor run-loop and concrete definition sources (static YAML, Python
//! plugins, etc.).
//!
//! This module defines the trait and the `ExecutorStage` enum the trait
//! returns.  `StaticDefinition` is a newtype over the existing concrete
//! [`GremlinDefinition`] that implements the trait with trivial delegation
//! and a no-op `next_stage` — the cursor state machine will replace that
//! in a follow-up.

use async_trait::async_trait;
use thiserror::Error;

use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
use crate::schemas::gremlin_definition::GremlinDefinition as ConcreteDefinition;
use crate::stages::agent::Agent;
use crate::stages::composite::ClientSpec;
use crate::stages::exec::Exec;
use crate::stages::parallel::ErrorPolicy;

// ---------------------------------------------------------------------------
// Sequence — shared payload for Sequence and Loop variants
// ---------------------------------------------------------------------------

/// A sequence of stages with an optional scope and artifact guard.
///
/// Used as the payload of [`ExecutorStage::Sequence`] and as the body of
/// [`ExecutorStage::Loop`].
pub struct Sequence {
    pub stages: Vec<ExecutorStage>,
    pub scope: Option<String>,
    pub skip_if_exists: String,
}

// ---------------------------------------------------------------------------
// ExecutorStage — what next_stage() returns
// ---------------------------------------------------------------------------

/// The next stage (or stages) the executor should run.
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
        children: Vec<Box<dyn GremlinDefinition>>,
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
            ExecutorStage::Sequence(seq) => seq.stages.first().map_or("", |s| s.name()),
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
// GremlinDefinition trait
// ---------------------------------------------------------------------------

/// The interface every gremlin definition must satisfy.
///
/// `Send` is required because the executor holds one definition per gremlin
/// and may move it across threads. `Sync` is not required — no concurrent
/// access.
#[async_trait]
pub trait GremlinDefinition: Send {
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
    fn deserialize(_data: &[u8]) -> Result<Box<dyn GremlinDefinition>, DefinitionError>
    where
        Self: Sized,
    {
        Err(DefinitionError::Message("not implemented".into()))
    }
}

// ---------------------------------------------------------------------------
// Blanket impl for Box<dyn GremlinDefinition>
// ---------------------------------------------------------------------------

#[async_trait]
impl GremlinDefinition for Box<dyn GremlinDefinition> {
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

    async fn next_stage(&mut self) -> Result<ExecutorStage, DefinitionError> {
        self.as_mut().next_stage().await
    }

    fn serialize(&self) -> Result<Vec<u8>, DefinitionError> {
        self.as_ref().serialize()
    }

    fn deserialize(_data: &[u8]) -> Result<Box<dyn GremlinDefinition>, DefinitionError>
    where
        Self: Sized,
    {
        Err(DefinitionError::Message("not implemented".into()))
    }
}

// ---------------------------------------------------------------------------
// DefinitionError
// ---------------------------------------------------------------------------

/// Errors that can arise from [`GremlinDefinition::next_stage`].
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

/// A [`GremlinDefinition`] trait implementation that wraps the existing
/// concrete [`ConcreteDefinition`] struct.
///
/// All accessors delegate to the inner struct. `next_stage()` returns
/// `Ok(ExecutorStage::Done)` unconditionally — the cursor state machine that
/// walks the stage list will replace this in a follow-up.
#[derive(Debug)]
pub struct StaticDefinition {
    pub inner: ConcreteDefinition,
}

#[async_trait]
impl GremlinDefinition for StaticDefinition {
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

    async fn next_stage(&mut self) -> Result<ExecutorStage, DefinitionError> {
        Ok(ExecutorStage::Done)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_definition() -> ConcreteDefinition {
        ConcreteDefinition::stub()
    }

    #[test]
    fn static_definition_delegates_name() {
        let def = StaticDefinition {
            inner: stub_definition(),
        };
        // stub name is "unknown"
        assert_eq!(def.name(), "unknown");
    }

    #[test]
    fn static_definition_delegates_default_client() {
        let def = StaticDefinition {
            inner: stub_definition(),
        };
        assert_eq!(def.default_client(), "");
    }

    #[test]
    fn static_definition_delegates_base_ref() {
        let def = StaticDefinition {
            inner: stub_definition(),
        };
        assert_eq!(def.base_ref(), "");
    }

    #[test]
    fn static_definition_delegates_bootstrap() {
        let def = StaticDefinition {
            inner: stub_definition(),
        };
        let bs = def.bootstrap();
        assert!(bs.source.is_none());
        assert!(bs.launch_cmds.is_empty());
    }

    #[test]
    fn static_definition_delegates_land_none() {
        let def = StaticDefinition {
            inner: stub_definition(),
        };
        assert!(def.land().is_none());
    }

    #[tokio::test]
    async fn static_definition_next_stage_returns_done() {
        let mut def = StaticDefinition {
            inner: stub_definition(),
        };
        let result = def.next_stage().await.unwrap();
        assert!(matches!(&result, ExecutorStage::Done));
        assert_eq!(result.stage_type(), "done");
    }

    #[test]
    fn static_definition_with_real_definition() {
        // Build a minimal but real GremlinDefinition to exercise delegation
        // beyond the stub.
        let inner = ConcreteDefinition {
            name: "test-gremlin".into(),
            path: "/tmp/test.yaml".into(),
            default_client: "openai:gpt-4".into(),
            base_ref: "main".into(),
            bootstrap: Bootstrap::default(),
            stages: vec![],
            land: None,
            expanded_yaml: serde_yaml::Value::Null,
        };
        let def = StaticDefinition { inner };
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
    fn executor_stage_sequence_name_returns_first_child() {
        let seq = ExecutorStage::Sequence(Sequence {
            stages: vec![make_agent("first"), make_exec("second")],
            scope: None,
            skip_if_exists: String::new(),
        });
        assert_eq!(seq.name(), "first");
        assert_eq!(seq.stage_type(), "sequence");
        assert!(seq.client().is_none());
    }

    #[test]
    fn executor_stage_sequence_name_empty_returns_empty() {
        let seq = ExecutorStage::Sequence(Sequence {
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
            stages: vec![],
            scope: None,
            skip_if_exists: "artifact://guard".into(),
        });
        assert_eq!(seq.skip_if_exists(), "artifact://guard");
    }

    // ---- Box<dyn GremlinDefinition> blanket impl tests ----

    #[tokio::test]
    async fn boxed_definition_delegates_name() {
        let def: Box<dyn GremlinDefinition> = Box::new(StaticDefinition {
            inner: stub_definition(),
        });
        assert_eq!(def.name(), "unknown");
    }

    #[tokio::test]
    async fn boxed_definition_delegates_land() {
        let def: Box<dyn GremlinDefinition> = Box::new(StaticDefinition {
            inner: stub_definition(),
        });
        assert!(def.land().is_none());
    }

    #[tokio::test]
    async fn boxed_definition_delegates_next_stage() {
        let mut def: Box<dyn GremlinDefinition> = Box::new(StaticDefinition {
            inner: stub_definition(),
        });
        let result = def.next_stage().await.unwrap();
        assert!(matches!(result, ExecutorStage::Done));
    }

    #[test]
    fn boxed_definition_stub_methods_return_not_implemented() {
        let def: Box<dyn GremlinDefinition> = Box::new(StaticDefinition {
            inner: stub_definition(),
        });
        let err = def.serialize().unwrap_err();
        assert!(matches!(err, DefinitionError::Message(m) if m == "not implemented"));
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
