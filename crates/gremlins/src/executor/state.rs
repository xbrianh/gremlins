//! state.json I/O, flock-guarded updates, StateData, and artifact registry.
//!
//! Every `state.json` mutation goes through [`write_state`] or [`locked_update`],
//! both of which hold the flock. Reads ([`read_str`], [`read_field`], [`get_field`],
//! [`stage_error`]) are lock-free snapshot reads, safe because
//! every mutation is rename-atomic.
//!
//! The [`StateStore`] trait merges the old `StateStore` with the old
//! `ArtifactRegistry` + `LocalizedArtifactRegistry` traits so that a single
//! backend handles both state.json and artifact storage.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::artifacts::uri::Uri;
use crate::config;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, thiserror::Error)]
#[error("artifact not bound: {key:?}")]
pub struct MissingArtifact {
    pub key: String,
}

#[derive(Debug, thiserror::Error)]
#[error("duplicate artifact: {key:?} already bound to {existing:?}, cannot rebind to {incoming:?}")]
pub struct DuplicateArtifact {
    pub key: String,
    pub existing: String,
    pub incoming: String,
}

/// Controls behaviour when a key being merged is already registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collision {
    /// Return a [`DuplicateArtifact`] error.
    Error,
    /// Skip the key silently.
    Ignore,
}

// ---------------------------------------------------------------------------
// StateBlob / BlobMode
// ---------------------------------------------------------------------------

/// A general handle for named state-directory files.
pub trait StateBlob: std::io::Read + std::io::Write + std::io::Seek + Send {}
impl<T: std::io::Read + std::io::Write + std::io::Seek + Send> StateBlob for T {}

/// How to open a named blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobMode {
    /// Open an existing file for reading and writing. Fails if absent.
    #[allow(dead_code)]
    ReadWrite,
    /// Create or truncate for writing.
    Write,
    /// Create or append.
    Append,
}

// ---------------------------------------------------------------------------
// StateStore trait — the unified storage backend seam
// ---------------------------------------------------------------------------

/// The set of operations that the executor runtime requires from a storage
/// backend: state.json reads/writes, blob I/O, and artifact registry
/// operations.
///
/// Implemented by [`FileSystemStateStore`] (the real filesystem-backed store).
#[async_trait::async_trait]
pub trait StateStore: Send + Sync + Debug {
    /// Create a store for the given gremlin identity.
    fn new(gremlin_id: Option<String>) -> Self
    where
        Self: Sized;

    // --- state.json ---

    /// Lock-free snapshot of the full state tree.
    fn state_tree(&self) -> Map<String, Value>;

    /// Write `data` as `state.json` into the store's directory.
    fn seed(&mut self, data: &Map<String, Value>) -> Result<(), StateError>;

    /// Open a named blob in the state directory. Creates parent directories
    /// as needed. The returned handle supports Read + Write + Seek.
    fn open(&self, name: &str, mode: BlobMode) -> Result<Box<dyn StateBlob>, StateError>;

    /// Check whether a named blob exists without creating it.
    fn exists(&self, name: &str) -> bool;

    /// Remove a stale bail file for the given attempt.
    fn clear_stage_error(&self, attempt: &str);

    // --- reads ---

    /// Lock-free snapshot read. Falsy values read as `""`.
    fn read_str(&self, field: &str) -> String;

    /// Present, non-null value — `None` when absent or null.
    fn read_field(&self, field: &str) -> Option<Value>;

    /// Value with fallback to its default.
    fn get_field(&self, field: &str) -> Option<Value>;

    /// The bail record for the current attempt, if any.
    fn stage_error(&self) -> Option<Map<String, Value>>;

    /// Read `parallel_worktrees[group_name]` as `(base_head, {child_key: path})`.
    fn parallel_worktrees(&self, group_name: &str) -> (String, HashMap<String, String>);

    // --- writes ---

    fn patch(&self, delete: &[String], fields: &Map<String, Value>);

    /// Write a bail record with first-writer-wins semantics.
    fn record_stage_error(&self, class: &str, detail: &str);

    fn accumulate_token_usage(&self, usage: &HashMap<String, i64>);

    /// Create the `finished` marker and patch terminal fields.
    fn write_terminal_state(&self, exit_code: i32);

    /// Write `data` to `state_dir/state.json` and update the store's path.
    fn persist(&mut self, state_dir: &Path, data: &Map<String, Value>) -> Result<(), StateError>;

    fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    );

    fn add_subprocess_cost(&self, amount: f64);

    fn patch_parallel_attempt(&self, child_key: &str, attempt: &str);

    // --- artifact registry ---

    /// Return the data URI (storage path or external URI) bound to `key`.
    async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact>;

    /// Read the content of the artifact identified by `uri_str`,
    /// optionally extracting a JSON path segment.
    async fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>>;

    /// Whether `key` is currently registered.
    async fn is_registered(&self, key: &str) -> bool;

    /// Compute the canonical filesystem path for `uri` without touching the registry.
    async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>>;

    /// Bind `key` to `path` and persist.
    async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>>;

    /// Write `content` to the path for `uri`, then commit.
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
    async fn merge_registry(
        &self,
        other: &(dyn StateStore + Sync),
        collision: Collision,
        key_prefix: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        merge_state_via_content(self, other, collision, key_prefix).await
    }

    /// Clone / fork this store's registry for use by a child gremlin.
    ///
    /// The child's artifacts will be stored under `child_artifact_dir`.
    async fn fork_registry(
        &self,
        child_artifact_dir: &Path,
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>>;

    /// Produce a localized (filesystem-scoped) store containing only the
    /// given subset of keys. The returned store lives in a separate
    /// directory so that unscoped artifacts cannot be discovered by
    /// sniffing the filesystem.
    async fn checkout_registry(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        let _ = keys;
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "checkout not supported for this store type",
        )))
    }

    /// Enable downcast to concrete store types (for optimisation paths).
    /// Default returns `None` — backends that support downcasting override this.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }

    // --- locality ---

    /// The artifact storage directory.
    fn artifact_dir(&self) -> &Path {
        Path::new("")
    }

    /// Check whether a file exists at `path` (empty files are valid).
    async fn has_file(&self, path: &str) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }
}

// ---------------------------------------------------------------------------
// merge_state_via_content — content-based merge fallback
// ---------------------------------------------------------------------------

