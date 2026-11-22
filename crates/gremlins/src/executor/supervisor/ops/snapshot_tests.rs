#![allow(clippy::await_holding_lock)]

use super::*;
use crate::executor::state::StateStore;
use crate::test_support::Sandbox;
use serde_json::json;
use std::fs;

#[tokio::test]
async fn snapshot_publishes_active_revision_and_preserves_history_and_terminal_state() {
    let sandbox = Sandbox::with_config(None);
    let source = sandbox.join("pipeline.json5");
    let original = r#"{default_model: "cmd/true", stages: [{name: "old", type: "exec", options: {cmds: ["true"]}}]}"#;
    fs::write(&source, original).unwrap();
    let bytes = StaticDefinition::from_json5_file(&source, None, None)
        .unwrap()
        .serialize()
        .unwrap();
    let dir = sandbox.join("state/gr-snapshot-0001");
    fs::create_dir_all(&dir).unwrap();
    let store = FileSystemStateStore::from_path(dir.clone());
    store
        .write_state(
            json!({
                "id": "gr-snapshot", "status": "done", "attempt": 1,
                "definition_path": source, "project_root": sandbox.path(),
                "stage": "old", "metadata": {"keep": true}
            })
            .as_object()
            .unwrap(),
        )
        .unwrap();
    // Start with a legacy alias; the first snapshot must archive it unchanged.
    fs::write(dir.join("definition.json"), &bytes).unwrap();
    fs::write(dir.join("finished"), "terminal").unwrap();
    let (tx, _rx) = tokio::sync::watch::channel(false);
    let registry = Arc::new(RunRegistry::new(tx));
    for (index, name) in ["new", "newer"].iter().enumerate() {
        fs::write(&source, original.replace("old", name)).unwrap();
        let expected_bytes = StaticDefinition::from_json5_file(&source, None, None)
            .unwrap()
            .serialize()
            .unwrap();
        let response = handle_snapshot(&json!({"id": "gr-snapshot"}), &registry).await;
        assert_eq!(response["type"], "ok", "{response}");
        let tree = store.state_tree();
        let history = tree["definition_history"].as_array().unwrap();
        assert_eq!(history.len(), index + 2);
        assert_eq!(tree["definition_revision"], history.last().unwrap()["id"]);
        assert_eq!(history.last().unwrap()["predecessor"], history[index]["id"]);
        assert_eq!(
            store
                .read_definition_revision(history[0]["id"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        assert_eq!(
            store
                .read_definition_revision(tree["definition_revision"].as_str().unwrap())
                .unwrap(),
            expected_bytes
        );
        assert_eq!(
            fs::read(dir.join("definition.json")).unwrap(),
            expected_bytes
        );
        assert_eq!(tree["status"], "done");
        assert_eq!(tree["attempt"], 1);
        assert_eq!(tree["metadata"], json!({"keep": true}));
        assert_eq!(
            fs::read_to_string(dir.join("finished")).unwrap(),
            "terminal"
        );
    }
}

#[test]
fn snapshot_rejects_stale_state_without_replacing_active_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSystemStateStore::from_path(dir.path().to_path_buf());
    store
        .write_state(
            json!({"status": "stopped", "attempt": 1})
                .as_object()
                .unwrap(),
        )
        .unwrap();
    store.initialize_definition(b"original").unwrap();
    let expected = store.state_tree();
    store.patch(&[], json!({"status": "running"}).as_object().unwrap());
    let changed = store.state_tree();
    assert!(store
        .publish_snapshot_conditional(&expected, b"replacement")
        .is_err());
    assert_eq!(store.state_tree(), changed);
    assert_eq!(
        fs::read(dir.path().join("definition.json")).unwrap(),
        b"original"
    );
    assert_eq!(
        store
            .read_definition_revision(changed["definition_revision"].as_str().unwrap())
            .unwrap(),
        b"original"
    );
}
