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

    pub fn register(&mut self, uri: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        let key = uri.to_string();
        if uri.scheme != "artifact" {
            log::warn!(
                "register({:?}): unrecognized scheme {:?} — typo? (expected 'artifact')",
                key,
                uri.scheme,
            );
        }
        if self.data.contains_key(&key) {
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
        log::debug!("register: {:?} -> {:?}", key, path_str);
        self.persist()?;
        Ok(path_str)
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

    pub fn exists(&self, uri: &str) -> bool {
        if !self.data.contains_key(uri) {
            return false;
        }
        let value = &self.data[uri];
        // Non-file-backed values (e.g. git://range, opaque://, raw strings)
        // are considered existing.  File-backed values are resolved to a
        // filesystem path and checked for presence.
        let p = if value.starts_with("file://session/") {
            let name = value.strip_prefix("file://session/").unwrap_or(value);
            self.artifact_dir.join(name)
        } else if value.starts_with("file://") {
            PathBuf::from(value.strip_prefix("file://").unwrap_or(value))
        } else {
            PathBuf::from(value)
        };
        if p.is_absolute() {
            fs::metadata(&p).is_ok()
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
        let path = reg.register(&uri).unwrap();
        assert!(path.contains("foo.txt"));

        // Reload from disk
        let reg2 = ArtifactRegistry::new(artifact_dir);
        assert_eq!(reg2.data_uri("artifact://foo.txt").unwrap(), &path);
    }

    #[test]
    fn test_register_duplicate_raises() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::new("artifact".to_string(), "a.txt".to_string());
        reg.register(&uri).unwrap();
        let err = reg.register(&uri).unwrap_err();
        assert!(err.to_string().contains("duplicate artifact"));
    }

    #[test]
    fn test_register_path_escape_prevention() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::new("artifact".to_string(), "../bad.txt".to_string());
        let err = reg.register(&uri);
        assert!(err.is_err());
    }

    #[test]
    fn test_content_reads_file() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir.clone());
        let uri = Uri::parse("artifact://hello.txt").unwrap();
        let path = reg.register(&uri).unwrap();
        fs::write(&path, "world").unwrap();
        assert_eq!(reg.content("artifact://hello.txt", None).unwrap(), "world");
    }

    #[test]
    fn test_content_with_json_path() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://data.json").unwrap();
        let path = reg.register(&uri).unwrap();
        fs::write(&path, r#"{"a":{"b":"c"}}"#).unwrap();
        let content = reg.content("artifact://data.json", Some("a.b")).unwrap();
        assert_eq!(content, "c");
    }

    #[test]
    fn test_content_raw_non_file_value() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://range").unwrap();
        let path = reg.register(&uri).unwrap();
        fs::write(&path, "git://range/abc..def").unwrap();
        assert_eq!(
            reg.content("artifact://range", None).unwrap(),
            "git://range/abc..def",
        );
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
        let uri = Uri::parse("artifact://missing.txt").unwrap();
        reg.register(&uri).unwrap();
        assert!(!reg.exists("artifact://missing.txt"));
    }

    #[test]
    fn test_exists_true_for_empty_file() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://empty.txt").unwrap();
        let path = reg.register(&uri).unwrap();
        fs::write(&path, "").unwrap();
        assert!(reg.exists("artifact://empty.txt"));
    }

    #[test]
    fn test_exists_true_for_non_empty_file() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        let uri = Uri::parse("artifact://stuff.txt").unwrap();
        let path = reg.register(&uri).unwrap();
        fs::write(&path, "data").unwrap();
        assert!(reg.exists("artifact://stuff.txt"));
    }

    #[test]
    fn test_keys_returns_registered_keys() {
        let (_tmp, artifact_dir) = setup();
        let mut reg = ArtifactRegistry::new(artifact_dir);
        reg.register(&Uri::parse("artifact://a").unwrap()).unwrap();
        reg.register(&Uri::parse("artifact://b").unwrap()).unwrap();
        let mut keys: Vec<&String> = reg.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["artifact://a", "artifact://b"]);
    }

    #[test]
    fn test_merge_from_identity_key_map() {
        let (_tmp, artifact_dir) = setup();
        let (_, other_dir) = setup();

        let mut other = ArtifactRegistry::new(other_dir);
        other
            .register(&Uri::parse("artifact://k1").unwrap())
            .unwrap();

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
        other
            .register(&Uri::parse("artifact://child").unwrap())
            .unwrap();

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
        let uri = Uri::parse("artifact://note.txt").unwrap();
        let src_file = other.register(&uri).unwrap();
        fs::write(&src_file, "hello").unwrap();

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
