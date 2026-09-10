use std::collections::HashMap;

use crate::schemas::interpolation;

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

/// Substitute `{var}` tokens in `text` using the same resolution order:
/// string options → extra (bind/interpolation) → framework_subs.
/// Framework subs win on collision.
pub fn substitute_vars(
    text: &str,
    string_options: &HashMap<String, String>,
    extra: &HashMap<String, String>,
    framework_subs: &HashMap<String, String>,
) -> String {
    let mut subs: HashMap<String, String> = HashMap::new();
    subs.extend(string_options.iter().map(|(k, v)| (k.clone(), v.clone())));
    subs.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    subs.extend(framework_subs.iter().map(|(k, v)| (k.clone(), v.clone())));
    interpolation::substitute_vars(text, &subs)
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
        substitute_vars(text, &string_options(self.options()), extra, framework_subs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_vars_basic() {
        let opts = HashMap::new();
        let extra = HashMap::from([("var".to_string(), "world".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("hello {var}", &opts, &extra, &fw);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_substitute_vars_hyphen_normalization() {
        let opts = HashMap::new();
        let extra = HashMap::from([("child_plan".to_string(), "value".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("{child-plan}", &opts, &extra, &fw);
        assert_eq!(result, "value");
    }

    #[test]
    fn test_substitute_vars_framework_overrides() {
        let opts = HashMap::new();
        let extra = HashMap::from([("name".to_string(), "extra".to_string())]);
        let fw = HashMap::from([("name".to_string(), "fw".to_string())]);
        let result = substitute_vars("{name}", &opts, &extra, &fw);
        assert_eq!(result, "fw");
    }

    #[test]
    fn test_substitute_vars_unknown_token() {
        let opts = HashMap::new();
        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = substitute_vars("hello {unknown}", &opts, &extra, &fw);
        assert_eq!(result, "hello {unknown}");
    }

    #[test]
    fn test_substitute_vars_escaped_brace() {
        let opts = HashMap::new();
        let extra = HashMap::from([("x".to_string(), "y".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("\\${x}", &opts, &extra, &fw);
        assert_eq!(result, "\\${x}");
    }

    #[test]
    fn test_substitute_vars_string_opts() {
        let opts = HashMap::from([("foo".to_string(), "opt".to_string())]);
        let extra = HashMap::from([("foo".to_string(), "extra".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("{foo}", &opts, &extra, &fw);
        assert_eq!(result, "extra");
    }

    #[test]
    fn test_substitute_vars_dollar_skip() {
        let opts = HashMap::new();
        let extra = HashMap::from([("x".to_string(), "y".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("${x}", &opts, &extra, &fw);
        assert_eq!(result, "${x}");
    }

    #[test]
    fn test_substitute_vars_doubled_braces() {
        let opts = HashMap::new();
        let extra = HashMap::from([("name".to_string(), "value".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("{{name}}", &opts, &extra, &fw);
        assert_eq!(result, "{value}");
    }

    #[test]
    fn test_substitute_vars_hyphen_keys_direct() {
        let opts = HashMap::from([("review-one".to_string(), "done".to_string())]);
        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = substitute_vars("{review-one}", &opts, &extra, &fw);
        assert_eq!(result, "done");
    }

    #[test]
    fn test_substitute_vars_non_string_options_filtered() {
        let mut opts: HashMap<String, serde_json::Value> = HashMap::new();
        opts.insert("count".to_string(), serde_json::Value::Number(42.into()));
        let str_opts = string_options(&opts);
        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = substitute_vars("{count}", &str_opts, &extra, &fw);
        assert_eq!(result, "{count}");
    }

    #[test]
    fn test_substitute_vars_empty_text() {
        let opts = HashMap::new();
        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = substitute_vars("", &opts, &extra, &fw);
        assert_eq!(result, "");
    }

    #[test]
    fn test_substitute_vars_no_braces() {
        let opts = HashMap::new();
        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = substitute_vars("no braces here", &opts, &extra, &fw);
        assert_eq!(result, "no braces here");
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
