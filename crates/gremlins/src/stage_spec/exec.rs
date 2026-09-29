use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Exec {
    pub name: String,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_struct_fields() {
        let exec = Exec {
            name: "test-exec".to_string(),
            options: HashMap::from([(
                "greeting".to_string(),
                serde_json::Value::String("hi".to_string()),
            )]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("key".to_string(), "value".to_string())]),
        };

        assert_eq!(exec.name, "test-exec");
        assert_eq!(exec.bind_map.get("key").unwrap(), "value");
        assert_eq!(
            exec.options.get("greeting").unwrap(),
            &serde_json::Value::String("hi".to_string())
        );
    }
}
