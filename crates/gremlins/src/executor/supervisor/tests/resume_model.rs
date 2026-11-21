//! Resume model overrides must change the persisted program, not just a live handle.
#![allow(clippy::await_holding_lock)]

use super::{spawn_test_actor, TestRegistry};
use crate::config;
use crate::definition::{GremlinDefinition, StaticDefinition};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::supervisor::handle_resume;
use crate::test_support::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ID: &str = "gr-resume-model";
const SOURCE: &str = r#"{
  default_model: "cmd/true",
  default_task_models: { Scout: "openai/original-scout" },
  stages: [
    { name: "pinned", type: "exec", model: "cmd/true", options: { cmds: ["true"] },
      "task-models": { Implement: "openai/original-implement" } },
    { name: "run", type: "exec", options: { cmds: ["true"] } }
  ]
}"#;

fn seed(sandbox: &Sandbox) -> PathBuf {
    let project = sandbox.join("project");
    fs::create_dir_all(project.join(".gremlins")).unwrap();
    let source = project.join(".gremlins/demo.json5");
    fs::write(&source, SOURCE).unwrap();
    let definition = StaticDefinition::from_json5_file(&source, None, None).unwrap();
    let dir = sandbox.join(format!("state/{ID}-0001"));
    fs::create_dir_all(dir.join("artifacts")).unwrap();
    fs::write(dir.join("definition.json"), definition.serialize().unwrap()).unwrap();
    state::write_state(
        &dir,
        json!({
            "id": ID, "kind": "demo", "project_root": project,
            "definition_path": source, "workdir": "", "status": "stopped",
            "stage": "run", "attempt": "original", "model_override": "cmd/true",
            "stage_inputs": {}, "metadata": { "acceptance": "keep" }
        })
        .as_object()
        .unwrap(),
    )
    .unwrap();
    fs::write(dir.join("finished"), "original").unwrap();
    dir
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn entries(sandbox: &Sandbox) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(sandbox.join("state"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
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

async fn assert_rejected_unchanged(sandbox: &Sandbox, dir: &Path, request: Value) {
    let before_state = fs::read(dir.join("state.json")).unwrap();
    let before_definition = fs::read(dir.join("definition.json")).ok();
    let before_entries = entries(sandbox);
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_resume(&request, &registry).await;
    assert_eq!(response["type"], "error", "{response}");
    assert!(
        !registry.contains_run(ID).await,
        "failed resume launched a worker"
    );
    assert_eq!(fs::read(dir.join("state.json")).unwrap(), before_state);
    assert_eq!(
        fs::read(dir.join("definition.json")).ok(),
        before_definition
    );
    assert_eq!(
        entries(sandbox),
        before_entries,
        "failed resume published a fork"
    );
    assert!(
        dir.join("finished").exists(),
        "failed resume removed terminal marker"
    );
}

async fn assert_unpinned_resolves(worker: &mut Gremlin, concrete: &str) {
    let pinned = worker.definition.next_stage().await.unwrap();
    assert!(pinned.model().is_some());
    let stage = worker.definition.next_stage().await.unwrap();
    assert_eq!(stage.name(), "run");
    assert!(stage.model().is_none());
    assert_eq!(
        crate::executor::run::resolve_client(&stage, worker, None)
            .unwrap()
            .to_string(),
        crate::model_uri::ModelURI::parse(concrete)
            .unwrap()
            .to_string()
    );
}

async fn resume_again_unchanged(
    sandbox: &Sandbox,
    dir: &Path,
    registry: &TestRegistry,
    concrete: &str,
) {
    let snapshot = fs::read(dir.join("definition.json")).unwrap();
    let mut data = read_json(dir.join("state.json"));
    data["status"] = json!("stopped");
    state::write_state(dir, data.as_object().unwrap()).unwrap();
    let response = handle_resume(&json!({"id": ID}), registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(registry).await;
    assert_eq!(fs::read(dir.join("definition.json")).unwrap(), snapshot);
    assert_eq!(
        read_json(dir.join("state.json"))["model_override"],
        concrete
    );
    let mut worker = Gremlin::from(ID).unwrap();
    assert_eq!(worker.model_override.as_deref(), Some(concrete));
    worker.init_runtime(None).await.unwrap();
    assert_eq!(
        worker.definition.default_model().to_string(),
        crate::model_uri::ModelURI::parse(concrete)
            .unwrap()
            .to_string()
    );
    assert_unpinned_resolves(&mut worker, concrete).await;
    assert_eq!(entries(sandbox).len(), 1, "flagless resume must not fork");
}

#[tokio::test]
async fn resume_model_persists_and_normal_worker_load_ignores_changed_source() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let before = read_json(dir.join("definition.json"));
    fs::write(
        sandbox.join("project/.gremlins/demo.json5"),
        "not a definition",
    )
    .unwrap();
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_resume(
        &json!({"id": ID, "model": "cmd/echo replacement"}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    let mut expected = before;
    expected["default_model"] = json!("cmd/echo replacement");
    assert_eq!(read_json(dir.join("definition.json")), expected);
    let mut worker = Gremlin::from(ID).unwrap();
    assert_eq!(
        worker.model_override.as_deref(),
        Some("cmd/echo replacement")
    );
    worker.init_runtime(None).await.unwrap();
    assert_eq!(
        worker.definition.default_model().to_string(),
        "cmd/echo replacement"
    );
    assert_eq!(worker.state.read_str("status"), "done");
    assert_eq!(
        read_json(dir.join("state.json"))["metadata"]["acceptance"],
        "keep"
    );
    assert_eq!(entries(&sandbox).len(), 1, "ordinary resume must not fork");
    assert_unpinned_resolves(&mut worker, "cmd/echo replacement").await;

    fs::create_dir_all(sandbox.join("config")).unwrap();
    fs::write(
        sandbox.join("config/settings.yaml"),
        "default-model: cmd/false\n",
    )
    .unwrap();
    config::init_global().unwrap();
    resume_again_unchanged(&sandbox, &dir, &registry, "cmd/echo replacement").await;
}

#[tokio::test]
async fn resume_profile_pins_options_without_importing_profile_task_or_stage_mappings() {
    let sandbox = Sandbox::with_config(Some(
        r#"
model-profiles:
  replacement:
    model: 'openai/replacement?temperature=0.2,reasoning_effort=high'
    task-models:
      Scout: openai/unwanted-scout
    model-by-stage:
      pinned: openai/unwanted-stage
"#,
    ));
    let dir = seed(&sandbox);
    let mut expected = read_json(dir.join("definition.json"));
    let concrete = "openai/replacement?temperature=0.2,reasoning_effort=high";
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_resume(
        &json!({"id": ID, "model": "profile/replacement"}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    expected["default_model"] = json!(concrete);
    assert_eq!(read_json(dir.join("definition.json")), expected);
    assert_eq!(
        read_json(dir.join("state.json"))["model_override"],
        concrete
    );
    fs::write(
        sandbox.join("config/settings.yaml"),
        "model-profiles:\n  replacement:\n    model: cmd/false\n",
    )
    .unwrap();
    config::init_global().unwrap();
    resume_again_unchanged(&sandbox, &dir, &registry, concrete).await;
    let mut worker = Gremlin::from(ID).unwrap();
    assert_eq!(worker.model_override.as_deref(), Some(concrete));
    worker.init_runtime(None).await.unwrap();
    assert_eq!(
        worker.definition.default_model().to_string(),
        crate::model_uri::ModelURI::parse(concrete)
            .unwrap()
            .to_string()
    );
    let pinned = worker.definition.next_stage().await.unwrap();
    assert_eq!(pinned.model().unwrap().to_string(), "cmd/true");
    assert_eq!(
        pinned.task_models().unwrap()["Implement"],
        "openai/original-implement"
    );
    assert_eq!(read_json(dir.join("definition.json")), expected);
    let unpinned = worker.definition.next_stage().await.unwrap();
    assert!(unpinned.model().is_none());
    assert_eq!(
        crate::executor::run::resolve_client(&unpinned, &worker, None)
            .unwrap()
            .to_string(),
        crate::model_uri::ModelURI::parse(concrete)
            .unwrap()
            .to_string()
    );
}

#[tokio::test]
async fn resume_stage_model_changes_only_fork_and_preserves_parent_bytes() {
    let sandbox = Sandbox::new();
    let parent = seed(&sandbox);
    let parent_state = fs::read(parent.join("state.json")).unwrap();
    let parent_definition = fs::read(parent.join("definition.json")).unwrap();
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_resume(
        &json!({"id": ID, "stage": "run", "model": "cmd/echo fork"}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    assert_eq!(fs::read(parent.join("state.json")).unwrap(), parent_state);
    assert_eq!(
        fs::read(parent.join("definition.json")).unwrap(),
        parent_definition
    );
    assert!(parent.join("finished").exists());
    let mut worker = Gremlin::from(ID).unwrap();
    assert_ne!(worker.state.state_dir(), parent);
    assert_eq!(worker.model_override.as_deref(), Some("cmd/echo fork"));
    let mut expected: Value = serde_json::from_slice(&parent_definition).unwrap();
    expected["default_model"] = json!("cmd/echo fork");
    assert_eq!(
        read_json(worker.state.state_dir().join("definition.json")),
        expected
    );
    worker.init_runtime(None).await.unwrap();
    assert_eq!(
        worker.definition.default_model().to_string(),
        "cmd/echo fork"
    );
    assert_eq!(entries(&sandbox).len(), 2);
}

#[tokio::test]
async fn resume_invalid_models_and_unknown_stages_are_side_effect_free() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    for model in [
        "",
        "not-a-model",
        "profile/missing",
        "model:/openai/oops",
        "openai/test?bad key=x",
    ] {
        for stage in [None, Some("run")] {
            let mut request = json!({"id": ID, "model": model});
            if let Some(stage) = stage {
                request["stage"] = json!(stage);
            }
            assert_rejected_unchanged(&sandbox, &dir, request).await;
        }
    }
    assert_rejected_unchanged(
        &sandbox,
        &dir,
        json!({"id": ID, "stage": "missing", "model": "cmd/echo new"}),
    )
    .await;
}

#[tokio::test]
async fn resume_ineligible_states_do_not_change_model_or_publish_attempt() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    for status in ["running", "done", "starting", "unknown"] {
        let mut data = read_json(dir.join("state.json"));
        data["status"] = json!(status);
        state::write_state(&dir, data.as_object().unwrap()).unwrap();
        assert_rejected_unchanged(&sandbox, &dir, json!({"id": ID, "model": "cmd/echo new"})).await;
    }
    let mut data = read_json(dir.join("state.json"));
    data["status"] = json!("stopped");
    data["stage"] = json!("starting");
    state::write_state(&dir, data.as_object().unwrap()).unwrap();
    assert_rejected_unchanged(&sandbox, &dir, json!({"id": ID, "model": "cmd/echo new"})).await;
}

#[tokio::test]
async fn resume_model_state_write_failure_does_not_replace_snapshot_or_start_worker() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    // A directory at the lock path reliably fails even when tests run as root.
    fs::remove_file(dir.join("state.json.lock")).unwrap();
    fs::create_dir(dir.join("state.json.lock")).unwrap();
    assert_rejected_unchanged(&sandbox, &dir, json!({"id": ID, "model": "cmd/echo new"})).await;
}

#[tokio::test]
async fn resume_model_missing_or_corrupt_snapshot_never_launches_or_forks() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    for bytes in [None, Some(b"not json".as_slice()), Some(b"{}".as_slice())] {
        match bytes {
            Some(bytes) => fs::write(dir.join("definition.json"), bytes).unwrap(),
            None => fs::remove_file(dir.join("definition.json")).unwrap(),
        }
        for stage in [None, Some("run")] {
            let mut request = json!({"id": ID, "model": "cmd/echo new"});
            if let Some(stage) = stage {
                request["stage"] = json!(stage);
            }
            assert_rejected_unchanged(&sandbox, &dir, request).await;
        }
    }
}

#[tokio::test]
async fn resume_without_model_does_not_read_broken_settings_or_change_snapshot() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let before = fs::read(dir.join("definition.json")).unwrap();
    fs::create_dir_all(sandbox.join("config")).unwrap();
    fs::write(
        sandbox.join("config/settings.yaml"),
        "model-profiles: [broken",
    )
    .unwrap();
    config::clear_global();
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_resume(&json!({"id": ID}), &registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    assert_eq!(fs::read(dir.join("definition.json")).unwrap(), before);
    assert_eq!(
        Gremlin::from(ID).unwrap().model_override.as_deref(),
        Some("cmd/true")
    );
}

#[tokio::test]
async fn resume_stage_publication_failure_leaves_no_partial_fork_or_model_change() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    // An occupied destination fails atomic publication after preparation, not validation.
    let occupied = sandbox.join(format!("state/{ID}-0002"));
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("occupied"), "keep").unwrap();
    assert_rejected_unchanged(
        &sandbox,
        &dir,
        json!({"id": ID, "stage": "run", "model": "cmd/echo new"}),
    )
    .await;
    assert_eq!(
        fs::read_to_string(occupied.join("occupied")).unwrap(),
        "keep"
    );
    assert!(!occupied.join("state.json").exists());
}

#[tokio::test]
async fn resume_completed_run_with_explicit_stage_and_model_is_eligible() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let mut data = read_json(dir.join("state.json"));
    data["status"] = json!("done");
    state::write_state(&dir, data.as_object().unwrap()).unwrap();
    let before = fs::read(dir.join("state.json")).unwrap();
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_resume(
        &json!({"id": ID, "stage": "run", "model": "cmd/echo completed"}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    assert_eq!(fs::read(dir.join("state.json")).unwrap(), before);
    assert_eq!(
        Gremlin::from(ID).unwrap().model_override.as_deref(),
        Some("cmd/echo completed")
    );
}

#[tokio::test]
async fn resume_model_configuration_failure_is_side_effect_free() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    fs::create_dir_all(sandbox.join("config")).unwrap();
    fs::write(
        sandbox.join("config/settings.yaml"),
        "model-profiles: [broken",
    )
    .unwrap();
    config::clear_global();
    for stage in [None, Some("run")] {
        let mut request = json!({"id": ID, "model": "profile/replacement"});
        if let Some(stage) = stage {
            request["stage"] = json!(stage);
        }
        assert_rejected_unchanged(&sandbox, &dir, request).await;
    }
}

#[tokio::test]
async fn resume_bailed_run_can_replace_default_model() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let mut data = read_json(dir.join("state.json"));
    data["status"] = Value::Null;
    state::write_state(&dir, data.as_object().unwrap()).unwrap();
    Gremlin::from(ID)
        .unwrap()
        .state
        .record_stage_error("other", "acceptance bail");
    let (registry, _shutdown) = spawn_test_actor().await;
    let response =
        handle_resume(&json!({"id": ID, "model": "cmd/echo recovered"}), &registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    assert_eq!(
        Gremlin::from(ID).unwrap().model_override.as_deref(),
        Some("cmd/echo recovered")
    );
    assert_eq!(
        read_json(dir.join("definition.json"))["default_model"],
        "cmd/echo recovered"
    );
}
