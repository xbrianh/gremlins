use std::collections::HashSet;
use std::path::PathBuf;

pub mod error;
pub use error::DiscoveryError;

fn project_overlay_dir(project_root: &std::path::Path) -> PathBuf {
    crate::config::project_overlay_dir(project_root)
}

fn project_pipeline_dirs(project_root: &std::path::Path) -> Vec<PathBuf> {
    project_pipeline_dirs_in(project_overlay_dir(project_root), project_root)
}

/// Pipelines live in `overlay`, and — for a project that does not use one —
/// directly in `.gremlins/` under `project_root`.
fn project_pipeline_dirs_in(overlay: PathBuf, project_root: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for d in &[overlay, project_root.join(crate::config::OVERLAY_DIRNAME)] {
        let resolved = d.canonicalize().unwrap_or_else(|_| d.clone());
        if seen.insert(resolved) {
            dirs.push(d.clone());
        }
    }
    dirs
}

pub fn list_pipelines(project_root: PathBuf) -> Vec<(String, PathBuf)> {
    let mut results: Vec<(String, PathBuf)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for local_dir in project_pipeline_dirs(&project_root) {
        if !local_dir.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&local_dir) {
            let mut yamls: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "yaml"))
                .collect();
            yamls.sort();
            for p in yamls {
                let stem = p.file_stem().and_then(|s| s.to_str()).map(String::from);
                if let Some(stem) = stem {
                    if seen.insert(stem.clone()) {
                        let resolved = p.canonicalize().unwrap_or(p);
                        results.push((stem, resolved));
                    }
                }
            }
        }
    }

    results
}

pub fn resolve_pipeline_name(name: &str, project_root: PathBuf) -> Result<PathBuf, DiscoveryError> {
    resolve_pipeline_name_in(name, None, project_root)
}

/// [`resolve_pipeline_name`], with `overlay` chosen by an `explicit` override
/// rather than read from the process environment.
pub fn resolve_pipeline_name_in(
    name: &str,
    explicit_overlay: Option<&std::path::Path>,
    project_root: PathBuf,
) -> Result<PathBuf, DiscoveryError> {
    let overlay = crate::config::overlay_dir_preferring(explicit_overlay, &project_root);
    for d in project_pipeline_dirs_in(overlay.clone(), &project_root) {
        let candidate = d.join(format!("{}.yaml", name));
        if candidate.exists() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }

    // Also search .gremlins/stages/ for stage-definition YAMLs.
    let stages_dir = overlay.join("stages");
    let candidate = stages_dir.join(format!("{}.yaml", name));
    if candidate.exists() {
        return Ok(candidate.canonicalize().unwrap_or(candidate));
    }

    let mut names: Vec<String> = Vec::new();
    for d in project_pipeline_dirs_in(overlay, &project_root) {
        if d.exists() {
            if let Ok(entries) = std::fs::read_dir(&d) {
                let mut stems: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext == "yaml"))
                    .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
                    .collect();
                stems.sort();
                names.extend(stems);
            }
        }
    }
    // Include stages/ names in error listing.
    if let Ok(entries) = std::fs::read_dir(&stages_dir) {
        let mut stems: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "yaml"))
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
            .collect();
        stems.sort();
        names.extend(stems);
    }
    let mut seen = HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    let available = names.join(", ");
    Err(DiscoveryError::Name {
        name: name.to_string(),
        available,
    })
}

pub fn resolve_pipeline_path(
    name_or_path: &str,
    base_dir: PathBuf,
) -> Result<PathBuf, DiscoveryError> {
    resolve_pipeline_path_in(name_or_path, None, base_dir)
}

