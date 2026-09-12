use std::path::{Path, PathBuf};

pub struct ChildStateResult {
    pub artifact_dir: PathBuf,
    pub child_key: String,
}

/// Fan-out children with a scratch dir write under `<child_scratch_dir>/artifacts`;
/// fan-out children without one fall back to the legacy
/// `<parent_artifact_dir>/<child_name>` layout.
pub fn compute_child_params(
    parent_artifact_dir: &Path,
    child_name: &str,
    child_scratch_dir: Option<&Path>,
) -> ChildStateResult {
    let artifact_dir = match child_scratch_dir {
        Some(dir) => dir.join("artifacts"),
        None => parent_artifact_dir.join(child_name),
    };

    ChildStateResult {
        artifact_dir,
        child_key: child_name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_out_child() {
        let r = compute_child_params(
            Path::new("/tmp/artifacts"),
            "child1",
            Some(Path::new("/tmp/scratch/abc123")),
        );
        assert_eq!(r.artifact_dir, Path::new("/tmp/scratch/abc123/artifacts"));
        assert_eq!(r.child_key, "child1");
    }

    #[test]
    fn fan_out_child_without_scratch_dir() {
        let r = compute_child_params(Path::new("/tmp/artifacts"), "child1", None);
        assert_eq!(r.artifact_dir, Path::new("/tmp/artifacts/child1"));
        assert_eq!(r.child_key, "child1");
    }
}
