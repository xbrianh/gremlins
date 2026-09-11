use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json;
use thiserror::Error;

use crate::artifacts::uri::Uri;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

// --- Registry ---

pub struct ArtifactRegistry {
    pub artifact_dir: PathBuf,
    pub registry_path: PathBuf,
    data: HashMap<String, String>,
}

impl ArtifactRegistry {
    pub fn new(artifact_dir: PathBuf) -> Self {
        let registry_path = artifact_dir
            .parent()
            .unwrap_or(&artifact_dir)
            .join("registry.json");
        let data = if registry_path.exists() {
            match fs::read_to_string(&registry_path) {
                Ok(content) => match serde_json::from_str::<HashMap<String, String>>(&content) {
                    Ok(data) => data,
                    Err(e) => {
                        log::error!(
                                "failed to parse registry.json at {}: {e} — starting with empty registry",
                                registry_path.display(),
                            );
                        HashMap::new()
                    }
                },
                Err(e) => {
                    log::error!(
                        "failed to read registry.json at {}: {e} — starting with empty registry",
                        registry_path.display(),
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        log::info!(
            "registry ready: {} entries from {}",
            data.len(),
            registry_path.display(),
        );
        ArtifactRegistry {
            artifact_dir,
            registry_path,
            data,
        }
    }

    pub fn load(artifact_dir: PathBuf) -> Result<Self, std::io::Error> {
        let registry_path = artifact_dir
            .parent()
            .unwrap_or(&artifact_dir)
            .join("registry.json");
        let data = if registry_path.exists() {
            let content = fs::read_to_string(&registry_path)?;
            serde_json::from_str(&content)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        } else {
            HashMap::new()
        };
        log::info!(
            "registry ready: {} entries from {}",
            data.len(),
            registry_path.display(),
        );
        Ok(ArtifactRegistry {
            artifact_dir,
            registry_path,
            data,
        })
    }

    pub fn data_uri(&self, key: &str) -> Result<&str, MissingArtifact> {
        self.data
            .get(key)
            .map(|s| s.as_str())
            .ok_or_else(|| MissingArtifact {
                key: key.to_string(),
            })
    }

    /// Compute the canonical filesystem path for `uri` without touching the registry.
    pub fn path_for_uri(&self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
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
        fs::create_dir_all(&self.artifact_dir)?;
        let base = fs::canonicalize(&self.artifact_dir)?;
        // Create parent dirs so parent-side canonicalization works
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
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
    pub fn commit(&mut self, key: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(existing) = self.data.get(key) {
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
        if !Path::new(path).exists() {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("artifact {key:?} has no file at {path}"),
            )));
        }
        self.data.insert(key.to_string(), path.to_string());
        log::debug!("commit: {:?} -> {:?}", key, path);
        self.persist()?;
        Ok(())
    }

    /// Write `content` to the path for `uri`, then commit. For bootstrap ingestion.
    pub fn write_into_registry(
        &mut self,
        uri: &Uri,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let path = self.path_for_uri(uri)?;
        fs::write(&path, content)?;
        self.commit(&uri.to_string(), &path)?;
        Ok(path)
    }

    /// Copy `source` to the path for `uri`, then commit. For bootstrap ingestion.
    pub fn copy_into_registry(
        &mut self,
        uri: &Uri,
        source: &Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let path = self.path_for_uri(uri)?;
        fs::copy(source, &path)?;
        self.commit(&uri.to_string(), &path)?;
        Ok(path)
    }

    pub fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let raw = self.data_uri(uri_str)?;
        let p = if raw.starts_with("file://session/") {
            let name = raw.strip_prefix("file://session/").unwrap_or(raw);
            self.artifact_dir.join(name)
        } else if let Some(stripped) = raw.strip_prefix("file://") {
            PathBuf::from(stripped)
        } else {
            PathBuf::from(raw)
        };
        if !p.exists() {
            return Err(Box::new(MissingArtifact {
                key: uri_str.to_string(),
            }));
        }
        let text = fs::read_to_string(&p)?;
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

    pub fn is_registered(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// True when `key` is registered and its file is still on disk.
    /// Non-file values (git://…, raw strings) are live on membership alone.
    pub fn is_live(&self, key: &str) -> bool {
        let value = match self.data.get(key) {
            Some(v) => v,
            None => return false,
        };
        let p = if let Some(name) = value.strip_prefix("file://session/") {
            self.artifact_dir.join(name)
        } else if let Some(rest) = value.strip_prefix("file://") {
            PathBuf::from(rest)
        } else {
            PathBuf::from(value)
        };
        !p.is_absolute() || fs::metadata(&p).is_ok()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.data.keys()
    }

    pub fn merge_from(
        &mut self,
        other: &ArtifactRegistry,
        key_map: Option<&HashMap<String, String>>,
        copy_files: bool,
        keys: Option<&std::collections::HashSet<String>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let iter: Box<dyn Iterator<Item = &String>> = match keys {
            Some(k) => Box::new(k.iter()),
            None => Box::new(other.data.keys()),
        };
        let mut merged = 0u64;
        for key in iter {
            let uri_str = match other.data.get(key) {
                Some(v) => v,
                None => continue,
            };
            let parent_key = key_map.and_then(|km| km.get(key)).unwrap_or(key);
            if self.data.contains_key(parent_key) {
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
                self.data
                    .insert(parent_key.clone(), dest_path.to_string_lossy().to_string());
                self.persist()?;
                merged += 1;
            } else {
                self.data.insert(parent_key.clone(), uri_str.clone());
                self.persist()?;
                merged += 1;
            }
        }
        log::info!("merge_from: merged {} entries", merged);
        Ok(())
    }

    pub fn from_registry_file(
        path: &Path,
        artifact_dir: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut registry = ArtifactRegistry::new(artifact_dir.clone());
        // Only load from path if it differs from the canonical registry_path
        if path != registry.registry_path && path.exists() {
            let content = fs::read_to_string(path)?;
            registry.data = serde_json::from_str(&content)?;
            registry.persist()?;
            log::info!(
                "loaded custom registry from {} ({} entries)",
                path.display(),
                registry.data.len(),
            );
        }
        Ok(registry)
    }

    fn persist(&self) -> Result<(), Box<dyn std::error::Error>> {
        let pid = std::process::id();
        let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!(".{}.{}.tmp", pid, count);
        let tmp_path = self.registry_path.with_file_name(
            self.registry_path
                .file_name()
                .map(|f| {
                    let mut s = f.to_string_lossy().to_string();
                    s.push_str(&tmp_name);
                    s
                })
                .unwrap_or_else(|| tmp_name),
        );
        if let Some(parent) = tmp_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(&self.data)?;
        fs::write(&tmp_path, &json)?;
        fs::rename(&tmp_path, &self.registry_path)?;
        log::debug!(
            "persist: wrote {} entries to {}",
            self.data.len(),
            self.registry_path.display(),
        );
        Ok(())
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

    fn write_file(reg: &mut ArtifactRegistry, name: &str, content: &str) -> String {
        let uri = Uri::parse(&format!("artifact://{name}")).unwrap();
        reg.write_into_registry(&uri, content).unwrap()
    }

    #[test]
    fn test_data_uri_unbound_raises_missing() {
        let (_tmp, artifact_dir) = setup();
        let registry = ArtifactRegistry::new(artifact_dir);
        let err = registry.data_uri("nonexistent").unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn test_write_into_registry_persists_and_roundtrip() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir.clone());
        let uri = Uri::new("artifact".to_string(), "foo.txt".to_string());
        let path = reg.write_into_registry(&uri, "hello").unwrap();
        assert!(path.contains("foo.txt"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");

        // Reload from disk
        let reg2 = ArtifactRegistry::new(artifact_dir);
        assert_eq!(reg2.data_uri("artifact://foo.txt").unwrap(), &path);
    }

    #[test]
    fn test_path_for_uri_does_not_register() {
        let (_tmp, artifact_dir) = setup();
        let reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://later.txt").unwrap();
        let path = reg.path_for_uri(&uri).unwrap();
        assert!(path.ends_with("later.txt"));
        assert!(!reg.is_registered("artifact://later.txt"));
    }

    #[test]
    fn test_commit_idempotent_same_path() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://a.txt").unwrap();
        let path = reg.path_for_uri(&uri).unwrap();
        fs::write(&path, "").unwrap();
        reg.commit("artifact://a.txt", &path).unwrap();
        reg.commit("artifact://a.txt", &path).unwrap();
    }

    #[test]
    fn test_commit_rejects_missing_file() {
        let (tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let missing = tmp.path().join("does-not-exist.txt");
        let err = reg
            .commit("artifact://gone.txt", &missing.to_string_lossy())
            .unwrap_err();
        assert!(err.to_string().contains("has no file at"));
        assert!(!reg.is_registered("artifact://gone.txt"));
    }

    #[test]
    fn test_commit_conflicting_path_raises() {
        let (tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        fs::write(&one, "").unwrap();
        fs::write(&two, "").unwrap();
        reg.commit("artifact://a.txt", &one.to_string_lossy())
            .unwrap();
        let err = reg
            .commit("artifact://a.txt", &two.to_string_lossy())
            .unwrap_err();
        assert!(err.to_string().contains("duplicate artifact"));
    }

    #[test]
    fn test_is_live_true_after_write() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        write_file(&mut reg, "live.txt", "data");
        assert!(reg.is_live("artifact://live.txt"));
    }

    #[test]
    fn test_is_live_false_after_delete() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let path = write_file(&mut reg, "dead.txt", "data");
        assert!(reg.is_live("artifact://dead.txt"));
        fs::remove_file(&path).unwrap();
        assert!(reg.is_registered("artifact://dead.txt"));
        assert!(!reg.is_live("artifact://dead.txt"));
    }

    #[test]
    fn test_is_live_false_for_unregistered_key() {
        let (_tmp, artifact_dir) = setup();
        let reg = ArtifactRegistry::new(artifact_dir);
        assert!(!reg.is_live("artifact://never"));
    }

    #[test]
    fn test_copy_into_registry() {
        let (tmp, artifact_dir) = setup();
        let src = tmp.path().join("src.txt");
        fs::write(&src, "copied").unwrap();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://dst.txt").unwrap();
        let path = reg.copy_into_registry(&uri, &src).unwrap();
        assert!(reg.is_registered("artifact://dst.txt"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "copied");
    }

    #[test]
    fn test_path_escape_prevention() {
        let (_tmp, artifact_dir) = setup();
        let reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::new("artifact".to_string(), "../bad.txt".to_string());
        assert!(reg.path_for_uri(&uri).is_err());
    }

    #[test]
    fn test_content_reads_file() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir.clone());
        write_file(&mut reg, "hello.txt", "world");
        assert_eq!(reg.content("artifact://hello.txt", None).unwrap(), "world");
    }

    #[test]
    fn test_content_with_json_path() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        write_file(&mut reg, "data.json", r#"{"a":{"b":"c"}}"#);
        let content = reg.content("artifact://data.json", Some("a.b")).unwrap();
        assert_eq!(content, "c");
    }

    #[test]
    fn test_content_reads_file_containing_uri_text() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        write_file(&mut reg, "range", "git://range/abc..def");
        assert_eq!(
            reg.content("artifact://range", None).unwrap(),
            "git://range/abc..def",
        );
    }

    #[test]
    fn test_is_registered_false_for_missing_key() {
        let (_tmp, artifact_dir) = setup();
        let reg = ArtifactRegistry::new(artifact_dir);
        assert!(!reg.is_registered("nonexistent"));
    }

    #[test]
    fn test_is_registered_true_after_write() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        write_file(&mut reg, "stuff.txt", "data");
        assert!(reg.is_registered("artifact://stuff.txt"));
    }

    #[test]
    fn test_keys_returns_registered_keys() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        write_file(&mut reg, "a", "");
        write_file(&mut reg, "b", "");
        let mut keys: Vec<&String> = reg.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["artifact://a", "artifact://b"]);
    }

