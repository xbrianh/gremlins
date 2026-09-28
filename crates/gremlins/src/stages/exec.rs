use std::collections::HashMap;

use crate::stages::constants::FRAMEWORK_KEYS;

#[derive(Debug, Clone)]
pub struct Exec {
    pub name: String,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
}

impl Exec {
    /// Parse an `Exec` from a stage mapping.
    ///
    /// Mirrors `PyExec::with_dict`: the `in`/`out` rejection, the same mapping
    /// shapes, and the framework-key collision check — without the `model`
    /// exemption an agent gets, since an exec has no model. The `client` key is
    /// not read here; the client spec belongs to the stage-tree node.
    pub fn from_dict(d: &HashMap<String, serde_json::Value>) -> Result<Exec, String> {
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
            if FRAMEWORK_KEYS.contains(key.as_str()) {
                return Err(format!(
                    "stage {name:?}: option key {key:?} collides with framework substitution variable"
                ));
            }
        }

        Ok(Exec {
            name,
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

impl crate::stages::base::Stage for Exec {
    fn name(&self) -> &str {
        &self.name
    }

    fn stage_type(&self) -> &str {
        "exec"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_implements_stage_trait() {
        let exec = Exec {
            name: "test-exec".to_string(),
            options: HashMap::from([(
                "greeting".to_string(),
                serde_json::Value::String("hi".to_string()),
            )]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("key".to_string(), "value".to_string())]),
        };

        let stage: &dyn crate::stages::base::Stage = &exec;
        assert_eq!(stage.name(), "test-exec");
        assert_eq!(stage.stage_type(), "exec");
        assert_eq!(stage.path(), "");
        assert!(stage.client().is_none());
        assert!(!stage.client_explicit());
        assert!(stage.body().is_empty());
        assert_eq!(stage.bind_map().get("key").unwrap(), "value");
        assert_eq!(
            stage.options().get("greeting").unwrap(),
            &serde_json::Value::String("hi".to_string())
        );
        assert_eq!(stage.skip_if_exists(), "");
    }

    // ---- from_dict tests ----

    fn exec_dict(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn from_dict_parses_options_and_maps() {
        let d = exec_dict(&[
            ("name", serde_json::json!("run")),
            ("options", serde_json::json!({"cmds": ["echo hi"]})),
            (
                "interpolation",
                serde_json::json!({"a": "content(\"artifact://a\")"}),
            ),
            ("bind", serde_json::json!({"b": "artifact://b"})),
        ]);
        let exec = Exec::from_dict(&d).unwrap();
        assert_eq!(exec.name, "run");
        assert_eq!(
            exec.options.get("cmds").unwrap(),
            &serde_json::json!(["echo hi"])
        );
        assert_eq!(
            exec.interpolation_map.get("a").unwrap(),
            "content(\"artifact://a\")"
        );
        assert_eq!(exec.bind_map.get("b").unwrap(), "artifact://b");
    }

    #[test]
    fn from_dict_defaults_are_empty() {
        let exec = Exec::from_dict(&exec_dict(&[])).unwrap();
        assert_eq!(exec.name, "");
        assert!(exec.options.is_empty());
        assert!(exec.interpolation_map.is_empty());
        assert!(exec.bind_map.is_empty());
    }

    #[test]
    fn from_dict_rejects_in_and_out() {
        for key in ["in", "out"] {
            let d = exec_dict(&[
                ("name", serde_json::json!("s")),
                (key, serde_json::json!({})),
            ]);
            let err = Exec::from_dict(&d).unwrap_err();
            assert!(
                err.contains("'in'/'out' keys are no longer supported"),
                "{err}"
            );
        }
    }

    #[test]
    fn from_dict_rejects_non_mapping_bind() {
        let d = exec_dict(&[
            ("name", serde_json::json!("s")),
            ("bind", serde_json::json!("not a mapping")),
        ]);
        let err = Exec::from_dict(&d).unwrap_err();
        assert!(err.contains("'bind' must be a mapping"), "{err}");
    }

    #[test]
    fn from_dict_rejects_null_options() {
        let d = exec_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::Value::Null),
        ]);
        assert!(Exec::from_dict(&d).is_err());
    }

    #[test]
    fn from_dict_rejects_every_framework_option_key_including_model() {
        // Unlike an agent, an exec rejects `model` too.
        for key in ["name", "model", "cwd", "base_ref"] {
            let d = exec_dict(&[
                ("name", serde_json::json!("s")),
                ("options", serde_json::json!({ key: "x" })),
            ]);
            let err = Exec::from_dict(&d).unwrap_err();
            assert!(
                err.contains("collides with framework substitution variable"),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn from_dict_rejects_non_object_options() {
        let d = exec_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::json!([1, 2, 3])),
        ]);
        assert!(Exec::from_dict(&d).is_err());
    }
}
