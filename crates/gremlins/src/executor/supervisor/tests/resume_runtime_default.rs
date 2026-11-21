//! Resume must accept snapshots whose default comes only from runtime config.
#![allow(clippy::await_holding_lock)]

use super::{spawn_test_actor, TestRegistry};
use crate::config;
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::supervisor::handle_resume;
use crate::test_support::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ID: &str = "gr-resume-runtime-default";
const CONFIGURED: &str = "cmd/true";
const REPLACEMENT: &str = "cmd/echo replacement";

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn seed(sandbox: &Sandbox) -> PathBuf {
    let project = sandbox.join("project");
    fs::create_dir_all(project.join(".gremlins")).unwrap();
    let source = project.join(".gremlins/demo.json5");
    fs::write(&source, "not a definition").unwrap();
    let dir = sandbox.join(format!("state/{ID}-0001"));
    fs::create_dir_all(dir.join("artifacts")).unwrap();
    fs::write(
        dir.join("definition.json"),
        serde_json::to_vec(&json!({
            "__gremlins_expanded__": true,
            "default_task_models": { "Scout": "openai/original-scout" },
            "stages": [
                { "name": "pinned", "type": "exec", "model": "cmd/true",
                  "options": { "cmds": ["true"] } },
                { "name": "run", "type": "exec", "options": { "cmds": ["true"] } }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    state::write_state(
        &dir,
        json!({
            "id": ID, "kind": "demo", "project_root": project,
            "definition_path": source, "workdir": "", "status": "stopped",
            "stage": "run", "attempt": "original", "stage_inputs": {},
            "metadata": { "acceptance": "keep" }
        })
        .as_object()
        .unwrap(),
    )
    .unwrap();
    fs::write(dir.join("finished"), "original").unwrap();
    dir
}

async fn drained(registry: &TestRegistry) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.contains_run(ID).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("resumed worker did not finish");
}

async fn assert_runtime_default_replaced(stage: Option<&str>) {
    let sandbox = Sandbox::with_config(Some("default-model: cmd/true\n"));
    config::init_global().unwrap();
    let parent = seed(&sandbox);
    assert!(read_json(parent.join("definition.json"))
        .get("default_model")
        .is_none());
    assert!(read_json(parent.join("state.json"))
        .get("model_override")
        .is_none());

    let mut original = Gremlin::from(ID).unwrap();
    assert!(original.model_override.is_none());
    original.init_runtime(None).await.unwrap();
    assert_eq!(original.definition.default_model().to_string(), CONFIGURED);
    assert_eq!(original.state.read_str("model"), CONFIGURED);
    drop(original);
    let parent_state = fs::read(parent.join("state.json")).unwrap();
    let parent_definition = fs::read(parent.join("definition.json")).unwrap();
    assert!(read_json(parent.join("definition.json"))
        .get("default_model")
        .is_none());
    assert!(read_json(parent.join("state.json"))
        .get("model_override")
        .is_none());

    let (registry, _shutdown) = spawn_test_actor().await;
    let mut request = json!({ "id": ID, "model": REPLACEMENT });
    if let Some(stage) = stage {
        request["stage"] = json!(stage);
    }
    let response = handle_resume(&request, &registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;

    let mut worker = Gremlin::from(ID).unwrap();
    let resumed = worker.state.state_dir();
    let persisted = read_json(resumed.join("state.json"));
    assert_eq!(persisted["model"], REPLACEMENT);
    assert_eq!(persisted["model_override"], REPLACEMENT);
    assert_eq!(persisted["status"], "done");
    assert_eq!(persisted["metadata"]["acceptance"], "keep");
    let mut expected: Value = serde_json::from_slice(&parent_definition).unwrap();
    expected["default_model"] = json!(REPLACEMENT);
    assert_eq!(read_json(resumed.join("definition.json")), expected);
    assert_eq!(worker.model_override.as_deref(), Some(REPLACEMENT));
    worker.init_runtime(None).await.unwrap();
    assert_eq!(worker.definition.default_model().to_string(), REPLACEMENT);
    let pinned = worker.definition.next_stage().await.unwrap();
    assert_eq!(pinned.model().unwrap().to_string(), CONFIGURED);
    let unpinned = worker.definition.next_stage().await.unwrap();
    assert_eq!(unpinned.name(), "run");
    assert!(unpinned.model().is_none());
    assert_eq!(
        crate::executor::run::resolve_client(&unpinned, &worker, None)
            .unwrap()
            .to_string(),
        REPLACEMENT
    );

    let attempts = fs::read_dir(sandbox.join("state")).unwrap().count();
    if stage.is_some() {
        assert_ne!(resumed, parent);
        assert_eq!(attempts, 2);
        assert_eq!(fs::read(parent.join("state.json")).unwrap(), parent_state);
        assert_eq!(
            fs::read(parent.join("definition.json")).unwrap(),
            parent_definition
        );
        assert_eq!(
            fs::read_to_string(parent.join("finished")).unwrap(),
            "original"
        );
    } else {
        assert_eq!(resumed, parent);
        assert_eq!(attempts, 1, "ordinary resume must not fork");
    }
}

#[tokio::test]
async fn resume_model_replaces_runtime_config_default_without_snapshot_or_state_override() {
    assert_runtime_default_replaced(None).await;
}

#[tokio::test]
async fn resume_stage_model_replaces_runtime_config_default_and_preserves_parent() {
    assert_runtime_default_replaced(Some("run")).await;
}
