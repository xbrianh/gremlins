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
use std::os::unix::fs::MetadataExt;
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

fn replace_selection(snapshot: &mut Value, requested: &str, concrete: &str) {
    snapshot["default_model"] = json!(concrete);
    snapshot["resolved_model_selection"] = json!({
        "requested": requested,
        "default_model": concrete,
        "stage_models_exact": {},
        "stage_models_prefix": {},
        "task_models": null
    });
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
    assert_rejected_with_failure(sandbox, dir, request, None).await;
}

async fn assert_rejected_with_failure(
    sandbox: &Sandbox,
    dir: &Path,
    request: Value,
    failure: Option<&'static str>,
) {
    let before_state = fs::read(dir.join("state.json")).unwrap();
    let before_definition = fs::read(dir.join("definition.json")).ok();
    let before_entries = entries(sandbox);
    let (registry, _shutdown) = spawn_test_actor().await;
    state::set_resume_write_failure(failure);
    let response = handle_resume(&request, &registry).await;
    state::set_resume_write_failure(None);
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
    assert_eq!(
        fs::read_to_string(dir.join("finished")).unwrap(),
        "original"
    );
    if failure.is_some() {
        let mut worker = Gremlin::from(ID).unwrap();
        assert_eq!(worker.state.state_dir(), dir, "original must remain latest");
        assert_eq!(worker.state.read_str("status"), "stopped");
        assert_eq!(worker.model_override.as_deref(), Some("cmd/true"));
        worker.init_runtime(None).await.unwrap();
        assert_eq!(worker.definition.default_model().to_string(), "cmd/true");
        assert_selection_resolves(&mut worker, "cmd/true").await;
    }
}

