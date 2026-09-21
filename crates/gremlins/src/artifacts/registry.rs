use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json;
use thiserror::Error;

use crate::artifacts::uri::Uri;

// --- Errors ---

#[derive(Error, Debug)]
#[error("artifact not bound: {key:?}")]
pub struct MissingArtifact {
    pub key: String,
}

#[derive(Error, Debug)]
#[error("duplicate artifact: {key:?} already bound to {existing:?}, cannot rebind to {incoming:?}")]
pub struct DuplicateArtifact {
    pub key: String,
    pub existing: String,
    pub incoming: String,
}

// --- ArtifactRegistry trait ---

/// The set of operations that `prepare_agent`, `commit_agent`, `prepare_exec`,
/// `commit_exec`, and `resolve_interpolation_map` require from an
/// [`ArtifactRegistry`].
///
/// Implemented by [`FileSystemArtifactRegistry`] (the real filesystem-backed registry)
/// and [`DryRunArtifactRegistry`] (a no-I/O stub for dry-run execution).
#[async_trait::async_trait]
pub trait ArtifactRegistry: Send + Sync {
    async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact>;
    async fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>>;
    async fn is_registered(&self, key: &str) -> bool;
    async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>>;
    async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>>;
    async fn write_into_registry(
        &self,
        uri: &Uri,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>>;
    /// Whether the given path was produced by this registry.
    ///
    /// Used by the commit phase to determine whether an output artifact
    /// should be committed, replacing direct filesystem probes.
    async fn is_path_produced(&self, path: &str) -> bool;
}

// --- FileSystemArtifactRegistry ---

pub struct FileSystemArtifactRegistry {
    pub artifact_dir: PathBuf,
    pub registry_path: PathBuf,
}

impl FileSystemArtifactRegistry {
    pub fn new(artifact_dir: PathBuf) -> Self {
        let registry_path = artifact_dir
            .parent()
            .unwrap_or(&artifact_dir)
            .join("registry.json");
        log::info!("registry ready at {}", registry_path.display());
        FileSystemArtifactRegistry {
            artifact_dir,
            registry_path,
        }
    }

    /// Read and parse `registry.json`, returning an empty map when the file is
    /// absent or unparseable (logging the reason).
    async fn read_registry_json(&self) -> HashMap<String, String> {
        match tokio::fs::read_to_string(&self.registry_path).await {
            Ok(content) => match serde_json::from_str::<HashMap<String, String>>(&content) {
                Ok(data) => data,
                Err(e) => {
                    log::error!(
                        "failed to parse registry.json at {}: {e} — starting with empty registry",
                        self.registry_path.display(),
                    );
                    HashMap::new()
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::error!(
                        "failed to read registry.json at {}: {e} — starting with empty registry",
                        self.registry_path.display(),
                    );
                }
                HashMap::new()
            }
        }
    }

    /// Acquire the flock on `registry_path`, read the current map, apply
    /// `f`, and atomically write the result back.
    async fn locked_write<R>(
        &self,
        apply: impl FnOnce(&mut HashMap<String, String>) -> Result<R, Box<dyn std::error::Error>>,
    ) -> Result<R, Box<dyn std::error::Error>> {
        if let Some(parent) = self.registry_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let _lock = crate::executor::state::acquire_lock_async(&self.registry_path).await?;
        let mut data = self.read_registry_json().await;
        let result = apply(&mut data)?;
        let data_map: serde_json::Map<String, serde_json::Value> = data
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        crate::executor::state::atomic_write_json_async(&self.registry_path, &data_map).await?;
        log::debug!(
            "locked_write: wrote {} entries to {}",
            data.len(),
            self.registry_path.display(),
        );
        Ok(result)
    }

    pub async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact> {
        self.read_registry_json()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| MissingArtifact {
                key: key.to_string(),
            })
    }

