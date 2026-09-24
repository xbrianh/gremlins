//! Builder for [`RunnableStage::Agent`].

use std::collections::HashMap;

use crate::builders::artifacts::{BindTarget, InterpolationValue};
use crate::schemas::error::SchemaError;
use crate::stages::agent::Agent;
use crate::stages::composite::ClientSpec;
use crate::stages::constants::FRAMEWORK_KEYS;
use crate::stages::node::RunnableStage;

/// Build an [`Agent`] stage.
///
/// Every setter consumes `self` and returns `Self`.  `build()` is infallible.
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let stage = AgentBuilder::new("plan")
///     .prompt("write the plan to {plan}")
///     .bind("plan", artifact("artifact://plan.md"))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct AgentBuilder {
    name: String,
    prompts: Vec<String>,
    options: HashMap<String, serde_json::Value>,
    interpolation_map: HashMap<String, String>,
    bind_map: HashMap<String, String>,
    skip_if_exists: String,
    client: Option<ClientSpec>,
}

impl AgentBuilder {
    /// Start building an agent stage with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        AgentBuilder {
            name: name.into(),
            prompts: Vec::new(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
            skip_if_exists: String::new(),
            client: None,
        }
    }

    /// Set the stage name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Append a prompt string.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompts.push(prompt.into());
        self
    }

    /// Replace all prompts.
    pub fn prompts(mut self, prompts: Vec<String>) -> Self {
        self.prompts = prompts;
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

    /// Replace all options.
    pub fn options(mut self, options: HashMap<String, serde_json::Value>) -> Self {
        self.options = options;
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

    /// Consume the builder and produce a [`RunnableStage::Agent`].
    pub fn build(self) -> Result<RunnableStage, SchemaError> {
        let name = self.name.clone();

        crate::artifacts::resolve::validate_interpolation_map(&self.interpolation_map, &name)
            .map_err(|msg| SchemaError::Stage {
                name: name.clone(),
                msg,
            })?;

        for key in self.options.keys() {
            if FRAMEWORK_KEYS.contains(key.as_str()) && key != "model" {
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!(
                        "option key {key:?} collides with framework substitution variable"
                    ),
                });
            }
        }

        let stage = Agent {
            name,
            prompts: self.prompts,
            options: self.options,
            interpolation_map: self.interpolation_map,
            bind_map: self.bind_map,
        };
        Ok(RunnableStage::Agent {
            stage,
            skip_if_exists: self.skip_if_exists,
            client: self.client,
        })
    }
}
