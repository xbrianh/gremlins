//! Lock-scoped resume publication and durable undo journal.
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{acquire_lock, resume_write_failure};

const JOURNAL: &str = ".resume-model-recovery.json";

// Read legacy byte-array journals, but always write UTF-8 strings.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Backup {
    Text(String),
    Legacy(Vec<u8>),
}
impl Backup {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Legacy(bytes) => bytes,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Journal {
    state: Backup,
    #[serde(skip_serializing_if = "Option::is_none")]
    definition: Option<Backup>,
}

fn stage(dir: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile, String> {
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    Ok(file)
}

fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    stage(dir, bytes)?
        .persist(dir.join(name))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}

pub(super) fn publish(
    dir: &Path,
    fields: &Map<String, Value>,
    definition: Option<&[u8]>,
) -> Result<(), String> {
    let _lock = acquire_lock(&dir.join("state.json")).map_err(|e| e.to_string())?;
    recover_unlocked(dir)?;
    // Never use a caller's stale full-state snapshot as the base of the merge.
    let original_state = fs::read_to_string(dir.join("state.json")).map_err(|e| e.to_string())?;
    let mut state: Map<String, Value> =
        serde_json::from_str(&original_state).map_err(|e| e.to_string())?;
    state.extend(fields.clone());
    let metadata = stage(dir, &serde_json::to_vec(&state).map_err(|e| e.to_string())?)?;
    let snapshot = definition.map(|bytes| stage(dir, bytes)).transpose()?;
    let journal = Journal {
        state: Backup::Text(original_state),
        definition: if definition.is_some() {
            Some(Backup::Text(
                fs::read_to_string(dir.join("definition.json")).map_err(|e| e.to_string())?,
            ))
        } else {
            None
        },
    };
    let journal_bytes = serde_json::to_vec(&journal).map_err(|e| e.to_string())?;
    if snapshot.is_some() {
        resume_write_failure("snapshot").map_err(|e| e.to_string())?;
    }
    replace(dir, JOURNAL, &journal_bytes)?;
    let publication = (|| {
        // The undo record must be durable before either target is replaced.
        sync_dir(dir)?;
        if let Some(snapshot) = snapshot {
            snapshot
                .persist(dir.join("definition.json"))
                .map_err(|e| e.to_string())?;
        }
        resume_write_failure("metadata").map_err(|e| e.to_string())?;
        metadata
            .persist(dir.join("state.json"))
            .map_err(|e| e.to_string())?;
        sync_dir(dir)?;
        resume_write_failure("journal-removal").map_err(|e| e.to_string())?;
        fs::remove_file(dir.join(JOURNAL)).map_err(|e| e.to_string())?;
        sync_dir(dir)
    })();
    if let Err(error) = publication {
        // Even failure to sync a successful unlink must roll back. Reinstall the
        // journal first so failed rollback still leaves durable recovery data.
        if !dir.join(JOURNAL).exists() {
            replace(dir, JOURNAL, &journal_bytes).and_then(|()| sync_dir(dir)).map_err(|e| {
                format!("failed to publish resume state: {error}; preserving recovery journal failed: {e}")
            })?;
        }
        recover_unlocked(dir).map_err(|rollback| {
            format!("failed to publish resume state: {error}; snapshot rollback failed: {rollback}")
        })?;
        return Err(format!("failed to publish resume state: {error}"));
    }
    Ok(())
}

pub(super) fn recover(dir: &Path) -> Result<bool, String> {
    let _lock = acquire_lock(&dir.join("state.json")).map_err(|e| e.to_string())?;
    recover_unlocked(dir)
}

// Caller owns state.json.lock, including when invoked during publication rollback.
fn recover_unlocked(dir: &Path) -> Result<bool, String> {
    let bytes = match fs::read(dir.join(JOURNAL)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    for (name, backup) in [
        ("definition.json", journal.definition.as_ref()),
        ("state.json", Some(&journal.state)),
    ] {
        let Some(backup) = backup else { continue };
        if fs::read(dir.join(name)).ok().as_deref() == Some(backup.bytes()) {
            continue;
        }
        if name == "definition.json" {
            resume_write_failure("snapshot").map_err(|e| e.to_string())?;
        }
        resume_write_failure("rollback").map_err(|e| e.to_string())?;
        replace(dir, name, backup.bytes())?;
    }
    sync_dir(dir)?;
    // Do not apply the publication-only removal fault to rollback cleanup.
    fs::remove_file(dir.join(JOURNAL)).map_err(|e| e.to_string())?;
    if let Err(error) = sync_dir(dir) {
        // Retain retry information when durability of cleanup is uncertain.
        replace(dir, JOURNAL, &bytes)
            .and_then(|()| sync_dir(dir))
            .map_err(|preserve| {
                format!(
                    "recovery directory sync failed: {error}; preserving journal failed: {preserve}"
                )
            })?;
        return Err(error);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::state::{set_resume_write_failure, FileSystemStateStore, StateStore};
    use serde_json::json;

    fn seed(dir: &Path) -> FileSystemStateStore {
        fs::write(dir.join("state.json"), r#"{"status":"paused","cost":7}"#).unwrap();
        fs::write(dir.join("definition.json"), r#"{"model":"old"}"#).unwrap();
        FileSystemStateStore::from_path(dir.to_path_buf())
    }

    #[test]
    fn publication_merges_fields_with_and_without_snapshot() {
        for definition in [None, Some(br#"{"model":"new"}"#.as_slice())] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            store
                .publish_resume(json!({"status":"running"}).as_object().unwrap(), definition)
                .unwrap();
            assert_eq!(store.state_tree()["cost"], 7);
            assert_eq!(store.state_tree()["status"], "running");
            assert!(!store.recover_resume().unwrap());
        }
    }

    #[test]
    fn publication_faults_restore_original_bytes() {
        for fault in ["snapshot", "metadata", "journal-removal"] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            let state = fs::read(dir.path().join("state.json")).unwrap();
            let definition = fs::read(dir.path().join("definition.json")).unwrap();
            set_resume_write_failure(Some(fault));
            let result = store.publish_resume(
                json!({"status":"running"}).as_object().unwrap(),
                Some(br#"{"model":"new"}"#),
            );
            set_resume_write_failure(None);
            assert!(result.is_err(), "{fault}");
            assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), state);
            assert_eq!(
                fs::read(dir.path().join("definition.json")).unwrap(),
                definition
            );
            assert!(!store.recover_resume().unwrap());
        }
    }

    #[test]
    fn generic_writes_ignore_resume_metadata_fault() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        let fields = json!({"status":"running"}).as_object().unwrap().clone();
        set_resume_write_failure(Some("metadata"));
        let write = store.write_state(&fields);
        let atomic = super::super::atomic_write_json(&dir.path().join("state.json"), &fields);
        let publication = store.publish_resume(&fields, None);
        set_resume_write_failure(None);
        write.unwrap();
        atomic.unwrap();
        assert!(publication.is_err());
    }

    #[test]
    fn failed_rollback_preserves_string_journal_for_recovery() {
        for fault in ["metadata-and-rollback", "journal-removal-and-rollback"] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            let original = fs::read(dir.path().join("state.json")).unwrap();
            set_resume_write_failure(Some(fault));
            let result = store.publish_resume(
                json!({"status":"running"}).as_object().unwrap(),
                Some(br#"{"model":"new"}"#),
            );
            set_resume_write_failure(None);
            assert!(result.unwrap_err().contains("rollback failed"));
            let journal: Value =
                serde_json::from_slice(&fs::read(dir.path().join(JOURNAL)).unwrap()).unwrap();
            assert!(journal["state"].is_string());
            assert!(journal["definition"].is_string());
            assert!(store.recover_resume().unwrap());
            assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), original);
            assert!(!store.recover_resume().unwrap());
        }
    }
}