    /// Compute the canonical filesystem path for `uri` without touching the registry.
    pub async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        let key = uri.to_string();
        if uri.scheme != "artifact" {
            log::warn!(
                "path_for_uri({:?}): unrecognized scheme {:?} — typo? (expected 'artifact')",
                key,
                uri.scheme,
            );
        }
        let mut name = uri.path.trim_start_matches('/').to_string();
        if let Some(rest) = name.strip_prefix("session/") {
            name = rest.to_string();
        }
        let path = self.artifact_dir.join(&name);
        // Ensure artifact_dir exists before canonicalizing
        tokio::fs::create_dir_all(&self.artifact_dir).await?;
        let base = tokio::fs::canonicalize(&self.artifact_dir).await?;
        // Create parent dirs so parent-side canonicalization works
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Resolve parent to handle symlinks, then rejoin filename (file may not exist yet)
        let parent_resolved = path
            .parent()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent directory")
            })?
            .canonicalize()?;
        let resolved = parent_resolved.join(path.file_name().unwrap());
        if !resolved.starts_with(&base) {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("path escapes artifact directory: {uri}"),
            )));
        }
        Ok(resolved.to_string_lossy().to_string())
    }

    /// Bind `key` to `path` and persist. The file at `path` must already exist;
    /// idempotent for an identical binding; a conflicting binding is a
    /// `DuplicateArtifact` error.
    pub async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.locked_write(|data| {
            if let Some(existing) = data.get(key) {
                if existing == path {
                    log::debug!("commit: {:?} already bound to same path — idempotent", key);
                    return Ok(());
                }
                return Err(Box::new(DuplicateArtifact {
                    key: key.to_string(),
                    existing: existing.clone(),
                    incoming: path.to_string(),
                }));
            }
            data.insert(key.to_string(), path.to_string());
            log::debug!("commit: {:?} -> {:?}", key, path);
            Ok(())
        })
        .await
    }

    /// Write `content` to the path for `uri`, then commit. For bootstrap ingestion.
    pub async fn write_into_registry(
        &self,
        uri: &Uri,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let path = self.path_for_uri(uri).await?;
        tokio::fs::write(&path, content).await?;
        self.commit(&uri.to_string(), &path).await?;
        Ok(path)
    }

    /// Copy `source` to the path for `uri`, then commit. For bootstrap ingestion.
    pub async fn copy_into_registry(
        &self,
        uri: &Uri,
        source: &Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let path = self.path_for_uri(uri).await?;
        tokio::fs::copy(source, &path).await?;
        self.commit(&uri.to_string(), &path).await?;
        Ok(path)
    }

    pub async fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let raw = self.data_uri(uri_str).await?;
        let p = if raw.starts_with("file://session/") {
            let name = raw.strip_prefix("file://session/").unwrap_or(&raw);
            self.artifact_dir.join(name)
        } else if let Some(stripped) = raw.strip_prefix("file://") {
            PathBuf::from(stripped)
        } else {
            PathBuf::from(&raw)
        };
        if !p.exists() {
            return Err(Box::new(MissingArtifact {
                key: uri_str.to_string(),
            }));
        }
        let text = tokio::fs::read_to_string(&p).await?;
        log::debug!("content({:?}) read {} bytes", uri_str, text.len());
        if let Some(jp) = json_path {
            let mut data: serde_json::Value = serde_json::from_str(&text)?;
            for segment in jp.split('.') {
                data = data
                    .get(segment)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("json path segment {segment:?} not found"),
                        )
                    })?
                    .clone();
            }
            Ok(match data {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
        } else {
            Ok(text)
        }
    }

    pub async fn is_registered(&self, key: &str) -> bool {
        self.read_registry_json().await.contains_key(key)
    }

    pub async fn keys(&self) -> Vec<String> {
        self.read_registry_json().await.into_keys().collect()
    }

    pub async fn merge_from(
        &self,
        other: &FileSystemArtifactRegistry,
        key_map: Option<&HashMap<String, String>>,
        copy_files: bool,
        keys: Option<&std::collections::HashSet<String>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let other_data = other.read_registry_json().await;
        let iter: Box<dyn Iterator<Item = &String>> = match keys {
            Some(k) => Box::new(k.iter()),
            None => Box::new(other_data.keys()),
        };
        let mut merged = 0u64;
        self.locked_write(|self_data| {
            for key in iter {
                let uri_str = match other_data.get(key) {
                    Some(v) => v,
                    None => continue,
                };
                let parent_key = key_map.and_then(|km| km.get(key)).unwrap_or(key);
                if self_data.contains_key(parent_key) {
                    continue;
                }
                let is_file_artifact =
                    uri_str.starts_with("file://") || Path::new(uri_str).is_absolute();
                if copy_files && is_file_artifact {
                    let src_path = if uri_str.starts_with("file://session/") {
                        let name = uri_str.strip_prefix("file://session/").unwrap_or(uri_str);
                        other.artifact_dir.join(name)
                    } else if uri_str.starts_with("file://") {
                        PathBuf::from(uri_str.strip_prefix("file://").unwrap_or(uri_str))
                    } else {
                        PathBuf::from(uri_str)
                    };
                    if !src_path.exists() {
                        log::warn!("child artifact missing: {}", src_path.display());
                        continue;
                    }
                    let unique_name = if key_map.is_some() {
                        let mut n = parent_key.replace('/', "_");
                        if let Some(ext) = src_path.extension() {
                            n.push('.');
                            n.push_str(&ext.to_string_lossy());
                        }
                        n
                    } else {
                        src_path
                            .file_name()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_else(|| parent_key.clone())
                    };
                    let dest_path = self.artifact_dir.join(&unique_name);
                    if let Some(parent) = dest_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&src_path, &dest_path)?;
                    self_data.insert(parent_key.clone(), dest_path.to_string_lossy().to_string());
                    // Also register under the original (un-remapped) key so
                    // stages can reference child artifacts by their bound URI.
                    if key_map.is_some() && key != parent_key && !self_data.contains_key(key) {
                        self_data.insert(key.clone(), dest_path.to_string_lossy().to_string());
                    }
                    merged += 1;
                } else {
                    self_data.insert(parent_key.clone(), uri_str.clone());
                    if key_map.is_some() && key != parent_key && !self_data.contains_key(key) {
                        self_data.insert(key.clone(), uri_str.clone());
                    }
                    merged += 1;
                }
            }
            Ok(())
        })
        .await?;
        log::info!("merge_from: merged {} entries", merged);
        Ok(())
    }

    pub async fn from_registry_file(
        path: &Path,
        artifact_dir: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let registry = FileSystemArtifactRegistry::new(artifact_dir);
        if path != registry.registry_path && tokio::fs::try_exists(path).await.unwrap_or(false) {
            let content = tokio::fs::read_to_string(path).await?;
            let parsed: HashMap<String, String> = serde_json::from_str(&content)?;
            let count = parsed.len();
            registry
                .locked_write(|data| {
                    *data = parsed;
                    Ok(())
                })
                .await?;
            log::info!(
                "loaded custom registry from {} ({} entries)",
                path.display(),
                count,
            );
        }
        Ok(registry)
    }
}

