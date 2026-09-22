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

pub async fn resolve_interpolation_map(
    artifacts: &(impl ArtifactRegistry + ?Sized),
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

            // For optional content(), skip the lookup if the artifact isn't
            // registered — avoids relying on downcast for MissingArtifact,
            // which breaks across the async_trait vtable boundary.
            if optional && !artifacts.is_registered(&uri_str).await {
                log::debug!(
                    "resolve: {var:?} = content({uri_str:?})? -> (empty, artifact not registered)"
                );
                result.insert(var.clone(), String::new());
                continue;
            }

            match artifacts.content(&uri_str, json_path).await {
                Ok(val) => {
                    log::debug!(
                        "resolve: {var:?} = content({uri_str:?}) -> {} bytes",
                        val.len()
                    );
                    result.insert(var.clone(), val);
                }
                Err(_e) if optional => {
                    // Shouldn't happen (we already checked is_registered),
                    // but tolerate it gracefully.
                    log::debug!(
                        "resolve: {var:?} = content({uri_str:?})? -> (empty, content failed)"
                    );
                    result.insert(var.clone(), String::new());
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
        match artifacts.data_uri(&key).await {
            Ok(val) => {
                log::debug!("resolve: {var:?} = {key:?} -> {} bytes", val.len());
                result.insert(var.clone(), val);
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
    use crate::artifacts::registry::FileSystemArtifactRegistry;
    use std::fs;
    use tempfile::TempDir;

    fn setup_registry() -> (TempDir, FileSystemArtifactRegistry) {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        (tmp, reg)
    }

    async fn register_file(reg: &FileSystemArtifactRegistry, name: &str, content: &str) -> String {
        let uri = crate::artifacts::uri::Uri::parse(&format!("artifact://{name}")).unwrap();
        reg.write_into_registry(&uri, content).await.unwrap()
    }

    fn unwrap_result<T>(r: Result<T, ResolveError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn test_resolve_bound_key() {
        let (_tmp, reg) = setup_registry();
        let path = register_file(&reg, "mykey", "myval").await;

        let mut map = HashMap::new();
        map.insert("var".to_string(), "artifact://mykey".to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, "").await);
        assert_eq!(result.get("var").unwrap(), &path);
    }

    #[tokio::test]
    async fn test_resolve_default_fallback() {
        let (_tmp, reg) = setup_registry();

        let mut map = HashMap::new();
        map.insert("var".to_string(), "missing?default_val".to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, "").await);
        assert_eq!(result.get("var").unwrap(), "default_val");
    }

    #[tokio::test]
    async fn test_resolve_optional_content() {
        let (_tmp, reg) = setup_registry();

        let mut map = HashMap::new();
        map.insert("var".to_string(), r#"content("missing.txt")?"#.to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, "").await);
        assert_eq!(result.get("var").unwrap(), "");
    }

    #[tokio::test]
    async fn test_resolve_content_with_json_path() {
        let (_tmp, reg) = setup_registry();
        register_file(&reg, "data.json", r#"{"x":{"y":"z"}}"#).await;
        let mut map = HashMap::new();
        map.insert(
            "var".to_string(),
            r#"content("artifact://data.json", "x.y")"#.to_string(),
        );
        let result = unwrap_result(resolve_interpolation_map(&reg, &map, "").await);
        assert_eq!(result.get("var").unwrap(), "z");
    }

    #[tokio::test]
    async fn test_resolve_loop_iter_substitution() {
        let (_tmp, reg) = setup_registry();
        register_file(&reg, "key_0", "val0").await;
        let path_1 = register_file(&reg, "key_1", "val1").await;

        let mut map = HashMap::new();
        map.insert("var".to_string(), "artifact://key_{loop_iter}".to_string());

        let result = unwrap_result(resolve_interpolation_map(&reg, &map, "1").await);
        assert_eq!(result.get("var").unwrap(), &path_1);
    }
}
