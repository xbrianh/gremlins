use std::collections::HashMap;

use crate::stages::constants::FRAMEWORK_KEYS;

// ---------------------------------------------------------------------------
// Agent struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub prompts: Vec<String>,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
}

impl Agent {
    /// Parse an `Agent` from a stage mapping.
    ///
    /// Mirrors `PyAgent::with_dict` field for field — the `in`/`out`
    /// rejection, the framework-key collision check (`model` excepted, since an
    /// agent may target one), and the requirement that `prompt` be a list of
    /// strings. The `client` key is deliberately not read here: the client spec
    /// is a definition concern and lives on the stage-tree node.
    pub fn from_dict(d: &HashMap<String, serde_json::Value>) -> Result<Agent, String> {
        let name = d
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        if d.contains_key("in") || d.contains_key("out") {
            return Err(format!(
                "stage {name:?}: 'in'/'out' keys are no longer supported; \
                 use 'interpolation'/'bind' with URI values"
            ));
        }

        let interpolation_map = string_mapping(d, "interpolation", &name)?;
        crate::artifacts::resolve::validate_interpolation_map(&interpolation_map, &name)?;
        let bind_map = string_mapping(d, "bind", &name)?;

        let options = match d.get("options") {
            None => HashMap::new(),
            Some(serde_json::Value::Object(options)) => options
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            Some(_) => {
                return Err(format!(
                    "stage {name:?}: 'options' must be a mapping of string keys to JSON-serializable values"
                ))
            }
        };

        for key in options.keys() {
            if FRAMEWORK_KEYS.contains(key.as_str()) && key != "model" {
                return Err(format!(
                    "stage {name:?}: option key {key:?} collides with framework substitution variable"
                ));
            }
        }

        let prompts = match d.get("prompt") {
            None => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(String::from).ok_or_else(|| {
                        format!("stage {name:?}: 'prompt' must be a list of strings")
                    })
                })
                .collect::<Result<Vec<String>, String>>()?,
            Some(_) => {
                return Err(format!(
                    "stage {name:?}: 'prompt' must be a list of strings"
                ))
            }
        };

        Ok(Agent {
            name,
            prompts,
            options,
            interpolation_map,
            bind_map,
        })
    }
}

/// Read a string-to-string mapping field: absent is empty, and a present value
/// must be a mapping whose entries are all strings.
fn string_mapping(
    d: &HashMap<String, serde_json::Value>,
    field: &str,
    name: &str,
) -> Result<HashMap<String, String>, String> {
    match d.get(field) {
        None => Ok(HashMap::new()),
        Some(serde_json::Value::Object(entries)) => entries
            .iter()
            .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
            .collect::<Option<HashMap<String, String>>>()
            .ok_or_else(|| format!("stage {name:?}: '{field}' must be a mapping")),
        Some(_) => Err(format!("stage {name:?}: '{field}' must be a mapping")),
    }
}

// ---------------------------------------------------------------------------
// Stage trait impl
// ---------------------------------------------------------------------------

impl crate::stages::base::Stage for Agent {
    fn name(&self) -> &str {
        &self.name
    }

    fn stage_type(&self) -> &str {
        "agent"
    }

    fn path(&self) -> &str {
        ""
    }

    fn set_path(&mut self, _path: &str) {}

    fn client(&self) -> Option<&str> {
        None
    }

    fn set_client(&mut self, _client: Option<String>) {}

    fn client_explicit(&self) -> bool {
        false
    }

    fn set_client_explicit(&mut self, _explicit: bool) {}

    fn body(&self) -> &[Box<dyn crate::stages::base::Stage>] {
        &[]
    }

    fn bind_map(&self) -> &HashMap<String, String> {
        &self.bind_map
    }

    fn options(&self) -> &HashMap<String, serde_json::Value> {
        &self.options
    }

    fn skip_if_exists(&self) -> &str {
        ""
    }

    fn set_skip_if_exists(&mut self, _skip: String) {}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- from_dict tests ----