async fn assert_selection_resolves(worker: &mut Gremlin, concrete: &str) {
    let pinned = worker.definition.next_stage().await.unwrap();
    assert_eq!(pinned.model().unwrap().to_string(), "cmd/true");
    assert_eq!(
        crate::executor::run::resolve_client(&pinned, worker, None)
            .unwrap()
            .to_string(),
        crate::model_uri::ModelURI::parse(
            worker
                .runtime_config
                .resolved_model_selection
                .as_ref()
                .map(|selection| selection.model_for_stage("pinned"))
                .unwrap_or("cmd/true")
        )
        .unwrap()
        .to_string()
    );
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
    let inode = fs::metadata(dir.join("definition.json")).unwrap().ino();
    let mut data = read_json(dir.join("state.json"));
    data["status"] = json!("stopped");
    state::write_state(dir, data.as_object().unwrap()).unwrap();
    let response = handle_resume(&json!({"id": ID}), registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(registry).await;
    assert_eq!(fs::read(dir.join("definition.json")).unwrap(), snapshot);
    assert_eq!(
        fs::metadata(dir.join("definition.json")).unwrap().ino(),
        inode
    );
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
    assert_selection_resolves(&mut worker, concrete).await;
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
    replace_selection(
        &mut expected,
        "cmd/echo replacement",
        "cmd/echo replacement",
    );
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
    assert_selection_resolves(&mut worker, "cmd/echo replacement").await;

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
async fn resume_padded_raw_model_persists_trimmed_without_losing_internal_spaces() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_resume(
        &json!({"id": ID, "model": " \tcmd/echo replacement \n"}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    let persisted = read_json(dir.join("state.json"));
    assert_eq!(persisted["model"], "cmd/echo replacement");
    assert_eq!(persisted["model_override"], "cmd/echo replacement");
    assert_eq!(
        read_json(dir.join("definition.json"))["default_model"],
        "cmd/echo replacement"
    );
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
    assert_selection_resolves(&mut worker, "cmd/echo replacement").await;
    resume_again_unchanged(&sandbox, &dir, &registry, "cmd/echo replacement").await;
}

async fn assert_json5_resume_before_runtime_failure(stage: Option<&str>) {
    let sandbox = Sandbox::new();
    let parent = seed(&sandbox);
    let snapshot = r#"{
      // Baked JSON5 deliberately has no default_model.
      __gremlins_expanded__: true,
      extra: { sentinel: ['keep', 42, null], },
      bootstrap: { env: 'exit 37', },
      default_task_models: { Scout: 'openai/original-scout', },
      stages: [
        { name: 'pinned', type: 'exec', model: 'cmd/true',
          options: { cmds: ['true'], extra: 'keep pinned options', },
          'task-models': { Implement: 'openai/original-implement', }, },
        { name: 'run', type: 'exec', options: { cmds: ['true'], }, },
      ],
    }"#;
    assert!(serde_json::from_str::<Value>(snapshot).is_err());
    StaticDefinition::from_expanded_bytes(snapshot.as_bytes(), Some("cmd/true"), None)
        .expect("old override must make the default-less baked snapshot valid");
    fs::write(parent.join("definition.json"), snapshot).unwrap();
    let mut data = read_json(parent.join("state.json"));
    data["model"] = json!("cmd/true");
    state::write_state(&parent, data.as_object().unwrap()).unwrap();
    fs::write(
        sandbox.join("project/.gremlins/demo.json5"),
        "not a definition",
    )
    .unwrap();
    let parent_state = fs::read(parent.join("state.json")).unwrap();
    let parent_definition = fs::read(parent.join("definition.json")).unwrap();
    let mut expected: Value = json5::from_str(snapshot).unwrap();
    replace_selection(
        &mut expected,
        "cmd/echo replacement",
        "cmd/echo replacement",
    );

    let (registry, _shutdown) = spawn_test_actor().await;
    let mut request = json!({"id": ID, "model": "cmd/echo replacement"});
    if let Some(stage) = stage {
        request["stage"] = json!(stage);
    }
    let response = handle_resume(&request, &registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;

    let mut worker = Gremlin::from(ID).unwrap();
    let resumed = worker.state.state_dir();
    let persisted = read_json(resumed.join("state.json"));
    assert_eq!(persisted["model"], "cmd/echo replacement");
    assert_eq!(persisted["model_override"], "cmd/echo replacement");
    assert_eq!(persisted["metadata"]["acceptance"], "keep");
    assert_eq!(read_json(resumed.join("definition.json")), expected);
    assert!(
        worker.init_runtime(None).await.is_err(),
        "bootstrap.env must prevent successful runtime initialization"
    );
    if stage.is_some() {
        assert_ne!(resumed, parent);
        assert_eq!(entries(&sandbox).len(), 2);
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
        assert_eq!(entries(&sandbox).len(), 1);
    }
}

#[tokio::test]
async fn resume_json5_without_default_model_persists_both_models_before_runtime_failure() {
    assert_json5_resume_before_runtime_failure(None).await;
}

#[tokio::test]
async fn resume_stage_json5_without_default_model_preserves_parent_before_runtime_failure() {
    assert_json5_resume_before_runtime_failure(Some("run")).await;
}

#[tokio::test]
async fn resume_profile_persists_options_and_full_task_and_stage_mappings() {
    let sandbox = Sandbox::with_config(Some("default-model: cmd/true\n"));
    config::init_global().unwrap();
    fs::write(
        sandbox.join("config/settings.yaml"),
        r#"
model-profiles:
  replacement:
    model: '  openai/replacement?temperature=0.2,reasoning_effort=high  '
    task-models:
      Scout: openai/replacement-scout
    model-by-stage:
      pinned: openai/replacement-stage
      'pin*': openai/replacement-prefix
"#,
    )
    .unwrap();
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
    replace_selection(&mut expected, "profile/replacement", concrete);
    expected["resolved_model_selection"]["stage_models_exact"] =
        json!({"pinned": "openai/replacement-stage"});
    expected["resolved_model_selection"]["stage_models_prefix"] =
        json!({"pin": "openai/replacement-prefix"});
    expected["resolved_model_selection"]["task_models"] =
        json!({"Scout": "openai/replacement-scout"});
    assert_eq!(read_json(dir.join("definition.json")), expected);
    assert_eq!(
        read_json(dir.join("state.json"))["model"],
        crate::model_uri::ModelURI::parse(concrete)
            .unwrap()
            .to_string()
    );
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
        crate::executor::run::resolve_client(&pinned, &worker, None)
            .unwrap()
            .to_string(),
        crate::model_uri::ModelURI::parse("openai/replacement-stage")
            .unwrap()
            .to_string()
    );
    assert_eq!(
        worker
            .runtime_config
            .resolved_model_selection
            .as_ref()
            .unwrap()
            .task_models
            .as_ref()
            .unwrap()["Scout"],
        "openai/replacement-scout"
    );
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
    replace_selection(&mut expected, "cmd/echo fork", "cmd/echo fork");
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
async fn resume_unknown_provider_is_rejected_before_mutation() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    for model in ["bogus/model", "model://bogus/model"] {
        assert!(crate::model_uri::ModelURI::parse(model).is_ok());
        for stage in [None, Some("run")] {
            let mut request = json!({"id": ID, "model": model});
            if let Some(stage) = stage {
                request["stage"] = json!(stage);
            }
            assert_rejected_unchanged(&sandbox, &dir, request).await;
        }
    }
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

#[tokio::test(flavor = "current_thread")]
async fn resume_model_state_write_failure_does_not_replace_snapshot_or_start_worker() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    for stage in [None, Some("run")] {
        let mut request = json!({"id": ID, "model": "cmd/echo new"});
        if let Some(stage) = stage {
            request["stage"] = json!(stage);
        }
        assert_rejected_with_failure(&sandbox, &dir, request, Some("metadata")).await;
    }
    // Journal cleanup happens after the conditional publication commit point.
    let (registry, _shutdown) = spawn_test_actor().await;
    state::set_resume_write_failure(Some("journal-removal"));
    let response = handle_resume(&json!({"id": ID, "model": "cmd/echo new"}), &registry).await;
    state::set_resume_write_failure(None);
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    let committed = fs::read(dir.join("definition.json")).unwrap();
    assert_eq!(
        read_json(dir.join("state.json"))["model_override"],
        "cmd/echo new"
    );
    assert_eq!(
        read_json(dir.join("definition.json"))["resolved_model_selection"]["requested"],
        "cmd/echo new"
    );
    assert!(dir.join(".resume-model-recovery.json").exists());
    resume_again_unchanged(&sandbox, &dir, &registry, "cmd/echo new").await;
    assert!(!dir.join(".resume-model-recovery.json").exists());
    assert_eq!(fs::read(dir.join("definition.json")).unwrap(), committed);
}

#[tokio::test]
async fn resume_model_missing_or_corrupt_snapshot_never_launches_or_forks() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    for bytes in [
        None,
        Some(b"not json".as_slice()),
        Some(b"{stages: false}".as_slice()),
    ] {
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
async fn flagless_stage_resume_keeps_defaultless_snapshot_validation() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let mut snapshot = read_json(dir.join("definition.json"));
    snapshot.as_object_mut().unwrap().remove("default_model");
    fs::write(
        dir.join("definition.json"),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
    // A persisted override must not change the legacy stage-validation path.
    assert_rejected_unchanged(&sandbox, &dir, json!({"id": ID, "stage": "run"})).await;
}

#[tokio::test]
async fn resume_without_model_does_not_read_broken_settings_or_change_snapshot() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let before = fs::read(dir.join("definition.json")).unwrap();
    let inode = fs::metadata(dir.join("definition.json")).unwrap().ino();
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
        fs::metadata(dir.join("definition.json")).unwrap().ino(),
        inode
    );
    assert_eq!(
        Gremlin::from(ID).unwrap().model_override.as_deref(),
        Some("cmd/true")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn resume_snapshot_publication_failure_preserves_original_loadable_attempt() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    assert_rejected_with_failure(
        &sandbox,
        &dir,
        json!({"id": ID, "model": "cmd/echo new"}),
        Some("snapshot"),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resume_persistent_snapshot_replacement_failure_allows_flagless_retry() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    let original_state = fs::read(dir.join("state.json")).unwrap();
    let original_definition = fs::read(dir.join("definition.json")).unwrap();
    let (registry, _shutdown) = spawn_test_actor().await;
    // This restriction applies to publication and recovery, and stays in force
    // through the flagless retry. Metadata writes remain available.
    state::set_resume_write_failure(Some("snapshot"));
    let response = handle_resume(
        &json!({"id": ID, "model": "cmd/echo replacement"}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "error", "{response}");
    assert!(!registry.contains_run(ID).await);
    assert_eq!(fs::read(dir.join("state.json")).unwrap(), original_state);
    assert_eq!(
        fs::read(dir.join("definition.json")).unwrap(),
        original_definition
    );
    assert!(!dir.join(".resume-model-recovery.json").exists());
    let response = handle_resume(&json!({"id": ID}), &registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    state::set_resume_write_failure(None);
    assert_eq!(
        fs::read(dir.join("definition.json")).unwrap(),
        original_definition
    );
    let mut worker = Gremlin::from(ID).unwrap();
    worker.init_runtime(None).await.unwrap();
    assert_eq!(worker.definition.default_model().to_string(), "cmd/true");
}

#[tokio::test(flavor = "current_thread")]
async fn resume_fork_definition_write_failure_never_publishes_or_launches() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    assert_rejected_with_failure(
        &sandbox,
        &dir,
        json!({"id": ID, "stage": "run", "model": "cmd/echo replacement"}),
        Some("fork-definition"),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resume_double_failure_blocks_retries_until_original_snapshot_is_recovered() {
    let sandbox = Sandbox::new();
    let dir = seed(&sandbox);
    // The snapshot, not a persisted override, must select the recovered model.
    let mut data = read_json(dir.join("state.json"));
    data.as_object_mut().unwrap().remove("model_override");
    state::write_state(&dir, data.as_object().unwrap()).unwrap();
    let original_state = fs::read(dir.join("state.json")).unwrap();
    let original_definition = fs::read(dir.join("definition.json")).unwrap();
    let journal = dir.join(".resume-model-recovery.json");
    let (registry, _shutdown) = spawn_test_actor().await;

    // Publication succeeds, but both metadata commit and immediate rollback fail.
    state::set_resume_write_failure(Some("metadata-and-rollback"));
    let response = handle_resume(
        &json!({"id": ID, "model": "cmd/echo replacement"}),
        &registry,
    )
    .await;
    state::set_resume_write_failure(None);
    assert_eq!(response["type"], "error", "{response}");
    assert!(!registry.contains_run(ID).await);
    assert_eq!(fs::read(dir.join("state.json")).unwrap(), original_state);
    let replacement_definition = fs::read(dir.join("definition.json")).unwrap();
    assert_ne!(replacement_definition, original_definition);
    let mut expected: Value = serde_json::from_slice(&original_definition).unwrap();
    replace_selection(
        &mut expected,
        "cmd/echo replacement",
        "cmd/echo replacement",
    );
    assert_eq!(read_json(dir.join("definition.json")), expected);
    let retained_journal =
        fs::read(&journal).expect("failed rollback must retain recovery journal");
    assert_eq!(entries(&sandbox), vec![dir.clone()]);
    assert_eq!(
        fs::read_to_string(dir.join("finished")).unwrap(),
        "original"
    );

    // Neither ordinary resumes nor stage forks may use the mismatched pair.
    // Snapshot restrictions must apply to recovery as well as publication.
    for failure in ["snapshot", "metadata-and-rollback"] {
        for request in [
            json!({"id": ID}),
            json!({"id": ID, "model": "cmd/echo retry"}),
            json!({"id": ID, "stage": "run"}),
            json!({"id": ID, "stage": "run", "model": "cmd/echo retry"}),
        ] {
            state::set_resume_write_failure(Some(failure));
            let response = handle_resume(&request, &registry).await;
            state::set_resume_write_failure(None);
            assert_eq!(
                response["type"], "error",
                "{failure}: {request}: {response}"
            );
            assert!(
                !registry.contains_run(ID).await,
                "failed recovery launched a worker"
            );
            assert_eq!(fs::read(dir.join("state.json")).unwrap(), original_state);
            assert_eq!(
                fs::read(dir.join("definition.json")).unwrap(),
                replacement_definition
            );
            assert_eq!(fs::read(&journal).unwrap(), retained_journal);
            assert_eq!(
                entries(&sandbox),
                vec![dir.clone()],
                "failed recovery published a fork"
            );
            assert_eq!(
                fs::read_to_string(dir.join("finished")).unwrap(),
                "original"
            );
        }
    }

    let response = handle_resume(&json!({"id": ID}), &registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry).await;
    assert!(!journal.exists());
    assert_eq!(
        fs::read(dir.join("definition.json")).unwrap(),
        original_definition
    );
    assert_eq!(entries(&sandbox), vec![dir.clone()]);
    assert_eq!(read_json(dir.join("state.json"))["model"], "cmd/true");
    let mut reopened = Gremlin::from(ID).unwrap();
    assert_eq!(reopened.state.state_dir(), dir);
    assert_eq!(reopened.state.read_str("status"), "done");
    assert!(reopened.model_override.is_none());
    reopened.init_runtime(None).await.unwrap();
    assert_eq!(reopened.definition.default_model().to_string(), "cmd/true");
    assert_selection_resolves(&mut reopened, "cmd/true").await;
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

#[tokio::test]
async fn resume_parallel_branch_preserves_actual_inherited_artifact_inputs() {
    use crate::executor::supervisor::handle_launch;

    let sandbox = Sandbox::new();
    let path = sandbox.join("project/.gremlins/inherited.json5");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "default_model": "cmd/true",
            "stages": [
                {"name": "produce", "type": "exec",
                 "interpolation": {"outputs": {"diff": "artifact://diff.txt"}},
                 "options": {"cmds": ["printf 'prior diff' > {diff}"]}},
                {"name": "fanout", "type": "parallel", "body": [
                    {"name": "branch", "type": "exec",
                     "interpolation": {"inputs": {"diff": "content(\"artifact://diff.txt\")"}},
                     "options": {"cmds": ["test \"{diff}\" = 'prior diff' && test -f resume-ready"]}}
                ]}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let (registry, _shutdown) = spawn_test_actor().await;
    let response = handle_launch(
        &json!({"definition": path, "project_root": sandbox.join("project")}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "ok", "{response}");
    let parent_id = response["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.contains_run(parent_id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("parent did not finish");

    let child_id = format!("{parent_id}:branch");
    let mut child = Gremlin::from(&child_id).unwrap();
    let dir = child.state.state_dir();
    let snapshot = read_json(dir.join("definition.json"));
    assert_eq!(snapshot["stages"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["stages"][0]["name"], "branch");
    assert!(snapshot["inherited_artifact_inputs"]
        .as_array()
        .expect("child snapshot must declare its actual inherited inputs")
        .contains(&json!("artifact://diff.txt")));
    assert_eq!(
        child
            .state
            .content("artifact://diff.txt", None)
            .await
            .unwrap(),
        "prior diff"
    );
    child
        .init_runtime(None)
        .await
        .expect("branch snapshot must reopen without the producer stage");

    let mut missing_inputs = snapshot.clone();
    missing_inputs
        .as_object_mut()
        .unwrap()
        .remove("inherited_artifact_inputs");
    let error = StaticDefinition::from_expanded_bytes(
        &serde_json::to_vec(&missing_inputs).unwrap(),
        None,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("diff.txt"), "{error}");

    // The explicit input list must not weaken validation of unrelated consumers.
    let mut unknown = snapshot.clone();
    unknown["stages"][0]["interpolation"]["inputs"]["diff"] =
        json!("content(\"artifact://never-produced.txt\")");
    let error =
        StaticDefinition::from_expanded_bytes(&serde_json::to_vec(&unknown).unwrap(), None, None)
            .unwrap_err();
    assert!(error.to_string().contains("never-produced.txt"), "{error}");

    // Repair the runtime prerequisite, not the revision-backed definition.
    // The inherited consumer must remain intact on reload.
    let mut data = read_json(dir.join("state.json"));
    data["status"] = json!("stopped");
    // The completed parent's teardown removes its non-git branch workspace.
    let workdir = Path::new(data["workdir"].as_str().unwrap());
    fs::create_dir_all(workdir).unwrap();
    fs::write(workdir.join("resume-ready"), "ready").unwrap();
    state::write_state(&dir, data.as_object().unwrap()).unwrap();
    let response = handle_resume(
        &json!({"id": child_id, "model": "cmd/echo replacement"}),
        &registry,
    )
    .await;
    assert_eq!(response["type"], "ok", "{response}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.contains_run(&child_id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("resumed branch did not finish");
    let mut reopened = Gremlin::from(&child_id).unwrap();
    assert_eq!(reopened.state.state_dir(), dir);
    assert_eq!(
        reopened.state.read_str("status"),
        "done",
        "state: {:?}; log: {:?}",
        read_json(dir.join("state.json")),
        fs::read_to_string(dir.join("log"))
    );
    reopened.init_runtime(None).await.unwrap();
    assert_eq!(
        reopened.definition.default_model().to_string(),
        "cmd/echo replacement"
    );
    assert_eq!(
        reopened
            .state
            .content("artifact://diff.txt", None)
            .await
            .unwrap(),
        "prior diff"
    );
}
