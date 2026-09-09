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
    pub data: HashMap<String, String>,
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
            "new registry from {} ({} entries)",
            registry_path.display(),
            data.len(),
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
            "loaded registry from {} ({} entries)",
            registry_path.display(),
            data.len(),
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

    pub fn register(
        &mut self,
        uri: &Uri,
        overwrite: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let key = uri.to_string();
        if uri.scheme != "artifact" {
            log::warn!(
                "register({:?}): unrecognized scheme {:?} — typo? (expected 'artifact')",
                key,
                uri.scheme,
            );
        }
        if self.data.contains_key(&key) && !overwrite {
            return Err(Box::new(DuplicateArtifact {
                key: key.clone(),
                existing: self.data[&key].clone(),
                incoming: uri.to_string(),
            }));
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
        let path_str = resolved.to_string_lossy().to_string();
        self.data.insert(key.clone(), path_str.clone());
        log::debug!("register: {} -> {}", key, path_str);
        self.persist()?;
        Ok(path_str)
    }

    pub fn content(
        &self,
        uri_str: &str,
        json_path: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let raw = self.data_uri(uri_str)?;
        let text = if raw.starts_with("file://session/") {
            let name = raw.strip_prefix("file://session/").unwrap_or(raw);
            let p = self.artifact_dir.join(name);
            if !p.exists() {
                return Err(Box::new(MissingArtifact {
                    key: uri_str.to_string(),
                }));
            }
            fs::read_to_string(&p)?
        } else if raw.starts_with("file://") {
            let p = PathBuf::from(raw.strip_prefix("file://").unwrap_or(raw));
            if !p.exists() {
                return Err(Box::new(MissingArtifact {
                    key: uri_str.to_string(),
                }));
            }
            fs::read_to_string(&p)?
        } else if raw.starts_with('/') {
            let p = PathBuf::from(raw);
            if !p.exists() {
                return Err(Box::new(MissingArtifact {
                    key: uri_str.to_string(),
                }));
            }
            fs::read_to_string(&p)?
        } else {
            log::warn!(
                "content({:?}): returning raw registry value as-is (not a file path)",
                uri_str,
            );
            return Ok(raw.to_string());
        };
        log::debug!("content({}) read {} bytes", uri_str, text.len());
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

    pub fn exists(&self, uri: &str) -> bool {
        if !self.data.contains_key(uri) {
            return false;
        }
        let value = &self.data[uri];
        // Non-string values are considered existing
        // For string values, resolve to filesystem path
        let p = if value.starts_with("file://session/") {
            let name = value.strip_prefix("file://session/").unwrap_or(value);
            self.artifact_dir.join(name)
        } else if value.starts_with("file://") {
            PathBuf::from(value.strip_prefix("file://").unwrap_or(value))
        } else {
            PathBuf::from(value)
        };
        if p.is_absolute() {
            match fs::metadata(&p) {
                Ok(m) => m.len() > 0,
                Err(_) => false,
            }
        } else {
            // Non-file values (e.g. git://range, opaque://, raw strings)
            true
        }
    }

    pub fn is_registered(&self, key: &str) -> bool {
        self.data.contains_key(key)
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
            } else {
                self.data.insert(parent_key.clone(), uri_str.clone());
                self.persist()?;
            }
        }
        log::debug!(
            "merge_from completed ({} entries in registry)",
            self.data.len(),
        );
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
            log::debug!(
                "from_registry_file: loaded {} entries from {}",
                registry.data.len(),
                path.display(),
            );
            registry.persist()?;
        }
        Ok(registry)
    }

    pub fn persist(&self) -> Result<(), Box<dyn std::error::Error>> {
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
            "persisted registry to {} ({} entries)",
            self.registry_path.display(),
            self.data.len(),
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

    #[test]
    fn test_data_uri_unbound_raises_missing() {
        let (_tmp, artifact_dir) = setup();
        let registry = ArtifactRegistry::new(artifact_dir);
        let err = registry.data_uri("nonexistent").unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn test_register_persists_and_roundtrip() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir.clone());
        let uri = Uri::new("artifact".to_string(), "foo.txt".to_string());
        let path = reg.register(&uri, true).unwrap();
        assert!(path.contains("foo.txt"));

        // Reload from disk
        let reg2 = ArtifactRegistry::new(artifact_dir);
        assert_eq!(reg2.data_uri("artifact://foo.txt").unwrap(), &path);
    }

    #[test]
    fn test_register_duplicate_no_overwrite_raises() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::new("artifact".to_string(), "a.txt".to_string());
        reg.register(&uri, true).unwrap();
        let err = reg.register(&uri, false).unwrap_err();
        assert!(err.to_string().contains("duplicate artifact"));
    }

    #[test]
    fn test_register_duplicate_with_overwrite_succeeds() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::new("artifact".to_string(), "a.txt".to_string());
        reg.register(&uri, true).unwrap();
        let path = reg.register(&uri, true).unwrap();
        assert!(path.contains("a.txt"));
    }

    #[test]
    fn test_register_path_escape_prevention() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::new("artifact".to_string(), "../bad.txt".to_string());
        let err = reg.register(&uri, true);
        assert!(err.is_err());
    }

    #[test]
    fn test_content_reads_file() {
        let (_tmp, artifact_dir) = setup();
        // Manually set up a file and register it
        let file_path = artifact_dir.join("hello.txt");
        fs::write(&file_path, "world").unwrap();
        let mut reg = ArtifactRegistry::new(artifact_dir.clone());
        reg.data.insert(
            "file://session/hello.txt".to_string(),
            file_path.to_string_lossy().to_string(),
        );
        let content = reg.content("file://session/hello.txt", None).unwrap();
        assert_eq!(content, "world");
    }

    #[test]
    fn test_content_with_json_path() {
        let (_tmp, artifact_dir) = setup();
        let file_path = artifact_dir.join("data.json");
        fs::write(&file_path, r#"{"a":{"b":"c"}}"#).unwrap();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.data.insert(
            "test://data.json".to_string(),
            file_path.to_string_lossy().to_string(),
        );
        let content = reg.content("test://data.json", Some("a.b")).unwrap();
        assert_eq!(content, "c");
    }

    #[test]
    fn test_content_raw_non_file_value() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.data.insert(
            "git://range/abc..def".to_string(),
            "git://range/abc..def".to_string(),
        );
        let content = reg.content("git://range/abc..def", None).unwrap();
        assert_eq!(content, "git://range/abc..def");
    }

    #[test]
    fn test_exists_false_for_missing_key() {
        let (_tmp, artifact_dir) = setup();
        let reg = ArtifactRegistry::new(artifact_dir);
        assert!(!reg.exists("nonexistent"));
    }

    #[test]
    fn test_exists_false_for_missing_file() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.data.insert(
            "artifact://missing.txt".to_string(),
            "/nonexistent/path/file.txt".to_string(),
        );
        assert!(!reg.exists("artifact://missing.txt"));
    }

    #[test]
    fn test_exists_false_for_empty_file() {
        let (_tmp, artifact_dir) = setup();
        let file_path = artifact_dir.join("empty.txt");
        fs::write(&file_path, "").unwrap();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.data.insert(
            "artifact://empty.txt".to_string(),
            file_path.to_string_lossy().to_string(),
        );
        assert!(!reg.exists("artifact://empty.txt"));
    }

    #[test]
    fn test_exists_true_for_non_empty_file() {
        let (_tmp, artifact_dir) = setup();
        let file_path = artifact_dir.join("stuff.txt");
        fs::write(&file_path, "data").unwrap();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.data.insert(
            "artifact://stuff.txt".to_string(),
            file_path.to_string_lossy().to_string(),
        );
        assert!(reg.exists("artifact://stuff.txt"));
    }

    #[test]
    fn test_keys_returns_registered_keys() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.data.insert("a".to_string(), "1".to_string());
        reg.data.insert("b".to_string(), "2".to_string());
        let mut keys: Vec<&String> = reg.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn test_merge_from_identity_key_map() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let mut other = ArtifactRegistry::new(other_dir);
        other.data.insert("k1".to_string(), "v1".to_string());

        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, None, false, None).unwrap();
        assert_eq!(reg.data_uri("k1").unwrap(), "v1");
    }

    #[test]
    fn test_merge_from_custom_key_map() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let mut other = ArtifactRegistry::new(other_dir);
        other
            .data
            .insert("child".to_string(), "child_val".to_string());

        let mut key_map = HashMap::new();
        key_map.insert("child".to_string(), "parent".to_string());

        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, Some(&key_map), false, None).unwrap();
        assert_eq!(reg.data_uri("parent").unwrap(), "child_val");
    }

    #[test]
    fn test_merge_from_with_file_copy() {
        let (_tmp, artifact_dir) = setup();
        let (tmp2, other_dir) = setup();

        // Create a file in other's artifact_dir
        let src_file = other_dir.join("note.txt");
        fs::write(&src_file, "hello").unwrap();

        let mut other = ArtifactRegistry::new(other_dir);
        other
            .data
            .insert("note".to_string(), src_file.to_string_lossy().to_string());

        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.merge_from(&other, None, true, None).unwrap();
        let stored = reg.data_uri("note").unwrap();
        let p = PathBuf::from(stored);
        assert!(p.exists());
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
        let _ = tmp2;
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