    fn agent_dict(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn from_dict_parses_prompt_and_maps() {
        let d = agent_dict(&[
            ("name", serde_json::json!("plan")),
            ("prompt", serde_json::json!(["first", "second"])),
            ("interpolation", serde_json::json!({"a": "artifact://a"})),
            ("bind", serde_json::json!({"b": "artifact://b"})),
            ("options", serde_json::json!({"model": "openai:gpt-5"})),
        ]);
        let agent = Agent::from_dict(&d).unwrap();
        assert_eq!(agent.name, "plan");
        assert_eq!(
            agent.prompts,
            vec!["first".to_string(), "second".to_string()]
        );
        assert_eq!(agent.interpolation_map.get("a").unwrap(), "artifact://a");
        assert_eq!(agent.bind_map.get("b").unwrap(), "artifact://b");
        assert_eq!(
            agent.options.get("model").unwrap(),
            &serde_json::json!("openai:gpt-5")
        );
    }

    #[test]
    fn from_dict_defaults_are_empty() {
        let agent = Agent::from_dict(&agent_dict(&[])).unwrap();
        assert_eq!(agent.name, "");
        assert!(agent.prompts.is_empty());
        assert!(agent.options.is_empty());
        assert!(agent.interpolation_map.is_empty());
        assert!(agent.bind_map.is_empty());
    }

    #[test]
    fn from_dict_rejects_in_and_out() {
        for key in ["in", "out"] {
            let d = agent_dict(&[
                ("name", serde_json::json!("s")),
                (key, serde_json::json!({})),
            ]);
            let err = Agent::from_dict(&d).unwrap_err();
            assert!(
                err.contains("'in'/'out' keys are no longer supported"),
                "{err}"
            );
        }
    }

    #[test]
    fn from_dict_rejects_non_mapping_interpolation() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("interpolation", serde_json::json!(["not", "a", "mapping"])),
        ]);
        let err = Agent::from_dict(&d).unwrap_err();
        assert!(err.contains("'interpolation' must be a mapping"), "{err}");
    }

    #[test]
    fn from_dict_rejects_null_interpolation() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("interpolation", serde_json::Value::Null),
        ]);
        assert!(Agent::from_dict(&d).is_err());
    }

    #[test]
    fn from_dict_rejects_non_string_bind_value() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("bind", serde_json::json!({"a": 42})),
        ]);
        let err = Agent::from_dict(&d).unwrap_err();
        assert!(err.contains("'bind' must be a mapping"), "{err}");
    }

    #[test]
    fn from_dict_rejects_framework_option_key_but_allows_model() {
        let bad = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::json!({"cwd": "/tmp"})),
        ]);
        let err = Agent::from_dict(&bad).unwrap_err();
        assert!(
            err.contains("collides with framework substitution variable"),
            "{err}"
        );

        let ok = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::json!({"model": "openai:gpt-5"})),
        ]);
        assert!(Agent::from_dict(&ok).is_ok());
    }

    #[test]
    fn from_dict_rejects_non_string_prompt_item() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("prompt", serde_json::json!(["ok", 7])),
        ]);
        let err = Agent::from_dict(&d).unwrap_err();
        assert!(err.contains("'prompt' must be a list of strings"), "{err}");
    }

    #[test]
    fn from_dict_rejects_missing_type_shape() {
        let d = agent_dict(&[("prompt", serde_json::json!("not a list"))]);
        assert!(Agent::from_dict(&d).is_err());
    }

    // ---- Stage trait forwarding ----

    #[test]
    fn test_agent_implements_stage_trait() {
        let agent = Agent {
            name: "test-agent".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::from([(
                "key".to_string(),
                serde_json::Value::String("val".to_string()),
            )]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("bind-key".to_string(), "bind-val".to_string())]),
        };

        let stage: &dyn crate::stages::base::Stage = &agent;
        assert_eq!(stage.name(), "test-agent");
        assert_eq!(stage.stage_type(), "agent");
        assert_eq!(stage.path(), "");
        assert!(stage.client().is_none());
        assert!(!stage.client_explicit());
        assert!(stage.body().is_empty());
        assert_eq!(stage.bind_map().get("bind-key").unwrap(), "bind-val");
        assert_eq!(
            stage.options().get("key").unwrap(),
            &serde_json::Value::String("val".to_string())
        );
        assert_eq!(stage.skip_if_exists(), "");
    }
}