// --- ArtifactRegistry impl for FileSystemArtifactRegistry ---

#[async_trait::async_trait]
impl ArtifactRegistry for FileSystemArtifactRegistry {
    async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact> {
        self.data_uri(key).await
    }

    async fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.content(uri_str, json_path).await
    }

    async fn is_registered(&self, key: &str) -> bool {
        self.is_registered(key).await
    }

    async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        self.path_for_uri(uri).await
    }

    async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.commit(key, path).await
    }

    async fn write_into_registry(
        &self,
        uri: &Uri,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.write_into_registry(uri, content).await
    }

    async fn is_path_produced(&self, path: &str) -> bool {
        tokio::fs::metadata(path)
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }
}

// --- DryRunArtifactRegistry ---

/// A no-I/O registry for dry-run execution.
///
/// All methods operate on an in-memory `HashMap<String, String>` (key → path)
/// behind a `Mutex`. `path_for_uri` and `write_into_registry` return sentinel
/// paths under `/dev/null/dry-run/` — no filesystem access, no directory
/// creation. `content` delegates to `data_uri`, mirroring the real
/// `FileSystemArtifactRegistry` so the two methods are always consistent.
pub struct DryRunArtifactRegistry {
    produced: Mutex<HashMap<String, String>>,
}

impl DryRunArtifactRegistry {
    /// Create a registry pre-populated with the given set of keys.
    /// Each key is mapped to a sentinel path derived from the key itself.
    pub fn seeded(keys: impl IntoIterator<Item = String>) -> Self {
        let map: HashMap<String, String> = keys
            .into_iter()
            .map(|k| {
                let path = Self::key_to_sentinel_path(&k);
                (k, path)
            })
            .collect();
        DryRunArtifactRegistry {
            produced: Mutex::new(map),
        }
    }

