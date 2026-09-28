use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

static VAR_SUB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([-\w]+)\}").unwrap());

/// Extract string-valued entries from an options map, filtering out
/// non-string JSON values (numbers, booleans, arrays, etc.).
pub(crate) fn string_options(
    options: &HashMap<String, serde_json::Value>,
) -> HashMap<String, String> {
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
/// Framework subs win on collision. Hyphen-normalized variants are added
/// for underscore keys.
pub(crate) fn substitute_vars(
    text: &str,
    string_options: &HashMap<String, String>,
    extra: &HashMap<String, String>,
    framework_subs: &HashMap<String, String>,
) -> String {
    let mut subs: HashMap<String, String> = HashMap::new();
    subs.extend(string_options.iter().map(|(k, v)| (k.clone(), v.clone())));
    subs.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    subs.extend(framework_subs.iter().map(|(k, v)| (k.clone(), v.clone())));

    let hyphenated: Vec<(String, String)> = subs
        .iter()
        .filter_map(|(k, v)| {
            if k.contains('_') {
                Some((k.replace('_', "-"), v.clone()))
            } else {
                None
            }
        })
        .collect();
    for (hk, hv) in hyphenated {
        subs.entry(hk).or_insert(hv);
    }

    VAR_SUB_RE
        .replace_all(text, |caps: &regex::Captures| {
            let start = caps.get(0).unwrap().start();
            if start > 0 && text.as_bytes()[start - 1] == b'$' {
                return caps.get(0).unwrap().as_str().to_string();
            }
            let key = caps.get(1).unwrap().as_str();
            if let Some(val) = subs.get(key) {
                return val.clone();
            }
            let alt = key.replace('-', "_");
            if let Some(val) = subs.get(&alt) {
                return val.clone();
            }
            caps.get(0).unwrap().as_str().to_string()
        })
        .to_string()
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
