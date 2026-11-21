use super::*;

#[path = "publication_boundary_tests.rs"]
mod boundary;
use std::io::{Cursor, Read};
use std::sync::{Barrier, Mutex};

#[derive(Debug, Default)]
struct MemoryData {
    state: Map<String, Value>,
    blobs: HashMap<String, Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
struct MemoryStore(Arc<Mutex<MemoryData>>);

#[async_trait::async_trait]
impl StateStore for MemoryStore {
    fn state_tree(&self) -> Map<String, Value> {
        self.0.lock().unwrap().state.clone()
    }
    fn write_state(&self, state: &Map<String, Value>) -> Result<(), StateError> {
        self.0.lock().unwrap().state = state.clone();
        Ok(())
    }
    fn publish_resume_conditional(
        &self,
        expected: &Map<String, Value>,
        fields: &Map<String, Value>,
        definition: Option<&[u8]>,
        metadata: &Map<String, Value>,
    ) -> Result<(), String> {
        let mut data = self.0.lock().unwrap();
        if &data.state != expected {
            return Err("resume conflict: state changed since planning".into());
        }
        let mut next = data.state.clone();
        next.extend(fields.clone());
        if let Some(bytes) = definition {
            resume_write_failure("snapshot").map_err(|e| e.to_string())?;
            let id = secure_token_hex(16).map_err(|e| e.to_string())?;
            data.blobs
                .insert(format!("definition-revisions/{id}.json"), bytes.to_vec());
            let mut entry = metadata.clone();
            entry.insert("id".into(), Value::String(id.clone()));
            entry.insert(
                "predecessor".into(),
                next.get("definition_revision")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            next.entry("definition_history")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or("invalid history")?
                .push(Value::Object(entry));
            next.insert("definition_revision".into(), Value::String(id));
        }
        resume_write_failure("metadata").map_err(|e| e.to_string())?;
        data.state = next;
        Ok(())
    }
    fn initialize_definition(&self, bytes: &[u8]) -> Result<(), String> {
        self.publish_resume_conditional(&self.state_tree(), &Map::new(), Some(bytes), &Map::new())
    }
    fn recover_resume(&self) -> Result<bool, String> {
        Ok(false)
    }
    fn open(&self, name: &str, mode: BlobMode) -> Result<Box<dyn StateBlob>, StateError> {
        if !matches!(mode, BlobMode::Read) {
            return Err(StateError::Other("read-only test blob".into()));
        }
        let bytes = self
            .0
            .lock()
            .unwrap()
            .blobs
            .get(name)
            .cloned()
            .ok_or_else(|| StateError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)))?;
        Ok(Box::new(Cursor::new(bytes)))
    }
    fn exists(&self, name: &str) -> bool {
        self.0.lock().unwrap().blobs.contains_key(name)
    }
    fn read_field(&self, field: &str) -> Option<Value> {
        self.state_tree()
            .get(field)
            .filter(|v| !v.is_null())
            .cloned()
    }
    fn get_field(&self, field: &str) -> Option<Value> {
        self.read_field(field)
    }
    fn read_str(&self, field: &str) -> String {
        self.read_field(field)
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
    fn patch(&self, delete: &[String], fields: &Map<String, Value>) {
        let mut data = self.0.lock().unwrap();
        for key in delete {
            data.state.remove(key);
        }
        data.state.extend(fields.clone());
    }
    fn clear_finished(&self) -> Result<(), String> {
        self.0.lock().unwrap().blobs.remove("finished");
        Ok(())
    }
    fn clear_stage_error(&self, _: &str) {
        // Bail bookkeeping is outside this publication-only backend.
    }
    fn stage_error(&self) -> Option<Map<String, Value>> {
        None
    }
    fn record_stage_error(&self, _: &str, _: &str) {}
    fn accumulate_token_usage(&self, _: &HashMap<String, i64>) {}
    fn write_terminal_state(&self, _: i32) {}
    fn add_subprocess_cost(&self, _: f64) {}
    fn patch_parallel_attempt(&self, _: &str, _: &str) {}
    async fn data_uri(&self, key: &str) -> Result<String, MissingArtifact> {
        Err(MissingArtifact { key: key.into() })
    }
    async fn content(
        &self,
        _: &str,
        _: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        Err("unsupported".into())
    }
    async fn is_registered(&self, _: &str) -> bool {
        false
    }
    async fn path_for_uri(&self, _: &Uri) -> Result<String, Box<dyn std::error::Error>> {
        Err("unsupported".into())
    }
    async fn commit(&self, _: &str, _: &str) -> Result<(), Box<dyn std::error::Error>> {
        Err("unsupported".into())
    }
    async fn write_into_registry(
        &self,
        _: &Uri,
        _: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        Err("unsupported".into())
    }
    async fn keys(&self) -> Vec<String> {
        Vec::new()
    }
    async fn fork(
        &self,
        _: &str,
    ) -> Result<Box<dyn StateStore + Send + Sync>, Box<dyn std::error::Error>> {
        Err("unsupported".into())
    }
}

fn read_definition(state: &StateData) -> Vec<u8> {
    let mut bytes = Vec::new();
    state
        .open_blob("definition.json", BlobMode::Read)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

struct Registration(String);
impl Drop for Registration {
    fn drop(&mut self) {
        StateData::unregister_backend_resolver(&self.0);
    }
}

#[test]
fn publication_contract_for_filesystem_and_memory() {
    for filesystem in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let id = format!("publication-contract-{}", secure_token_hex(8).unwrap());
        let resolver_id = id.clone();
        let memory = MemoryStore::default();
        if filesystem {
            std::fs::write(dir.path().join("state.json"), "{}").unwrap();
            let root = dir.path().to_path_buf();
            StateData::register_backend_resolver(&id, move |candidate| {
                (candidate == resolver_id).then(|| {
                    Ok(Box::new(FileSystemStateStore::from_path(root.clone()))
                        as Box<dyn StateStore + Send + Sync>)
                })
            });
        } else {
            let backend = memory.clone();
            StateData::register_backend_resolver(&id, move |candidate| {
                (candidate == resolver_id)
                    .then(|| Ok(Box::new(backend.clone()) as Box<dyn StateStore + Send + Sync>))
            });
        }
        let _registration = Registration(id.clone());
        let state = StateData::open(&id).unwrap();
        state.initialize_definition(b"initial snapshot").unwrap();
        let initial = state.state_tree();
        for fault in ["snapshot", "metadata"] {
            set_resume_write_failure(Some(fault));
            let result = state.publish_resume_conditional(
                &initial,
                &Map::new(),
                Some(b"failed snapshot"),
                &Map::new(),
            );
            set_resume_write_failure(None);
            assert!(result.is_err(), "{filesystem}: {fault}");
            drop(state.store_handle());
            let restarted = StateData::open(&id).unwrap();
            assert_eq!(restarted.state_tree(), initial);
            assert_eq!(read_definition(&restarted), b"initial snapshot");
        }
        let barrier = Arc::new(Barrier::new(3));
        let writers: Vec<_> = (0..2)
            .map(|index| {
                let store = state.store_handle();
                let expected = initial.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .publish_resume_conditional(
                            &expected,
                            &Map::new(),
                            Some(format!("winner {index}").as_bytes()),
                            &Map::new(),
                        )
                        .is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            writers
                .into_iter()
                .map(|thread| usize::from(thread.join().unwrap()))
                .sum::<usize>(),
            1
        );
        let committed = state.state_tree();
        let revision = committed["definition_revision"].as_str().unwrap();
        let bytes = state.read_definition_revision(revision).unwrap();
        // The mutable compatibility alias is deliberately stale on restart.
        if filesystem {
            std::fs::write(dir.path().join("definition.json"), b"stale alias").unwrap();
        } else {
            memory
                .0
                .lock()
                .unwrap()
                .blobs
                .insert("definition.json".into(), b"stale alias".to_vec());
        }
        drop(state);
        let restarted = StateData::open(&id).unwrap();
        assert_eq!(restarted.state_tree(), committed);
        assert_eq!(read_definition(&restarted), bytes);
        assert!(!restarted.store_handle().recover_resume().unwrap());
        assert_eq!(restarted.state_tree(), committed);
    }
}

#[test]
fn ordinary_and_rewind_publications_conflict_in_both_orders() {
    crate::test_support::with_sandbox(None, |_| {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for rewind_first in [false, true] {
                let id = format!("publication-order-{rewind_first}");
                let initial = serde_json::json!({"id": id, "attempt": 1});
                let store =
                    FileSystemStateStore::create(&id, initial.as_object().unwrap(), false).unwrap();
                store.initialize_definition(b"A").unwrap();
                let expected = store.state_tree();
                let mut child_fields = expected.clone();
                child_fields.insert("attempt".into(), Value::from(2));
                if rewind_first {
                    let child = store
                        .fork_to_stage_conditional(&expected, &[], &child_fields, Some(b"B"))
                        .await
                        .unwrap();
                    assert_eq!(store.state_tree(), expected);
                    let result = store.publish_resume_conditional(
                        &expected,
                        &Map::new(),
                        Some(b"C"),
                        &Map::new(),
                    );
                    assert!(result.unwrap_err().contains("newer attempt"));
                    assert_eq!(store.state_tree(), expected);
                    let child_state = child.state_tree();
                    assert_eq!(
                        child_state["definition_history"].as_array().unwrap().len(),
                        2
                    );
                    assert_eq!(
                        child_state["definition_history"][1]["predecessor"],
                        expected["definition_revision"]
                    );
                    assert_eq!(
                        child
                            .read_definition_revision(
                                expected["definition_revision"].as_str().unwrap()
                            )
                            .unwrap(),
                        b"A"
                    );
                } else {
                    store
                        .publish_resume_conditional(&expected, &Map::new(), Some(b"C"), &Map::new())
                        .unwrap();
                    let committed = store.state_tree();
                    let result = store
                        .fork_to_stage_conditional(&expected, &[], &child_fields, Some(b"B"))
                        .await;
                    assert!(result.unwrap_err().to_string().contains("conflict"));
                    assert_eq!(store.state_tree(), committed);
                    assert!(!attempt_dir_path(&id, 2).exists());
                }
            }
        });
    });
}

