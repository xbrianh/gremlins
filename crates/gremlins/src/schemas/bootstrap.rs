use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::schemas::error::SchemaError;
use crate::schemas::interpolation::{HereDocInterpolation, Interpolator};

const VALID_SOURCE_TYPES: [&str; 2] = ["filepath", "string"];
const BOOTSTRAP_KEYS: [&str; 5] = ["source", "launch_cmds", "cmds", "cli_out", "env"];

#[derive(Debug, Clone)]
pub struct InputSource {
    pub name: String,
    pub types: Vec<String>,
    pub optional: bool,
}

impl InputSource {
    pub fn new(name: String, types: Vec<String>, optional: bool) -> Result<Self, SchemaError> {
        if types.is_empty() {
            return Err(SchemaError::InputSource {
                name,
                msg: "types list must not be empty".to_string(),
            });
        }
        let valid: HashSet<&str> = VALID_SOURCE_TYPES.iter().copied().collect();
        for t in &types {
            if !valid.contains(t.as_str()) {
                return Err(SchemaError::InputSource {
                    name: name.clone(),
                    msg: format!("unknown type {t:?}. Supported types: filepath, string"),
                });
            }
        }
        Ok(InputSource {
            name,
            types,
            optional,
        })
    }
}

#[derive(Debug, Clone)]
pub struct InputSources {
    pub sources: HashMap<String, InputSource>,
}

impl InputSources {
    pub fn new(sources: HashMap<String, InputSource>) -> Self {
        InputSources { sources }
    }