    #[test]
    fn test_merge_from_identity_key_map() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let mut other = ArtifactRegistry::new(other_dir);
        write_file(&mut other, "k1", "v");

        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, None, false, None).unwrap();
        assert_eq!(
            reg.data_uri("artifact://k1").unwrap(),
            other.data_uri("artifact://k1").unwrap(),
        );
    }

    #[test]
    fn test_merge_from_custom_key_map() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let mut other = ArtifactRegistry::new(other_dir);
        write_file(&mut other, "child", "v");

        let mut key_map = HashMap::new();
        key_map.insert("artifact://child".to_string(), "parent".to_string());

        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, Some(&key_map), false, None).unwrap();
        assert_eq!(
            reg.data_uri("parent").unwrap(),
            other.data_uri("artifact://child").unwrap(),
        );
    }

    #[test]
    fn test_merge_from_with_file_copy() {
        let (_tmp, artifact_dir) = setup();
        let (tmp2, other_dir) = setup();
        let _ = &tmp2;

        let mut other = ArtifactRegistry::new(other_dir);
        let src_file = write_file(&mut other, "note.txt", "hello");
        assert!(Path::new(&src_file).exists());

        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, None, true, None).unwrap();
        let stored = reg.data_uri("artifact://note.txt").unwrap();
        let p = PathBuf::from(stored);
        assert!(p.exists());
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
    }

    #[test]
    fn test_from_registry_file_constructor() {
        let (_tmp, artifact_dir) = setup();
        let reg_file = artifact_dir.parent().unwrap().join("custom_registry.json");
        fs::write(&reg_file, r#"{"a":"b"}"#).unwrap();
        let reg = ArtifactRegistry::from_registry_file(&reg_file, artifact_dir).unwrap();
        assert_eq!(reg.data_uri("a").unwrap(), "b");
    }
}
