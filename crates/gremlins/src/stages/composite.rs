use std::path::{Path, PathBuf};

pub struct ChildStateResult {
    pub artifact_dir: PathBuf,
    pub child_key: String,
}

pub fn compute_child_params(
    parent_artifact_dir: &Path,
    child_name: &str,
    child_id: Option<&str>,
    scratch_root: &Path,
) -> ChildStateResult {
    let artifact_dir = match child_id {
        Some(cid) => scratch_root.join(cid).join("artifacts"),
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
    fn with_child_id() {
        let r = compute_child_params(
            Path::new("/tmp/artifacts"),
            "child1",
            Some("abc123"),
            Path::new("/tmp/scratch"),
        );
        assert_eq!(r.artifact_dir, Path::new("/tmp/scratch/abc123/artifacts"));
        assert_eq!(r.child_key, "child1");
    }

    #[test]
    fn without_child_id() {
        let r = compute_child_params(
            Path::new("/tmp/artifacts"),
            "child1",
            None,
            Path::new("/tmp/scratch"),
        );
        assert_eq!(r.artifact_dir, Path::new("/tmp/artifacts/child1"));
        assert_eq!(r.child_key, "child1");
    }
}
