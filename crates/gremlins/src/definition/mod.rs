//! The `GremlinDefinition` trait — the abstraction boundary between the
//! executor run-loop and concrete definition sources (static YAML, Python
//! plugins, etc.).
//!
//! This module defines the trait and the `ExecutorStage` enum the trait
//! returns.  `StaticDefinition` is a cursor-driven implementation that owns
//! the definition data directly and converts one top-level
//! [`crate::stage_spec::node::StageSpec`] into an [`ExecutorStage`] per
//! `next_stage()` call.

use std::path::Path;

use async_trait::async_trait;
use indexmap::IndexMap;
use thiserror::Error;

use crate::clients::client::ModelURI;
use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
pub use crate::stage_spec::agent::Agent;
pub use crate::stage_spec::exec::Exec;
pub use crate::stage_spec::node::StageSpec;
pub use crate::stage_spec::parallel::{ErrorPolicy, ForkSpec, JoinSpec};

pub(crate) mod chat;
pub use chat::ChatDefinition;
pub(crate) mod r#static;
pub use r#static::bundle::bundle_definition;
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
    pub model: Option<ModelURI>,
    pub task_models: Option<IndexMap<String, String>>,
    pub max_iterations: u32,
    pub interval: Option<f64>,
}

// ---------------------------------------------------------------------------
// ExecutorStage — what next_stage() returns
// ---------------------------------------------------------------------------

/// The next stage (or stages) the executor should run.
/// Converted from [`crate::stage_spec::node::StageSpec`] by
/// [`StaticDefinition::convert_stage`].
pub enum ExecutorStage {
    /// Run an agent stage.
    Agent {
        stage: Agent,
        model: Option<ModelURI>,
        task_models: Option<IndexMap<String, String>>,
    },
    /// Run an exec stage.
    Exec {
        stage: Exec,
        model: Option<ModelURI>,
        task_models: Option<IndexMap<String, String>>,
    },
    /// Run a sequence of stages in order.
    Sequence(Sequence),
    /// Run children in parallel.
    Parallel {
        name: String,
        max_concurrent: Option<u32>,
        cancel_on_error: bool,
        error_policy: ErrorPolicy,
        model: Option<ModelURI>,
        task_models: Option<IndexMap<String, String>>,
        children: Vec<Box<dyn GremlinDefinition>>,
        skip_if_exists: String,
        fork: Option<ForkSpec>,
        join: Option<JoinSpec>,
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

    /// The stage's own model, if it declared one.
    pub fn model(&self) -> Option<&ModelURI> {
        match self {
            ExecutorStage::Agent { model, .. }
            | ExecutorStage::Exec { model, .. }
            | ExecutorStage::Parallel { model, .. } => model.as_ref(),
            ExecutorStage::Sequence(seq) => seq.model.as_ref(),
            ExecutorStage::Done => None,
        }
    }

    /// The stage's own task-model overrides, if any.
    pub fn task_models(&self) -> Option<&IndexMap<String, String>> {
        match self {
            ExecutorStage::Agent { task_models, .. }
            | ExecutorStage::Exec { task_models, .. }
            | ExecutorStage::Parallel { task_models, .. } => task_models.as_ref(),
            ExecutorStage::Sequence(seq) => seq.task_models.as_ref(),
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

    /// The model every stage uses unless it declares its own.
    fn default_model(&self) -> &ModelURI;

    /// Bootstrap commands and input sources.
    fn bootstrap(&self) -> &Bootstrap;

    /// The optional `land` stage — always an exec stage named `land`.
    fn land(&self) -> Option<ExecutorStage>;

    /// Clean commands to run during `gremlins clean`, before workspace
    /// removal. Defaults to an empty slice.
    fn clean_cmds(&self) -> &[String] {
        &[]
    }

    /// Whether the cursor is at position 0 (a fresh start, not a resume).
    fn is_at_start(&self) -> bool;

    /// Whether this definition is a stub (not yet loaded).
    fn is_stub(&self) -> bool;

    /// Bake an overriding model spec into this provider so that
    /// [`default_model`](Self::default_model) returns it.
    ///
    /// Called by [`Gremlin::fork`](crate::executor::gremlin::Gremlin::fork)
    /// when an effective model propagates from an enclosing group.
    fn with_model(&mut self, model: &ModelURI);

    /// Clone this provider into a new heap-allocated box.
    fn clone_box(&self) -> Box<dyn GremlinDefinition>;

    /// The name of the first stage in this provider, for child identification
    /// in parallel groups. Defaults to [`Self::name`].
    fn first_stage_name(&self) -> &str {
        self.name()
    }

    /// The ordered list of stage names in this definition.
    ///
    /// Used by scratchpad accumulation to order injected memory in
    /// pipeline execution order rather than lexical order. Returns an empty
    /// slice when the definition cannot enumerate its stages statically
    /// (chat gremlins, where chat-* stage names already sort correctly).
    fn stage_names(&self) -> &[String] {
        &[]
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

    /// Escape hatch for downcasting to concrete definition types.
    ///
    /// Defaults to `None`. Override in definition types that need
    /// concrete access from callers that know the type at compile time.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

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

    fn default_model(&self) -> &ModelURI {
        self.as_ref().default_model()
    }

    fn bootstrap(&self) -> &Bootstrap {
        self.as_ref().bootstrap()
    }

    fn land(&self) -> Option<ExecutorStage> {
        self.as_ref().land()
    }

    fn clean_cmds(&self) -> &[String] {
        self.as_ref().clean_cmds()
    }

    fn is_at_start(&self) -> bool {
        self.as_ref().is_at_start()
    }

    fn is_stub(&self) -> bool {
        self.as_ref().is_stub()
    }

    fn with_model(&mut self, model: &ModelURI) {
        self.as_mut().with_model(model)
    }

    fn clone_box(&self) -> Box<dyn GremlinDefinition> {
        self.as_ref().clone_box()
    }

    fn first_stage_name(&self) -> &str {
        self.as_ref().first_stage_name()
    }

    fn stage_names(&self) -> &[String] {
        self.as_ref().stage_names()
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

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        self.as_mut().as_any_mut()
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
