//! Builder for [`RunnableStage::Exec`].

use std::collections::HashMap;

use crate::builders::artifacts::{BindTarget, InterpolationValue};
use crate::schemas::error::SchemaError;
use crate::stages::composite::ClientSpec;
use crate::stages::constants::FRAMEWORK_KEYS;
use crate::stages::exec::Exec;
use crate::stages::node::RunnableStage;

/// Build an [`Exec`] stage.
///
/// Every setter consumes `self` and returns `Self`.  `build()` is infallible.
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let stage = ExecBuilder::new("run")
///     .cmd("cat {plan}")
///     .interpolate("plan", content("artifact://plan.md"))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct ExecBuilder {
    name: String,
    options: HashMap<String, serde_json::Value>,
    interpolation_map: HashMap<String, String>,
    bind_map: HashMap<String, String>,
    skip_if_exists: String,
    client: Option<ClientSpec>,
}

impl ExecBuilder {
    /// Start building an exec stage with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        ExecBuilder {
            name: name.into(),
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

    /// Consume the builder and produce a [`RunnableStage::Exec`].
    pub fn build(self) -> Result<RunnableStage, SchemaError> {
        let name = self.name.clone();

        crate::artifacts::resolve::validate_interpolation_map(&self.interpolation_map, &name)
            .map_err(|msg| SchemaError::Stage {
                name: name.clone(),
                msg,
            })?;

        for key in self.options.keys() {
            if FRAMEWORK_KEYS.contains(key.as_str()) {
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!(
                        "option key {key:?} collides with framework substitution variable"
                    ),
                });
            }
        }

        let stage = Exec {
            name,
            options: self.options,
            interpolation_map: self.interpolation_map,
            bind_map: self.bind_map,
        };
        Ok(RunnableStage::Exec {
            stage,
            skip_if_exists: self.skip_if_exists,
            client: self.client,
        })
    }
}