/// [`resolve_pipeline_path`], with `overlay` chosen by an `explicit` override
/// rather than read from the process environment.
pub fn resolve_pipeline_path_in(
    name_or_path: &str,
    explicit_overlay: Option<&std::path::Path>,
    base_dir: PathBuf,
) -> Result<PathBuf, DiscoveryError> {
    let candidate = PathBuf::from(name_or_path);
    if candidate.extension().is_some_and(|ext| ext == "yaml") || candidate.components().count() > 1
    {
        let resolved = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if !resolved.exists() {
            return Err(DiscoveryError::File { path: resolved });
        }
        return Ok(resolved);
    }

    let overlay = crate::config::overlay_dir_preferring(explicit_overlay, &base_dir);
    let pipeline_dirs = project_pipeline_dirs_in(overlay.clone(), &base_dir);
    for d in &pipeline_dirs {
        let project_scoped = d.join(format!("{}.yaml", name_or_path));
        if project_scoped.exists() {
            return Ok(project_scoped.canonicalize().unwrap_or(project_scoped));
        }
    }

    // Also search .gremlins/stages/ for stage-definition YAMLs.
    let stages_dir = overlay.join("stages");
    let candidate = stages_dir.join(format!("{}.yaml", name_or_path));
    if candidate.exists() {
        return Ok(candidate.canonicalize().unwrap_or(candidate));
    }

    let dirs: Vec<String> = pipeline_dirs
        .iter()
        .chain(std::iter::once(&stages_dir))
        .map(|d| d.display().to_string())
        .collect();
    Err(DiscoveryError::Path {
        name: name_or_path.to_string(),
        dirs: dirs.join(" or "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    use crate::test_support::EnvGuard;

    /// A throwaway project with an empty `.gremlins/` overlay. The guard
    /// clears the override variables for the duration, so these tests see the
    /// project's own overlay rather than whatever `GREMLINS_OVERLAY_DIR` a
    /// sibling test left behind.
    fn setup_dirs() -> (EnvGuard, TempDir) {
        let env = EnvGuard::lock();
        let project = TempDir::new().unwrap();
        let overlay = project.path().join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();
        (env, project)
    }

    #[test]
    fn test_list_pipelines_empty() {
        let (_env, project) = setup_dirs();
        let result = list_pipelines(project.path().to_path_buf());
        assert!(result.is_empty());
    }

    #[test]
    fn test_list_pipelines_project_only() {
        let (_env, project) = setup_dirs();
        let overlay = project.path().join(".gremlins");
        fs::write(overlay.join("foo.yaml"), "stages: []").unwrap();
        fs::write(overlay.join("bar.yaml"), "stages: []").unwrap();

        let result = list_pipelines(project.path().to_path_buf());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "bar");
        assert_eq!(result[1].0, "foo");
    }

    #[test]
    fn test_list_pipelines_dedup() {
        let (_env, project) = setup_dirs();
        let overlay = project.path().join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();
        fs::write(overlay.join("dup.yaml"), "stages: []").unwrap();
        let result = list_pipelines(project.path().to_path_buf());
        assert_eq!(result.iter().filter(|(n, _)| n == "dup").count(), 1);
    }

    #[test]
    fn test_list_pipelines_project_wins_over_bundled() {
        let (_env, project) = setup_dirs();
        let overlay = project.path().join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();
        fs::write(overlay.join("boss.yaml"), "stages: [a]").unwrap();

        let result = list_pipelines(project.path().to_path_buf());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "boss");
        assert!(result[0]
            .1
            .starts_with(overlay.canonicalize().unwrap_or(overlay).as_path()));
    }

    #[test]
    fn test_resolve_pipeline_name_found_overlay() {
        let (_env, project) = setup_dirs();
        let overlay = project.path().join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();
        fs::write(overlay.join("test.yaml"), "stages: []").unwrap();

        let result = resolve_pipeline_name("test", project.path().to_path_buf()).unwrap();
        assert!(result.ends_with("test.yaml"));
    }

    #[test]
    fn test_resolve_pipeline_name_not_found() {
        let (_env, project) = setup_dirs();
        let err = resolve_pipeline_name("nope", project.path().to_path_buf()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn test_resolve_pipeline_path_yaml_extension() {
        let (_env, project) = setup_dirs();
        let overlay = project.path().join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();
        let p = overlay.join("explicit.yaml");
        fs::write(&p, "stages: []").unwrap();

        let result =
            resolve_pipeline_path(p.to_str().unwrap(), project.path().to_path_buf()).unwrap();
        assert!(result.ends_with("explicit.yaml"));
    }

    #[test]
    fn test_resolve_pipeline_path_bare_name() {
        let (_env, project) = setup_dirs();
        let overlay = project.path().join(".gremlins");
        fs::create_dir_all(&overlay).unwrap();
        fs::write(overlay.join("bare.yaml"), "stages: []").unwrap();

        let result = resolve_pipeline_path("bare", project.path().to_path_buf()).unwrap();
        assert!(result.ends_with("bare.yaml"));
    }

    #[test]
    fn test_resolve_pipeline_path_missing() {
        let (_env, project) = setup_dirs();
        let err = resolve_pipeline_path("nope.yaml", project.path().to_path_buf()).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_pipeline_name_from_stages_dir() {
        let (_env, project) = setup_dirs();
        let stages = project.path().join(".gremlins").join("stages");
        fs::create_dir_all(&stages).unwrap();
        fs::write(stages.join("foo.yaml"), "stages: []").unwrap();

        let result = resolve_pipeline_name("foo", project.path().to_path_buf()).unwrap();
        assert!(result.ends_with("foo.yaml"));
        assert!(result.to_str().unwrap().contains("stages"));
    }

    #[test]
    fn explicit_overlay_wins_over_the_env_override() {
        let mut env = EnvGuard::lock();
        let project = TempDir::new().unwrap();
        let project_overlay = project.path().join(".gremlins");
        fs::create_dir_all(&project_overlay).unwrap();
        fs::write(project_overlay.join("demo.yaml"), "stages: []").unwrap();

        let explicit = project.path().join("elsewhere");
        fs::create_dir_all(&explicit).unwrap();
        fs::write(explicit.join("demo.yaml"), "stages: []").unwrap();

        // A running gremlin's export points at its own overlay; the caller
        // that names one explicitly must still be believed.
        env.set("GREMLINS_OVERLAY_DIR", project_overlay.as_path());
        let found = resolve_pipeline_path_in("demo", Some(&explicit), project.path().to_path_buf())
            .unwrap();
        assert!(
            found.starts_with(explicit.canonicalize().unwrap()),
            "{found:?}"
        );
    }

    #[test]
    fn test_list_pipelines_excludes_stages() {
        let (_env, project) = setup_dirs();
        let overlay = project.path().join(".gremlins");
        fs::write(overlay.join("pipeline.yaml"), "stages: []").unwrap();
        let stages = overlay.join("stages");
        fs::create_dir_all(&stages).unwrap();
        fs::write(stages.join("foo.yaml"), "stages: []").unwrap();

        let result = list_pipelines(project.path().to_path_buf());
        let names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"pipeline"));
        assert!(!names.contains(&"foo"));
    }
}
