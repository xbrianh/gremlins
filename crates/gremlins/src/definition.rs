//! The `GremlinDefinition` trait — the abstraction boundary between the
//! executor run-loop and concrete definition sources (static YAML, Python
//! plugins, etc.).
//!
//! This module defines the trait, the transition-state struct the run-loop
//! passes to `next_stage()`, and the `NextStage` enum the trait returns.
//! `StaticDefinition` is a newtype over the existing concrete
//! [`GremlinDefinition`] that implements the trait with trivial delegation
//! and a no-op `next_stage` — the cursor state machine will replace that
//! in a follow-up.

use std::collections::HashMap;

use thiserror::Error;

use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
use crate::schemas::gremlin_definition::GremlinDefinition as ConcreteDefinition;
use crate::stages::node::RunnableStage;

// ---------------------------------------------------------------------------
// TransitionState — what the run-loop feeds into next_stage()
// ---------------------------------------------------------------------------

/// The outcome of the most recently completed stage, passed to
/// [`GremlinDefinition::next_stage`] so the definition can decide what
/// to run next.
#[derive(Debug, Clone)]
pub struct TransitionState {
    /// The name of the stage that just finished, if any.
    pub last_stage: Option<String>,
    /// Whether the last stage succeeded.
    pub last_ok: bool,
    /// The error message from the last stage, if it failed.
    pub last_error: Option<String>,
    /// Artifacts produced so far, keyed by URI.
    pub produced_artifacts: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// NextStage — what next_stage() returns
// ---------------------------------------------------------------------------

/// The next stage (or stages) the executor should run.
///
/// `Parallel` is deferred — it requires `Box<dyn GremlinDefinition>` in the
/// enum, which pulls in object-safety questions best solved in a dedicated
/// gremlin.
#[derive(Debug)]
pub enum NextStage {
    /// Run a single stage.
    Single(Box<RunnableStage>),
    /// Run a sequence of stages in order.
    Sequence(Vec<RunnableStage>),
    /// No more stages — the gremlin is done.
    Done,
}

// ---------------------------------------------------------------------------
// GremlinDefinition trait
// ---------------------------------------------------------------------------

/// The interface every gremlin definition must satisfy.
///
/// `Send` is required because the executor holds one definition per gremlin
/// and may move it across threads. `Sync` is not required — no concurrent
/// access.
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
    fn land(&self) -> Option<&RunnableStage>;

    /// Given the outcome of the last stage, return the next stage(s) to run.
    ///
    /// Returns `Ok(NextStage::Done)` when the definition has no more stages.
    fn next_stage(&mut self, state: &TransitionState) -> Result<NextStage, DefinitionError>;
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
/// `Ok(NextStage::Done)` unconditionally — the cursor state machine that
/// walks the stage list will replace this in a follow-up.
#[derive(Debug)]
pub struct StaticDefinition {
    pub inner: ConcreteDefinition,
}

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

    fn land(&self) -> Option<&RunnableStage> {
        self.inner.land.as_ref()
    }

    fn next_stage(&mut self, _state: &TransitionState) -> Result<NextStage, DefinitionError> {
        Ok(NextStage::Done)
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

    #[test]
    fn static_definition_next_stage_returns_done() {
        let mut def = StaticDefinition {
            inner: stub_definition(),
        };
        let state = TransitionState {
            last_stage: None,
            last_ok: true,
            last_error: None,
            produced_artifacts: HashMap::new(),
        };
        let result = def.next_stage(&state).unwrap();
        match result {
            NextStage::Done => {}
            other => panic!("expected Done, got {other:?}"),
        }
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
}