/// Content-based merge implementation used by the default
/// [`StateStore::merge_registry`] and as a fallback by backends that
/// cannot do a direct file-copy optimisation.
async fn merge_state_via_content<D: StateStore + Sync + ?Sized>(
    dest: &D,
    src: &(dyn StateStore + Sync),
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
        // bound URI.
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
fn is_file_artifact(data_uri: &str) -> bool {
    data_uri.starts_with('/')
}

// ---------------------------------------------------------------------------
// FileSystemStateStore — filesystem-backed implementation
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct FileSystemStateStore {
    state_file: Option<PathBuf>,
    artifact_dir: Option<PathBuf>,
    registry_path: Option<PathBuf>,
}

impl FileSystemStateStore {
    /// Derive `artifact_dir` and `registry_path` from `state_file`.
    fn set_state_file(&mut self, sf: PathBuf) {
        let parent = sf.parent().map(|p| p.to_path_buf());
        self.artifact_dir = parent.as_ref().map(|p| p.join("artifacts"));
        self.registry_path = parent.as_ref().map(|p| p.join("registry.json"));
        self.state_file = Some(sf);
    }

    /// Create a store pointed at an explicit `state.json` path.
    /// Derives `artifact_dir` and `registry_path` from the parent directory.
    pub fn at_path(state_file: PathBuf) -> Self {
        let parent = state_file.parent().map(|p| p.to_path_buf());
        let artifact_dir = parent.as_ref().map(|p| p.join("artifacts"));
        let registry_path = parent.as_ref().map(|p| p.join("registry.json"));
        FileSystemStateStore {
            state_file: Some(state_file),
            artifact_dir,
            registry_path,
        }
    }

    /// Load a registry from an explicit `registry.json` path, storing
    /// artifacts under `artifact_dir`.
    pub async fn from_registry_file(
        path: &Path,
        artifact_dir: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let registry_path = artifact_dir
            .parent()
            .unwrap_or(&artifact_dir)
            .join("registry.json");
        let state_file = artifact_dir.parent().map(|p| p.join("state.json"));
        let store = FileSystemStateStore {
            state_file,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(registry_path),
        };
        if path != store.registry_path.as_deref().unwrap_or(path)
            && tokio::fs::try_exists(path).await.unwrap_or(false)
        {
            let content = tokio::fs::read_to_string(path).await?;
            let parsed: HashMap<String, String> = serde_json::from_str(&content)?;
            let count = parsed.len();
            store
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
        Ok(store)
    }

    // --- registry helpers ---

    /// Read and parse `registry.json`, returning an empty map when the file is
    /// absent or unparseable (logging the reason).
    async fn read_registry_json(&self) -> HashMap<String, String> {
        let Some(rp) = self.registry_path.as_ref() else {
            return HashMap::new();
        };
        match tokio::fs::read_to_string(rp).await {
            Ok(content) => match serde_json::from_str::<HashMap<String, String>>(&content) {
                Ok(data) => data,
                Err(e) => {
                    log::error!(
                        "failed to parse registry.json at {}: {e} — starting with empty registry",
                        rp.display(),
                    );
                    HashMap::new()
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::error!(
                        "failed to read registry.json at {}: {e} — starting with empty registry",
                        rp.display(),
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
        let rp = self.registry_path.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no registry path configured")
        })?;
        if let Some(parent) = rp.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let _lock = acquire_lock_async(rp).await?;
        let mut data = self.read_registry_json().await;
        let result = apply(&mut data)?;
        let data_map: serde_json::Map<String, serde_json::Value> = data
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        atomic_write_json_async(rp, &data_map).await?;
        log::debug!(
            "locked_write: wrote {} entries to {}",
            data.len(),
            rp.display(),
        );
        Ok(result)
    }

    // --- artifact methods (inherent, also exposed via trait) ---

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
        let ad = self.artifact_dir.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no artifact directory configured",
            )
        })?;
        let path = ad.join(&name);
        // Ensure artifact_dir exists before canonicalizing
        tokio::fs::create_dir_all(ad).await?;
        let base = tokio::fs::canonicalize(ad).await?;
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
        self.artifact_dir.as_deref().unwrap_or(Path::new(""))
    }

    pub async fn has_file(&self, path: &str) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    /// Merge that prefers a direct file copy when `other` is also a
    /// [`FileSystemStateStore`], falling back to the content-based
    /// path otherwise.
    pub async fn merge_registry(
        &self,
        other: &(dyn StateStore + Sync),
        collision: Collision,
        key_prefix: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        // Fast path: both sides are filesystem-backed — copy files directly.
        // Also try to unwrap a ScopedFileSystemStateStore (the checkout
        // wrapper) so that merging from a scoped store hits the fast path.
        let fs: Option<&FileSystemStateStore> = other.as_any().and_then(|a| {
            a.downcast_ref::<FileSystemStateStore>().or_else(|| {
                a.downcast_ref::<ScopedFileSystemStateStore>()
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
        merge_state_via_content(self, other, collision, key_prefix).await
    }

    /// Produce a localized store containing only the given keys.
    pub async fn checkout_registry(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::TempDir::new()?;
        let artifact_dir = temp_dir.path().join("artifacts");
        tokio::fs::create_dir_all(&artifact_dir).await?;
        let state_file = temp_dir.path().join("state.json");
        let new_store = FileSystemStateStore {
            state_file: Some(state_file),
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
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
                    new_store.copy_into_registry(&uri, &src_path).await?;
                } else {
                    // Non-file artifact: read content as string and write.
                    let content = self.content(key, None).await?;
                    new_store.write_into_registry(&uri, &content).await?;
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
                let path = new_store.path_for_uri(&uri).await?;
                // Create parent dirs so the agent/exec can write the file.
                if let Some(parent) = Path::new(&path).parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }
            allowed.insert(key.clone());
        }

        Ok(Box::new(ScopedFileSystemStateStore {
            inner: new_store,
            _temp: temp_dir,
            allowed_keys: allowed,
        }))
    }
}

// ---------------------------------------------------------------------------
// StateStore impl for FileSystemStateStore
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl StateStore for FileSystemStateStore {
    fn new(gremlin_id: Option<String>) -> Self {
        let sf = resolve_state_file(gremlin_id.as_deref());
        match sf {
            Some(path) => Self::at_path(path),
            None => FileSystemStateStore {
                state_file: None,
                artifact_dir: None,
                registry_path: None,
            },
        }
    }

    fn state_tree(&self) -> Map<String, Value> {
        read_state_json(self.state_file.as_deref())
    }

    fn seed(&mut self, data: &Map<String, Value>) -> Result<(), StateError> {
        let state_dir = self
            .state_file
            .as_ref()
            .and_then(|sf| sf.parent().map(|p| p.to_path_buf()))
            .ok_or_else(|| StateError::Other("no state directory".into()))?;
        write_state(&state_dir, data)?;
        self.set_state_file(state_dir.join("state.json"));
        Ok(())
    }

    fn open(&self, name: &str, mode: BlobMode) -> Result<Box<dyn StateBlob>, StateError> {
        // Reject traversal and absolute paths.
        if name.contains("..") || name.starts_with('/') || name.contains('\0') {
            return Err(StateError::Other(format!(
                "invalid blob name {name:?}: traversal or absolute path rejected"
            )));
        }
        let state_dir = self
            .state_file
            .as_ref()
            .and_then(|sf| sf.parent())
            .ok_or_else(|| StateError::Other("no state directory".into()))?;
        let path = state_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        match mode {
            BlobMode::ReadWrite => {
                opts.read(true).write(true).create(false);
            }
            BlobMode::Write => {
                opts.write(true).create(true).truncate(true);
            }
            BlobMode::Append => {
                opts.append(true).create(true);
            }
        }
        let file = opts.open(&path)?;
        Ok(Box::new(file))
    }

    fn exists(&self, name: &str) -> bool {
        let Some(state_dir) = self.state_file.as_ref().and_then(|sf| sf.parent()) else {
            return false;
        };
        // Reject traversal.
        if name.contains("..") || name.starts_with('/') || name.contains('\0') {
            return false;
        }
        state_dir.join(name).exists()
    }

    fn clear_stage_error(&self, attempt: &str) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        let Some(parent) = sf.parent() else {
            return;
        };
        let bail_path = parent.join(format!("bail_{attempt}.json"));
        let _ = std::fs::remove_file(&bail_path);
    }

    fn get_field(&self, name: &str) -> Option<Value> {
        let default = default_for(name)?;
        let data = read_state_json(self.state_file.as_deref());
        Some(data.get(name).cloned().unwrap_or(default))
    }

    fn read_field(&self, field: &str) -> Option<Value> {
        let sf = self.state_file.as_ref()?;
        if !sf.exists() {
            return None;
        }
        let data = read_state_json(Some(sf));
        match data.get(field) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.clone()),
        }
    }

    fn read_str(&self, field: &str) -> String {
        let Some(sf) = self.state_file.as_ref() else {
            return String::new();
        };
        if !sf.exists() {
            return String::new();
        }
        match read_state_json(Some(sf)).get(field) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
            Some(Value::Bool(true)) => "True".into(),
            _ => String::new(),
        }
    }

    fn persist(&mut self, state_dir: &Path, data: &Map<String, Value>) -> Result<(), StateError> {
        write_state(state_dir, data)?;
        self.set_state_file(state_dir.join("state.json"));
        Ok(())
    }

    fn patch(&self, delete: &[String], fields: &Map<String, Value>) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let fields = fields.clone();
        let delete = delete.to_vec();
        let _ = locked_update(sf, move |data| {
            for k in &delete {
                data.remove(k);
            }
            for (k, v) in fields {
                data.insert(k, v);
            }
        });
    }

    fn record_stage_error(&self, class: &str, detail: &str) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() || class.is_empty() {
            return;
        }
        let attempt = attempt_of(&read_state_json(Some(sf)));
        if attempt.is_empty() {
            return;
        }
        let Some(state_dir) = sf.parent() else {
            return;
        };
        let bail_path = state_dir.join(format!("bail_{attempt}.json"));
        let payload = serde_json::json!({
            "class": class,
            "detail": detail,
            "ts": now_iso(),
        });
        write_bail_atomically(state_dir, &bail_path, &attempt, &payload);
    }

    fn accumulate_token_usage(&self, usage: &HashMap<String, i64>) {
        if usage.is_empty() {
            return;
        }
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let usage = usage.clone();
        let _ = locked_update(sf, move |data| {
            let mut total: Map<String, Value> = data
                .get("token_usage")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            for (k, v) in &usage {
                let cur = total.get(k).map(as_i64).unwrap_or(0);
                total.insert(k.clone(), Value::from(cur + v));
            }
            data.insert("token_usage".into(), Value::Object(total));
        });
    }

    fn stage_error(&self) -> Option<Map<String, Value>> {
        let sf = self.state_file.as_ref()?;
        if !sf.exists() {
            return None;
        }
        let attempt = attempt_of(&read_state_json(Some(sf)));
        if attempt.is_empty() {
            return None;
        }
        let bail_path = sf.parent()?.join(format!("bail_{attempt}.json"));
        serde_json::from_str(&std::fs::read_to_string(bail_path).ok()?).ok()
    }

    fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    ) {
        if group_name.is_empty() {
            return;
        }
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let group_name = group_name.to_string();
        let base_head = base_head.map(String::from);
        let paths = paths.cloned();
        let _ = locked_update(sf, move |data| {
            let mut groups = data
                .get("parallel_worktrees")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            if base_head.is_none() && paths.is_none() {
                groups.remove(&group_name);
            } else {
                let mut entry = Map::new();
                entry.insert(
                    "base_head".into(),
                    Value::String(base_head.unwrap_or_default()),
                );
                let mut ps = Map::new();
                for (k, v) in paths.unwrap_or_default() {
                    ps.insert(k, Value::String(v));
                }
                entry.insert("paths".into(), Value::Object(ps));
                groups.insert(group_name, Value::Object(entry));
            }
            if groups.is_empty() {
                data.remove("parallel_worktrees");
            } else {
                data.insert("parallel_worktrees".into(), Value::Object(groups));
            }
        });
    }

    fn add_subprocess_cost(&self, amount: f64) {
        if amount == 0.0 || !amount.is_finite() || amount < 0.0 {
            return;
        }
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let _ = locked_update(sf, move |data| {
            let current = data
                .get("subprocess_cost_usd")
                .map(as_i64_f64)
                .unwrap_or(0.0);
            data.insert("subprocess_cost_usd".into(), Value::from(current + amount));
        });
    }

    fn patch_parallel_attempt(&self, child_key: &str, attempt: &str) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() || attempt.is_empty() {
            return;
        }
        let child_key = child_key.to_string();
        let attempt = attempt.to_string();
        let _ = locked_update(sf, move |data| {
            let mut pa = data
                .get("parallel_attempts")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            pa.insert(child_key, Value::String(attempt));
            data.insert("parallel_attempts".into(), Value::Object(pa));
        });
    }

    fn parallel_worktrees(&self, group_name: &str) -> (String, HashMap<String, String>) {
        let Some(sf) = self.state_file.as_ref() else {
            return (String::new(), HashMap::new());
        };
        if !sf.exists() {
            return (String::new(), HashMap::new());
        }
        let data = read_state_json(Some(sf));
        let Some(entry) = data
            .get("parallel_worktrees")
            .and_then(|v| v.as_object())
            .and_then(|o| o.get(group_name))
            .and_then(|v| v.as_object())
        else {
            return (String::new(), HashMap::new());
        };
        let base_head = entry
            .get("base_head")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let paths = entry
            .get("paths")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        (base_head, paths)
    }

    fn write_terminal_state(&self, exit_code: i32) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if let Some(state_dir) = sf.parent() {
            let _ = File::create(state_dir.join("finished"));
        }
        let mut fields = Map::new();
        fields.insert(
            "status".into(),
            Value::String(if exit_code == 0 { "done" } else { "stopped" }.into()),
        );
        fields.insert("ended_at".into(), Value::String(now_stamp()));
        fields.insert("exit_code".into(), Value::from(exit_code));
        self.patch(&[], &fields);
    }

    // --- artifact methods ---

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
        self.keys().await
    }

    async fn merge_registry(
        &self,
        other: &(dyn StateStore + Sync),
        collision: Collision,
        key_prefix: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        self.merge_registry(other, collision, key_prefix).await
    }

    async fn fork_registry(
        &self,
        child_artifact_dir: &Path,
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        let rp = self.registry_path.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no registry path configured for fork",
            )
        })?;
        FileSystemStateStore::from_registry_file(rp, child_artifact_dir.to_path_buf())
            .await
            .map(|r| Box::new(r) as Box<dyn StateStore>)
    }

    async fn checkout_registry(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        self.checkout_registry(keys).await
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn artifact_dir(&self) -> &Path {
        self.artifact_dir()
    }

    async fn has_file(&self, path: &str) -> bool {
        self.has_file(path).await
    }
}

