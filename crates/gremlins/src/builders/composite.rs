//! Builders for composite stages: [`SequenceBuilder`], [`ParallelBuilder`],
//! [`LoopBuilder`].

use crate::schemas::error::SchemaError;
use crate::stages::composite::{ClientSpec, StageAttrs};
use crate::stages::node::RunnableStage;
use crate::stages::parallel::{validate_child_names, ErrorPolicy};

// ---------------------------------------------------------------------------
// SequenceBuilder
// ---------------------------------------------------------------------------

/// Build a [`RunnableStage::Sequence`].
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let seq = SequenceBuilder::new("workflow")
///     .stage(ExecBuilder::new("step-a").cmd("echo a").build().unwrap())
///     .stage(ExecBuilder::new("step-b").cmd("echo b").build().unwrap())
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct SequenceBuilder {
    name: String,
    body: Vec<RunnableStage>,
    skip_if_exists: String,
    client: Option<ClientSpec>,
}

impl SequenceBuilder {
    /// Start building a sequence with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        SequenceBuilder {
            name: name.into(),
            body: Vec::new(),
            skip_if_exists: String::new(),
            client: None,
        }
    }

    /// Set the stage name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Append a child stage.
    pub fn stage(mut self, stage: RunnableStage) -> Self {
        self.body.push(stage);
        self
    }

    /// Append many child stages.
    pub fn stages(mut self, stages: Vec<RunnableStage>) -> Self {
        self.body.extend(stages);
        self
    }

    /// Set the `skip_if_exists` artifact guard.
    pub fn skip_if_exists(mut self, uri: impl Into<String>) -> Self {
        self.skip_if_exists = uri.into();
        self
    }

    /// Set the stage's own client spec.
    pub fn client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(ClientSpec(client.into()));
        self
    }

    /// Consume the builder and produce a [`RunnableStage::Sequence`].
    pub fn build(mut self) -> Result<RunnableStage, SchemaError> {
        let name = self.name.clone();

        if self.body.is_empty() {
            return Err(SchemaError::Stage {
                name,
                msg: "'body' must not be empty".to_string(),
            });
        }

        // Fill names for unnamed children (same as the YAML path).
        crate::builders::definition::fill_builder_names(&mut self.body);

        let mut attrs = StageAttrs::new(self.name);
        attrs.stage_type = "sequence".to_string();
        attrs.skip_if_exists = self.skip_if_exists;
        attrs.client_explicit = self.client.is_some();
        Ok(RunnableStage::Sequence {
            attrs,
            client: self.client,
            body: self.body,
        })
    }
}

// ---------------------------------------------------------------------------
// ParallelBuilder
// ---------------------------------------------------------------------------

/// Build a [`RunnableStage::Parallel`].
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let par = ParallelBuilder::new("reviews")
///     .stage(AgentBuilder::new("review-one").prompt("review").build().unwrap())
///     .stage(AgentBuilder::new("review-two").prompt("review").build().unwrap())
///     .max_concurrent(2)
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct ParallelBuilder {
    name: String,
    body: Vec<RunnableStage>,
    max_concurrent: Option<u32>,
    cancel_on_error: bool,
    error_policy: ErrorPolicy,
    skip_if_exists: String,
    client: Option<ClientSpec>,
}

impl ParallelBuilder {
    /// Start building a parallel group with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        ParallelBuilder {
            name: name.into(),
            body: Vec::new(),
            max_concurrent: None,
            cancel_on_error: false,
            error_policy: ErrorPolicy::Any,
            skip_if_exists: String::new(),
            client: None,
        }
    }

    /// Set the stage name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Append a child stage.
    pub fn stage(mut self, stage: RunnableStage) -> Self {
        self.body.push(stage);
        self
    }

    /// Append many child stages.
    pub fn stages(mut self, stages: Vec<RunnableStage>) -> Self {
        self.body.extend(stages);
        self
    }

    /// Set the maximum number of concurrent children.
    pub fn max_concurrent(mut self, n: u32) -> Self {
        self.max_concurrent = Some(n);
        self
    }

    /// Whether to cancel siblings on the first child error.
    pub fn cancel_on_error(mut self, cancel: bool) -> Self {
        self.cancel_on_error = cancel;
        self
    }

    /// Set the error policy (`any` or `all`).
    pub fn error_policy(mut self, policy: ErrorPolicy) -> Self {
        self.error_policy = policy;
        self
    }

    /// Set the `skip_if_exists` artifact guard.
    pub fn skip_if_exists(mut self, uri: impl Into<String>) -> Self {
        self.skip_if_exists = uri.into();
        self
    }

    /// Set the stage's own client spec.
    pub fn client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(ClientSpec(client.into()));
        self
    }

    /// Consume the builder and produce a [`RunnableStage::Parallel`].
    pub fn build(mut self) -> Result<RunnableStage, SchemaError> {
        let name = self.name.clone();

        // Reject max_concurrent(0) — YAML rejects it, and the executor
        // silently treats zero as unlimited.
        if self.max_concurrent == Some(0) {
            return Err(SchemaError::Stage {
                name: name.clone(),
                msg: "max_concurrent must be >= 1, got 0".to_string(),
            });
        }

        // Reject nested parallel children.
        for child in &self.body {
            if child.stage_type() == "parallel" {
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!("nested parallel groups are not allowed (stage {name:?})"),
                });
            }
        }

        // Fill names for unnamed children before validating them.
        crate::builders::definition::fill_builder_names(&mut self.body);

        // Validate child names.
        let child_names: Vec<String> = self.body.iter().map(|c| c.name().to_string()).collect();
        validate_child_names(&name, &child_names).map_err(|msg| SchemaError::Stage {
            name: name.clone(),
            msg,
        })?;

        let mut attrs = StageAttrs::new(self.name);
        attrs.stage_type = "parallel".to_string();
        attrs.skip_if_exists = self.skip_if_exists;
        attrs.client_explicit = self.client.is_some();
        Ok(RunnableStage::Parallel {
            attrs,
            max_concurrent: self.max_concurrent,
            cancel_on_error: self.cancel_on_error,
            error_policy: self.error_policy,
            client: self.client,
            body: self.body,
        })
    }
}

