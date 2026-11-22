//! Lock-scoped resume publication with a durable commit-aware recovery journal.
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{acquire_lock, resume_write_failure};

const JOURNAL: &str = ".resume-model-recovery.json";
const PUBLICATION_ID: &str = "resume_publication_id";

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
    #[serde(default)]
    remove_definition: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committed_state: Option<Backup>,
    #[serde(default)]
    committed: bool,
    #[serde(default)]
    invalidate_finished: bool,
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

fn invalidate_finished(dir: &Path) -> Result<(), String> {
    resume_write_failure("terminal-marker").map_err(|e| e.to_string())?;
    match fs::remove_file(dir.join("finished")) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.to_string()),
    }
    resume_write_failure("terminal-marker-sync").map_err(|e| e.to_string())?;
    sync_dir(dir)
}

fn archive(dir: &Path, bytes: &[u8]) -> Result<String, String> {
    let id = super::secure_token_hex(16).map_err(|e| e.to_string())?;
    let revisions = dir.join("definition-revisions");
    fs::create_dir_all(&revisions).map_err(|e| e.to_string())?;
    stage(&revisions, bytes)?
        .persist_noclobber(revisions.join(format!("{id}.json")))
        .map_err(|e| e.to_string())?;
    sync_dir(&revisions)?;
    sync_dir(dir)?;
    Ok(id)
}

fn append_revision(
    dir: &Path,
    state: &mut Map<String, Value>,
    bytes: &[u8],
    metadata: &Map<String, Value>,
) -> Result<(), String> {
    let prepared = super::revision::prepare(state, bytes, metadata);
    let id = archive(dir, bytes)?;
    super::revision::append(state, id, &prepared)
}

pub(super) fn initialize(dir: &Path, bytes: &[u8]) -> Result<(), String> {
    publish(dir, &Map::new(), Some(bytes), None, &Map::new())
}

pub(super) fn publish(
    dir: &Path,
    fields: &Map<String, Value>,
    definition: Option<&[u8]>,
    expected: Option<&Map<String, Value>>,
    revision_metadata: &Map<String, Value>,
) -> Result<(), String> {
    publish_definition(
        dir,
        fields,
        definition,
        expected,
        revision_metadata,
        expected.is_some(),
    )
}