// ---------------------------------------------------------------------------
// ScopedFileSystemStateStore — checkout wrapper
// ---------------------------------------------------------------------------

/// A [`FileSystemStateStore`] that only allows access to a
/// pre-approved set of keys. Owns a [`TempDir`] so the checkout directory
/// is cleaned up when the store is dropped.
struct ScopedFileSystemStateStore {
    inner: FileSystemStateStore,
    _temp: tempfile::TempDir,
    allowed_keys: HashSet<String>,
}

impl ScopedFileSystemStateStore {
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

impl Debug for ScopedFileSystemStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedFileSystemStateStore")
            .field("inner", &self.inner)
            .field("allowed_keys", &self.allowed_keys)
            .finish()
    }
}

#[async_trait::async_trait]
impl StateStore for ScopedFileSystemStateStore {
    fn new(_gremlin_id: Option<String>) -> Self {
        // Scoped stores are only created via checkout; new() is unreachable
        // but must exist for the trait.
        unimplemented!("ScopedFileSystemStateStore cannot be created via new()")
    }

    // --- state methods: delegate to inner ---

    fn state_tree(&self) -> Map<String, Value> {
        self.inner.state_tree()
    }

    fn seed(&mut self, data: &Map<String, Value>) -> Result<(), StateError> {
        self.inner.seed(data)
    }

    fn open(&self, name: &str, mode: BlobMode) -> Result<Box<dyn StateBlob>, StateError> {
        self.inner.open(name, mode)
    }

    fn exists(&self, name: &str) -> bool {
        self.inner.exists(name)
    }

    fn clear_stage_error(&self, attempt: &str) {
        self.inner.clear_stage_error(attempt)
    }

    fn read_str(&self, field: &str) -> String {
        self.inner.read_str(field)
    }

    fn read_field(&self, field: &str) -> Option<Value> {
        self.inner.read_field(field)
    }

    fn get_field(&self, field: &str) -> Option<Value> {
        self.inner.get_field(field)
    }

    fn stage_error(&self) -> Option<Map<String, Value>> {
        self.inner.stage_error()
    }

    fn parallel_worktrees(&self, group_name: &str) -> (String, HashMap<String, String>) {
        self.inner.parallel_worktrees(group_name)
    }

    fn patch(&self, delete: &[String], fields: &Map<String, Value>) {
        self.inner.patch(delete, fields)
    }

    fn record_stage_error(&self, class: &str, detail: &str) {
        self.inner.record_stage_error(class, detail)
    }

    fn accumulate_token_usage(&self, usage: &HashMap<String, i64>) {
        self.inner.accumulate_token_usage(usage)
    }

    fn write_terminal_state(&self, exit_code: i32) {
        self.inner.write_terminal_state(exit_code)
    }

    fn persist(&mut self, state_dir: &Path, data: &Map<String, Value>) -> Result<(), StateError> {
        self.inner.persist(state_dir, data)
    }

    fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    ) {
        self.inner
            .patch_parallel_worktrees(group_name, base_head, paths)
    }

    fn add_subprocess_cost(&self, amount: f64) {
        self.inner.add_subprocess_cost(amount)
    }

    fn patch_parallel_attempt(&self, child_key: &str, attempt: &str) {
        self.inner.patch_parallel_attempt(child_key, attempt)
    }

    // --- artifact methods: check key, then delegate ---

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
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        self.inner.fork_registry(child_artifact_dir).await
    }

    async fn checkout_registry(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        self.inner.checkout_registry(keys).await
    }

    fn artifact_dir(&self) -> &Path {
        self.inner.artifact_dir()
    }

    async fn has_file(&self, path: &str) -> bool {
        self.inner.has_file(path).await
    }
}

// ---------------------------------------------------------------------------
// StateData — the public handle that owns identity + store
// ---------------------------------------------------------------------------

pub struct StateData {
    pub gremlin_id: Option<String>,
    store: Box<dyn StateStore + Send + Sync>,
}

impl StateData {
    pub fn new(gremlin_id: Option<String>) -> Self {
        let store = Box::new(FileSystemStateStore::new(gremlin_id.clone()));
        Self { gremlin_id, store }
    }

    pub fn from_store(
        gremlin_id: Option<String>,
        store: Box<dyn StateStore + Send + Sync>,
    ) -> Self {
        Self { gremlin_id, store }
    }

    /// Access the inner [`StateStore`] for callers that need the trait object
    /// (e.g. `resolve_interpolation_map`, `prepare_agent`).
    pub(crate) fn store_ref(&self) -> &(dyn StateStore + Send + Sync) {
        self.store.as_ref()
    }

    // --- delegating reads ---

    pub fn get_field(&self, name: &str) -> Option<Value> {
        self.store.get_field(name)
    }

    pub fn read_field(&self, field: &str) -> Option<Value> {
        self.store.read_field(field)
    }

    pub fn read_str(&self, field: &str) -> String {
        self.store.read_str(field)
    }

    pub fn stage_error(&self) -> Option<Map<String, Value>> {
        self.store.stage_error()
    }

    pub fn parallel_worktrees(&self, group_name: &str) -> (String, HashMap<String, String>) {
        self.store.parallel_worktrees(group_name)
    }

    // --- delegating writes ---

    pub fn patch(&self, delete: &[String], fields: &Map<String, Value>) {
        self.store.patch(delete, fields);
    }

    pub fn record_stage_error(&self, class: &str, detail: &str) {
        self.store.record_stage_error(class, detail);
    }

    pub fn accumulate_token_usage(&self, usage: &HashMap<String, i64>) {
        self.store.accumulate_token_usage(usage);
    }

    pub fn persist(
        &mut self,
        state_dir: &Path,
        data: &Map<String, Value>,
    ) -> Result<(), StateError> {
        let Some(gid) = self.gremlin_id.clone() else {
            return Err(StateError::Other(
                "cannot persist StateData with no gremlin_id".into(),
            ));
        };
        let mut out = data.clone();
        out.insert("id".into(), Value::String(gid));
        self.store.persist(state_dir, &out)
    }

    pub fn state_tree(&self) -> Map<String, Value> {
        self.store.state_tree()
    }

    pub fn seed(&mut self, data: &Map<String, Value>) -> Result<(), StateError> {
        self.store.seed(data)
    }

    pub(crate) fn open(
        &self,
        name: &str,
        mode: BlobMode,
    ) -> Result<Box<dyn StateBlob>, StateError> {
        self.store.open(name, mode)
    }

    pub(crate) fn exists(&self, name: &str) -> bool {
        self.store.exists(name)
    }

    pub fn clear_stage_error(&self, attempt: &str) {
        self.store.clear_stage_error(attempt);
    }

    pub fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    ) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() {
            return;
        }
        self.store
            .patch_parallel_worktrees(group_name, base_head, paths);
    }

    pub fn add_subprocess_cost(&self, amount: f64) {
        self.store.add_subprocess_cost(amount);
    }

    pub fn patch_parallel_attempt(&self, child_key: &str, attempt: &str) {
        self.store.patch_parallel_attempt(child_key, attempt);
    }

    // --- artifact delegating methods ---

    pub async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact> {
        self.store.data_uri(key).await
    }

    pub async fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.store.content(uri_str, json_path).await
    }

    pub async fn is_registered(&self, key: &str) -> bool {
        self.store.is_registered(key).await
    }

    pub async fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        self.store.path_for_uri(uri).await
    }

    pub async fn commit(&self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.store.commit(key, path).await
    }

    pub async fn write_into_registry(
        &self,
        uri: &Uri,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.store.write_into_registry(uri, content).await
    }

    pub async fn copy_into_registry(
        &self,
        uri: &Uri,
        source: &Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.store.copy_into_registry(uri, source).await
    }

    pub async fn keys(&self) -> Vec<String> {
        self.store.keys().await
    }

    pub async fn merge_registry(
        &self,
        other: &(dyn StateStore + Sync),
        collision: Collision,
        key_prefix: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        self.store
            .merge_registry(other, collision, key_prefix)
            .await
    }

    pub async fn fork_registry(
        &self,
        child_artifact_dir: &Path,
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        self.store.fork_registry(child_artifact_dir).await
    }

    pub async fn checkout_registry(
        &self,
        keys: &[String],
    ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
        self.store.checkout_registry(keys).await
    }

    pub fn artifact_dir(&self) -> &Path {
        self.store.artifact_dir()
    }

    pub async fn has_file(&self, path: &str) -> bool {
        self.store.has_file(path).await
    }

    // --- methods with guards that stay on StateData ---

    /// Create a `StateData` pointed at an explicit state directory.
    /// Only available in tests — production code uses [`StateData::new`]
    /// which derives the directory from `config::state_root`.
    #[cfg(test)]
    pub(crate) fn with_state_dir(state_dir: &std::path::Path) -> Self {
        let sf = state_dir.join("state.json");
        let _ = std::fs::create_dir_all(state_dir);
        if !sf.exists() {
            let _ = std::fs::write(&sf, "{}");
        }
        Self {
            gremlin_id: Some("test".into()),
            store: Box::new(FileSystemStateStore::at_path(sf)),
        }
    }

    pub fn set_stage(&self, stage: &str, sub_stage: Option<&Value>, parent_stage: &str) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() {
            return;
        }
        let scoped = !parent_stage.is_empty();
        let target_stage = if scoped { parent_stage } else { stage };
        if target_stage.is_empty() {
            return;
        }
        let mut fields = Map::new();
        fields.insert("stage".into(), Value::String(target_stage.to_string()));
        fields.insert("stage_updated_at".into(), Value::String(now_stamp()));
        match if scoped {
            Some(Value::String(stage.to_string()))
        } else {
            sub_stage.cloned()
        } {
            Some(sub) => {
                fields.insert("sub_stage".into(), sub);
                self.store.patch(&[], &fields);
            }
            None => self.store.patch(&["sub_stage".to_string()], &fields),
        }
    }

    /// Delete the whole `parallel_attempts` map.
    pub fn clear_parallel_attempts(&self) {
        self.store
            .patch(&["parallel_attempts".to_string()], &Map::new());
    }

    pub fn write_terminal_state(&self, exit_code: i32) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() {
            return;
        }
        self.store.write_terminal_state(exit_code);
    }
}

// ---------------------------------------------------------------------------
// Free functions (unchanged)
// ---------------------------------------------------------------------------

fn rand_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    let filled = File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !filled {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token_hex(n_bytes: usize) -> String {
    rand_hex(n_bytes)
}

fn now_utc() -> libc::tm {
    let mut t: libc::time_t = 0;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::time(std::ptr::addr_of_mut!(t));
        libc::gmtime_r(std::ptr::addr_of!(t), std::ptr::addr_of_mut!(tm));
    }
    tm
}

/// `%Y-%m-%dT%H:%M:%SZ` — second precision, UTC.
pub fn now_stamp() -> String {
    let tm = now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

/// `[YYYY-MM-DDTHH:MM:SS.sssZ]` — millisecond precision, UTC.
/// Matches `env_logger`'s `format_timestamp_millis()` format.
pub fn now_stamp_millis() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::gmtime_r(std::ptr::addr_of!(t), std::ptr::addr_of_mut!(tm));
        tm
    };
    format!(
        "[{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z]",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        millis,
    )
}

