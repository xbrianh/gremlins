//! Publication-boundary coverage using the directoryless backend.
//! Included as a child of `publication_tests` so its backend stays test-private.
use super::*;
use crate::executor::supervisor::{handle_resume, run_registry::RunRegistry};
use serde_json::json;

fn registered_memory() -> (String, MemoryStore, Registration) {
    let id = format!("gr-boundary-{}", secure_token_hex(8).unwrap());
    let memory = MemoryStore::default();
    memory
        .write_state(
            json!({
                "id": id, "kind": "boundary", "status": "stopped",
                "stage": "work", "attempt": "original", "stage_inputs": {},
                "ended_at": "original-end", "exit_code": 17,
                "model": "cmd/true", "model_override": "cmd/true",
                "model_selection_profile": ""
            })
            .as_object()
            .unwrap(),
        )
        .unwrap();
    let backend = memory.clone();
    let candidate_id = id.clone();
    StateData::register_backend_resolver(&id, move |candidate| {
        (candidate == candidate_id)
            .then(|| Ok(Box::new(backend.clone()) as Box<dyn StateStore + Send + Sync>))
    });
    let registration = Registration(id.clone());
    let state = StateData::open(&id).unwrap();
    state
        .initialize_definition(
            &serde_json::to_vec(&json!({
                "__gremlins_expanded__": true,
                "default_model": "cmd/true",
                "stages": [{"name": "work", "type": "agent", "prompt": ["Do not dispatch."]}]
            }))
            .unwrap(),
        )
        .unwrap();
    memory
        .0
        .lock()
        .unwrap()
        .blobs
        .insert("finished".into(), b"original".to_vec());
    (id, memory, registration)
}

struct ResetFailure;
impl Drop for ResetFailure {
    fn drop(&mut self) {
        set_resume_write_failure(None);
    }
}

// A current-thread runtime keeps the thread-local publication fault on the
// thread executing handle_resume. No provider or worker should be reached.
#[test]
fn handle_resume_memory_invalid_profile_and_publication_failure_do_not_dispatch() {
    crate::test_support::with_sandbox(None, |_| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (id, memory, _registration) = registered_memory();
                let state = StateData::open(&id).unwrap();
                assert!(
                    !state.state_dir().exists(),
                    "must exercise a directoryless store"
                );
                let original = state.state_tree();
                let definition = read_definition(&state);
                let revision = original["definition_revision"].as_str().unwrap();
                let (shutdown, _receiver) = tokio::sync::watch::channel(false);
                let registry = Arc::new(RunRegistry::new(shutdown));
                for (model, fault, message) in [
                    ("profile/nonexistent-boundary-profile", None, "profile"),
                    (
                        "cmd/true",
                        Some("snapshot"),
                        "failed to publish resume state",
                    ),
                    (
                        "cmd/true",
                        Some("metadata"),
                        "failed to publish resume state",
                    ),
                    (
                        "cmd/true",
                        Some("terminal-marker"),
                        "failed to publish resume state",
                    ),
                ] {
                    set_resume_write_failure(fault);
                    let reset = ResetFailure;
                    let response =
                        handle_resume(&json!({"id": id, "model": model}), &registry).await;
                    drop(reset);
                    assert_eq!(response["type"], "error", "{response}");
                    assert!(response.to_string().contains(message), "{response}");
                    assert!(
                        !registry.contains_run(&id).await,
                        "failure dispatched a worker"
                    );
                    tokio::task::yield_now().await;
                    assert!(!registry.contains_run(&id).await);
                    let reopened = StateData::open(&id).unwrap();
                    assert_eq!(reopened.state_tree(), original);
                    assert_eq!(read_definition(&reopened), definition);
                    assert_eq!(
                        reopened.read_definition_revision(revision).unwrap(),
                        definition
                    );
                    assert_eq!(memory.0.lock().unwrap().blobs["finished"], b"original");
                }
            });
    });
}

