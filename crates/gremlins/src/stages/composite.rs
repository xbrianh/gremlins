use std::path::{Path, PathBuf};

/// Parsed client descriptor from a stage dict's `client` key.
/// A plain String so gremlins-core stays free of PyO3.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientSpec(pub String);

/// Attributes shared by composite stages (Loop, Sequence, Parallel) and
/// duck-typed test stages.
#[derive(Debug, Clone, PartialEq)]
pub struct StageAttrs {
    pub name: String,
    pub stage_type: String,
    pub path: String,
    pub client_explicit: bool,
    pub skip_if_exists: String,
}

impl StageAttrs {
    pub fn new(name: String) -> Self {
        StageAttrs {
            name,
            stage_type: String::new(),
            path: String::new(),
            client_explicit: false,
            skip_if_exists: String::new(),
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
        assert!(!s.client_explicit);
        assert_eq!(s.skip_if_exists, "");
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
