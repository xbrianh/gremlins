//! The `GremlinDefinition` trait — the abstraction boundary between the
//! executor run-loop and concrete definition sources (static YAML, Python
//! plugins, etc.).
//!
//! This module defines the trait and the `ExecutorStage` enum the trait
//! returns.  `StaticDefinition` is a cursor-driven implementation that owns
//! the definition data directly and converts one top-level [`BuilderStage`]
//! into an [`ExecutorStage`] per `next_stage()` call.

use std::path::Path;

use async_trait::async_trait;
use thiserror::Error;

use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
pub use crate::stages::agent::Agent;
use crate::stages::composite::ClientSpec;
pub use crate::stages::exec::Exec;
pub use crate::stages::parallel::ErrorPolicy;

pub mod r#static;
pub use r#static::StaticDefinition;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The name a not-yet-loaded definition carries. [`Gremlin::init_runtime`]
/// treats it as "nothing loaded yet", so it must never be a real definition's
/// name — the builder derives that from the YAML file stem.
pub const UNLOADED_NAME: &str = "unknown";

// ---------------------------------------------------------------------------
// Sequence — repeating-stage pipeline payload
// ---------------------------------------------------------------------------

/// A sequence of stages with an optional scope, artifact guard, and
/// repetition controls.  When `max_iterations > 1` the body repeats up to
/// that many times, sleeping `interval` seconds between iterations, and
/// stopping early when `skip_if_exists` is satisfied.
pub struct Sequence {
    pub name: String,
    pub stages: Vec<ExecutorStage>,
    pub scope: Option<String>,
    pub skip_if_exists: String,
    pub client: Option<ClientSpec>,
    pub max_iterations: u32,
    pub interval: Option<f64>,
}

// ---------------------------------------------------------------------------
// ExecutorStage — what next_stage() returns
// ---------------------------------------------------------------------------

/// The next stage (or stages) the executor should run.
/// Converted from [`BuilderStage`] by [`StaticDefinition::convert_stage`].
pub enum ExecutorStage {
    /// Run an agent stage.
    Agent {
        stage: Agent,
        client: Option<ClientSpec>,
    },
    /// Run an exec stage.
    Exec {
        stage: Exec,
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
            ExecutorStage::Done => "done",
        }
    }

    /// The stage's own client, if it declared one.
    pub fn client(&self) -> Option<&ClientSpec> {
        match self {
            ExecutorStage::Agent { client, .. }
            | ExecutorStage::Exec { client, .. }
            | ExecutorStage::Parallel { client, .. } => client.as_ref(),
            ExecutorStage::Sequence(seq) => seq.client.as_ref(),
            ExecutorStage::Done => None,
        }
    }

    /// The artifact guard that makes the stage a conditional producer.
    /// Only [`ExecutorStage::Sequence`] and [`ExecutorStage::Parallel`]
    /// carry guards; leaf stages always return an empty string.
    pub fn skip_if_exists(&self) -> &str {
        match self {
            ExecutorStage::Agent { .. } | ExecutorStage::Exec { .. } => "",
            ExecutorStage::Sequence(seq) => &seq.skip_if_exists,
            ExecutorStage::Parallel { skip_if_exists, .. } => skip_if_exists,
            ExecutorStage::Done => "",
        }
    }
}

// ---------------------------------------------------------------------------
// GremlinDefinition trait
// ---------------------------------------------------------------------------

/// The interface every gremlin definition must satisfy.
/// `Send + Sync` is required because the executor holds one definition per gremlin
/// and may move it across threads.
#[async_trait]
pub trait GremlinDefinition: Send + Sync {
    /// The definition's identity (the YAML file stem, or equivalent).
    fn name(&self) -> &str;

    /// The client every stage uses unless it declares its own.
    fn default_client(&self) -> &str;

    /// The git ref the worktree branches from.
    fn base_ref(&self) -> &str;

    /// Bootstrap commands and input sources.
    fn bootstrap(&self) -> &Bootstrap;

    /// The optional `land` stage — always an exec stage named `land`.
    fn land(&self) -> Option<ExecutorStage>;

    /// Whether the cursor is at position 0 (a fresh start, not a resume).
    fn is_at_start(&self) -> bool;

    /// Whether this definition is a stub (not yet loaded).
    fn is_stub(&self) -> bool;

    /// Bake an overriding client spec into this provider so that
    /// [`default_client`](Self::default_client) returns it.
    ///
    /// Called by [`Gremlin::fork`](crate::executor::gremlin::Gremlin::fork)
    /// when an effective client propagates from an enclosing group.
    fn with_client(&mut self, client: &str);

    /// Clone this provider into a new heap-allocated box.
    fn clone_box(&self) -> Box<dyn GremlinDefinition>;

    /// The name of the first stage in this provider, for child identification
    /// in parallel groups. Defaults to [`Self::name`].
    fn first_stage_name(&self) -> &str {
        self.name()
    }

    /// The filesystem path the definition was loaded from.
    fn path(&self) -> &Path;

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

    fn land(&self) -> Option<ExecutorStage> {
        self.as_ref().land()
    }

    fn is_at_start(&self) -> bool {
        self.as_ref().is_at_start()
    }

    fn is_stub(&self) -> bool {
        self.as_ref().is_stub()
    }

    fn with_client(&mut self, client: &str) {
        self.as_mut().with_client(client)
    }

    fn clone_box(&self) -> Box<dyn GremlinDefinition> {
        self.as_ref().clone_box()
    }

    fn first_stage_name(&self) -> &str {
        self.as_ref().first_stage_name()
    }

    fn path(&self) -> &Path {
        self.as_ref().path()
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
