use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use thiserror::Error;

use crate::artifacts::registry::{ArtifactRegistry, MissingArtifact};

static CONTENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^content\("([^"]+)"(?:,\s*"([^"]+)")?\)\s*$"#).unwrap());

#[derive(Error, Debug)]
pub enum ResolveError {
    #[error("artifact not bound: {0:?}")]
    MissingArtifact(String),
    #[error(transparent)]
    Other(Box<dyn std::error::Error>),
}

pub fn resolve_interpolation_map(
    artifacts: &ArtifactRegistry,
    interpolation_map: &HashMap<String, String>,
    loop_iter: &str,
) -> Result<HashMap<String, String>, ResolveError> {
    let mut result = HashMap::new();
    for (var, raw) in interpolation_map {
        let trimmed = raw.trim_end();
        let optional = trimmed.ends_with('?');
        let raw_clean = trimmed.trim_end_matches('?');

        if let Some(caps) = CONTENT_RE.captures(raw_clean) {
            let mut uri_str = caps.get(1).unwrap().as_str().to_string();
            if !loop_iter.is_empty() {
                uri_str = uri_str.replace("{loop_iter}", loop_iter);
            }
            let json_path = caps.get(2).map(|m| m.as_str());
            match artifacts.content(&uri_str, json_path) {
                Ok(val) => {
                    log::debug!(
                        "resolve: {var:?} = content({uri_str:?}) -> {} bytes",
                        val.len()
                    );
                    result.insert(var.clone(), val);
                }
                Err(e) if optional && e.downcast_ref::<MissingArtifact>().is_some() => {
                    result.insert(var.clone(), String::new());
                    log::debug!(
                        "resolve: {var:?} = content({uri_str:?})? -> (empty, artifact not bound)"
                    );
                }
                Err(e) if optional => {
                    return Err(ResolveError::Other(e));
                }
                Err(e) => {
                    if let Some(ma) = e.downcast_ref::<MissingArtifact>() {
                        return Err(ResolveError::MissingArtifact(ma.key.clone()));
                    }
                    return Err(ResolveError::Other(e));
                }
            }
            continue;
        }

        // '?' is the key/default separator
        let (key, default): (&str, Option<&str>) = if let Some(pos) = raw.find('?') {
            (&raw[..pos], Some(&raw[pos + 1..]))
        } else {
            (raw, None)
        };
        let mut key = key.to_string();
        if !loop_iter.is_empty() {
            key = key.replace("{loop_iter}", loop_iter);
        }
        match artifacts.data_uri(&key) {
            Ok(val) => {
                result.insert(var.clone(), val.to_string());
                log::debug!("resolve: {var:?} = {key:?} -> {} bytes", val.len());
            }
            Err(_) if default.is_some() => {
                result.insert(var.clone(), default.unwrap_or("").to_string());
                log::debug!("resolve: {var:?} = {key:?}? -> using default");
            }
            Err(e) => {
                return Err(ResolveError::MissingArtifact(e.key.clone()));
            }
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

    fn setup_registry() -> (TempDir, ArtifactRegistry) {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let reg = ArtifactRegistry::new(artifact_dir);
        (tmp, reg)
    }

    fn register_file(reg: &mut ArtifactRegistry, name: &str, content: &str) -> String {
        let uri = crate::artifacts::uri::Uri::parse(&format!("artifact://{name}")).unwrap();
        reg.write_into_registry(&uri, content).unwrap()
    }

    fn unwrap_result<T>(r: Result<T, ResolveError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn test_resolve_bound_key() {
        let (_tmp, mut reg) = setup_registry();
        let path = register_file(&mut reg, "mykey", "myval");

        let mut map = HashMap::new();
        map.insert("var".to_string(), "artifact://mykey".to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, ""));
        assert_eq!(result.get("var").unwrap(), &path);
    }

    #[test]
    fn test_resolve_default_fallback() {
        let (_tmp, reg) = setup_registry();

        let mut map = HashMap::new();
        map.insert("var".to_string(), "missing?default_val".to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, ""));
        assert_eq!(result.get("var").unwrap(), "default_val");
    }

    #[test]
    fn test_resolve_optional_content() {
        let (_tmp, reg) = setup_registry();

        let mut map = HashMap::new();
        map.insert("var".to_string(), r#"content("missing.txt")?"#.to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, ""));
        assert_eq!(result.get("var").unwrap(), "");
    }

    #[test]
    fn test_resolve_content_with_json_path() {
        let (_tmp, mut reg) = setup_registry();
        register_file(&mut reg, "data.json", r#"{"x":{"y":"z"}}"#);
        let mut map = HashMap::new();
        map.insert(
            "var".to_string(),
            r#"content("artifact://data.json", "x.y")"#.to_string(),
        );
        let result = unwrap_result(resolve_interpolation_map(&reg, &map, ""));
        assert_eq!(result.get("var").unwrap(), "z");
    }

    #[test]
    fn test_resolve_loop_iter_substitution() {
        let (_tmp, mut reg) = setup_registry();
        register_file(&mut reg, "key_0", "val0");
        let path_1 = register_file(&mut reg, "key_1", "val1");

        let mut map = HashMap::new();
        map.insert("var".to_string(), "artifact://key_{loop_iter}".to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, "1"));
        assert_eq!(result.get("var").unwrap(), &path_1);
    }
}