pub(super) fn publish_definition(
    dir: &Path,
    fields: &Map<String, Value>,
    definition: Option<&[u8]>,
    expected: Option<&Map<String, Value>>,
    revision_metadata: &Map<String, Value>,
    invalidate_terminal: bool,
) -> Result<(), String> {
    let _lock = acquire_lock(&dir.join("state.json")).map_err(|e| e.to_string())?;
    recover_unlocked(dir)?;
    if expected.is_some() {
        super::validate_resume_attempt(dir)?;
    }
    // Never use a caller's stale full-state snapshot as the base of the merge.
    let original_state = fs::read_to_string(dir.join("state.json")).map_err(|e| e.to_string())?;
    let mut state: Map<String, Value> =
        serde_json::from_str(&original_state).map_err(|e| e.to_string())?;
    if expected.is_some_and(|expected| expected != &state) {
        return Err("resume conflict: state changed since planning".into());
    }
    let old_definition = match fs::read(dir.join("definition.json")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to read predecessor definition: {error}")),
    };
    if state.get("definition_revision").is_none() {
        if let Some(bytes) = old_definition.as_deref() {
            append_revision(dir, &mut state, bytes, &Map::new())?;
        }
    }
    state.extend(fields.clone());
    if let Some(bytes) = definition {
        append_revision(dir, &mut state, bytes, revision_metadata)?;
    }
    // Commit evidence travels atomically with state and survives worker patches,
    // even if writing the journal's committed flag fails after publication.
    state.insert(
        PUBLICATION_ID.into(),
        Value::String(super::secure_token_hex(16).map_err(|e| e.to_string())?),
    );
    let target_state = serde_json::to_string(&state).map_err(|e| e.to_string())?;
    let metadata = stage(dir, target_state.as_bytes())?;
    let snapshot = definition.map(|bytes| stage(dir, bytes)).transpose()?;
    let mut journal = Journal {
        state: Backup::Text(original_state),
        definition: if definition.is_some() {
            old_definition.map(|bytes| match String::from_utf8(bytes) {
                Ok(text) => Backup::Text(text),
                Err(e) => Backup::Legacy(e.into_bytes()),
            })
        } else {
            None
        },
        remove_definition: definition.is_some() && !dir.join("definition.json").exists(),
        committed_state: Some(Backup::Text(target_state)),
        committed: false,
        invalidate_finished: invalidate_terminal,
    };
    let journal_bytes = serde_json::to_vec(&journal).map_err(|e| e.to_string())?;
    if snapshot.is_some() {
        resume_write_failure("snapshot").map_err(|e| e.to_string())?;
    }
    replace(dir, JOURNAL, &journal_bytes)?;
    let publication = (|| {
        // The undo record must be durable before either target is replaced.
        sync_dir(dir)?;
        #[cfg(test)]
        publication_boundary(dir, "before-commit");
        resume_write_failure("metadata").map_err(|e| e.to_string())?;
        metadata
            .persist(dir.join("state.json"))
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })();
    if let Err(error) = publication {
        // A failed precommit publication retains undo data until recovery succeeds.
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
    // Rename makes the commit visible; directory sync makes it durable. On
    // uncertainty retain the journal and forbid dispatch or a blind retry.
    resume_write_failure("commit-sync")
        .map_err(|e| e.to_string())
        .and_then(|()| sync_dir(dir))
        .map_err(|error| format!("resume publication indeterminate after state rename: {error}; recover before retrying"))?;
    journal.committed = true;
    if journal.invalidate_finished {
        invalidate_finished(dir).map_err(|error| {
            format!("resume publication committed; terminal marker invalidation failed: {error}; recover before retrying")
        })?;
    }
    let cleanup = (|| {
        #[cfg(test)]
        publication_boundary(dir, "after-commit");
        // The flag also supports recovery of journals written by older versions.
        resume_write_failure("journal-commit").map_err(|e| e.to_string())?;
        replace(
            dir,
            JOURNAL,
            &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
        )?;
        sync_dir(dir)?;
        // Legacy readers keep the old alias until commit; revision-backed
        // readers use the durable archive even while this alias is still old.
        if let Some(snapshot) = snapshot {
            resume_write_failure("alias").map_err(|e| e.to_string())?;
            snapshot
                .persist(dir.join("definition.json"))
                .map_err(|e| e.to_string())?;
            sync_dir(dir)?;
        }
        resume_write_failure("journal-removal").map_err(|e| e.to_string())?;
        fs::remove_file(dir.join(JOURNAL)).map_err(|e| e.to_string())?;
        sync_dir(dir)
    })();
    if let Err(error) = cleanup {
        log::warn!("resume committed; recovery journal cleanup deferred: {error}");
    }
    Ok(())
}

pub(super) fn recover(dir: &Path) -> Result<bool, String> {
    let _lock = acquire_lock(&dir.join("state.json")).map_err(|e| e.to_string())?;
    recover_unlocked(dir)
}

fn journal_is_committed(dir: &Path, journal: &Journal) -> bool {
    if journal.committed {
        return true;
    }
    let Ok(current) = fs::read(dir.join("state.json")) else {
        return false;
    };
    if let Some(target) = &journal.committed_state {
        if current == target.bytes() {
            return true;
        }
        let publication_id = |bytes: &[u8]| {
            serde_json::from_slice::<Map<String, Value>>(bytes)
                .ok()?
                .get(PUBLICATION_ID)?
                .as_str()
                .map(str::to_owned)
        };
        if let Some(id) = publication_id(target.bytes()) {
            return publication_id(&current).as_ref() == Some(&id);
        }
    }
    // A generic writer may have changed state during an interrupted legacy
    // publication. Without an explicit target, only the backup pair is known.
    false
}

fn restore_committed_alias(dir: &Path, journal: &Journal) -> Result<(), String> {
    let Some(target) = &journal.committed_state else {
        return Ok(());
    };
    let state: Map<String, Value> =
        serde_json::from_slice(target.bytes()).map_err(|e| e.to_string())?;
    if let Some(Value::String(id)) = state.get("definition_revision") {
        if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid definition revision in recovery journal".into());
        }
        let bytes = fs::read(dir.join("definition-revisions").join(format!("{id}.json")))
            .map_err(|e| e.to_string())?;
        replace(dir, "definition.json", &bytes)?;
        sync_dir(dir)?;
    }
    Ok(())
}

