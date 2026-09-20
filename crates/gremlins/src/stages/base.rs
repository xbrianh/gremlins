use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

static VAR_SUB_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([-\w]+)\}").unwrap());

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
/// Framework subs win on collision. Hyphen-normalized variants are added
/// for underscore keys.
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

/// Sanitize a key into a `GREMLINS_<KEY>` environment variable name:
/// uppercase, every non-ASCII-alphanumeric character mapped to `_`.
fn sanitize_key(key: &str) -> String {
    let sanitized: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("GREMLINS_{}", sanitized)
}

/// Substitute `{var}` tokens in `text` with `$GREMLINS_<KEY>` environment
/// variable references instead of literal values. Populates `env_map` with
/// `GREMLINS_<KEY> → value` entries and tracks name assignments in
/// `key_to_env` and `used_names` so that repeated calls with the same
/// accumulators produce consistent env var names.
///
/// Uses the same resolution order as [`substitute_vars`]: string options →
/// extra → framework_subs (framework wins). Hyphen-normalized variants are
/// added for underscore keys. Token semantics: `${key}` left verbatim,
/// `{{key}}` collapses to `{$GREMLINS_KEY}`, unknown tokens left verbatim.
pub fn substitute_vars_to_env(
    text: &str,
    string_options: &HashMap<String, String>,
    extra: &HashMap<String, String>,
    framework_subs: &HashMap<String, String>,
    env_map: &mut HashMap<String, String>,
    key_to_env: &mut HashMap<String, String>,
    used_names: &mut HashMap<String, u32>,
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

            // Resolve the key to a value, then look up or assign an env var name.
            let resolve = |k: &str| -> Option<(&String, String)> {
                subs.get(k).map(|val| (val, k.to_string()))
            };

            if let Some((val, resolved_key)) = resolve(key).or_else(|| {
                let alt = key.replace('-', "_");
                if alt != key {
                    resolve(&alt)
                } else {
                    None
                }
            }) {
                // Normalize to underscores so that hyphen and underscore
                // aliases (e.g. {child-plan} and {child_plan}) share the
                // same env-var name.
                let canonical_key = resolved_key.replace('-', "_");
                if let Some(env_name) = key_to_env.get(&canonical_key) {
                    return format!("${{{}}}", env_name);
                }
                let base = sanitize_key(&canonical_key);
                // Allocate a unique env-var name, checking env_map so that
                // keys whose sanitized names collide with an already-assigned
                // suffixed name (e.g. a_1 vs a-1 vs a_1_1) never overwrite.
                let mut suffix: u32 = 0;
                let env_name = loop {
                    let candidate = if suffix == 0 {
                        base.clone()
                    } else {
                        format!("{}_{}", base, suffix)
                    };
                    if !env_map.contains_key(&candidate) {
                        break candidate;
                    }
                    suffix += 1;
                };
                // Track the count so repeated calls with the same
                // accumulators continue from the right suffix.
                used_names.insert(base, suffix + 1);
                key_to_env.insert(canonical_key, env_name.clone());
                env_map.insert(env_name.clone(), val.clone());
                return format!("${{{}}}", env_name);
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

    // --- sanitize_key ---

    #[test]
    fn test_sanitize_key_basic() {
        assert_eq!(sanitize_key("pr_title"), "GREMLINS_PR_TITLE");
    }

    #[test]
    fn test_sanitize_key_hyphen() {
        assert_eq!(sanitize_key("pr-title"), "GREMLINS_PR_TITLE");
    }

    #[test]
    fn test_sanitize_key_special_chars() {
        assert_eq!(sanitize_key("a.b!c@d#e"), "GREMLINS_A_B_C_D_E");
    }

    #[test]
    fn test_sanitize_key_empty() {
        assert_eq!(sanitize_key(""), "GREMLINS_");
    }

    // --- substitute_vars_to_env ---

    #[test]
    fn test_substitute_vars_to_env_basic() {
        let opts = HashMap::new();
        let extra = HashMap::from([("var".to_string(), "world".to_string())]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "hello {var}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "hello ${GREMLINS_VAR}");
        assert_eq!(env_map.get("GREMLINS_VAR").unwrap(), "world");
    }

    #[test]
    fn test_substitute_vars_to_env_dollar_skip() {
        let opts = HashMap::new();
        let extra = HashMap::from([("x".to_string(), "y".to_string())]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "${x}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "${x}");
        assert!(env_map.is_empty());
    }

    #[test]
    fn test_substitute_vars_to_env_unknown_token() {
        let opts = HashMap::new();
        let extra = HashMap::new();
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "hello {unknown}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "hello {unknown}");
        assert!(env_map.is_empty());
    }

    #[test]
    fn test_substitute_vars_to_env_framework_overrides() {
        let opts = HashMap::new();
        let extra = HashMap::from([("name".to_string(), "extra".to_string())]);
        let fw = HashMap::from([("name".to_string(), "fw".to_string())]);
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "{name}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "${GREMLINS_NAME}");
        assert_eq!(env_map.get("GREMLINS_NAME").unwrap(), "fw");
    }

    #[test]
    fn test_substitute_vars_to_env_hyphen_normalization() {
        let opts = HashMap::new();
        let extra = HashMap::from([("child_plan".to_string(), "value".to_string())]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "{child-plan}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "${GREMLINS_CHILD_PLAN}");
        assert_eq!(env_map.get("GREMLINS_CHILD_PLAN").unwrap(), "value");
    }

    #[test]
    fn test_substitute_vars_to_env_collision_suffix() {
        // When two keys are hyphen/underscore aliases of each other, they
        // share a single env var (the first-encountered value wins).
        // The alias system adds "a-b" as an alias for "a_b", so both
        // tokens resolve to the same canonical key and share one env var.
        let opts = HashMap::new();
        let extra = HashMap::from([
            ("a-b".to_string(), "first".to_string()),
            ("a_b".to_string(), "second".to_string()),
        ]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "{a-b} {a_b}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "${GREMLINS_A_B} ${GREMLINS_A_B}");
        assert_eq!(env_map.len(), 1);
    }

    #[test]
    fn test_substitute_vars_to_env_hyphen_alias_shared_env() {
        // {child_plan} and {child-plan} are aliases for the same variable
        // and must share a single env var.
        let opts = HashMap::new();
        let extra = HashMap::from([("child_plan".to_string(), "shared".to_string())]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "{child_plan} {child-plan}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "${GREMLINS_CHILD_PLAN} ${GREMLINS_CHILD_PLAN}");
        assert_eq!(env_map.len(), 1);
        assert_eq!(env_map.get("GREMLINS_CHILD_PLAN").unwrap(), "shared");
    }

    #[test]
    fn test_substitute_vars_to_env_env_map_collision_avoided() {
        // A key whose sanitized name collides with an already-assigned
        // suffixed name must not overwrite it.
        //
        // a_1      → canonical a_1  → sanitize GREMLINS_A_1
        // a-1      → canonical a_1  → same env var (alias)
        // a_1_1    → canonical a_1_1 → sanitize GREMLINS_A_1_1 (no collision)
        //
        // The real danger is a_1 vs a-1 vs a_1_1 where the old counter-
        // based allocator could assign GREMLINS_A_1_1 to both a-1 (as
        // suffix _1 on base GREMLINS_A_1) and a_1_1 (as unsuffixed base).
        let opts = HashMap::new();
        let extra = HashMap::from([
            ("a_1".to_string(), "v1".to_string()),
            ("a-1".to_string(), "v1-alias".to_string()),
            ("a_1_1".to_string(), "v2".to_string()),
        ]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "{a_1} {a-1} {a_1_1}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        // a_1 and a-1 share GREMLINS_A_1; a_1_1 gets GREMLINS_A_1_1.
        assert_eq!(result, "${GREMLINS_A_1} ${GREMLINS_A_1} ${GREMLINS_A_1_1}");
        assert_eq!(env_map.len(), 2);
        // a_1 is encountered first, so its value wins for the shared env var.
        assert_eq!(env_map.get("GREMLINS_A_1").unwrap(), "v1");
        assert_eq!(env_map.get("GREMLINS_A_1_1").unwrap(), "v2");
    }

    #[test]
    fn test_sanitize_key_unicode_rejected() {
        // Unicode alphanumerics (e.g. é) must be mapped to '_', not left
        // as non-ASCII characters in the env var name.
        assert_eq!(sanitize_key("café"), "GREMLINS_CAF_");
        assert_eq!(sanitize_key("niño"), "GREMLINS_NI_O");
    }

    #[test]
    fn test_substitute_vars_to_env_injection_payload_not_escaped() {
        // The value is placed verbatim in the env map — no shell escaping.
        let opts = HashMap::new();
        let extra = HashMap::from([(
            "pr_title".to_string(),
            "`touch /tmp/pwned`; $(rm -rf /)".to_string(),
        )]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "printf '%s' \"{pr_title}\"",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "printf '%s' \"${GREMLINS_PR_TITLE}\"");
        assert_eq!(
            env_map.get("GREMLINS_PR_TITLE").unwrap(),
            "`touch /tmp/pwned`; $(rm -rf /)"
        );
    }

    #[test]
    fn test_substitute_vars_to_env_same_key_reused() {
        // Same key used twice gets the same env var name.
        let opts = HashMap::new();
        let extra = HashMap::from([("x".to_string(), "val".to_string())]);
        let fw = HashMap::new();
        let mut env_map = HashMap::new();
        let mut key_to_env = HashMap::new();
        let mut used_names = HashMap::new();
        let result = substitute_vars_to_env(
            "{x} {x}",
            &opts,
            &extra,
            &fw,
            &mut env_map,
            &mut key_to_env,
            &mut used_names,
        );
        assert_eq!(result, "${GREMLINS_X} ${GREMLINS_X}");
        assert_eq!(env_map.len(), 1);
        assert_eq!(env_map.get("GREMLINS_X").unwrap(), "val");
    }
}
