//! Builder for [`RunnableStage::Exec`].

use std::collections::{HashMap, HashSet};

use crate::builders::artifacts::{BindTarget, InterpolationValue};
use crate::schemas::error::SchemaError;
use crate::schemas::expand::key_referenced_in_text;
use crate::stages::composite::ClientSpec;
use crate::stages::constants::FRAMEWORK_KEYS;
use crate::stages::exec::Exec;
use crate::stages::node::RunnableStage;

/// Build an [`Exec`] stage.
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

        // --- Collision check: keys in both bind: and interpolation: ---
        {
            let bind_keys: HashSet<String> = self
                .bind_map
                .keys()
                .filter(|k| !k.contains('{'))
                .map(|k| k.strip_suffix('?').unwrap_or(k).to_string())
                .collect();
            for interp_key in self.interpolation_map.keys() {
                if interp_key.contains('{') {
                    continue;
                }
                if bind_keys.contains(interp_key.as_str()) {
                    return Err(SchemaError::Stage {
                        name: name.clone(),
                        msg: format!(
                            "key {interp_key:?} declared in both bind: and interpolation: — a stage cannot both produce and consume the same key"
                        ),
                    });
                }
            }
        }

        // --- Unused-key check ---
        {
            // Collect all text from cmds
            let mut text = String::new();
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

            // Check bind keys
            for key in self.bind_map.keys() {
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
                        "key {key:?} declared in bind: is not referenced in any prompt or command"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::artifacts::artifact;

    #[test]
    fn all_keys_referenced_ok() {
        ExecBuilder::new("test")
            .cmd("cat {foo} {bar}")
            .bind("foo", artifact("artifact://foo.txt"))
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
    fn unused_bind_key_error() {
        let err = ExecBuilder::new("test")
            .cmd("cat {foo}")
            .bind("foo", artifact("artifact://foo.txt"))
            .bind("unused", artifact("artifact://unused.txt"))
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("unused"), "{err}");
        assert!(err.to_string().contains("bind:"), "{err}");
    }

    #[test]
    fn unused_interpolation_key_error() {
        let err = ExecBuilder::new("test")
            .cmd("cat {foo}")
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
    fn bind_interp_collision_error() {
        let err = ExecBuilder::new("test")
            .cmd("cat {shared}")
            .bind("shared", artifact("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both bind:"), "{err}");
    }

    #[test]
    fn optional_bind_collides_with_interp() {
        let err = ExecBuilder::new("test")
            .cmd("cat {shared}")
            .bind("shared?", artifact("artifact://shared.txt"))
            .interpolate(
                "shared",
                crate::builders::artifacts::InterpolationValue::from(
                    "content(\"artifact://shared.txt\")",
                ),
            )
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("both bind:"), "{err}");
    }

    #[test]
    fn optional_bind_referenced_via_stripped_form_ok() {
        ExecBuilder::new("test")
            .cmd("cat {foo}")
            .bind("foo?", artifact("artifact://foo.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn hyphen_underscore_normalization_ok() {
        ExecBuilder::new("test")
            .cmd("cat {child-plan}")
            .bind("child_plan", artifact("artifact://plan.txt"))
            .build()
            .unwrap();
    }

    #[test]
    fn framework_template_key_skipped() {
        ExecBuilder::new("test")
            .cmd("echo {model}")
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
