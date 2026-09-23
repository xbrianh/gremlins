use std::collections::{HashMap, HashSet};
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

// --- Collision mode ---

/// Controls behaviour when a key being merged is already registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collision {
    /// Return a [`DuplicateArtifact`] error.
    Error,
    /// Skip the key silently.
    Ignore,
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
    /// Copy a filesystem file into the registry under `uri`.
    async fn copy_into_registry(
        &self,
        uri: &Uri,
        source: &Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let content = tokio::fs::read_to_string(source).await?;
        self.write_into_registry(uri, &content).await
    }
    /// All artifact URIs currently registered.
    async fn keys(&self) -> Vec<String>;

    /// Merge every artifact from `other` into `self`.
    ///
    /// When `key_prefix` is set, each key from `other` is registered under
    /// both `artifact://{prefix}/{bare}` and its original key (if different).
    /// `collision` controls what happens when a destination key is already
    /// registered. Returns the number of keys merged.
    ///
    /// **Serialization:** the default implementation is not safe for
    /// concurrent callers. The caller must serialize merges into the same
    /// destination registry (today's single callsite in `parallel.rs`
    /// merges children sequentially, satisfying this).
    async fn merge_registry(
        &self,
        other: &(dyn ArtifactRegistry + Sync),
        collision: Collision,
        key_prefix: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        merge_registry_via_content(self, other, collision, key_prefix).await
    }

    /// Clone / fork this registry for use by a child gremlin.
    ///
    /// The child's artifacts will be stored under `child_artifact_dir`.
    async fn fork_registry(
        &self,
        child_artifact_dir: &Path,
    ) -> Result<Box<dyn ArtifactRegistry>, Box<dyn std::error::Error>>;

    /// Produce a localized (filesystem-scoped) registry containing only the
    /// given subset of keys. The returned registry lives in a separate
    /// directory so that unscoped artifacts cannot be discovered by
    /// sniffing the filesystem.
    async fn checkout(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn LocalizedArtifactRegistry>, Box<dyn std::error::Error>> {
        let _ = keys;
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "checkout not supported for this registry type",
        )))
    }

    /// Enable downcast to concrete registry types (for optimisation paths).
    /// Default returns `None` — backends that support downcasting override this.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

// --- LocalizedArtifactRegistry trait ---

/// A registry that is backed by a concrete filesystem directory (or a
/// sentinel path for dry-run). Provides methods that require locality:
/// path resolution, file copy, and directory inspection.
///
/// Anything that implements [`LocalizedArtifactRegistry`] also implements
/// [`ArtifactRegistry`] (supertrait), so artifact lookups work transparently.
#[async_trait::async_trait]
pub trait LocalizedArtifactRegistry: ArtifactRegistry {
    /// The artifact storage directory (or sentinel path for dry-run).
    fn artifact_dir(&self) -> &Path;

    /// Check whether a file exists and is non-empty at `path`.
    /// For dry-run registries this always returns true.
    async fn has_file(&self, path: &str) -> bool;
}

// --- Helpers ---

/// Content-based merge implementation used by the default
/// [`ArtifactRegistry::merge_registry`] and as a fallback by backends that
/// cannot do a direct file-copy optimisation.
async fn merge_registry_via_content<D: ArtifactRegistry + Sync + ?Sized>(
    dest: &D,
    src: &(dyn ArtifactRegistry + Sync),
    collision: Collision,
    key_prefix: Option<&str>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut merged = 0usize;
    for key in src.keys().await {
        let data_uri = src.data_uri(&key).await.unwrap_or_default();
        if data_uri.is_empty() {
            continue;
        }

        // Snapshot content from the source before we compute destination
        // keys or check collisions.
        let content = src.content(&key, None).await.unwrap_or_default();

        let bare = key.strip_prefix("artifact://").unwrap_or(&key);
        let dest_key = if let Some(prefix) = key_prefix {
            format!("artifact://{}/{}", prefix, bare)
        } else {
            key.clone()
        };

        // Check for collisions on the primary destination key.
        if dest.is_registered(&dest_key).await {
            match collision {
                Collision::Error => {
                    let existing = dest.data_uri(&dest_key).await.unwrap_or_default();
                    return Err(Box::new(DuplicateArtifact {
                        key: dest_key,
                        existing,
                        incoming: data_uri,
                    }));
                }
                Collision::Ignore => continue,
            }
        }

        // When a prefix is in use, also check collisions on the
        // original (un-prefixed) key before committing anything.
        if key_prefix.is_some() && key != dest_key && dest.is_registered(&key).await {
            match collision {
                Collision::Error => {
                    let existing = dest.data_uri(&key).await.unwrap_or_default();
                    return Err(Box::new(DuplicateArtifact {
                        key,
                        existing,
                        incoming: data_uri,
                    }));
                }
                Collision::Ignore => {
                    // Fall through — skip the alias below.
                }
            }
        }

        let dest_uri = Uri::parse(&dest_key).map_err(|e| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid destination URI {dest_key:?}: {e}"),
            ))
        })?;
        let _new_path = dest.write_into_registry(&dest_uri, &content).await?;
        merged += 1;

        // Also register under the original (un-prefixed) key so
        // downstream stages can reference child artifacts by their
        // bound URI. Use write_into_registry so that dry-run backends
        // preserve the content string for the alias.
        if key_prefix.is_some() && key != dest_key && !dest.is_registered(&key).await {
            let alias_uri = Uri::parse(&key).map_err(|e| {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid alias URI {key:?}: {e}"),
                ))
            })?;
            dest.write_into_registry(&alias_uri, &content).await?;
            merged += 1;
        }
    }
    Ok(merged)
}

