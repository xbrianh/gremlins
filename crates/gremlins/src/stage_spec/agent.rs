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
    pub outputs_map: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_struct_fields() {
        let agent = Agent {
            name: "test-agent".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::from([(
                "key".to_string(),
                serde_json::Value::String("val".to_string()),
            )]),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("bind-key".to_string(), "bind-val".to_string())]),
        };

        assert_eq!(agent.name, "test-agent");
        assert_eq!(agent.prompts, vec!["hi".to_string()]);
        assert_eq!(agent.outputs_map.get("bind-key").unwrap(), "bind-val");
        assert_eq!(
            agent.options.get("key").unwrap(),
            &serde_json::Value::String("val".to_string())
        );
    }
}