    pub fn from_yaml(raw: &serde_yaml::Mapping) -> Result<Self, SchemaError> {
        let mut sources: HashMap<String, InputSource> = HashMap::new();
        for (key_val, entry_val) in raw {
            let key = key_val
                .as_str()
                .ok_or_else(|| SchemaError::Generic("source keys must be strings".to_string()))?;
            let entry_map = entry_val
                .as_mapping()
                .ok_or_else(|| SchemaError::InputSource {
                    name: key.to_string(),
                    msg: format!("expected a mapping, got {:?}", entry_val),
                })?;

            let type_raw = entry_map
                .get(serde_yaml::Value::String("type".to_string()))
                .ok_or_else(|| SchemaError::InputSource {
                    name: key.to_string(),
                    msg: "missing required 'type' field".to_string(),
                })?;

            let types: Vec<String> = if let Some(s) = type_raw.as_str() {
                vec![s.to_string()]
            } else if let Some(seq) = type_raw.as_sequence() {
                if seq.is_empty() {
                    return Err(SchemaError::InputSource {
                        name: key.to_string(),
                        msg: "type list must not be empty".to_string(),
                    });
                }
                seq.iter()
                    .map(|v| {
                        v.as_str()
                            .map(String::from)
                            .ok_or_else(|| SchemaError::InputSource {
                                name: key.to_string(),
                                msg: "all type entries must be strings".to_string(),
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                return Err(SchemaError::InputSource {
                    name: key.to_string(),
                    msg: "'type' must be a string or list of strings".to_string(),
                });
            };

            let optional = match entry_map.get(serde_yaml::Value::String("optional".to_string())) {
                Some(v) => v.as_bool().ok_or_else(|| SchemaError::InputSource {
                    name: key.to_string(),
                    msg: "'optional' must be a boolean".to_string(),
                })?,
                None => false,
            };

            sources.insert(
                key.to_string(),
                InputSource::new(key.to_string(), types, optional)?,
            );
        }
        Ok(InputSources { sources })
    }

    pub fn get(&self, key: &str) -> Option<&InputSource> {
        self.sources.get(key)
    }

    pub fn all_sources(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.sources.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn required_sources(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .sources
            .iter()
            .filter(|(_, src)| !src.optional)
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }
}

#[derive(Debug, Clone, Default)]
pub struct Bootstrap {
    pub source: Option<InputSources>,
    pub launch_cmds: Vec<String>,
    pub cmds: Vec<String>,
    pub cli_out: HashMap<String, String>,
    pub env: String,
}

impl Bootstrap {
    pub fn from_yaml(raw: Option<&serde_yaml::Value>) -> Result<Self, SchemaError> {
        match raw {
            None => Ok(Bootstrap::default()),
            Some(v) => {
                let mapping = v.as_mapping().ok_or_else(|| {
                    SchemaError::Generic(
                        "'bootstrap' must be a mapping with optional source:/launch_cmds:/cmds:/cli_out:/env:".to_string(),
                    )
                })?;

                if mapping.contains_key(serde_yaml::Value::String("out".to_string())) {
                    return Err(SchemaError::Generic(
                        "'bootstrap.out' is not valid; use 'cli_out'".to_string(),
                    ));
                }

                let unknown: Vec<String> = mapping
                    .keys()
                    .filter_map(|k| k.as_str().map(String::from))
                    .filter(|k| !BOOTSTRAP_KEYS.contains(&k.as_str()))
                    .collect();
                if !unknown.is_empty() {
                    return Err(SchemaError::Generic(format!(
                        "unknown bootstrap key(s): {}",
                        unknown.join(", ")
                    )));
                }

                let source = mapping
                    .get(serde_yaml::Value::String("source".to_string()))
                    .map(|v| {
                        let m = v.as_mapping().ok_or_else(|| {
                            SchemaError::Generic("'bootstrap.source' must be a mapping".to_string())
                        })?;
                        InputSources::from_yaml(m)
                    })
                    .transpose()?;

                let cli_out = mapping
                    .get(serde_yaml::Value::String("cli_out".to_string()))
                    .map(|v| {
                        let m = v.as_mapping().ok_or_else(|| {
                            SchemaError::Generic(
                                "'bootstrap.cli_out' must be a mapping".to_string(),
                            )
                        })?;
                        let mut out = HashMap::new();
                        for (k, v) in m {
                            let ks = k.as_str().map(String::from).ok_or_else(|| {
                                SchemaError::Generic(
                                    "'bootstrap.cli_out' keys must be strings".to_string(),
                                )
                            })?;
                            let vs = v.as_str().ok_or_else(|| {
                                SchemaError::Generic(format!(
                                    "'bootstrap.cli_out.{ks}' must be a string"
                                ))
                            })?;
                            out.insert(ks, vs.to_string());
                        }
                        Ok::<HashMap<String, String>, SchemaError>(out)
                    })
                    .transpose()?
                    .unwrap_or_default();

                let env = match mapping.get(serde_yaml::Value::String("env".to_string())) {
                    Some(v) => {
                        let s = v.as_str().ok_or_else(|| {
                            SchemaError::Generic("'bootstrap.env' must be a string".to_string())
                        })?;
                        s.to_string()
                    }
                    None => String::new(),
                };

                Ok(Bootstrap {
                    source,
                    launch_cmds: string_list(
                        mapping.get(serde_yaml::Value::String("launch_cmds".to_string())),
                        "bootstrap.launch_cmds",
                    )?,
                    cmds: string_list(
                        mapping.get(serde_yaml::Value::String("cmds".to_string())),
                        "bootstrap.cmds",
                    )?,
                    cli_out,
                    env,
                })
            }
        }
    }
}

fn string_list(raw: Option<&serde_yaml::Value>, label: &str) -> Result<Vec<String>, SchemaError> {
    match raw {
        None => Ok(Vec::new()),
        Some(v) => {
            let seq = v.as_sequence().ok_or_else(|| {
                SchemaError::Generic(format!("{label:?} must be a list of strings"))
            })?;
            let mut result = Vec::new();
            for item in seq {
                let s = item.as_str().ok_or_else(|| {
                    SchemaError::Generic(format!("{label:?} must be a list of strings"))
                })?;
                result.push(s.to_string());
            }
            Ok(result)
        }
    }
}

/// Validate CLI/source values. filepath-only requires an existing file.
pub fn validate_source_values(
    source: &InputSources,
    values: &HashMap<String, String>,
) -> Result<(), SchemaError> {
    for (key, src) in &source.sources {
        let value = values.get(key);
        match value {
            None => {
                if !src.optional {
                    return Err(SchemaError::Generic(format!(
                        "required bootstrap.source {key:?} is not available"
                    )));
                }
            }
            Some(v) if v.is_empty() => {
                if !src.optional {
                    return Err(SchemaError::Generic(format!(
                        "required bootstrap.source {key:?} is not available"
                    )));
                }
            }
            Some(v) => {
                if src.types.len() == 1
                    && src.types[0] == "filepath"
                    && !std::path::Path::new(v).is_file()
                {
                    return Err(SchemaError::Generic(format!(
                        "bootstrap.source {key:?}: expected an existing file, got {v:?}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Substitute `{cwd}` and `{key}` placeholders in a command string. Each value
/// is escaped for the quoting context it lands in, so user-controlled source
/// values cannot inject shell metacharacters. `${key}` tokens are skipped and
/// underscore keys also match their hyphenated form; a substituted value is
/// not re-scanned.
///
/// Fails on a placeholder inside a here-document body, where no escaping can
/// neutralise the value.
pub fn substitute_bootstrap_vars(
    cmd: &str,
    cwd: &Path,
    values: &HashMap<String, String>,
) -> Result<String, HereDocInterpolation> {
    Interpolator::new()
        .with("cwd", cwd.to_string_lossy().to_string())
        .with_map(values)
        .shell(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_source_empty_types() {
        let result = InputSource::new("test".to_string(), vec![], false);
        assert!(result.is_err());
    }

    #[test]
    fn test_input_source_unknown_type() {
        let result = InputSource::new("test".to_string(), vec!["bad".to_string()], false);
        assert!(result.is_err());
    }

    #[test]
    fn test_input_source_valid() {
        let result = InputSource::new("test".to_string(), vec!["filepath".to_string()], true);
        assert!(result.is_ok());
        let src = result.unwrap();
        assert_eq!(src.name, "test");
        assert!(src.optional);
    }

    #[test]
    fn test_input_sources_from_yaml() {
        let mut raw = serde_yaml::Mapping::new();
        let mut entry = serde_yaml::Mapping::new();
        entry.insert(
            serde_yaml::Value::String("type".to_string()),
            serde_yaml::Value::String("filepath".to_string()),
        );
        entry.insert(
            serde_yaml::Value::String("optional".to_string()),
            serde_yaml::Value::Bool(true),
        );
        raw.insert(
            serde_yaml::Value::String("my_input".to_string()),
            serde_yaml::Value::Mapping(entry),
        );

        let result = InputSources::from_yaml(&raw).unwrap();
        assert_eq!(result.all_sources(), vec!["my_input"]);
        assert!(result.required_sources().is_empty());
    }

    #[test]
    fn test_bootstrap_default() {
        let bs = Bootstrap::default();
        assert!(bs.source.is_none());
        assert!(bs.launch_cmds.is_empty());
    }

    #[test]
    fn test_bootstrap_from_yaml_none() {
        let bs = Bootstrap::from_yaml(None).unwrap();
        assert!(bs.source.is_none());
    }

    #[test]
    fn test_validate_source_values_missing_required() {
        let src = InputSources::new(
            vec![(
                "key".to_string(),
                InputSource::new("key".to_string(), vec!["string".to_string()], false).unwrap(),
            )]
            .into_iter()
            .collect(),
        );
        let values = HashMap::new();
        let err = validate_source_values(&src, &values).unwrap_err();
        assert!(err.to_string().contains("required bootstrap.source"));
    }

    fn subs(cmd: &str, values: &HashMap<String, String>) -> String {
        substitute_bootstrap_vars(cmd, Path::new("/tmp"), values).unwrap()
    }

    #[test]
    fn test_substitute_bootstrap_vars_cwd() {
        assert_eq!(
            substitute_bootstrap_vars("echo {cwd}", Path::new("/tmp/cwd"), &HashMap::new())
                .unwrap(),
            "echo '/tmp/cwd'"
        );
    }

    #[test]
    fn test_substitute_bootstrap_vars_values() {
        let values = HashMap::from([("instructions".to_string(), "do the thing".to_string())]);
        assert_eq!(
            subs("test -n {instructions}", &values),
            "test -n 'do the thing'"
        );
    }

    #[test]
    fn test_substitute_bootstrap_vars_keeps_template_double_quotes() {
        let values = HashMap::from([("plan".to_string(), "a b".to_string())]);
        assert_eq!(
            subs("printf '%s' \"{plan}\"", &values),
            "printf '%s' \"a b\""
        );
    }

    #[test]
    fn test_substitute_bootstrap_vars_shell_escape() {
        let values = HashMap::from([("x".to_string(), "'; rm -rf /; echo 'pwned".to_string())]);
        assert_eq!(
            subs("echo {x}", &values),
            "echo ''\\''; rm -rf /; echo '\\''pwned'"
        );
    }

    #[test]
    fn test_substitute_bootstrap_vars_blocks_double_quote_breakout() {
        let values = HashMap::from([("x".to_string(), "\"; rm -rf /; echo \"".to_string())]);
        assert_eq!(
            subs("echo \"{x}\"", &values),
            "echo \"\\\"; rm -rf /; echo \\\"\""
        );
    }

    #[test]
    fn test_substitute_bootstrap_vars_skips_dollar_tokens() {
        let values = HashMap::from([("x".to_string(), "y".to_string())]);
        assert_eq!(subs("echo ${x}", &values), "echo ${x}");
    }

    #[test]
    fn test_substitute_bootstrap_vars_hyphen_normalization() {
        let values = HashMap::from([("child_plan".to_string(), "v".to_string())]);
        assert_eq!(subs("echo {child-plan}", &values), "echo 'v'");
    }

    #[test]
    fn test_substitute_bootstrap_vars_unknown_key_left_verbatim() {
        assert_eq!(subs("echo {nope}", &HashMap::new()), "echo {nope}");
    }

    #[test]
    fn test_substitute_bootstrap_vars_refuses_here_document_body() {
        let values = HashMap::from([("plan".to_string(), "do the thing".to_string())]);
        let err = substitute_bootstrap_vars("cat <<EOF\n{plan}\nEOF", Path::new("/tmp"), &values)
            .unwrap_err();
        assert_eq!(err.delimiter, "EOF");
        assert_eq!(err.key, "plan");
    }
}