/// Whether `data_uri` points to a filesystem path that can be copied.
///
/// Returns `true` for absolute paths (`/…`).
/// Returns `false` for non-file URIs (`http://`, `s3://`, `data:`, etc.).
///
/// Note: `file://` URIs are intentionally not recognized — they have been
/// removed from the registry design.
fn is_file_artifact(data_uri: &str) -> bool {
    data_uri.starts_with('/')
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
        let name = uri.path.trim_start_matches('/').to_string();
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
        let p = PathBuf::from(&raw);
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

    /// The artifact storage directory.
    pub fn artifact_dir(&self) -> &Path {
        &self.artifact_dir
    }

    /// Merge that prefers a direct file copy when `other` is also a
    /// [`FileSystemArtifactRegistry`], falling back to the content-based
    /// path otherwise.
    pub async fn merge_registry(
        &self,
        other: &(dyn ArtifactRegistry + Sync),
        collision: Collision,
        key_prefix: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        // Fast path: both sides are filesystem-backed — copy files directly.
        // Also try to unwrap a ScopedFileSystemArtifactRegistry (the checkout
        // wrapper) so that merging from a scoped registry hits the fast path.
        let fs: Option<&FileSystemArtifactRegistry> = other
            .as_any()
            .and_then(|a| {
                a.downcast_ref::<FileSystemArtifactRegistry>()
                    .or_else(|| {
                        a.downcast_ref::<ScopedFileSystemArtifactRegistry>()
                            .map(|s| &s.inner)
                    })
            });
        if let Some(other_fs) = fs {
            let mut merged = 0usize;
            for key in other_fs.keys().await {
                let data_uri = other_fs.data_uri(&key).await.unwrap_or_default();
                if data_uri.is_empty() {
                    continue;
                }

                let bare = key.strip_prefix("artifact://").unwrap_or(&key);
                let dest_key = if let Some(prefix) = key_prefix {
                    format!("artifact://{}/{}", prefix, bare)
                } else {
                    key.clone()
                };

                // Collision check on primary destination key.
                if self.is_registered(&dest_key).await {
                    match collision {
                        Collision::Error => {
                            let existing = self.data_uri(&dest_key).await.unwrap_or_default();
                            return Err(Box::new(DuplicateArtifact {
                                key: dest_key,
                                existing,
                                incoming: data_uri.clone(),
                            }));
                        }
                        Collision::Ignore => continue,
                    }
                }

                // Collision check on original key when prefix is in use.
                if key_prefix.is_some() && key != dest_key && self.is_registered(&key).await {
                    match collision {
                        Collision::Error => {
                            let existing = self.data_uri(&key).await.unwrap_or_default();
                            return Err(Box::new(DuplicateArtifact {
                                key,
                                existing,
                                incoming: data_uri.clone(),
                            }));
                        }
                        Collision::Ignore => {
                            // Fall through — skip the alias below.
                        }
                    }
                }

                // Resolve source path for file artifacts;
                // register non-file URIs directly.
                if is_file_artifact(&data_uri) {
                    let src_path = PathBuf::from(&data_uri);

                    let dest_uri = Uri::parse(&dest_key).map_err(|e| {
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("invalid destination URI {dest_key:?}: {e}"),
                        ))
                    })?;
                    let dest_path_str = self.path_for_uri(&dest_uri).await?;
                    let dest_path = Path::new(&dest_path_str);
                    if let Some(parent) = dest_path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::copy(&src_path, dest_path).await?;
                    self.commit(&dest_key, &dest_path_str).await?;
                    merged += 1;

                    // Alias under original key.
                    if key_prefix.is_some() && key != dest_key && !self.is_registered(&key).await {
                        self.commit(&key, &dest_path_str).await?;
                        merged += 1;
                    }
                } else {
                    // Non-file artifact (e.g. http://, s3://): register the URI
                    // string directly without a file copy.
                    self.commit(&dest_key, &data_uri).await?;
                    merged += 1;

                    if key_prefix.is_some() && key != dest_key && !self.is_registered(&key).await {
                        self.commit(&key, &data_uri).await?;
                        merged += 1;
                    }
                }
            }
            return Ok(merged);
        }

        // Fallback: content-based merge.
        merge_registry_via_content(self, other, collision, key_prefix).await
    }
}