    /// Create an empty registry.
    pub fn new() -> Self {
        DryRunArtifactRegistry {
            produced: Mutex::new(HashMap::new()),
        }
    }

    fn sentinel_path(uri: &Uri) -> String {
        format!("/dev/null/dry-run/{}", uri.path.trim_start_matches('/'))
    }

    /// Derive a sentinel path from a key string (which is expected to be a
    /// URI like `artifact://foo`).
    fn key_to_sentinel_path(key: &str) -> String {
        if let Ok(uri) = Uri::parse(key) {
            Self::sentinel_path(&uri)
        } else {
            format!("/dev/null/dry-run/{}", key.trim_start_matches('/'))
        }
    }
}

impl Default for DryRunArtifactRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ArtifactRegistry for DryRunArtifactRegistry {
    async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact> {
        let map = self.produced.lock().unwrap();
        map.get(key).cloned().ok_or_else(|| MissingArtifact {
            key: key.to_string(),
        })
    }

    async fn content(
        &self,
        uri_str: &str,
        _json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        // Delegate to data_uri so the two methods stay consistent — same
        // pattern as the real FileSystemArtifactRegistry.
        let _path = self.data_uri(uri_str).await?;
        Ok("dry-run".to_string())
    }

    async fn is_registered(&self, key: &str) -> bool {
        self.produced.lock().unwrap().contains_key(key)
    }

