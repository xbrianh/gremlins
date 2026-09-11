use std::collections::HashMap;
use std::path::Path;

use crate::assets;
use crate::schemas::error::SchemaError;
use crate::schemas::expand::GREMLINS_PREFIX;

pub(crate) fn read_prompts(
    prompt_field: &serde_yaml::Value,
    prompt_dir: &Path,
    named_prompts: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>, SchemaError> {
    let raw: Vec<String> = match prompt_field {
        serde_yaml::Value::String(s) => vec![s.clone()],
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .map(|v| match v {
                serde_yaml::Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect(),
        _ => {
            return Err(SchemaError::Generic(format!(
                "prompt must be a string or list, got {:?}",
                prompt_field
            )));
        }
    };

    let mut texts: Vec<String> = Vec::new();
    for p in &raw {
        if let Some(named) = named_prompts.get(p) {
            texts.extend(named.clone());
        } else if let Some(name) = p.strip_prefix(GREMLINS_PREFIX) {
            if name.is_empty() {
                return Err(SchemaError::Generic(format!(
                    "prompt {p:?} is missing a name after {GREMLINS_PREFIX:?}"
                )));
            }
            // Try bundled prompts first, then fall back to file-system lookup
            match read_bundled_prompt(name) {
                Ok(text) => texts.push(text),
                Err(SchemaError::PromptFileNotFound { .. }) => {
                    let path = prompt_dir.join(name);
                    texts.push(read_prompt_file(&path)?);
                }
                Err(e) => return Err(e),
            }
        } else if p.contains('\n') {
            texts.push(p.clone());
        } else {
            let path = if std::path::PathBuf::from(p).is_absolute() {
                std::path::PathBuf::from(p)
            } else {
                prompt_dir.join(p)
            };
            let path = path.canonicalize().unwrap_or(path);
            if !path.exists() && !named_prompts.is_empty() {
                return Err(SchemaError::PromptFileNotFound {
                    path: format!(
                        "{p:?} not found as a named entry or file under {}",
                        prompt_dir.display()
                    ),
                });
            }
            texts.push(read_prompt_file(&path)?);
        }
    }

    Ok(texts)
}

pub(crate) fn read_bundled_prompt(name: &str) -> Result<String, SchemaError> {
    let text = assets::PROMPTS
        .get(name)
        .ok_or_else(|| SchemaError::PromptFileNotFound {
            path: name.to_string(),
        })?;
    if text.trim().is_empty() {
        return Err(SchemaError::PromptFileEmpty {
            path: name.to_string(),
        });
    }
    Ok(text.to_string())
}

pub(crate) fn read_prompt_file(path: &std::path::PathBuf) -> Result<String, SchemaError> {
    if !path.exists() {
        return Err(SchemaError::PromptFileNotFound {
            path: path.display().to_string(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|_| SchemaError::PromptFileNotFound {
        path: path.display().to_string(),
    })?;
    if text.trim().is_empty() {
        return Err(SchemaError::PromptFileEmpty {
            path: path.display().to_string(),
        });
    }
    Ok(text)
}

pub(crate) fn parse_named_prompts(
    prompts_raw: Option<&serde_yaml::Value>,
    prompt_dir: &Path,
) -> Result<HashMap<String, Vec<String>>, SchemaError> {
    let mut named: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(mapping) = prompts_raw.and_then(|v| v.as_mapping()) {
        for (k, v) in mapping {
            let name = k.as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let empty: HashMap<String, Vec<String>> = HashMap::new();
            let texts = read_prompts(v, prompt_dir, &empty)?;
            named.insert(name, texts);
        }
    }
    Ok(named)
}
