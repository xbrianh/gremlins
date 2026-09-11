use std::collections::HashMap;

use crate::schemas::interpolation::Interpolator;

/// Extract string-valued entries from an options map, filtering out
/// non-string JSON values (numbers, booleans, arrays, etc.).
pub fn string_options(options: &HashMap<String, serde_json::Value>) -> HashMap<String, String> {
    options
        .iter()
        .filter_map(|(k, v)| {
            if let serde_json::Value::String(s) = v {
                Some((k.clone(), s.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// Trait representing the contract every Rust stage implements.
/// Mirrors the Python `Stage` ABC + `StageProtocol` surface.
pub trait Stage: Send + Sync {
    fn name(&self) -> &str;
    fn stage_type(&self) -> &str;
    fn path(&self) -> &str;
    fn set_path(&mut self, path: &str);
    fn client(&self) -> Option<&str>;
    fn set_client(&mut self, client: Option<String>);
    fn client_explicit(&self) -> bool;
    fn set_client_explicit(&mut self, explicit: bool);
    fn body(&self) -> &[Box<dyn Stage>];
    fn bind_map(&self) -> &HashMap<String, String>;
    fn options(&self) -> &HashMap<String, serde_json::Value>;
    fn skip_if_exists(&self) -> &str;
    fn set_skip_if_exists(&mut self, skip: String);

    /// Substitute `{var}` tokens using the standard resolution order:
    /// string options → extra → framework subs (framework wins).
    fn substitute_vars(
        &self,
        text: &str,
        extra: &HashMap<String, String>,
        framework_subs: &HashMap<String, String>,
    ) -> String {
        Interpolator::new()
            .with_map(&string_options(self.options()))
            .with_map(extra)
            .with_map(framework_subs)
            .text(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_options_filters_non_string_values() {
        let mut opts: HashMap<String, serde_json::Value> = HashMap::new();
        opts.insert("count".to_string(), serde_json::Value::Number(42.into()));
        let str_opts = string_options(&opts);
        assert!(str_opts.is_empty());
        let result = Interpolator::new().with_map(&str_opts).text("{count}");
        assert_eq!(result, "{count}");
    }

    #[test]
    fn test_stage_trait_substitute_vars_default_impl() {
        struct MinimalStage {
            options: HashMap<String, serde_json::Value>,
            bind_map: HashMap<String, String>,
        }

        impl Stage for MinimalStage {
            fn name(&self) -> &str {
                "minimal"
            }
            fn stage_type(&self) -> &str {
                "minimal"
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
            fn body(&self) -> &[Box<dyn Stage>] {
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

        let stage = MinimalStage {
            options: HashMap::from([(
                "greeting".to_string(),
                serde_json::Value::String("hi".to_string()),
            )]),
            bind_map: HashMap::new(),
        };
        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = stage.substitute_vars("{greeting}", &extra, &fw);
        assert_eq!(result, "hi");
    }
}
