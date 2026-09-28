use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Agent struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: String,
    pub prompts: Vec<String>,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
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