#[test]
fn first_legacy_rewind_archives_parent_provenance_without_mutation() {
    crate::test_support::with_sandbox(None, |_| {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let id = "publication-legacy-provenance";
            let initial =
                serde_json::json!({"id": id, "attempt": 1, "model_override": "profile/parent"});
            let store =
                FileSystemStateStore::create(id, initial.as_object().unwrap(), false).unwrap();
            let parent_definition = br#"{"default_model":"cmd/parent"}"#;
            std::fs::write(store.root.path().join("definition.json"), parent_definition).unwrap();
            let expected = store.state_tree();
            let mut fields = expected.clone();
            fields.insert("attempt".into(), Value::from(2));
            fields.insert("model_override".into(), Value::from("profile/child"));
            let child = store
                .fork_to_stage_conditional(
                    &expected,
                    &[],
                    &fields,
                    Some(br#"{"default_model":"cmd/child"}"#),
                )
                .await
                .unwrap();
            assert_eq!(store.state_tree(), expected);
            assert_eq!(
                std::fs::read(store.root.path().join("definition.json")).unwrap(),
                parent_definition
            );
            assert!(!store.root.path().join("definition-revisions").exists());
            let state = child.state_tree();
            let history = state["definition_history"].as_array().unwrap();
            assert_eq!(history.len(), 2);
            assert_eq!(history[0]["attempt"], 1);
            assert_eq!(history[0]["requested_selection"], "profile/parent");
            assert_eq!(history[0]["predecessor"], Value::Null);
            assert_eq!(history[1]["attempt"], 2);
            assert_eq!(history[1]["requested_selection"], "profile/child");
            assert_eq!(history[1]["predecessor"], history[0]["id"]);
            assert_eq!(
                child
                    .read_definition_revision(history[0]["id"].as_str().unwrap())
                    .unwrap(),
                parent_definition
            );
        });
    });
}

#[test]
fn filesystem_stage_fork_rejects_stale_plans() {
    crate::test_support::with_sandbox(None, |_| {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let id = "publication-stale-fork";
            let mut initial = Map::new();
            initial.insert("id".into(), Value::String(id.into()));
            let store = FileSystemStateStore::create(id, &initial, false).unwrap();
            let expected = store.state_tree();
            let mut changed = expected.clone();
            changed.insert("status".into(), Value::String("changed".into()));
            store.write_state(&changed).unwrap();
            let result = store
                .fork_to_stage_conditional(&expected, &[], &expected, None)
                .await;
            assert!(result.unwrap_err().to_string().contains("conflict"));
            assert_eq!(store.state_tree(), changed);
            assert!(!attempt_dir_path(id, 2).exists());
            store
                .fork_to_stage_conditional(&changed, &[], &changed, None)
                .await
                .unwrap();
            // Even unchanged parent state cannot publish a second successor.
            let result = store
                .fork_to_stage_conditional(&changed, &[], &changed, None)
                .await;
            assert!(result.unwrap_err().to_string().contains("conflict"));
        });
    });
}
