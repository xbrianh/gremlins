//! Builder for [`StageSpec::Agent`].

use std::collections::{HashMap, HashSet};

use crate::builders::artifacts::{InterpolationValue, OutputTarget};
use crate::definition::r#static::expand::key_referenced_in_text;
use crate::definition::ClientSpec;
use crate::schemas::error::SchemaError;
use crate::stage_spec::agent::Agent;
use crate::stage_spec::constants::FRAMEWORK_KEYS;
use crate::stage_spec::node::StageSpec;

/// Build an [`Agent`] stage.
///
/// Every setter consumes `self` and returns `Self`.  `build()` returns
/// `Result` and may fail on validation errors (interpolation syntax,
/// framework key collisions).
///
/// # Example
///
/// ```ignore
/// use gremlins::builders::*;
///
/// let stage = AgentBuilder::new("plan")
///     .prompt("write the plan to {plan}")
///     .output("plan", output("artifact://plan.md"))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct AgentBuilder {
    name: String,
    prompts: Vec<String>,
    options: HashMap<String, serde_json::Value>,
    interpolation_map: HashMap<String, String>,
    outputs_map: HashMap<String, String>,
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
            outputs_map: HashMap::new(),
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

    /// Add an output entry.
    pub fn output(mut self, key: impl Into<String>, target: impl Into<OutputTarget>) -> Self {
        self.outputs_map.insert(key.into(), target.into().into());
        self
    }

    /// Replace the entire outputs map.
    pub fn outputs_map(mut self, map: HashMap<String, String>) -> Self {
        self.outputs_map = map;
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

    /// Set the stage's own client spec.
    pub fn client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(ClientSpec(client.into()));
        self
    }

    /// Consume the builder and produce a [`StageSpec::Agent`].
    pub fn build(self) -> Result<StageSpec, SchemaError> {
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

        // --- Collision check: keys in both outputs: and interpolation: ---
        {
            let output_keys: HashSet<String> = self
                .outputs_map
                .keys()
                .filter(|k| !k.contains('{'))
                .map(|k| k.strip_suffix('?').unwrap_or(k).to_string())
                .collect();
            for interp_key in self.interpolation_map.keys() {
                if interp_key.contains('{') {
                    continue;
                }
                if output_keys.contains(interp_key.as_str()) {
                    return Err(SchemaError::Stage {
                        name: name.clone(),
                        msg: format!(
                            "key {interp_key:?} declared in both outputs: and interpolation: — a stage cannot both produce and consume the same key"
                        ),
                    });
                }
            }
        }

        // --- Unused-key check ---
        {
            // Collect all text: prompts + cmds from options
            let mut text = String::new();
            for p in &self.prompts {
                text.push_str(p);
                text.push('\n');
            }
            if let Some(cmds) = self.options.get("cmds").and_then(|v| v.as_array()) {
                for cmd in cmds {
                    if let Some(s) = cmd.as_str() {
                        text.push_str(s);
                        text.push('\n');
                    }
                }
            }

            // Check interpolation keys
            for key in self.interpolation_map.keys() {
                if key.contains('{') {
                    continue;
                }
                if !key_referenced_in_text(key, &text) {
                    return Err(SchemaError::Stage {
                        name: name.clone(),
                        msg: format!(
                            "key {key:?} declared in interpolation: is not referenced in any prompt or command"
                        ),
                    });
                }
            }

            // Check output keys
            for key in self.outputs_map.keys() {
                if key.contains('{') {
                    continue;
                }
                let stripped = key.strip_suffix('?').unwrap_or(key);
                if key_referenced_in_text(stripped, &text) {
                    continue;
                }
                return Err(SchemaError::Stage {
                    name: name.clone(),
                    msg: format!(
                        "key {key:?} declared in outputs: is not referenced in any prompt or command"
                    ),
                });
            }
        }

        let stage = Agent {
            name,
            prompts: self.prompts,
            options: self.options,
            interpolation_map: self.interpolation_map,
            outputs_map: self.outputs_map,
        };
        Ok(StageSpec::Agent {
            stage,
            client: self.client,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::artifacts::output;

    #[test]
    fn all_keys_referenced_ok() {
        AgentBuilder::new("test")
            .prompt("use {foo} and {bar}")
            .output("foo", output("artifact://foo.txt"))
            .interpolate(
                "bar",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://bar.txt\")",
                ),
            )
            .build()
            .unwrap();
    }

    #[test]
    fn unused_output_key_error() {
        let err = AgentBuilder::new("test")
            .prompt("use {foo}")
            .output("foo", output("artifact://foo.txt"))
            .output("unused", output("artifact://unused.txt"))
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("unused"), "{err}");
        assert!(err.to_string().contains("outputs:"), "{err}");
    }

    #[test]
    fn unused_interpolation_key_error() {
        let err = AgentBuilder::new("test")
            .prompt("use {foo}")
            .interpolate(
                "foo",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://foo.txt\")",
                ),
            )
            .interpolate(
                "unused",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://unused.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("unused"), "{err}");
        assert!(err.to_string().contains("interpolation:"), "{err}");
    }

    #[test]
    fn output_interp_collision_error() {
        let err = AgentBuilder::new("test")
            .prompt("use {shared}")
            .output("shared", output("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both outputs:"), "{err}");
    }

    #[test]
    fn optional_output_collides_with_interp() {
        let err = AgentBuilder::new("test")
            .prompt("use {shared}")
            .output("shared?", output("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both outputs:"), "{err}");
    }

    #[test]
    fn optional_output_referenced_via_stripped_form_ok() {
        AgentBuilder::new("test")
            .prompt("use {foo}")
            .output("foo?", output("artifact://foo.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn hyphen_underscore_normalization_ok() {
        AgentBuilder::new("test")
            .prompt("use {child-plan}")
            .output("child_plan", output("artifact://plan.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn framework_template_key_skipped() {
        // Keys containing `{` are framework template keys and should be skipped.
        AgentBuilder::new("test")
            .prompt("model: {model}")
            .interpolate(
                "{name}",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://name.txt\")",
                ),
            )
            .build()
            .unwrap();
    }
}
