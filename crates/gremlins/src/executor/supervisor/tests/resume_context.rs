//! Persisted child programs retain scope and selection context across resume.
#![allow(clippy::await_holding_lock)]

use super::{spawn_test_actor, TestRegistry};
use crate::definition::ExecutorStage;
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::supervisor::{handle_launch, handle_resume};
use crate::test_support::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

pub(super) fn source(sandbox: &Sandbox, program: Value) -> PathBuf {
    let path = sandbox.join("project/.gremlins/context.json5");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&program).unwrap()).unwrap();
    path
}

pub(super) async fn drained(registry: &TestRegistry, id: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.contains_run(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker did not finish");
}

pub(super) async fn launch(registry: &TestRegistry, path: &Path, model: Option<&str>) -> String {
    let mut request =
        json!({"definition": path, "project_root": path.parent().unwrap().parent().unwrap()});
    if let Some(model) = model {
        request["model"] = json!(model);
    }
    let response = handle_launch(&request, registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    let id = response["id"].as_str().unwrap().to_owned();
    drained(registry, &id).await;
    id
}

async fn resume(registry: &TestRegistry, id: &str, model: Option<&str>) {
    let mut request = json!({"id": id});
    if let Some(model) = model {
        request["model"] = json!(model);
    }
    let response = handle_resume(&request, registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(registry, id).await;
}

pub(super) fn stopped(dir: &Path) {
    let mut data = read_json(dir.join("state.json"));
    data["status"] = json!("stopped");
    state::write_state(dir, data.as_object().unwrap()).unwrap();
}

fn bytes(dir: &Path) -> (Vec<u8>, Vec<u8>) {
    (
        fs::read(dir.join("state.json")).unwrap(),
        fs::read(dir.join("definition.json")).unwrap_or_else(|error| {
            panic!("missing definition snapshot in {}: {error}", dir.display())
        }),
    )
}

async fn child_context(explicit: bool) {
    let sandbox = Sandbox::new();
    let mut enclosing = json!({
        "name": "enclosing", "type": "sequence",
        "task-models": {"Scout": "openai/enclosing-scout"},
        "body": [{"name": "fanout", "type": "parallel",
            "task-models": {"Implement": "openai/parallel-implement"},
            "body": [
                {"name": "branch", "type": "exec", "options": {"cmds": ["false"]}},
                {"name": "sibling", "type": "exec", "options": {"cmds": ["false"]}}
            ]}]
    });
    if explicit {
        enclosing["model"] = json!("cmd/echo enclosing");
    }
    let path = source(
        &sandbox,
        json!({
            "default_model": "cmd/true",
            "default_task_models": {"Scout": "openai/default-scout"},
            "stages": [enclosing,
                {"name": "outside", "type": "exec", "options": {"cmds": ["true"]}}]
        }),
    );
    let (registry, _shutdown) = spawn_test_actor().await;
    let parent_id = launch(&registry, &path, None).await;
    let child_id = format!("{parent_id}:branch");
    let sibling_id = format!("{parent_id}:sibling");
    let parent_dir = Gremlin::from(&parent_id).unwrap().state.state_dir();
    let sibling_dir = Gremlin::from(&sibling_id).unwrap().state.state_dir();
    let child_dir = Gremlin::from(&child_id).unwrap().state.state_dir();
    let parent_before = bytes(&parent_dir);
    let sibling_before = bytes(&sibling_dir);
    let before = read_json(child_dir.join("definition.json"));
    assert_eq!(before["stages"].as_array().unwrap().len(), 1);
    assert_eq!(before["stages"][0]["name"], "branch");
    let context = read_json(child_dir.join("state.json"));
    assert_eq!(
        context["enclosing_task_models"]["Scout"],
        "openai/enclosing-scout"
    );
    assert_eq!(
        context["enclosing_task_models"]["Implement"],
        "openai/parallel-implement"
    );
    stopped(&child_dir);
    resume(&registry, &child_id, Some("cmd/echo replacement")).await;
    let mut expected = before;
    expected["default_model"] = json!("cmd/echo replacement");
    expected["resolved_model_selection"] = json!({
        "requested": "cmd/echo replacement", "default_model": "cmd/echo replacement",
        "stage_models_exact": {}, "stage_models_prefix": {}, "task_models": null
    });
    assert_eq!(read_json(child_dir.join("definition.json")), expected);
    let snapshot = fs::read(child_dir.join("definition.json")).unwrap();
    stopped(&child_dir);
    resume(&registry, &child_id, None).await;
    assert_eq!(
        fs::read(child_dir.join("definition.json")).unwrap(),
        snapshot
    );
    let mut worker = Gremlin::from(&child_id).unwrap();
    assert_eq!(
        worker.state.state_dir(),
        child_dir,
        "ID resume must not fork"
    );
    assert_eq!(
        worker.model_override.as_deref(),
        Some("cmd/echo replacement")
    );
    assert_eq!(
        worker.enclosing_model.as_ref().map(ToString::to_string),
        explicit.then(|| "cmd/echo enclosing".to_owned())
    );
    let reloaded = read_json(child_dir.join("state.json"));
    assert_eq!(
        reloaded["enclosing_task_models"],
        context["enclosing_task_models"]
    );
    assert_eq!(reloaded["enclosing_model"], context["enclosing_model"]);
    assert_eq!(
        worker.enclosing_task_models.as_ref().unwrap()["Scout"],
        "openai/enclosing-scout"
    );
    assert_eq!(
        worker.enclosing_task_models.as_ref().unwrap()["Implement"],
        "openai/parallel-implement"
    );
    worker.init_runtime(None).await.unwrap();
    let branch = worker.definition.next_stage().await.unwrap();
    assert_eq!(branch.name(), "branch");
    let selected =
        crate::executor::run::resolve_client(&branch, &worker, worker.enclosing_model.as_ref())
            .unwrap();
    assert_eq!(selected.to_string(), "cmd/echo replacement");
    assert!(matches!(
        worker.definition.next_stage().await.unwrap(),
        ExecutorStage::Done
    ));
    assert_eq!(bytes(&parent_dir), parent_before);
    assert_eq!(bytes(&sibling_dir), sibling_before);
}

#[tokio::test]
async fn resume_parallel_child_preserves_explicit_enclosing_model_and_task_context() {
    child_context(true).await;
}

#[tokio::test]
async fn resume_parallel_child_replaces_fallback_without_expanding_branch_scope() {
    child_context(false).await;
}

#[tokio::test]
async fn resume_model_replaces_launch_profile_stage_rules_on_flagless_reload() {
    let sandbox = Sandbox::with_config(Some(
        r#"
model-profiles:
  launch:
    model: cmd/true
    model-by-stage:
      mapped: cmd/echo launch-mapped
      'prefix*': cmd/echo launch-prefix
  replacement:
    model: cmd/echo replacement
    model-by-stage:
      mapped: cmd/echo unwanted
"#,
    ));
    let path = source(
        &sandbox,
        json!({"default_model": "cmd/true", "stages": [
            {"name": "mapped", "type": "exec", "options": {"cmds": ["false"]}},
            {"name": "prefix-leaf", "type": "exec", "options": {"cmds": ["true"]}},
            {"name": "fallback", "type": "exec", "options": {"cmds": ["true"]}},
            {"name": "explicit", "type": "exec", "model": "cmd/echo explicit", "options": {"cmds": ["true"]}}
        ]}),
    );
    let (registry, _shutdown) = spawn_test_actor().await;
    let id = launch(&registry, &path, Some("profile/launch")).await;
    let dir = Gremlin::from(&id).unwrap().state.state_dir();
    // Old states derive selection from model_override; an explicit empty
    // context must not accidentally select that override's profile.
    let mut legacy = read_json(dir.join("state.json"));
    legacy
        .as_object_mut()
        .unwrap()
        .remove("model_selection_profile");
    state::write_state(&dir, legacy.as_object().unwrap()).unwrap();
    let legacy_worker = Gremlin::from(&id).unwrap();
    assert_eq!(
        legacy_worker.runtime_config.profile_stage_models_exact["mapped"],
        "cmd/echo launch-mapped"
    );
    legacy["model_selection_profile"] = json!("");
    state::write_state(&dir, legacy.as_object().unwrap()).unwrap();
    let empty_worker = Gremlin::from(&id).unwrap();
    assert!(!empty_worker
        .runtime_config
        .profile_stage_models_exact
        .contains_key("mapped"));
    legacy["model_selection_profile"] = json!("profile/launch");
    state::write_state(&dir, legacy.as_object().unwrap()).unwrap();
    stopped(&dir);
    resume(&registry, &id, Some("profile/replacement")).await;
    assert_eq!(
        read_json(dir.join("state.json"))["model_selection_profile"],
        Value::Null
    );
    let mut flagged = Gremlin::from(&id).unwrap();
    flagged.init_runtime(None).await.unwrap();
    for expected in [
        "cmd/echo unwanted",
        "cmd/echo replacement",
        "cmd/echo replacement",
        "cmd/echo replacement",
    ] {
        let stage = flagged.definition.next_stage().await.unwrap();
        assert_eq!(
            crate::executor::run::resolve_client(&stage, &flagged, None)
                .unwrap()
                .to_string(),
            expected
        );
    }
    let snapshot = fs::read(dir.join("definition.json")).unwrap();
    stopped(&dir);
    resume(&registry, &id, None).await;
    assert_eq!(fs::read(dir.join("definition.json")).unwrap(), snapshot);
    let mut worker = Gremlin::from(&id).unwrap();
    assert_eq!(
        worker.model_override.as_deref(),
        Some("cmd/echo replacement")
    );
    worker.init_runtime(None).await.unwrap();
    for expected in [
        "cmd/echo unwanted",
        "cmd/echo replacement",
        "cmd/echo replacement",
        "cmd/echo replacement",
    ] {
        let stage = worker.definition.next_stage().await.unwrap();
        assert_eq!(
            crate::executor::run::resolve_client(&stage, &worker, None)
                .unwrap()
                .to_string(),
            expected
        );
    }
}