// ---------------------------------------------------------------------------
// LoopBuilder
// ---------------------------------------------------------------------------

/// Build a [`RunnableStage::Loop`].
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let lp = LoopBuilder::new("verify")
///     .max_iterations(5)
///     .stop_when_exists("artifact://{loop_iter}/done")
///     .stage(ExecBuilder::new("cmd").cmd("make test").build().unwrap())
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct LoopBuilder {
    name: String,
    body: Vec<RunnableStage>,
    max_iterations: u32,
    stop_when_exists: Option<String>,
    interval: Option<f64>,
    skip_if_exists: String,
    client: Option<ClientSpec>,
}

impl LoopBuilder {
    /// Start building a loop with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        LoopBuilder {
            name: name.into(),
            body: Vec::new(),
            max_iterations: 3,
            stop_when_exists: None,
            interval: None,
            skip_if_exists: String::new(),
            client: None,
        }
    }

    /// Set the stage name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Append a child stage.
    pub fn stage(mut self, stage: RunnableStage) -> Self {
        self.body.push(stage);
        self
    }

    /// Append many child stages.
    pub fn stages(mut self, stages: Vec<RunnableStage>) -> Self {
        self.body.extend(stages);
        self
    }

    /// Set the maximum number of iterations.
    pub fn max_iterations(mut self, n: u32) -> Self {
        self.max_iterations = n;
        self
    }

    /// Set the artifact URI that stops the loop when it exists.
    pub fn stop_when_exists(mut self, uri: impl Into<String>) -> Self {
        self.stop_when_exists = Some(uri.into());
        self
    }

    /// Set the interval between iterations (in seconds).
    pub fn interval(mut self, seconds: f64) -> Self {
        self.interval = Some(seconds);
        self
    }

    /// Set the `skip_if_exists` artifact guard.
    pub fn skip_if_exists(mut self, uri: impl Into<String>) -> Self {
        self.skip_if_exists = uri.into();
        self
    }

    /// Set the stage's own client spec.
    pub fn client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(ClientSpec(client.into()));
        self
    }

    /// Consume the builder and produce a [`RunnableStage::Loop`].
    pub fn build(self) -> Result<RunnableStage, SchemaError> {
        if self.max_iterations < 1 {
            return Err(SchemaError::Stage {
                name: self.name.clone(),
                msg: format!("max_iterations must be >= 1, got {}", self.max_iterations),
            });
        }

        let mut attrs = StageAttrs::new(self.name);
        attrs.stage_type = "loop".to_string();
        attrs.skip_if_exists = self.skip_if_exists;
        attrs.client_explicit = self.client.is_some();
        Ok(RunnableStage::Loop {
            attrs,
            max_iterations: self.max_iterations,
            stop_when_exists: self.stop_when_exists,
            interval: self.interval,
            client: self.client,
            body: self.body,
        })
    }

    /// Wrap an already-parsed [`RunnableStage::Loop`] into a builder so
    /// callers can override fields before calling [`build`].
    ///
    /// Panics if `stage` is not a [`RunnableStage::Loop`].
    pub fn from_parsed(stage: RunnableStage) -> Self {
        match stage {
            RunnableStage::Loop {
                attrs,
                max_iterations,
                stop_when_exists,
                interval,
                client,
                body,
            } => LoopBuilder {
                name: attrs.name,
                body,
                max_iterations,
                stop_when_exists,
                interval,
                skip_if_exists: attrs.skip_if_exists,
                client,
            },
            other => panic!(
                "LoopBuilder::from_parsed expected Loop, got {}",
                other.stage_type()
            ),
        }
    }
}