// --- ScopedFileSystemArtifactRegistry ---

/// A [`FileSystemArtifactRegistry`] that only allows access to a
/// pre-approved set of keys. Owns a [`TempDir`] so the checkout directory
/// is cleaned up when the registry is dropped.
struct ScopedFileSystemArtifactRegistry {
    inner: FileSystemArtifactRegistry,
    _temp: tempfile::TempDir,
    allowed_keys: HashSet<String>,
}

impl ScopedFileSystemArtifactRegistry {
    fn check_key(&self, key: &str) -> Result<(), Box<dyn std::error::Error>> {
        if !self.allowed_keys.contains(key) {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("key {key:?} is not in the checked-out subset"),
            )));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ArtifactRegistry for ScopedFileSystemArtifactRegistry {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact> {
        self.check_key(key).map_err(|e| MissingArtifact {
            key: format!("{key}: {e}"),
        })?;
        self.inner.data_uri(key).await
    }

    async fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.check_key(uri_str)?;
        self.inner.content(uri_str, json_path).await
    }

    async fn is_registered(&self, key: &str) -> bool {
        self.allowed_keys.contains(key) && self.inner.is_registered(key).await
    }

    async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        self.check_key(&uri.to_string())?;
        self.inner.path_for_uri(uri).await
    }

    async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.check_key(key)?;
        self.inner.commit(key, path).await
    }

    async fn write_into_registry(
        &self,
        uri: &Uri,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.check_key(&uri.to_string())?;
        self.inner.write_into_registry(uri, content).await
    }

    async fn keys(&self) -> Vec<String> {
        self.allowed_keys.iter().cloned().collect()
    }

    async fn fork_registry(
        &self,
        child_artifact_dir: &Path,
    ) -> Result<Box<dyn ArtifactRegistry>, Box<dyn std::error::Error>> {
        self.inner.fork_registry(child_artifact_dir).await
    }

    async fn checkout(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn LocalizedArtifactRegistry>, Box<dyn std::error::Error>> {
        self.inner.checkout(keys).await
    }
}

#[async_trait::async_trait]
impl LocalizedArtifactRegistry for ScopedFileSystemArtifactRegistry {
    fn artifact_dir(&self) -> &Path {
        self.inner.artifact_dir()
    }

    async fn has_file(&self, path: &str) -> bool {
        self.inner.has_file(path).await
    }
}

