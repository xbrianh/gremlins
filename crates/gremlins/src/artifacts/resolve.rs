use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::artifacts::registry::{ArtifactRegistry, MissingArtifact};

static CONTENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^content\("([^"]+)"(?:,\s*"([^"]+)")?\)\s*$"#).unwrap());

pub fn resolve_interpolation_map(
    artifacts: &ArtifactRegistry,
    interpolation_map: &HashMap<String, String>,
    loop_iter: &str,
) -> Result<HashMap<String, String>, MissingArtifact> {
    let mut result = HashMap::new();
    for (var, raw) in interpolation_map {
        let optional = raw.ends_with('?');
        let raw_clean = raw.trim_end_matches('?');

        if let Some(caps) = CONTENT_RE.captures(raw_clean) {
            let mut uri_str = caps.get(1).unwrap().as_str().to_string();
            if !loop_iter.is_empty() {
                uri_str = uri_str.replace("{loop_iter}", loop_iter);
            }
            let json_path = caps.get(2).map(|m| m.as_str());
            match artifacts.content(&uri_str, json_path) {
                Ok(val) => {
                    result.insert(var.clone(), val);
                }
                Err(_) if optional => {
                    result.insert(var.clone(), String::new());
                }
                Err(e) => {
                    if let Some(ma) = e.downcast_ref::<MissingArtifact>() {
                        return Err(MissingArtifact {
                            key: ma.key.clone(),
                        });
                    }
                    return Err(MissingArtifact {
                        key: uri_str.clone(),
                    });
                }
            }
            continue;
        }

        // Use raw (not raw_clean) for partition — '?' is the key/default separator
        let raw_str: &str = raw;
        let (key, default): (&str, Option<&str>) = if let Some(pos) = raw_str.find('?') {
            (&raw_str[..pos], Some(&raw_str[pos + 1..]))
        } else {
            (raw_str, None)
        };
        let mut key = key.to_string();
        if !loop_iter.is_empty() {
            key = key.replace("{loop_iter}", loop_iter);
        }
        match artifacts.data_uri(&key) {
            Ok(val) => {
                result.insert(var.clone(), val.to_string());
            }
            Err(_) if default.is_some() => {
                result.insert(var.clone(), default.unwrap_or("").to_string());
            }
            Err(e) => return Err(e),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::registry::ArtifactRegistry;
    use std::fs;
    use tempfile::TempDir;

    fn setup_registry(data: HashMap<String, String>) -> (TempDir, ArtifactRegistry) {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.data = data;
        (tmp, reg)
    }

    #[test]
    fn test_resolve_bound_key() {
        let mut data = HashMap::new();
        data.insert("mykey".to_string(), "myval".to_string());
        let (_tmp, reg) = setup_registry(data);

        let mut map = HashMap::new();
        map.insert("var".to_string(), "mykey".to_string());

        let result = resolve_interpolation_map(&reg, &map, "").unwrap();
        assert_eq!(result.get("var").unwrap(), "myval");
    }

    #[test]
    fn test_resolve_default_fallback() {
        let data = HashMap::new();
        let (_tmp, reg) = setup_registry(data);

        let mut map = HashMap::new();
        map.insert("var".to_string(), "missing?default_val".to_string());

        let result = resolve_interpolation_map(&reg, &map, "").unwrap();
        assert_eq!(result.get("var").unwrap(), "default_val");
    }

    #[test]
    fn test_resolve_optional_content() {
        let data = HashMap::new();
        let (_tmp, reg) = setup_registry(data);

        let mut map = HashMap::new();
        map.insert("var".to_string(), r#"content("missing.txt")?"#.to_string());

        let result = resolve_interpolation_map(&reg, &map, "").unwrap();
        assert_eq!(result.get("var").unwrap(), "");
    }

    #[test]
    fn test_resolve_content_with_json_path() {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let file_path = artifact_dir.join("data.json");
        fs::write(&file_path, r#"{"x":{"y":"z"}}"#).unwrap();
        let mut reg = ArtifactRegistry::new(artifact_dir.clone());
        reg.data.insert(
            "artifact://data.json".to_string(),
            file_path.to_string_lossy().to_string(),
        );
        let mut map = HashMap::new();
        map.insert(
            "var".to_string(),
            r#"content("artifact://data.json", "x.y")"#.to_string(),
        );
        let result = resolve_interpolation_map(&reg, &map, "").unwrap();
        assert_eq!(result.get("var").unwrap(), "z");
        let _ = tmp;
    }

    #[test]
    fn test_resolve_loop_iter_substitution() {
        let mut data = HashMap::new();
        data.insert("key_0".to_string(), "val0".to_string());
        data.insert("key_1".to_string(), "val1".to_string());
        let (_tmp, reg) = setup_registry(data);

        let mut map = HashMap::new();
        map.insert("var".to_string(), "key_{loop_iter}".to_string());

        let result = resolve_interpolation_map(&reg, &map, "1").unwrap();
        assert_eq!(result.get("var").unwrap(), "val1");
    }
}