    async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        Ok(Self::sentinel_path(uri))
    }

    async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.produced
            .lock()
            .unwrap()
            .insert(key.to_string(), path.to_string());
        Ok(())
    }

    async fn write_into_registry(
        &self,
        uri: &Uri,
        _content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let key = uri.to_string();
        let path = Self::sentinel_path(uri);
        self.produced.lock().unwrap().insert(key, path.clone());
        Ok(path)
    }

    async fn is_path_produced(&self, _path: &str) -> bool {
        true
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::uri::Uri;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        (tmp, artifact_dir)
    }

    async fn write_file(reg: &FileSystemArtifactRegistry, name: &str, content: &str) -> String {
        let uri = Uri::parse(&format!("artifact://{name}")).unwrap();
        reg.write_into_registry(&uri, content).await.unwrap()
    }

    #[test]
    fn artifact_registry_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<FileSystemArtifactRegistry>();
        assert_sync::<FileSystemArtifactRegistry>();
    }

    #[test]
    fn dry_run_registry_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<DryRunArtifactRegistry>();
        assert_sync::<DryRunArtifactRegistry>();
    }

    // --- DryRunArtifactRegistry tests ---

    #[tokio::test]
    async fn test_dry_run_is_registered_after_commit() {
        let reg = DryRunArtifactRegistry::new();
        assert!(!reg.is_registered("artifact://out.md").await);
        reg.commit("artifact://out.md", "/dev/null/dry-run/out.md")
            .await
            .unwrap();
        assert!(reg.is_registered("artifact://out.md").await);
    }

    #[tokio::test]
    async fn test_dry_run_data_uri_returns_sentinel() {
        let reg = DryRunArtifactRegistry::seeded(["artifact://x".to_string()]);
        assert_eq!(
            reg.data_uri("artifact://x").await.unwrap(),
            "/dev/null/dry-run/x"
        );
    }

    #[tokio::test]
    async fn test_dry_run_data_uri_missing() {
        let reg = DryRunArtifactRegistry::new();
        let err = reg.data_uri("artifact://x").await.unwrap_err();
        assert!(err.to_string().contains("artifact://x"));
    }

    #[tokio::test]
    async fn test_dry_run_content_returns_placeholder() {
        let reg = DryRunArtifactRegistry::seeded(["artifact://x".to_string()]);
        assert_eq!(reg.content("artifact://x", None).await.unwrap(), "dry-run");
    }

    #[tokio::test]
    async fn test_dry_run_content_missing() {
        let reg = DryRunArtifactRegistry::new();
        let err = reg.content("artifact://x", None).await.unwrap_err();
        assert!(err.to_string().contains("artifact://x"));
    }

    #[tokio::test]
    async fn test_dry_run_path_for_uri_returns_sentinel() {
        let reg = DryRunArtifactRegistry::new();
        let uri = Uri::parse("artifact://out.md").unwrap();
        assert_eq!(
            reg.path_for_uri(&uri).await.unwrap(),
            "/dev/null/dry-run/out.md"
        );
    }

    #[tokio::test]
    async fn test_dry_run_write_into_registry_returns_sentinel() {
        let reg = DryRunArtifactRegistry::new();
        let uri = Uri::parse("artifact://out.md").unwrap();
        let path = reg.write_into_registry(&uri, "content").await.unwrap();
        assert_eq!(path, "/dev/null/dry-run/out.md");
        assert!(reg.is_registered("artifact://out.md").await);
    }

    // --- FileSystemArtifactRegistry tests ---

    #[tokio::test]
    async fn test_data_uri_unbound_raises_missing() {
        let (_tmp, artifact_dir) = setup();
        let registry = FileSystemArtifactRegistry::new(artifact_dir);
        let err = registry.data_uri("nonexistent").await.unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[tokio::test]
    async fn test_write_into_registry_persists_and_roundtrip() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir.clone());
        let uri = Uri::new("artifact".to_string(), "foo.txt".to_string());
        let path = reg.write_into_registry(&uri, "hello").await.unwrap();
        assert!(path.contains("foo.txt"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");

        // Reload from disk
        let reg2 = FileSystemArtifactRegistry::new(artifact_dir);
        assert_eq!(reg2.data_uri("artifact://foo.txt").await.unwrap(), path);
    }

    #[tokio::test]
    async fn test_path_for_uri_does_not_register() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://later.txt").unwrap();
        let path = reg.path_for_uri(&uri).await.unwrap();
        assert!(path.ends_with("later.txt"));
        assert!(!reg.is_registered("artifact://later.txt").await);
    }

    #[tokio::test]
    async fn test_commit_idempotent_same_path() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://a.txt").unwrap();
        let path = reg.path_for_uri(&uri).await.unwrap();
        fs::write(&path, "").unwrap();
        reg.commit("artifact://a.txt", &path).await.unwrap();
        reg.commit("artifact://a.txt", &path).await.unwrap();
    }

    #[tokio::test]
    async fn test_commit_with_missing_file_succeeds() {
        let (tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let missing = tmp.path().join("does-not-exist.txt");
        // commit does not check file existence; it only enforces key uniqueness
        reg.commit("artifact://gone.txt", &missing.to_string_lossy())
            .await
            .unwrap();
        assert!(reg.is_registered("artifact://gone.txt").await);
    }

    #[tokio::test]
    async fn test_commit_conflicting_path_raises() {
        let (tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        fs::write(&one, "").unwrap();
        fs::write(&two, "").unwrap();
        reg.commit("artifact://a.txt", &one.to_string_lossy())
            .await
            .unwrap();
        let err = reg
            .commit("artifact://a.txt", &two.to_string_lossy())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate artifact"));
    }

    #[tokio::test]
    async fn test_copy_into_registry() {
        let (tmp, artifact_dir) = setup();
        let src = tmp.path().join("src.txt");
        fs::write(&src, "copied").unwrap();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://dst.txt").unwrap();
        let path = reg.copy_into_registry(&uri, &src).await.unwrap();
        assert!(reg.is_registered("artifact://dst.txt").await);
        assert_eq!(fs::read_to_string(&path).unwrap(), "copied");
    }

    #[tokio::test]
    async fn test_path_escape_prevention() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let uri = Uri::new("artifact".to_string(), "../bad.txt".to_string());
        assert!(reg.path_for_uri(&uri).await.is_err());
    }

    #[tokio::test]
    async fn test_content_reads_file() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir.clone());
        write_file(&reg, "hello.txt", "world").await;
        assert_eq!(
            reg.content("artifact://hello.txt", None).await.unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn test_content_with_json_path() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        write_file(&reg, "data.json", r#"{"a":{"b":"c"}}"#).await;
        let content = reg
            .content("artifact://data.json", Some("a.b"))
            .await
            .unwrap();
        assert_eq!(content, "c");
    }

    #[tokio::test]
    async fn test_content_reads_file_containing_uri_text() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        write_file(&reg, "range", "git://range/abc..def").await;
        assert_eq!(
            reg.content("artifact://range", None).await.unwrap(),
            "git://range/abc..def",
        );
    }

    #[tokio::test]
    async fn test_is_registered_false_for_missing_key() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        assert!(!reg.is_registered("nonexistent").await);
    }

    #[tokio::test]
    async fn test_is_registered_true_after_write() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        write_file(&reg, "stuff.txt", "data").await;
        assert!(reg.is_registered("artifact://stuff.txt").await);
    }

    #[tokio::test]
    async fn test_is_registered_true_after_file_deleted() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let path = write_file(&reg, "dead.txt", "data").await;
        assert!(reg.is_registered("artifact://dead.txt").await);
        fs::remove_file(&path).unwrap();
        assert!(reg.is_registered("artifact://dead.txt").await);
    }

    #[tokio::test]
    async fn test_keys_returns_registered_keys() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        write_file(&reg, "a", "").await;
        write_file(&reg, "b", "").await;
        let mut keys = reg.keys().await;
        keys.sort();
        assert_eq!(keys, vec!["artifact://a", "artifact://b"]);
    }

    #[tokio::test]
    async fn test_merge_from_identity_key_map() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let other = FileSystemArtifactRegistry::new(other_dir);
        write_file(&other, "k1", "v").await;

        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, None, false, None).await.unwrap();
        assert_eq!(
            reg.data_uri("artifact://k1").await.unwrap(),
            other.data_uri("artifact://k1").await.unwrap(),
        );
    }

    #[tokio::test]
    async fn test_merge_from_custom_key_map() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let other = FileSystemArtifactRegistry::new(other_dir);
        write_file(&other, "child", "v").await;

        let mut key_map = HashMap::new();
        key_map.insert("artifact://child".to_string(), "parent".to_string());

        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, Some(&key_map), false, None)
            .await
            .unwrap();
        assert_eq!(
            reg.data_uri("parent").await.unwrap(),
            other.data_uri("artifact://child").await.unwrap(),
        );
    }

    #[tokio::test]
    async fn test_merge_from_with_file_copy() {
        let (_tmp, artifact_dir) = setup();
        let (tmp2, other_dir) = setup();
        let _ = &tmp2;

        let other = FileSystemArtifactRegistry::new(other_dir);
        let src_file = write_file(&other, "note.txt", "hello").await;
        assert!(Path::new(&src_file).exists());

        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, None, true, None).await.unwrap();
        let stored = reg.data_uri("artifact://note.txt").await.unwrap();
        let p = PathBuf::from(stored);
        assert!(p.exists());
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_from_registry_file_constructor() {
        let (_tmp, artifact_dir) = setup();
        let reg_file = artifact_dir.parent().unwrap().join("custom_registry.json");
        fs::write(&reg_file, r#"{"a":"b"}"#).unwrap();
        let reg = FileSystemArtifactRegistry::from_registry_file(&reg_file, artifact_dir)
            .await
            .unwrap();
        assert_eq!(reg.data_uri("a").await.unwrap(), "b");
    }
}