impl FileSystemArtifactRegistry {
    /// Produce a localized registry containing only the given keys.
    pub async fn checkout(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn LocalizedArtifactRegistry>, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::TempDir::new()?;
        let artifact_dir = temp_dir.path().join("artifacts");
        tokio::fs::create_dir_all(&artifact_dir).await?;
        let new_reg = FileSystemArtifactRegistry::new(artifact_dir);
        let mut allowed = HashSet::new();

        for key in keys {
            if self.is_registered(key).await {
                let data_uri = self.data_uri(key).await?;
                let uri = Uri::parse(key).map_err(|e| {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid key URI {key:?}: {e}"),
                    ))
                })?;

                if is_file_artifact(&data_uri) {
                    // Copy the file directly (handles binary artifacts).
                    let src_path = PathBuf::from(&data_uri);
                    new_reg.copy_into_registry(&uri, &src_path).await?;
                } else {
                    // Non-file artifact: read content as string and write.
                    let content = self.content(key, None).await?;
                    new_reg.write_into_registry(&uri, &content).await?;
                }
            } else {
                // Key is not yet registered — pre-create the path so that
                // path_for_uri works and the agent/exec can write to it.
                // Do NOT commit — the stage's commit_agent/commit_exec will
                // do that after the file is produced.
                let uri = Uri::parse(key).map_err(|e| {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid key URI {key:?}: {e}"),
                    ))
                })?;
                let path = new_reg.path_for_uri(&uri).await?;
                // Create parent dirs so the agent/exec can write the file.
                if let Some(parent) = Path::new(&path).parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }
            allowed.insert(key.clone());
        }

        Ok(Box::new(ScopedFileSystemArtifactRegistry {
            inner: new_reg,
            _temp: temp_dir,
            allowed_keys: allowed,
        }))
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

    async fn copy_into_registry(
        &self,
        uri: &Uri,
        source: &Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.copy_into_registry(uri, source).await
    }

    async fn keys(&self) -> Vec<String> {
        self.read_registry_json().await.into_keys().collect()
    }

    async fn merge_registry(
        &self,
        other: &(dyn ArtifactRegistry + Sync),
        collision: Collision,
        key_prefix: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        self.merge_registry(other, collision, key_prefix).await
    }

    async fn fork_registry(
        &self,
        child_artifact_dir: &Path,
    ) -> Result<Box<dyn ArtifactRegistry>, Box<dyn std::error::Error>> {
        FileSystemArtifactRegistry::from_registry_file(
            &self.registry_path,
            child_artifact_dir.to_path_buf(),
        )
        .await
        .map(|r| Box::new(r) as Box<dyn ArtifactRegistry>)
    }

    async fn checkout(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn LocalizedArtifactRegistry>, Box<dyn std::error::Error>> {
        self.checkout(keys).await
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

// --- LocalizedArtifactRegistry impl for FileSystemArtifactRegistry ---

#[async_trait::async_trait]
impl LocalizedArtifactRegistry for FileSystemArtifactRegistry {
    fn artifact_dir(&self) -> &Path {
        self.artifact_dir()
    }

    async fn has_file(&self, path: &str) -> bool {
        tokio::fs::metadata(path)
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }
}

// --- DryRunArtifactRegistry ---

/// A no-I/O registry for dry-run execution.
///
/// All methods operate on an in-memory `HashMap<String, (String, String)>`
/// (key → (path, content)) behind a `Mutex`. `path_for_uri` and
/// `write_into_registry` return sentinel paths under `/dev/null/dry-run/` —
/// no filesystem access, no directory creation. `content` returns the stored
/// content (or empty string for artifacts registered via `commit`/`copy`,
/// whose real content lives on the filesystem).
pub struct DryRunArtifactRegistry {
    /// key → (path, content)
    produced: Mutex<HashMap<String, (String, String)>>,
}

impl Clone for DryRunArtifactRegistry {
    fn clone(&self) -> Self {
        let map = self.produced.lock().unwrap().clone();
        DryRunArtifactRegistry {
            produced: Mutex::new(map),
        }
    }
}

impl DryRunArtifactRegistry {
    /// Create a registry pre-populated with the given set of keys.
    /// Each key is mapped to a sentinel path derived from the key itself,
    /// with empty content.
    pub fn seeded(keys: impl IntoIterator<Item = String>) -> Self {
        let map: HashMap<String, (String, String)> = keys
            .into_iter()
            .map(|k| {
                let path = Self::key_to_sentinel_path(&k);
                (k, (path, String::new()))
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

    /// Sentinel artifact directory path.
    pub fn artifact_dir(&self) -> &Path {
        Path::new("/dev/null/dry-run")
    }

    /// Produce a filtered in-memory registry containing only the given keys.
    /// Unregistered keys get sentinel entries so path_for_uri works.
    pub async fn checkout(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn LocalizedArtifactRegistry>, Box<dyn std::error::Error>> {
        let map = self.produced.lock().unwrap();
        let mut filtered: HashMap<String, (String, String)> = HashMap::new();
        for k in keys {
            if let Some(v) = map.get(k) {
                filtered.insert(k.clone(), v.clone());
            } else {
                // Unregistered key — create a sentinel entry with empty
                // path so merge_registry_via_content skips it (data_uri
                // returns empty). path_for_uri still works because it
                // doesn't consult the produced map.
                filtered.insert(k.clone(), (String::new(), String::new()));
            }
        }
        Ok(Box::new(DryRunArtifactRegistry {
            produced: Mutex::new(filtered),
        }))
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
        map.get(key)
            .map(|(path, _)| path.clone())
            .ok_or_else(|| MissingArtifact {
                key: key.to_string(),
            })
    }

    async fn content(
        &self,
        uri_str: &str,
        _json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let map = self.produced.lock().unwrap();
        let (_, content) = map.get(uri_str).ok_or_else(|| {
            Box::new(MissingArtifact {
                key: uri_str.to_string(),
            })
        })?;
        Ok(content.clone())
    }

    async fn is_registered(&self, key: &str) -> bool {
        self.produced.lock().unwrap().contains_key(key)
    }

    async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        Ok(Self::sentinel_path(uri))
    }

    async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Store the path with empty content — real content lives on the
        // filesystem, which is inaccessible in dry-run mode.
        self.produced
            .lock()
            .unwrap()
            .insert(key.to_string(), (path.to_string(), String::new()));
        Ok(())
    }

    async fn write_into_registry(
        &self,
        uri: &Uri,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let key = uri.to_string();
        let path = Self::sentinel_path(uri);
        self.produced
            .lock()
            .unwrap()
            .insert(key, (path.clone(), content.to_string()));
        Ok(path)
    }

    async fn copy_into_registry(
        &self,
        uri: &Uri,
        _source: &Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let key = uri.to_string();
        let path = Self::sentinel_path(uri);
        self.produced
            .lock()
            .unwrap()
            .insert(key, (path.clone(), String::new()));
        Ok(path)
    }

    async fn keys(&self) -> Vec<String> {
        self.produced.lock().unwrap().keys().cloned().collect()
    }

    async fn fork_registry(
        &self,
        _child_artifact_dir: &Path,
    ) -> Result<Box<dyn ArtifactRegistry>, Box<dyn std::error::Error>> {
        Ok(Box::new(self.clone()))
    }

    async fn checkout(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn LocalizedArtifactRegistry>, Box<dyn std::error::Error>> {
        self.checkout(keys).await
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

// --- LocalizedArtifactRegistry impl for DryRunArtifactRegistry ---

#[async_trait::async_trait]
impl LocalizedArtifactRegistry for DryRunArtifactRegistry {
    fn artifact_dir(&self) -> &Path {
        self.artifact_dir()
    }

    async fn has_file(&self, _path: &str) -> bool {
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
    async fn test_dry_run_content_returns_stored_value() {
        let reg = DryRunArtifactRegistry::seeded(["artifact://x".to_string()]);
        // Seeded artifacts have empty content.
        assert_eq!(reg.content("artifact://x", None).await.unwrap(), "");
        // Artifacts written via write_into_registry return the stored content.
        let uri = Uri::parse("artifact://y").unwrap();
        reg.write_into_registry(&uri, "hello").await.unwrap();
        assert_eq!(reg.content("artifact://y", None).await.unwrap(), "hello");
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
    async fn test_merge_registry_identity() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let other = FileSystemArtifactRegistry::new(other_dir);
        write_file(&other, "k1", "v").await;

        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let count = reg
            .merge_registry(&other, Collision::Error, None)
            .await
            .unwrap();
        assert_eq!(count, 1);
        // The key is registered and its content is accessible.
        assert!(reg.is_registered("artifact://k1").await);
        assert_eq!(reg.content("artifact://k1", None).await.unwrap(), "v");
    }

    #[tokio::test]
    async fn test_merge_registry_with_prefix() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let other = FileSystemArtifactRegistry::new(other_dir);
        write_file(&other, "child", "v").await;

        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let count = reg
            .merge_registry(&other, Collision::Error, Some("parent"))
            .await
            .unwrap();
        // Both the prefixed key and the original key are registered.
        assert!(count >= 1);
        assert!(reg.is_registered("artifact://parent/child").await);
        assert!(reg.is_registered("artifact://child").await);
    }

    #[tokio::test]
    async fn test_merge_registry_with_file_copy() {
        let (_tmp, artifact_dir) = setup();
        let (tmp2, other_dir) = setup();
        let _ = &tmp2;

        let other = FileSystemArtifactRegistry::new(other_dir);
        let src_file = write_file(&other, "note.txt", "hello").await;
        assert!(Path::new(&src_file).exists());

        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        let count = reg
            .merge_registry(&other, Collision::Error, None)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let stored = reg.data_uri("artifact://note.txt").await.unwrap();
        let p = PathBuf::from(stored);
        assert!(p.exists());
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
    }

    // --- merge_registry: dangling-reference regression test ---

    #[tokio::test]
    async fn test_merge_registry_survives_source_deletion() {
        let (_tmp, artifact_dir) = setup();
        let (tmp2, other_dir) = setup();

        let other = FileSystemArtifactRegistry::new(other_dir.clone());
        write_file(&other, "data.txt", "survive-me").await;

        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        reg.merge_registry(&other, Collision::Error, None)
            .await
            .unwrap();

        // Delete the source registry's artifact directory.
        drop(other);
        drop(tmp2);

        // The destination registry should still be able to read the content.
        let content = reg.content("artifact://data.txt", None).await.unwrap();
        assert_eq!(content, "survive-me");
    }

    // --- merge_registry: collision mode tests ---

    #[tokio::test]
    async fn test_merge_registry_collision_error() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let other = FileSystemArtifactRegistry::new(other_dir);
        write_file(&other, "dup", "from-other").await;

        let reg = FileSystemArtifactRegistry::new(artifact_dir.clone());
        // Pre-register the same key in the destination.
        write_file(&reg, "dup", "existing").await;

        let err = reg
            .merge_registry(&other, Collision::Error, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate artifact"));
    }

    #[tokio::test]
    async fn test_merge_registry_collision_ignore() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let other = FileSystemArtifactRegistry::new(other_dir);
        write_file(&other, "dup", "from-other").await;

        let reg = FileSystemArtifactRegistry::new(artifact_dir.clone());
        write_file(&reg, "dup", "existing").await;

        let count = reg
            .merge_registry(&other, Collision::Ignore, None)
            .await
            .unwrap();
        // The duplicate key was skipped.
        assert_eq!(count, 0);
        // The existing value is preserved.
        let content = reg.content("artifact://dup", None).await.unwrap();
        assert_eq!(content, "existing");
    }

    // --- DryRun merge_registry ---

    #[tokio::test]
    async fn test_dry_run_merge_registry() {
        let src = DryRunArtifactRegistry::seeded(["artifact://k1".to_string()]);
        let dst = DryRunArtifactRegistry::new();
        let count = dst
            .merge_registry(&src, Collision::Error, None)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(dst.is_registered("artifact://k1").await);
        // The stored path should be a dry-run sentinel.
        let uri = dst.data_uri("artifact://k1").await.unwrap();
        assert!(uri.starts_with("/dev/null/dry-run/"));
    }

    #[tokio::test]
    async fn test_dry_run_merge_preserves_content() {
        // Write an artifact into the source with real content.
        let src = DryRunArtifactRegistry::new();
        let uri = Uri::parse("artifact://note.txt").unwrap();
        src.write_into_registry(&uri, "hello world").await.unwrap();

        let dst = DryRunArtifactRegistry::new();
        dst.merge_registry(&src, Collision::Error, None)
            .await
            .unwrap();

        // The merged artifact must return the original content.
        let got = dst.content("artifact://note.txt", None).await.unwrap();
        assert_eq!(got, "hello world");
    }

    #[tokio::test]
    async fn test_dry_run_merge_registry_with_prefix() {
        let src = DryRunArtifactRegistry::seeded(["artifact://child".to_string()]);
        let dst = DryRunArtifactRegistry::new();
        let count = dst
            .merge_registry(&src, Collision::Error, Some("parent"))
            .await
            .unwrap();
        assert!(count >= 1);
        assert!(dst.is_registered("artifact://parent/child").await);
        assert!(dst.is_registered("artifact://child").await);
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

    // --- checkout tests ---

    #[tokio::test]
    async fn test_filesystem_checkout_subset() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        write_file(&reg, "a", "content-a").await;
        write_file(&reg, "b", "content-b").await;
        write_file(&reg, "c", "content-c").await;

        let localized = reg
            .checkout(&["artifact://a".to_string(), "artifact://c".to_string()])
            .await
            .unwrap();

        assert!(localized.is_registered("artifact://a").await);
        assert!(!localized.is_registered("artifact://b").await);
        assert!(localized.is_registered("artifact://c").await);

        assert_eq!(
            localized.content("artifact://a", None).await.unwrap(),
            "content-a"
        );
        assert_eq!(
            localized.content("artifact://c", None).await.unwrap(),
            "content-c"
        );
    }

    #[tokio::test]
    async fn test_filesystem_checkout_empty_keys() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        write_file(&reg, "a", "content-a").await;

        let localized = reg.checkout(&[]).await.unwrap();
        assert!(localized.keys().await.is_empty());
    }

    #[tokio::test]
    async fn test_filesystem_checkout_isolated_directory() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir);
        write_file(&reg, "secret", "classified").await;

        let localized = reg
            .checkout(&["artifact://secret".to_string()])
            .await
            .unwrap();

        // The checkout lives in a different directory from the source.
        assert_ne!(localized.artifact_dir(), reg.artifact_dir());
        assert!(!localized.artifact_dir().starts_with(reg.artifact_dir()));
    }

    #[tokio::test]
    async fn test_dry_run_checkout_subset() {
        let reg = DryRunArtifactRegistry::new();
        let uri_a = Uri::parse("artifact://a").unwrap();
        let uri_b = Uri::parse("artifact://b").unwrap();
        reg.write_into_registry(&uri_a, "content-a").await.unwrap();
        reg.write_into_registry(&uri_b, "content-b").await.unwrap();

        let localized = reg.checkout(&["artifact://a".to_string()]).await.unwrap();

        assert!(localized.is_registered("artifact://a").await);
        assert!(!localized.is_registered("artifact://b").await);
        assert_eq!(
            localized.content("artifact://a", None).await.unwrap(),
            "content-a"
        );
    }

    #[tokio::test]
    async fn test_dry_run_checkout_empty_keys() {
        let reg = DryRunArtifactRegistry::new();
        let uri = Uri::parse("artifact://x").unwrap();
        reg.write_into_registry(&uri, "x").await.unwrap();

        let localized = reg.checkout(&[]).await.unwrap();
        assert!(localized.keys().await.is_empty());
    }

    // --- LocalizedArtifactRegistry tests ---

    #[tokio::test]
    async fn test_filesystem_artifact_dir() {
        let (_tmp, artifact_dir) = setup();
        let reg = FileSystemArtifactRegistry::new(artifact_dir.clone());
        assert_eq!(reg.artifact_dir(), artifact_dir.as_path());
    }

    #[tokio::test]
    async fn test_dry_run_artifact_dir() {
        let reg = DryRunArtifactRegistry::new();
        assert_eq!(reg.artifact_dir(), Path::new("/dev/null/dry-run"));
    }

    #[tokio::test]
    async fn test_default_checkout_unsupported() {
        // Use a minimal struct that only implements ArtifactRegistry to
        // verify the default checkout stub.
        struct StubRegistry;
        #[async_trait::async_trait]
        impl ArtifactRegistry for StubRegistry {
            async fn data_uri(&self, _key: &str) -> Result<String, MissingArtifact> {
                unimplemented!()
            }
            async fn content(
                &self,
                _uri_str: &str,
                _json_path: Option<&str>,
            ) -> Result<String, Box<dyn std::error::Error>> {
                unimplemented!()
            }
            async fn is_registered(&self, _key: &str) -> bool {
                unimplemented!()
            }
            async fn path_for_uri(&self, _uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
                unimplemented!()
            }
            async fn commit(
                &self,
                _key: &str,
                _path: &str,
            ) -> Result<(), Box<dyn std::error::Error>> {
                unimplemented!()
            }
            async fn write_into_registry(
                &self,
                _uri: &Uri,
                _content: &str,
            ) -> Result<String, Box<dyn std::error::Error>> {
                unimplemented!()
            }
            async fn keys(&self) -> Vec<String> {
                unimplemented!()
            }
            async fn fork_registry(
                &self,
                _child_artifact_dir: &Path,
            ) -> Result<Box<dyn ArtifactRegistry>, Box<dyn std::error::Error>> {
                unimplemented!()
            }
            fn as_any(&self) -> Option<&dyn std::any::Any> {
                None
            }
        }

        let reg = StubRegistry;
        let result = reg.checkout(&["key".to_string()]).await;
        match result {
            Err(e) => assert!(e.to_string().contains("not supported")),
            Ok(_) => panic!("expected error"),
        }
    }
}