#[cfg(test)]
type PublicationBoundary = fn(&Path, &str);

#[cfg(test)]
thread_local! {
    static PUBLICATION_BOUNDARY: std::cell::RefCell<Option<PublicationBoundary>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn publication_boundary(dir: &Path, boundary: &str) {
    PUBLICATION_BOUNDARY.with(|hook| {
        if let Some(hook) = *hook.borrow() {
            hook(dir, boundary);
        }
    });
}

// Caller owns state.json.lock, including when invoked during publication rollback.
pub(super) fn recover_unlocked(dir: &Path) -> Result<bool, String> {
    let bytes = match fs::read(dir.join(JOURNAL)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if journal_is_committed(dir, &journal) {
        if journal.invalidate_finished {
            invalidate_finished(dir).map_err(|error| {
                format!("resume publication committed; terminal marker invalidation failed: {error}; recover before retrying")
            })?;
        }
        restore_committed_alias(dir, &journal)?;
        fs::remove_file(dir.join(JOURNAL)).map_err(|e| e.to_string())?;
        sync_dir(dir)?;
        return Ok(true);
    }
    // New-format publication never changes either backup before the state
    // commit. An unmatched target must not overwrite unrelated worker progress.
    if journal.committed_state.is_some() {
        fs::remove_file(dir.join(JOURNAL)).map_err(|e| e.to_string())?;
        sync_dir(dir)?;
        return Ok(true);
    }
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
    if journal.remove_definition {
        match fs::remove_file(dir.join("definition.json")) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
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
    fn revisions_archive_exact_legacy_bytes_and_link_selection() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        let legacy = b"{ \"model\" : \"old\" }\n";
        fs::write(dir.path().join("definition.json"), legacy).unwrap();
        let expected = store.state_tree();
        let metadata = json!({"requested_selection":{"model":"new"},"resolved_selection":{"model":"provider/new"}});
        store
            .publish_resume_conditional(
                &expected,
                &Map::new(),
                Some(b"new snapshot"),
                metadata.as_object().unwrap(),
            )
            .unwrap();
        let state = store.state_tree();
        let history = state["definition_history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(
            store
                .read_definition_revision(history[0]["id"].as_str().unwrap())
                .unwrap(),
            legacy
        );
        assert_eq!(history[1]["predecessor"], history[0]["id"]);
        assert_eq!(
            history[1]["requested_selection"],
            metadata["requested_selection"]
        );
        assert_eq!(
            store
                .read_definition_revision(state["definition_revision"].as_str().unwrap())
                .unwrap(),
            b"new snapshot"
        );
        let second = store.state_tree();
        store
            .publish_resume_conditional(
                &second,
                &Map::new(),
                Some(b"third snapshot\n"),
                metadata.as_object().unwrap(),
            )
            .unwrap();
        let state = store.state_tree();
        let history = state["definition_history"].as_array().unwrap();
        assert_eq!(history.len(), 3);
        assert!(history[0]["predecessor"].is_null());
        for (index, bytes) in [legacy.as_slice(), b"new snapshot", b"third snapshot\n"]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                store
                    .read_definition_revision(history[index]["id"].as_str().unwrap())
                    .unwrap(),
                bytes
            );
            if index > 0 {
                assert_eq!(history[index]["predecessor"], history[index - 1]["id"]);
                assert_eq!(
                    history[index]["requested_selection"],
                    metadata["requested_selection"]
                );
                assert_eq!(
                    history[index]["resolved_selection"],
                    metadata["resolved_selection"]
                );
            }
        }
        assert_eq!(state["definition_revision"], history[2]["id"]);
        let before = fs::read(dir.path().join("state.json")).unwrap();
        assert!(store
            .publish_resume_conditional(&expected, &Map::new(), Some(b"stale"), &Map::new())
            .unwrap_err()
            .contains("conflict"));
        assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), before);
    }

    #[test]
    fn conditional_success_invalidates_marker_with_or_without_definition() {
        for definition in [None, Some(b"successor".as_slice())] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            fs::write(dir.path().join("finished"), b"original").unwrap();
            store
                .publish_resume_conditional(
                    &store.state_tree(),
                    &Map::new(),
                    definition,
                    &Map::new(),
                )
                .unwrap();
            assert!(!store.exists("finished"));
            store
                .publish_resume_conditional(&store.state_tree(), &Map::new(), None, &Map::new())
                .unwrap();
            assert!(!store.exists("finished"));
            assert!(!store.recover_resume().unwrap());
        }
    }

    #[test]
    fn conditional_marker_failure_is_committed_and_recovery_retries_invalidation() {
        for fault in ["terminal-marker", "terminal-marker-sync"] {
            for definition in [None, Some(b"successor".as_slice())] {
                let dir = tempfile::tempdir().unwrap();
                let store = seed(dir.path());
                fs::write(dir.path().join("finished"), b"original").unwrap();
                let expected = store.state_tree();
                set_resume_write_failure(Some(fault));
                let result = store.publish_resume_conditional(
                    &expected,
                    json!({"status":"running"}).as_object().unwrap(),
                    definition,
                    &Map::new(),
                );
                set_resume_write_failure(None);
                let error = result.unwrap_err();
                assert!(error.contains("committed"), "{error}");
                assert!(error.contains("terminal marker"), "{error}");
                assert!(dir.path().join(JOURNAL).exists());
                assert_eq!(store.state_tree()["status"], "running");
                assert_eq!(store.exists("finished"), fault == "terminal-marker");
                // Worker patches do not invalidate the journal's commit evidence.
                super::super::locked_update(&dir.path().join("state.json"), |state| {
                    state.insert("cost".into(), json!(99));
                })
                .unwrap();
                let committed = store.state_tree();
                set_resume_write_failure(Some("terminal-marker"));
                let recovery = store.recover_resume();
                set_resume_write_failure(None);
                assert!(recovery.unwrap_err().contains("committed"));
                assert!(dir.path().join(JOURNAL).exists());
                drop(store);
                let reopened = FileSystemStateStore::from_path(dir.path().to_path_buf());
                assert!(reopened.recover_resume().unwrap());
                assert!(!reopened.exists("finished"));
                assert_eq!(reopened.state_tree(), committed);
                assert!(!reopened.recover_resume().unwrap());
            }
        }
    }

    #[test]
    fn recovery_only_invalidates_markers_for_conditional_publications() {
        for conditional in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            fs::write(dir.path().join("finished"), b"original").unwrap();
            let expected = store.state_tree();
            set_resume_write_failure(Some("commit-sync"));
            let result = publish(
                dir.path(),
                &Map::new(),
                Some(b"successor"),
                conditional.then_some(&expected),
                &Map::new(),
            );
            set_resume_write_failure(None);
            assert!(result.unwrap_err().contains("indeterminate"));
            assert!(store.exists("finished"));
            assert!(store.recover_resume().unwrap());
            assert_eq!(store.exists("finished"), !conditional);
        }
    }

    #[test]
    fn marker_removal_io_error_retains_committed_recovery_journal() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        fs::create_dir(dir.path().join("finished")).unwrap();
        let error = store
            .publish_resume_conditional(&store.state_tree(), &Map::new(), None, &Map::new())
            .unwrap_err();
        assert!(error.contains("committed"));
        assert!(dir.path().join(JOURNAL).exists());
        fs::remove_dir(dir.path().join("finished")).unwrap();
        assert!(store.recover_resume().unwrap());
        assert!(!store.exists("finished"));
    }

    #[test]
    fn predecessor_read_error_aborts_without_publication() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        let original = fs::read(dir.path().join("state.json")).unwrap();
        fs::remove_file(dir.path().join("definition.json")).unwrap();
        fs::create_dir(dir.path().join("definition.json")).unwrap();
        let error = store
            .publish_resume(&Map::new(), Some(b"successor"))
            .unwrap_err();
        assert!(error.contains("predecessor definition"));
        assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), original);
        assert!(!dir.path().join(JOURNAL).exists());
        assert!(!dir.path().join("definition-revisions").exists());
    }

    #[test]
    fn indeterminate_commit_retains_evidence_and_recovers_visible_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        set_resume_write_failure(Some("commit-sync"));
        let result = store.publish_resume(
            json!({"status":"running"}).as_object().unwrap(),
            Some(b"successor"),
        );
        set_resume_write_failure(None);
        assert!(result.unwrap_err().contains("indeterminate"));
        assert!(dir.path().join(JOURNAL).exists());
        let committed = store.state_tree();
        assert_eq!(committed["status"], "running");
        assert!(store.recover_resume().unwrap());
        assert_eq!(store.state_tree(), committed);
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            b"successor"
        );
    }

    #[test]
    fn uncommitted_new_journal_preserves_unrelated_definition_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        let current = fs::read(dir.path().join("state.json")).unwrap();
        fs::write(dir.path().join("definition.json"), b"unrelated snapshot").unwrap();
        fs::write(
            dir.path().join(JOURNAL),
            serde_json::to_vec(&json!({
                "state": "{}", "definition": "backup", "remove_definition": true,
                "committed_state": "{\"resume_publication_id\":\"unpublished\"}"
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(store.recover_resume().unwrap());
        assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), current);
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            b"unrelated snapshot"
        );
    }

    #[test]
    fn launch_initial_revision_and_backend_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state.json"), "{}").unwrap();
        let store = FileSystemStateStore::from_path(dir.path().to_path_buf());
        store.initialize_definition(b"initial").unwrap();
        assert_eq!(
            store.state_tree()["definition_history"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        fs::write(dir.path().join("finished"), "").unwrap();
        store.clear_finished().unwrap();
        store.clear_finished().unwrap();
        assert!(!store.exists("finished"));
        assert!(store.read_definition_revision("../bad").is_err());
    }

    #[test]
    fn readers_see_matching_definition_at_publication_boundaries() {
        use crate::executor::state::{BlobMode, StateData};
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        PUBLICATION_BOUNDARY.with(|hook| {
            *hook.borrow_mut() = Some(|dir, boundary| {
                let store = FileSystemStateStore::from_path(dir.to_path_buf());
                let state = store.state_tree();
                let data = StateData::from_store(None, Box::new(store));
                let mut bytes = Vec::new();
                data.open_blob("definition.json", BlobMode::Read)
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                // The alias is still old on both sides of the state commit.
                assert_eq!(
                    fs::read(dir.join("definition.json")).unwrap(),
                    br#"{"model":"old"}"#
                );
                if boundary == "before-commit" {
                    assert_eq!(state["status"], "paused");
                    assert!(state.get("definition_revision").is_none());
                    assert_eq!(bytes, br#"{"model":"old"}"#);
                } else {
                    assert_eq!(state["status"], "running");
                    assert!(state["definition_revision"].is_string());
                    assert_eq!(bytes, b"new snapshot");
                }
            });
        });
        let result = store.publish_resume(
            json!({"status":"running"}).as_object().unwrap(),
            Some(b"new snapshot"),
        );
        PUBLICATION_BOUNDARY.with(|hook| *hook.borrow_mut() = None);
        result.unwrap();
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            b"new snapshot"
        );
    }

    #[test]
    fn alias_failure_is_committed_and_repaired_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        set_resume_write_failure(Some("alias"));
        let result = store.publish_resume(
            json!({"status":"running"}).as_object().unwrap(),
            Some(b"new snapshot"),
        );
        set_resume_write_failure(None);
        result.unwrap();
        assert_eq!(store.state_tree()["status"], "running");
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            br#"{"model":"old"}"#
        );
        assert!(store.recover_resume().unwrap());
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            b"new snapshot"
        );
        assert!(!store.recover_resume().unwrap());
    }

    #[test]
    fn alias_failure_preserves_committed_history_and_later_update_on_restart() {
        committed_history_and_later_update_survive_restart("alias");
    }

    #[test]
    fn journal_commit_failure_preserves_committed_history_and_later_update_on_restart() {
        committed_history_and_later_update_survive_restart("journal-commit");
    }

    fn committed_history_and_later_update_survive_restart(fault: &'static str) {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        store.initialize_definition(b"revision A").unwrap();
        let predecessor = store.state_tree()["definition_revision"].clone();

        set_resume_write_failure(Some(fault));
        let result = store.publish_resume(
            json!({"status":"running"}).as_object().unwrap(),
            Some(b"revision B"),
        );
        set_resume_write_failure(None);
        result.unwrap();
        let committed = store.state_tree();
        assert_ne!(committed["definition_revision"], predecessor);
        let journal: Value =
            serde_json::from_slice(&fs::read(dir.path().join(JOURNAL)).unwrap()).unwrap();
        assert_eq!(journal["committed"], fault != "journal-commit");
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            b"revision A"
        );

        super::super::locked_update(&dir.path().join("state.json"), |state| {
            state.insert("cost".into(), json!(99));
            state.insert("stage".into(), json!("worker-progress"));
        })
        .unwrap();
        let updated = fs::read(dir.path().join("state.json")).unwrap();
        drop(store);

        let reopened = FileSystemStateStore::from_path(dir.path().to_path_buf());
        assert!(reopened.recover_resume().unwrap());
        assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), updated);
        let recovered = reopened.state_tree();
        assert_eq!(
            recovered["definition_revision"],
            committed["definition_revision"]
        );
        assert_eq!(
            recovered["definition_history"],
            committed["definition_history"]
        );
        let history = recovered["definition_history"].as_array().unwrap();
        assert_eq!(history.last().unwrap()["predecessor"], predecessor);
        assert_eq!(recovered["cost"], 99);
        assert_eq!(recovered["stage"], "worker-progress");
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            b"revision B"
        );
        assert!(!reopened.recover_resume().unwrap());
    }

    #[test]
    fn uncommitted_publication_id_does_not_match_worker_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        store.initialize_definition(b"revision A").unwrap();
        let original = fs::read_to_string(dir.path().join("state.json")).unwrap();
        let mut target: Value = serde_json::from_str(&original).unwrap();
        target[PUBLICATION_ID] = json!("uncommitted-publication");
        fs::write(
            dir.path().join(JOURNAL),
            serde_json::to_vec(&json!({
                "state": original,
                "committed_state": target.to_string(),
                "committed": false
            }))
            .unwrap(),
        )
        .unwrap();
        super::super::locked_update(&dir.path().join("state.json"), |state| {
            state.insert("cost".into(), json!(99));
        })
        .unwrap();

        assert!(store.recover_resume().unwrap());
        assert_eq!(store.state_tree()["cost"], 99);
        assert_eq!(
            store.state_tree()[PUBLICATION_ID],
            serde_json::from_str::<Value>(&original).unwrap()[PUBLICATION_ID]
        );
        assert!(!store.recover_resume().unwrap());
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
        for fault in ["snapshot", "metadata"] {
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
        {
            let fault = "metadata-and-rollback";
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            let original = fs::read(dir.path().join("state.json")).unwrap();
            let journal = json!({
                "state": String::from_utf8(original.clone()).unwrap(),
                "definition": "{\"model\":\"old\"}"
            });
            fs::write(
                dir.path().join(JOURNAL),
                serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
            fs::write(dir.path().join("definition.json"), b"interrupted snapshot").unwrap();
            set_resume_write_failure(Some(fault));
            let result = store.recover_resume();
            set_resume_write_failure(None);
            assert!(result.unwrap_err().to_string().contains("rollback"));
            let journal: Value =
                serde_json::from_slice(&fs::read(dir.path().join(JOURNAL)).unwrap()).unwrap();
            assert!(journal["state"].is_string());
            assert!(journal["definition"].is_string());
            assert!(store.recover_resume().unwrap());
            assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), original);
            assert!(!store.recover_resume().unwrap());
        }
    }

    #[test]
    fn committed_publication_survives_restart_with_journal() {
        for fault in ["journal-removal", "journal-removal-and-rollback"] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            set_resume_write_failure(Some(fault));
            let result = store.publish_resume(
                json!({"status":"running"}).as_object().unwrap(),
                Some(b"new snapshot"),
            );
            set_resume_write_failure(None);
            result.unwrap();
            let committed = fs::read(dir.path().join("state.json")).unwrap();
            assert!(dir.path().join(JOURNAL).exists());
            drop(store);
            let reopened = FileSystemStateStore::from_path(dir.path().to_path_buf());
            reopened.recover_resume().unwrap();
            assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), committed);
            assert_eq!(
                fs::read(dir.path().join("definition.json")).unwrap(),
                b"new snapshot"
            );
            assert!(!reopened.recover_resume().unwrap());
        }
    }

    #[test]
    fn recovery_requires_explicit_commit_evidence() {
        for legacy in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = seed(dir.path());
            let original = fs::read_to_string(dir.path().join("state.json")).unwrap();
            set_resume_write_failure(Some("journal-removal"));
            let result = store.publish_resume(
                json!({"status":"running"}).as_object().unwrap(),
                Some(b"new snapshot"),
            );
            set_resume_write_failure(None);
            result.unwrap();
            let mut journal: Value =
                serde_json::from_slice(&fs::read(dir.path().join(JOURNAL)).unwrap()).unwrap();
            journal.as_object_mut().unwrap().remove("committed");
            if legacy {
                journal.as_object_mut().unwrap().remove("committed_state");
            }
            fs::write(
                dir.path().join(JOURNAL),
                serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
            assert!(store.recover_resume().unwrap());
            if legacy {
                assert_eq!(
                    fs::read_to_string(dir.path().join("state.json")).unwrap(),
                    original
                );
                assert_eq!(
                    fs::read(dir.path().join("definition.json")).unwrap(),
                    br#"{"model":"old"}"#
                );
            } else {
                assert_ne!(
                    fs::read_to_string(dir.path().join("state.json")).unwrap(),
                    original
                );
                assert_eq!(store.state_tree()["status"], "running");
            }
        }
    }

    #[test]
    fn legacy_precommit_journal_restores_definition() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path());
        let original = fs::read_to_string(dir.path().join("state.json")).unwrap();
        let journal = json!({"state":original,"definition":b"old snapshot".to_vec()});
        fs::write(
            dir.path().join(JOURNAL),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        fs::write(dir.path().join("definition.json"), b"uncommitted snapshot").unwrap();
        // Valid JSON from an unrelated writer is not proof of a legacy commit.
        fs::write(
            dir.path().join("state.json"),
            br#"{"status":"paused","cost":99}"#,
        )
        .unwrap();
        assert!(store.recover_resume().unwrap());
        assert_eq!(
            fs::read(dir.path().join("definition.json")).unwrap(),
            b"old snapshot"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("state.json")).unwrap(),
            original
        );
    }
}