#[test]
fn handle_resume_filesystem_postcommit_failures_do_not_dispatch() {
    crate::test_support::with_sandbox(None, |_| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                for fault in ["commit-sync", "terminal-marker", "terminal-marker-sync"] {
                    let id = format!("gr-commit-sync-{}", secure_token_hex(8).unwrap());
                    let initial = json!({
                        "id": id, "kind": "boundary", "status": "stopped",
                        "stage": "work", "attempt": 1, "stage_inputs": {},
                        "ended_at": "original-end", "exit_code": 17,
                        "model": "cmd/true", "model_override": "cmd/true",
                        "model_selection_profile": ""
                    });
                    let store =
                        FileSystemStateStore::create(&id, initial.as_object().unwrap(), false)
                            .unwrap();
                    store.initialize_definition(&serde_json::to_vec(&json!({
                    "__gremlins_expanded__": true,
                    "default_model": "cmd/true",
                    "stages": [{"name": "work", "type": "agent", "prompt": ["Do not dispatch."]}]
                })).unwrap()).unwrap();
                    std::fs::write(store.root_dir().join("finished"), b"original").unwrap();
                    let original = store.state_tree();
                    let (shutdown, _receiver) = tokio::sync::watch::channel(false);
                    let registry = Arc::new(RunRegistry::new(shutdown));
                    set_resume_write_failure(Some(fault));
                    let reset = ResetFailure;
                    let response =
                        handle_resume(&json!({"id": id, "model": "cmd/true"}), &registry).await;
                    drop(reset);
                    assert_eq!(response["type"], "error", "{response}");
                    let message = if fault == "commit-sync" {
                        "indeterminate"
                    } else {
                        "committed"
                    };
                    assert!(response.to_string().contains(message), "{response}");
                    assert!(store
                        .root_dir()
                        .join(".resume-model-recovery.json")
                        .exists());
                    assert!(
                        !registry.contains_run(&id).await,
                        "failed publication dispatched a worker"
                    );
                    tokio::task::yield_now().await;
                    assert!(!registry.contains_run(&id).await);
                    // Recovery preserves the selected revision without dispatching work.
                    let reopened = StateData::open(&id).unwrap();
                    assert!(reopened.store_handle().recover_resume().unwrap());
                    let committed = reopened.state_tree();
                    assert_ne!(
                        committed["definition_revision"],
                        original["definition_revision"]
                    );
                    let revision = committed["definition_revision"].as_str().unwrap();
                    assert_eq!(
                        read_definition(&reopened),
                        reopened.read_definition_revision(revision).unwrap()
                    );
                    assert!(!reopened.exists("finished"));
                    assert!(!registry.contains_run(&id).await);
                }
            });
    });
}

#[test]
fn memory_readers_resolve_committed_revisions_during_publication() {
    let (id, memory, _registration) = registered_memory();
    let state = StateData::open(&id).unwrap();
    let original = state.state_tree();
    let predecessor = original["definition_revision"].as_str().unwrap().to_owned();
    let original_bytes = read_definition(&state);
    let barrier = Arc::new(Barrier::new(2));
    let writer_barrier = barrier.clone();
    let writer = memory.clone();
    let thread = std::thread::spawn(move || {
        writer_barrier.wait();
        for index in 0..100 {
            let expected = writer.state_tree();
            let bytes = format!("snapshot {index}");
            let fields = json!({"snapshot_number": index})
                .as_object()
                .unwrap()
                .clone();
            writer
                .publish_resume_conditional(&expected, &fields, Some(bytes.as_bytes()), &Map::new())
                .unwrap();
            std::thread::yield_now();
        }
    });
    barrier.wait();
    // Resolve the revision selected by each captured metadata snapshot, not a
    // mutable alias that can legitimately advance between two separate reads.
    for _ in 0..200 {
        let snapshot = state.state_tree();
        let revision = snapshot["definition_revision"].as_str().unwrap();
        let bytes = state.read_definition_revision(revision).unwrap();
        match snapshot.get("snapshot_number") {
            Some(index) => assert_eq!(
                bytes,
                format!("snapshot {}", index.as_u64().unwrap()).as_bytes()
            ),
            None => assert_eq!(bytes, original_bytes),
        }
        std::thread::yield_now();
    }
    thread.join().unwrap();
    assert_eq!(
        state.read_definition_revision(&predecessor).unwrap(),
        original_bytes
    );
    assert_eq!(
        state.state_tree()["definition_history"]
            .as_array()
            .unwrap()
            .len(),
        101
    );
}

#[test]
fn memory_competition_and_failed_successor_retain_predecessors() {
    let (id, memory, _registration) = registered_memory();
    let state = StateData::open(&id).unwrap();
    let initial = state.state_tree();
    let initial_revision = initial["definition_revision"].as_str().unwrap().to_owned();
    let initial_bytes = read_definition(&state);
    let barrier = Arc::new(Barrier::new(3));
    let writers: Vec<_> = (0..2)
        .map(|index| {
            let memory = memory.clone();
            let expected = initial.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                memory.publish_resume_conditional(
                    &expected,
                    &Map::new(),
                    Some(format!("winner {index}").as_bytes()),
                    &Map::new(),
                )
            })
        })
        .collect();
    barrier.wait();
    let outcomes: Vec<_> = writers
        .into_iter()
        .map(|writer| writer.join().unwrap())
        .collect();
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(outcomes
        .iter()
        .filter_map(|result| result.as_ref().err())
        .all(|error| error.contains("conflict")));
    let committed = state.state_tree();
    let history = committed["definition_history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1]["predecessor"], initial_revision);
    let winner = read_definition(&state);
    set_resume_write_failure(Some("metadata"));
    let reset = ResetFailure;
    let failure = memory.publish_resume_conditional(
        &committed,
        &Map::new(),
        Some(b"uncommitted successor"),
        &Map::new(),
    );
    drop(reset);
    assert!(failure.is_err());
    let reopened = StateData::open(&id).unwrap();
    assert_eq!(reopened.state_tree(), committed);
    assert_eq!(read_definition(&reopened), winner);
    assert_eq!(
        reopened
            .read_definition_revision(&initial_revision)
            .unwrap(),
        initial_bytes
    );
    assert_eq!(
        reopened
            .read_definition_revision(committed["definition_revision"].as_str().unwrap())
            .unwrap(),
        winner
    );
}
