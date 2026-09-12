use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed client descriptor from a stage dict's `client` key.
/// A plain String so gremlins-core stays free of PyO3.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientSpec(pub String);

/// Extract and validate a client spec from a stage dict.
/// `None` when the key is absent; an error when present but not a string.
pub fn get_client_from_dict(
    d: &HashMap<String, serde_json::Value>,
    stage_name: &str,
) -> Result<Option<ClientSpec>, String> {
    match d.get("client") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(ClientSpec(s.clone()))),
        Some(v) => Err(format!(
            "stage {stage_name:?}: 'client' must be a string, got {v:?}"
        )),
    }
}

/// Attributes shared by composite stages (Loop, Sequence, Parallel) and
/// duck-typed test stages.
#[derive(Debug, Clone)]
pub struct StageAttrs {
    pub name: String,
    pub stage_type: String,
    pub path: String,
    pub client: Option<String>,
    pub client_explicit: bool,
    pub skip_if_exists: String,
    pub options: HashMap<String, serde_json::Value>,
    pub bind_map: HashMap<String, String>,
}

impl StageAttrs {
    pub fn new(name: String) -> Self {
        StageAttrs {
            name,
            stage_type: String::new(),
            path: String::new(),
            client: None,
            client_explicit: false,
            skip_if_exists: String::new(),
            options: HashMap::new(),
            bind_map: HashMap::new(),
        }
    }
}

/// Per-child artifact directory and key for a fan-out child.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildParams {
    pub artifact_dir: PathBuf,
    pub child_key: String,
}

/// Artifact directory and child key for a fan-out child: `<scratch>/artifacts`
/// when a child scratch dir is known, else `<parent_artifact_dir>/<child_name>`.
pub fn compute_child_params(
    parent_artifact_dir: &Path,
    child_name: &str,
    child_scratch_dir: Option<&Path>,
) -> ChildParams {
    let artifact_dir = match child_scratch_dir {
        Some(scratch) => scratch.join("artifacts"),
        None => parent_artifact_dir.join(child_name),
    };
    ChildParams {
        artifact_dir,
        child_key: child_name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_attrs_defaults() {
        let s = StageAttrs::new("my-stage".into());
        assert_eq!(s.name, "my-stage");
        assert_eq!(s.stage_type, "");
        assert_eq!(s.path, "");
        assert_eq!(s.client, None);
        assert!(!s.client_explicit);
        assert_eq!(s.skip_if_exists, "");
        assert!(s.options.is_empty());
        assert!(s.bind_map.is_empty());
    }

    #[test]
    fn client_absent() {
        let d = HashMap::from([("name".into(), serde_json::Value::String("s".into()))]);
        assert_eq!(get_client_from_dict(&d, "s").unwrap(), None);
    }

    #[test]
    fn client_null_is_absent() {
        let d = HashMap::from([("client".into(), serde_json::Value::Null)]);
        assert_eq!(get_client_from_dict(&d, "s").unwrap(), None);
    }

    #[test]
    fn client_parses() {
        let d = HashMap::from([(
            "client".into(),
            serde_json::Value::String("xai:grok-5".into()),
        )]);
        assert_eq!(
            get_client_from_dict(&d, "s").unwrap(),
            Some(ClientSpec("xai:grok-5".into()))
        );
    }

    #[test]
    fn client_rejects_non_string() {
        let d = HashMap::from([("client".into(), serde_json::Value::Number(5.into()))]);
        assert!(get_client_from_dict(&d, "s").is_err());
    }

    #[test]
    fn child_params_without_scratch() {
        let p = compute_child_params(Path::new("/tmp/artifacts"), "review-lens", None);
        assert_eq!(p.artifact_dir, PathBuf::from("/tmp/artifacts/review-lens"));
        assert_eq!(p.child_key, "review-lens");
    }

    #[test]
    fn child_params_with_scratch() {
        let p = compute_child_params(
            Path::new("/tmp/artifacts"),
            "review-lens",
            Some(Path::new("/scratch/child-1")),
        );
        assert_eq!(p.artifact_dir, PathBuf::from("/scratch/child-1/artifacts"));
        assert_eq!(p.child_key, "review-lens");
    }
}