/// ISO-8601 with microseconds and `+00:00`, matching Python `datetime.isoformat()`.
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs();
    let micros = since_epoch.subsec_micros();
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::gmtime_r(std::ptr::addr_of!(t), std::ptr::addr_of_mut!(tm));
        tm
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        micros,
    )
}

pub fn default_for(name: &str) -> Option<Value> {
    Some(match name {
        "attempt" | "kind" | "project_root" | "workdir" | "setup_kind" | "worktree_base"
        | "status" | "started_at" | "description" | "parent_id" | "client" | "definition_path"
        | "stage" | "group_name" | "child_key" => Value::String(String::new()),
        "definition_args" => Value::Array(Vec::new()),
        "stage_inputs" | "metadata" => Value::Object(Map::new()),
        "pid" | "exit_code" => Value::Null,
        _ => return None,
    })
}

pub fn field_names() -> [&'static str; 20] {
    [
        "attempt",
        "kind",
        "project_root",
        "workdir",
        "setup_kind",
        "worktree_base",
        "status",
        "started_at",
        "description",
        "parent_id",
        "definition_args",
        "client",
        "definition_path",
        "stage",
        "pid",
        "stage_inputs",
        "metadata",
        "group_name",
        "child_key",
        "exit_code",
    ]
}

/// The state directory for `gremlin_id`.
///
/// This is the single source of truth for where a gremlin's state directory
/// lives on disk. Every caller that needs to locate a gremlin's state dir
/// — whether to open its `state.json`, read its log, or merge its
/// artifacts — must go through this function, not through `config`.
pub fn state_dir_for(gremlin_id: &str) -> PathBuf {
    config::state_root().join(gremlin_id)
}

pub fn resolve_state_file(gremlin_id: Option<&str>) -> Option<PathBuf> {
    let id = gremlin_id.filter(|s| !s.is_empty())?;
    Some(state_dir_for(id).join("state.json"))
}

/// Enumerate `(id, state.json path)` pairs under the state root.
///
/// Only subdirectories that actually contain a `state.json` are returned.
/// Callers that need to skip closed gremlins check the marker file next to the
/// returned path themselves.
pub fn list_state_dirs() -> Vec<(String, PathBuf)> {
    let root = config::state_root();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_json_path = path.join("state.json");
        if !state_json_path.is_file() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        out.push((name, state_json_path));
    }
    out.sort();
    out
}

pub fn read_state_json(sf: Option<&Path>) -> Map<String, Value> {
    let Some(p) = sf else {
        return Map::new();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn write_state(state_dir: &Path, data: &Map<String, Value>) -> Result<(), StateError> {
    let sf = state_dir.join("state.json");
    let _lock = acquire_lock(&sf)?;
    let tmp = state_dir.join(format!(
        "state.json.{}.{}.tmp",
        std::process::id(),
        rand_hex(4)
    ));
    std::fs::write(&tmp, serde_json::to_string(data)?)?;
    std::fs::rename(&tmp, &sf)?;
    Ok(())
}

fn lock_path(sf: &Path) -> PathBuf {
    let name = sf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    sf.with_file_name(format!("{name}.lock"))
}

pub fn acquire_lock(sf: &Path) -> Result<File, StateError> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(lock_path(sf))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(StateError::Io(std::io::Error::last_os_error()));
    }
    Ok(f)
}

/// Async counterpart of [`acquire_lock`]: runs the flock on a blocking thread
/// so the tokio runtime is never blocked on filesystem locks.
pub async fn acquire_lock_async(sf: &Path) -> Result<File, StateError> {
    let sf = sf.to_path_buf();
    tokio::task::spawn_blocking(move || acquire_lock(&sf))
        .await
        .map_err(|e| StateError::Io(std::io::Error::other(e.to_string())))?
}

pub fn read_json_map(sf: &Path) -> Result<Map<String, Value>, StateError> {
    Ok(serde_json::from_str(&std::fs::read_to_string(sf)?)?)
}

pub fn atomic_write_json(sf: &Path, data: &Map<String, Value>) -> Result<(), StateError> {
    let name = sf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    let tmp = sf.with_file_name(format!("{name}.{}.{}.tmp", std::process::id(), rand_hex(8)));
    std::fs::write(&tmp, serde_json::to_string(data)?)?;
    std::fs::rename(&tmp, sf)?;
    Ok(())
}

/// Async counterpart of [`atomic_write_json`]: uses [`tokio::fs::write`] and
/// [`tokio::fs::rename`] so the write + rename never block the runtime.
pub async fn atomic_write_json_async(
    sf: &Path,
    data: &Map<String, Value>,
) -> Result<(), StateError> {
    let name = sf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    let tmp = sf.with_file_name(format!("{name}.{}.{}.tmp", std::process::id(), rand_hex(8)));
    tokio::fs::write(&tmp, serde_json::to_string(data)?).await?;
    tokio::fs::rename(&tmp, sf).await?;
    Ok(())
}

/// Exclusive-lock, read, apply, atomically replace.
pub fn locked_update(
    sf: &Path,
    apply: impl FnOnce(&mut Map<String, Value>),
) -> Result<(), StateError> {
    let _lock = acquire_lock(sf)?;
    let mut data = read_json_map(sf)?;
    apply(&mut data);
    atomic_write_json(sf, &data)
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn attempt_of(data: &Map<String, Value>) -> String {
    match data.get("attempt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Write `payload` to `bail_path` with first-writer-wins semantics.
///
/// The destination is created with no-replace semantics via a hard link from a
/// uniquely named temporary file, so two concurrent writers can never clobber
/// each other's payload: the loser's link fails with `AlreadyExists` and its
/// temporary file is removed. This replaces the racy `exists()`-then-`rename`
/// check-then-act sequence.
fn write_bail_atomically(state_dir: &Path, bail_path: &Path, attempt: &str, payload: &Value) {
    let tmp = state_dir.join(format!(".bail_{attempt}_{}.tmp", rand_hex(4)));
    if std::fs::write(&tmp, payload.to_string()).is_err() {
        return;
    }
    // `hard_link` fails if the destination already exists, giving us an atomic
    // create-if-absent without a separate existence check. Either way the
    // temporary file is no longer needed.
    let _ = std::fs::hard_link(&tmp, bail_path);
    let _ = std::fs::remove_file(&tmp);
}

fn as_i64_f64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_sandbox;

    fn seed(sandbox: &crate::test_support::Sandbox, gremlin_id: &str) -> PathBuf {
        let state_dir = sandbox.path().join("state").join(gremlin_id);
        std::fs::create_dir_all(&state_dir).unwrap();
        let sf = state_dir.join("state.json");
        std::fs::write(
            &sf,
            format!(r#"{{"id": "{gremlin_id}", "stage": "implement"}}"#),
        )
        .unwrap();
        sf
    }

    // -----------------------------------------------------------------------
    // State tests (existing)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_state_file_builds_path() {
        with_sandbox(None, |sandbox| {
            let p = resolve_state_file(Some("abc")).unwrap();
            assert!(p.ends_with("abc/state.json"), "{p:?}");
            assert!(resolve_state_file(None).is_none());
            assert!(resolve_state_file(Some("")).is_none());
            let _ = sandbox;
        });
    }

    #[test]
    fn field_reads_after_disk_write() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            assert_eq!(d.get_field("stage").unwrap(), "implement");
            let mut fields = Map::new();
            fields.insert("stage".into(), Value::String("review".into()));
            d.patch(&[], &fields);
            assert_eq!(d.get_field("stage").unwrap(), "review");
            let _ = sf;
        });
    }

    #[test]
    fn write_and_read_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = Map::new();
        data.insert("id".into(), Value::String("g1".into()));
        data.insert("stage".into(), Value::String("running".into()));
        write_state(dir.path(), &data).unwrap();
        let read = read_state_json(Some(&dir.path().join("state.json")));
        assert_eq!(read.get("stage").unwrap(), "running");
        assert!(read_state_json(None).is_empty());
        assert!(read_state_json(Some(dir.path())).is_empty());
    }

    #[test]
    fn defaults_for_every_field() {
        for name in field_names() {
            assert!(default_for(name).is_some(), "missing default for {name}");
        }
        assert!(default_for("nope").is_none());
        assert_eq!(
            default_for("definition_args").unwrap(),
            Value::Array(vec![])
        );
        assert_eq!(default_for("pid").unwrap(), Value::Null);
        assert_eq!(default_for("exit_code").unwrap(), Value::Null);
    }

    #[test]
    fn get_field_falls_back_to_default() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            assert_eq!(d.get_field("attempt").unwrap(), "");
            assert_eq!(d.get_field("stage").unwrap(), "implement");
            assert_eq!(
                d.get_field("definition_args").unwrap(),
                Value::Array(vec![])
            );
            assert!(d.get_field("bogus").is_none());
            let _ = sf;
        });
    }

    #[test]
    fn patch_merges_and_deletes() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            let mut fields = Map::new();
            fields.insert("stage".into(), Value::String("review".into()));
            d.patch(&["sub_stage".into()], &fields);
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("attempt").unwrap(), "a1");
            assert_eq!(raw.get("stage").unwrap(), "review");
            assert_eq!(raw.get("id").unwrap(), "gr-test");
            assert!(!raw.contains_key("sub_stage"));
        });
    }

    #[test]
    fn patch_noop_without_gremlin_id() {
        let d = StateData::new(None);
        d.patch(&[], &Map::new());
        d.record_stage_error("other", "x");
        d.set_stage("running", None, "");
    }

    #[test]
    fn set_stage_writes_stamp_and_deletes_sub_stage() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.set_stage("implement", Some(&serde_json::json!({"k": 1})), "");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("stage").unwrap(), "implement");
            assert_eq!(raw.get("sub_stage").unwrap(), &serde_json::json!({"k": 1}));
            let ts = raw.get("stage_updated_at").unwrap().as_str().unwrap();
            assert!(ts.ends_with('Z'));
            assert_eq!(ts.len(), 20);

            d.set_stage("review-code", None, "");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("stage").unwrap(), "review-code");
            assert!(!raw.contains_key("sub_stage"));
        });
    }

    #[test]
    fn set_stage_parent_pins_stage_and_sub_stage() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.set_stage("github-review-pull-request", None, "reviews");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("stage").unwrap(), "reviews");
            assert_eq!(raw.get("sub_stage").unwrap(), "github-review-pull-request");
        });
    }

    #[test]
    fn record_stage_error_requires_attempt() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.record_stage_error("other", "no attempt yet");
            let state_dir = sf.parent().unwrap();
            assert!(std::fs::read_dir(state_dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("bail_")));

            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            d.record_stage_error("other", "boom");
            let bail = state_dir.join("bail_a1.json");
            assert!(bail.exists());
            let info = d.stage_error().unwrap();
            assert_eq!(info.get("class").unwrap().as_str(), Some("other"));
            assert_eq!(info.get("detail").unwrap().as_str(), Some("boom"));
            assert!(info.get("ts").unwrap().as_str().unwrap().len() > 20);

            // Second write must not clobber the existing bail file.
            d.record_stage_error("security", "second");
            assert_eq!(
                d.stage_error().unwrap().get("class"),
                Some(&Value::String("other".into()))
            );
        });
    }

    #[test]
    fn record_stage_error_is_first_writer_wins_under_concurrency() {
        // Many threads race to write the same bail file; exactly one payload
        // must win and no temporary files may be left behind.
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);

            let handles: Vec<_> = (0..16)
                .map(|i| {
                    std::thread::spawn(move || {
                        let data = StateData::new(Some("gr-test".into()));
                        data.record_stage_error("other", &format!("writer-{i}"));
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            let state_dir = sf.parent().unwrap();
            let bail = state_dir.join("bail_a1.json");
            assert!(bail.exists());
            // The file is valid JSON with a single winner's detail.
            let info = d.stage_error().unwrap();
            assert_eq!(info.get("class").unwrap().as_str(), Some("other"));
            let detail = info.get("detail").unwrap().as_str().unwrap();
            assert!(
                detail.starts_with("writer-"),
                "unexpected detail {detail:?}"
            );

            // No leftover temp files.
            let leftovers: Vec<String> = std::fs::read_dir(state_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with(".bail_"))
                .collect();
            assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        });
    }

    #[test]
    fn stage_error_keeps_non_string_values() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            let state_dir = sf.parent().unwrap();
            std::fs::write(
                state_dir.join("bail_a1.json"),
                r#"{"class": "other", "detail": "boom", "count": 3, "nested": {"k": [1, null]}}"#,
            )
            .unwrap();
            let info = d.stage_error().unwrap();
            assert_eq!(info.get("count"), Some(&Value::from(3)));
            assert_eq!(
                info.get("nested"),
                Some(&serde_json::json!({"k": [1, null]}))
            );

            std::fs::write(state_dir.join("bail_a1.json"), "[1, 2]").unwrap();
            assert!(d.stage_error().is_none());
        });
    }

    #[test]
    fn accumulate_token_usage_adds_integers() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.accumulate_token_usage(&HashMap::from([("prompt_tokens".to_string(), 5)]));
            d.accumulate_token_usage(&HashMap::from([
                ("prompt_tokens".to_string(), 3),
                ("turns".to_string(), 2),
            ]));
            let raw = read_state_json(Some(&sf));
            let usage = raw.get("token_usage").unwrap().as_object().unwrap();
            assert_eq!(usage.get("prompt_tokens").unwrap(), 8);
            assert_eq!(usage.get("turns").unwrap(), 2);
        });
    }

    #[test]
    fn parallel_worktrees_add_and_clear() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.patch_parallel_worktrees(
                "reviews",
                Some("abc123"),
                Some(&HashMap::from([("a".to_string(), "/wt/a".to_string())])),
            );
            let raw = read_state_json(Some(&sf));
            let entry = &raw.get("parallel_worktrees").unwrap()["reviews"];
            assert_eq!(entry["base_head"], "abc123");
            assert_eq!(entry["paths"]["a"], "/wt/a");

            d.patch_parallel_worktrees("reviews", None, None);
            assert!(!read_state_json(Some(&sf)).contains_key("parallel_worktrees"));
        });
    }

    #[test]
    fn subprocess_cost_accumulates_and_validates() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.add_subprocess_cost(0.25);
            d.add_subprocess_cost(0.5);
            d.add_subprocess_cost(-1.0);
            d.add_subprocess_cost(f64::NAN);
            d.add_subprocess_cost(0.0);
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("subprocess_cost_usd").unwrap(), 0.75);
        });
    }

    #[test]
    fn terminal_state_touches_finished_and_patches() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.write_terminal_state(0);
            let state_dir = sf.parent().unwrap();
            assert!(state_dir.join("finished").exists());
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("status").unwrap(), "done");
            assert_eq!(raw.get("exit_code").unwrap(), 0);

            d.write_terminal_state(3);
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("status").unwrap(), "stopped");
            assert_eq!(raw.get("exit_code").unwrap(), 3);
        });
    }

    #[test]
    fn persist_writes_id_and_sets_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = StateData::new(Some("child".into()));
        let mut payload = Map::new();
        payload.insert("definition_path".into(), Value::String("/p.yaml".into()));
        d.persist(dir.path(), &payload).unwrap();
        let raw = d.state_tree();
        assert_eq!(raw.get("id").unwrap(), "child");
        assert_eq!(raw.get("definition_path").unwrap(), "/p.yaml");

        let mut none = StateData::new(None);
        assert!(none.persist(dir.path(), &Map::new()).is_err());
    }

    #[test]
    fn open_creates_and_reads_blob() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            // Write through open
            {
                let mut blob = d.open("test.txt", BlobMode::Write).unwrap();
                blob.write_all(b"hello").unwrap();
            }
            // Read back
            let state_dir = sf.parent().unwrap();
            let content = std::fs::read_to_string(state_dir.join("test.txt")).unwrap();
            assert_eq!(content, "hello");
        });
    }

    #[test]
    fn seed_writes_state_and_tree_reads_it() {
        with_sandbox(None, |sandbox| {
            let _sf = seed(sandbox, "gr-test");
            let mut d = StateData::new(Some("gr-test".into()));
            let mut data = Map::new();
            data.insert("stage".into(), Value::String("seeded".into()));
            d.seed(&data).unwrap();
            let tree = d.state_tree();
            assert_eq!(tree.get("stage").unwrap(), "seeded");
        });
    }

    #[test]
    fn clear_stage_error_removes_bail_file() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            d.record_stage_error("other", "boom");
            let state_dir = sf.parent().unwrap();
            assert!(state_dir.join("bail_a1.json").exists());
            d.clear_stage_error("a1");
            assert!(!state_dir.join("bail_a1.json").exists());
        });
    }

    #[test]
    fn locked_update_read_modify_write() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            locked_update(&sf, |data| {
                let n = data.get("counter").map(as_i64).unwrap_or(0) + 1;
                data.insert("counter".into(), Value::from(n));
            })
            .unwrap();
            assert_eq!(read_state_json(Some(&sf)).get("counter").unwrap(), 1);
            let state_dir = sf.parent().unwrap();
            assert!(state_dir.join("state.json.lock").exists());
        });
    }

    #[test]
    fn parallel_attempt_patch() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.patch_parallel_attempt("bail-child", "attempt-bail");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw["parallel_attempts"]["bail-child"], "attempt-bail");
        });
    }

    #[test]
    fn parallel_worktrees_reads_back() {
        with_sandbox(None, |sandbox| {
            let _sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut paths = HashMap::new();
            paths.insert("a".to_string(), "/wt/a".to_string());
            d.patch_parallel_worktrees("reviews", Some("abc123"), Some(&paths));
            let (base, read_paths) = d.parallel_worktrees("reviews");
            assert_eq!(base, "abc123");
            assert_eq!(read_paths.get("a").map(String::as_str), Some("/wt/a"));
            assert!(d.parallel_worktrees("missing").1.is_empty());
        });
    }

    #[test]
    fn clear_parallel_attempts_removes_key() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.patch_parallel_attempt("a", "attempt-a");
            d.clear_parallel_attempts();
            assert!(read_state_json(Some(&sf))
                .get("parallel_attempts")
                .is_none());
        });
    }

    #[test]
    fn write_state_acquires_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = Map::new();
        data.insert("id".into(), Value::String("g1".into()));
        write_state(dir.path(), &data).unwrap();
        assert!(dir.path().join("state.json.lock").exists());
    }

    #[test]
    fn state_data_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<StateData>();
        assert_sync::<StateData>();
    }

    // -----------------------------------------------------------------------
    // Registry tests (adapted from registry.rs)
    // -----------------------------------------------------------------------

    fn setup_registry() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        (tmp, artifact_dir)
    }

    async fn write_file(store: &FileSystemStateStore, name: &str, content: &str) -> String {
        let uri = Uri::parse(&format!("artifact://{name}")).unwrap();
        store.write_into_registry(&uri, content).await.unwrap()
    }

    #[test]
    fn file_system_state_store_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<FileSystemStateStore>();
        assert_sync::<FileSystemStateStore>();
    }

    // --- FileSystemStateStore artifact tests ---

    #[tokio::test]
    async fn test_data_uri_unbound_raises_missing() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let err = store.data_uri("nonexistent").await.unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[tokio::test]
    async fn test_write_into_registry_persists_and_roundtrip() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let uri = Uri::new("artifact".to_string(), "foo.txt".to_string());
        let path = store.write_into_registry(&uri, "hello").await.unwrap();
        assert!(path.contains("foo.txt"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");

        // Reload from disk
        let store2 = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        assert_eq!(store2.data_uri("artifact://foo.txt").await.unwrap(), path);
    }

    #[tokio::test]
    async fn test_path_for_uri_does_not_register() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let uri = Uri::parse("artifact://later.txt").unwrap();
        let path = store.path_for_uri(&uri).await.unwrap();
        assert!(path.ends_with("later.txt"));
        assert!(!store.is_registered("artifact://later.txt").await);
    }

    #[tokio::test]
    async fn test_commit_idempotent_same_path() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let uri = Uri::parse("artifact://a.txt").unwrap();
        let path = store.path_for_uri(&uri).await.unwrap();
        std::fs::write(&path, "").unwrap();
        store.commit("artifact://a.txt", &path).await.unwrap();
        store.commit("artifact://a.txt", &path).await.unwrap();
    }

    #[tokio::test]
    async fn test_commit_with_missing_file_succeeds() {
        let (tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let missing = tmp.path().join("does-not-exist.txt");
        // commit does not check file existence; it only enforces key uniqueness
        store
            .commit("artifact://gone.txt", &missing.to_string_lossy())
            .await
            .unwrap();
        assert!(store.is_registered("artifact://gone.txt").await);
    }

    #[tokio::test]
    async fn test_commit_conflicting_path_raises() {
        let (tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        std::fs::write(&one, "").unwrap();
        std::fs::write(&two, "").unwrap();
        store
            .commit("artifact://a.txt", &one.to_string_lossy())
            .await
            .unwrap();
        let err = store
            .commit("artifact://a.txt", &two.to_string_lossy())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate artifact"));
    }

    #[tokio::test]
    async fn test_copy_into_registry() {
        let (tmp, artifact_dir) = setup_registry();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, "copied").unwrap();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let uri = Uri::parse("artifact://dst.txt").unwrap();
        let path = store.copy_into_registry(&uri, &src).await.unwrap();
        assert!(store.is_registered("artifact://dst.txt").await);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "copied");
    }

    #[tokio::test]
    async fn test_path_escape_prevention() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let uri = Uri::new("artifact".to_string(), "../bad.txt".to_string());
        assert!(store.path_for_uri(&uri).await.is_err());
    }

    #[tokio::test]
    async fn test_content_reads_file() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "hello.txt", "world").await;
        assert_eq!(
            store.content("artifact://hello.txt", None).await.unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn test_content_with_json_path() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "data.json", r#"{"a":{"b":"c"}}"#).await;
        let content = store
            .content("artifact://data.json", Some("a.b"))
            .await
            .unwrap();
        assert_eq!(content, "c");
    }

    #[tokio::test]
    async fn test_content_reads_file_containing_uri_text() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "range", "git://range/abc..def").await;
        assert_eq!(
            store.content("artifact://range", None).await.unwrap(),
            "git://range/abc..def",
        );
    }

    #[tokio::test]
    async fn test_is_registered_false_for_missing_key() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        assert!(!store.is_registered("nonexistent").await);
    }

    #[tokio::test]
    async fn test_is_registered_true_after_write() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "stuff.txt", "data").await;
        assert!(store.is_registered("artifact://stuff.txt").await);
    }

    #[tokio::test]
    async fn test_is_registered_true_after_file_deleted() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let path = write_file(&store, "dead.txt", "data").await;
        assert!(store.is_registered("artifact://dead.txt").await);
        std::fs::remove_file(&path).unwrap();
        assert!(store.is_registered("artifact://dead.txt").await);
    }

    #[tokio::test]
    async fn test_keys_returns_registered_keys() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "a", "").await;
        write_file(&store, "b", "").await;
        let mut keys = store.keys().await;
        keys.sort();
        assert_eq!(keys, vec!["artifact://a", "artifact://b"]);
    }

    #[tokio::test]
    async fn test_merge_registry_identity() {
        let (_tmp, artifact_dir) = setup_registry();
        let (_, other_dir) = setup_registry();

        let other = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(other_dir.clone()),
            registry_path: Some(
                other_dir
                    .parent()
                    .unwrap_or(&other_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&other, "k1", "v").await;

        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let count = store
            .merge_registry(&other, Collision::Error, None)
            .await
            .unwrap();
        assert_eq!(count, 1);
        // The key is registered and its content is accessible.
        assert!(store.is_registered("artifact://k1").await);
        assert_eq!(store.content("artifact://k1", None).await.unwrap(), "v");
    }

    #[tokio::test]
    async fn test_merge_registry_with_prefix() {
        let (_tmp, artifact_dir) = setup_registry();
        let (_, other_dir) = setup_registry();

        let other = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(other_dir.clone()),
            registry_path: Some(
                other_dir
                    .parent()
                    .unwrap_or(&other_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&other, "child", "v").await;

        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let count = store
            .merge_registry(&other, Collision::Error, Some("parent"))
            .await
            .unwrap();
        // Both the prefixed key and the original key are registered.
        assert!(count >= 1);
        assert!(store.is_registered("artifact://parent/child").await);
        assert!(store.is_registered("artifact://child").await);
    }

    #[tokio::test]
    async fn test_merge_registry_with_file_copy() {
        let (_tmp, artifact_dir) = setup_registry();
        let (tmp2, other_dir) = setup_registry();
        let _ = &tmp2;

        let other = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(other_dir.clone()),
            registry_path: Some(
                other_dir
                    .parent()
                    .unwrap_or(&other_dir)
                    .join("registry.json"),
            ),
        };
        let src_file = write_file(&other, "note.txt", "hello").await;
        assert!(Path::new(&src_file).exists());

        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        let count = store
            .merge_registry(&other, Collision::Error, None)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let stored = store.data_uri("artifact://note.txt").await.unwrap();
        let p = PathBuf::from(stored);
        assert!(p.exists());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
    }

    // --- merge_registry: dangling-reference regression test ---

    #[tokio::test]
    async fn test_merge_registry_survives_source_deletion() {
        let (_tmp, artifact_dir) = setup_registry();
        let (tmp2, other_dir) = setup_registry();

        let other = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(other_dir.clone()),
            registry_path: Some(
                other_dir
                    .parent()
                    .unwrap_or(&other_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&other, "data.txt", "survive-me").await;

        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        store
            .merge_registry(&other, Collision::Error, None)
            .await
            .unwrap();

        // Delete the source registry's artifact directory.
        drop(other);
        drop(tmp2);

        // The destination registry should still be able to read the content.
        let content = store.content("artifact://data.txt", None).await.unwrap();
        assert_eq!(content, "survive-me");
    }

    // --- merge_registry: collision mode tests ---

    #[tokio::test]
    async fn test_merge_registry_collision_error() {
        let (_tmp, artifact_dir) = setup_registry();
        let (_, other_dir) = setup_registry();

        let other = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(other_dir.clone()),
            registry_path: Some(
                other_dir
                    .parent()
                    .unwrap_or(&other_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&other, "dup", "from-other").await;

        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        // Pre-register the same key in the destination.
        write_file(&store, "dup", "existing").await;

        let err = store
            .merge_registry(&other, Collision::Error, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate artifact"));
    }

    #[tokio::test]
    async fn test_merge_registry_collision_ignore() {
        let (_tmp, artifact_dir) = setup_registry();
        let (_, other_dir) = setup_registry();

        let other = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(other_dir.clone()),
            registry_path: Some(
                other_dir
                    .parent()
                    .unwrap_or(&other_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&other, "dup", "from-other").await;

        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "dup", "existing").await;

        let count = store
            .merge_registry(&other, Collision::Ignore, None)
            .await
            .unwrap();
        // The duplicate key was skipped.
        assert_eq!(count, 0);
        // The existing value is preserved.
        let content = store.content("artifact://dup", None).await.unwrap();
        assert_eq!(content, "existing");
    }

    #[tokio::test]
    async fn test_from_registry_file_constructor() {
        let (_tmp, artifact_dir) = setup_registry();
        let reg_file = artifact_dir.parent().unwrap().join("custom_registry.json");
        std::fs::write(&reg_file, r#"{"a":"b"}"#).unwrap();
        let store = FileSystemStateStore::from_registry_file(&reg_file, artifact_dir)
            .await
            .unwrap();
        assert_eq!(store.data_uri("a").await.unwrap(), "b");
    }

    // --- checkout tests ---

    #[tokio::test]
    async fn test_filesystem_checkout_subset() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "a", "content-a").await;
        write_file(&store, "b", "content-b").await;
        write_file(&store, "c", "content-c").await;

        let localized = store
            .checkout_registry(&["artifact://a".to_string(), "artifact://c".to_string()])
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
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "a", "content-a").await;

        let localized = store.checkout_registry(&[]).await.unwrap();
        assert!(localized.keys().await.is_empty());
    }

    #[tokio::test]
    async fn test_filesystem_checkout_isolated_directory() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        write_file(&store, "secret", "classified").await;

        let localized = store
            .checkout_registry(&["artifact://secret".to_string()])
            .await
            .unwrap();

        // The checkout lives in a different directory from the source.
        assert_ne!(localized.artifact_dir(), store.artifact_dir());
        assert!(!localized.artifact_dir().starts_with(store.artifact_dir()));
    }

    // --- artifact_dir / has_file tests ---

    #[tokio::test]
    async fn test_filesystem_artifact_dir() {
        let (_tmp, artifact_dir) = setup_registry();
        let store = FileSystemStateStore {
            state_file: None,
            artifact_dir: Some(artifact_dir.clone()),
            registry_path: Some(
                artifact_dir
                    .parent()
                    .unwrap_or(&artifact_dir)
                    .join("registry.json"),
            ),
        };
        assert_eq!(store.artifact_dir(), artifact_dir.as_path());
    }

    #[tokio::test]
    async fn test_default_checkout_unsupported() {
        // Use a minimal struct that only implements StateStore to
        // verify the default checkout stub.
        struct StubStore;
        impl Debug for StubStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("StubStore")
            }
        }
        #[async_trait::async_trait]
        impl StateStore for StubStore {
            fn new(_gremlin_id: Option<String>) -> Self {
                unimplemented!()
            }
            fn state_tree(&self) -> Map<String, Value> {
                unimplemented!()
            }
            fn seed(&mut self, _data: &Map<String, Value>) -> Result<(), StateError> {
                unimplemented!()
            }
            fn open(&self, _name: &str, _mode: BlobMode) -> Result<Box<dyn StateBlob>, StateError> {
                unimplemented!()
            }
            fn exists(&self, _name: &str) -> bool {
                unimplemented!()
            }
            fn clear_stage_error(&self, _attempt: &str) {
                unimplemented!()
            }
            fn read_str(&self, _field: &str) -> String {
                unimplemented!()
            }
            fn read_field(&self, _field: &str) -> Option<Value> {
                unimplemented!()
            }
            fn get_field(&self, _field: &str) -> Option<Value> {
                unimplemented!()
            }
            fn stage_error(&self) -> Option<Map<String, Value>> {
                unimplemented!()
            }
            fn parallel_worktrees(&self, _group_name: &str) -> (String, HashMap<String, String>) {
                unimplemented!()
            }
            fn patch(&self, _delete: &[String], _fields: &Map<String, Value>) {
                unimplemented!()
            }
            fn record_stage_error(&self, _class: &str, _detail: &str) {
                unimplemented!()
            }
            fn accumulate_token_usage(&self, _usage: &HashMap<String, i64>) {
                unimplemented!()
            }
            fn write_terminal_state(&self, _exit_code: i32) {
                unimplemented!()
            }
            fn persist(
                &mut self,
                _state_dir: &Path,
                _data: &Map<String, Value>,
            ) -> Result<(), StateError> {
                unimplemented!()
            }
            fn patch_parallel_worktrees(
                &self,
                _group_name: &str,
                _base_head: Option<&str>,
                _paths: Option<&HashMap<String, String>>,
            ) {
                unimplemented!()
            }
            fn add_subprocess_cost(&self, _amount: f64) {
                unimplemented!()
            }
            fn patch_parallel_attempt(&self, _child_key: &str, _attempt: &str) {
                unimplemented!()
            }
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
            ) -> Result<Box<dyn StateStore>, Box<dyn std::error::Error>> {
                unimplemented!()
            }
            fn as_any(&self) -> Option<&dyn std::any::Any> {
                None
            }
        }

        let store = StubStore;
        let result = store.checkout_registry(&["key".to_string()]).await;
        match result {
            Err(e) => assert!(e.to_string().contains("not supported")),
            Ok(_) => panic!("expected error"),
        }
    }
}
