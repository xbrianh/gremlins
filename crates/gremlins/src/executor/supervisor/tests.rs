//! Tests for the supervisor module.

use super::{
    add_usage_stats, create_chat_gremlin, handle_artifact, handle_chat_model, handle_compact,
    handle_launch, handle_model, parallel_runtime_keys, resolve_chat_env,
    resolve_filepath_source_values, usage_stats_from_state,
};
use crate::clients::client::{Client, ModelURI};
use crate::clients::interactive::InteractiveEvent;
use crate::clients::protocol::UsageStats;
use crate::executor::gremlin::Gremlin;
use crate::executor::socket;
use crate::executor::stage_stream::{
    agent_stream_line, exec_stream_line, interactive_event_to_stage_line, stage_line_envelope,
};
use crate::schemas::bootstrap::{InputSource, InputSources};
use crate::test_support::Sandbox;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};

fn model(spec: &str) -> ModelURI {
    ModelURI::parse(spec).unwrap()
}

#[tokio::test]
async fn turn_completion_watch_is_lossless_and_broadcasts() {
    let (tx, rx) = watch::channel(false);

    // Publish before any waiter subscribes: a late subscriber must still
    // observe the completion immediately.
    let _ = tx.send(true);
    let mut late = rx.clone();
    let _ = late.wait_for(|v| *v).await;
    assert!(*late.borrow());

    // A subscriber that clones after publication also sees the sticky
    // value without waiting for another send.
    let mut latest = rx.clone();
    let _ = latest.wait_for(|v| *v).await;

    // One publication releases every concurrent waiter.
    let (tx, rx) = watch::channel(false);
    let (mut a, mut b) = (rx.clone(), rx.clone());
    let a_task = tokio::spawn(async move { a.wait_for(|v| *v).await.is_ok() });
    let b_task = tokio::spawn(async move { b.wait_for(|v| *v).await.is_ok() });
    tokio::task::yield_now().await;
    let _ = tx.send(true);
    assert!(a_task.await.unwrap());
    assert!(b_task.await.unwrap());
}

/// After a soft interrupt, `current_turn` stays installed with `done ==
/// true` and `interrupted == true`.  If a stop path then removes the
/// parked gremlin without clearing `current_turn`, the next chat must
/// not loop forever in [`reusable_chat_gremlin_id`] waiting on a
/// turn that already completed.
#[tokio::test]
async fn reusable_chat_gremlin_id_returns_none_when_done_turn_has_no_gremlin() {
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new("test-sess".to_string()),
    ));
    let (tx, rx) = watch::channel(true); // already completed
    {
        let mut s = session.lock().await;
        s.current_turn = Some(super::CurrentTurn {
            id: "grem-1".to_string(),
            turn: Some("1".to_string()),
            cancel: super::CancelToken::new(),
            soft_cancel: Arc::new(AtomicBool::new(false)),
            done: rx,
            done_tx: Arc::new(tx),
            interrupted: true,
        });
        // gremlin was stopped externally — chat_gremlins is empty
    }

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        super::reusable_chat_gremlin_id(&session),
    )
    .await;

    match result {
        Ok(None) => {} // expected: no gremlin, no infinite loop
        Ok(Some(id)) => panic!("expected None, got Some({id:?})"),
        Err(_elapsed) => panic!(
            "reusable_chat_gremlin_id hung waiting on a completed turn with no parked gremlin"
        ),
    }
}

#[test]
fn parallel_runtime_keys_classifies_markers_relative_to_target() {
    let stages = vec![
        crate::builders::exec::ExecBuilder::new("first")
            .cmds(vec!["true".to_string()])
            .build()
            .unwrap(),
        crate::builders::composite::ParallelBuilder::new("group")
            .stage(
                crate::builders::exec::ExecBuilder::new("child-a")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                crate::builders::exec::ExecBuilder::new("child-b")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap(),
        crate::builders::exec::ExecBuilder::new("last")
            .cmds(vec!["true".to_string()])
            .build()
            .unwrap(),
    ];

    // Resuming at the group re-runs it: its completion markers (with the
    // loop-iteration scope prefix) and child-prefixed bindings are dropped.
    let at_group = parallel_runtime_keys(&stages, "group");
    assert!(at_group.is_rerun("artifact://1/group/done/child-a"));
    assert!(at_group.is_rerun("artifact://1/group/done/child-b"));
    assert!(at_group.is_rerun("artifact://child-a/report.md"));
    assert!(!at_group.is_rerun("artifact://project/README.md"));

    // Resuming at a later stage keeps the already-complete group's keys.
    let at_last = parallel_runtime_keys(&stages, "last");
    assert!(!at_last.is_rerun("artifact://1/group/done/child-a"));
    assert!(!at_last.is_rerun("artifact://child-a/report.md"));
    assert!(!at_last.is_rerun("artifact://project/README.md"));
}

async fn read_until_type(
    reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    event_type: &str,
) -> Value {
    read_until_type_inner(reader, event_type)
        .await
        .unwrap_or_else(|e| panic!("waiting for {event_type}: {e}"))
}

async fn read_until_type_inner(
    reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    event_type: &str,
) -> Result<Value, String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = socket::read_json_line(reader)
                .await
                .map_err(|e| e.to_string())?;
            let Some(line) = line else {
                return Err("chat event stream closed".to_string());
            };
            if line.get("type").and_then(|v| v.as_str()) == Some(event_type) {
                return Ok(line);
            }
        }
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

/// Wait until the chat gremlin has been re-inserted after its turn, then
/// extract a value through the provided closure.
async fn wait_for_chat_gremlin<T>(
    session: &super::SessionHandle,
    id: &str,
    extract: impl Fn(&Gremlin) -> T,
) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let val = {
                let s = session.lock().await;
                s.chat_gremlins.get(id).map(&extract)
            };
            if let Some(v) = val {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for chat gremlin {id} to re-insert"))
}

/// Create a Sandbox with project/ and state/ directories pre-created.
fn sandbox_dirs(config: Option<&str>) -> (Sandbox, PathBuf, PathBuf) {
    let sandbox = Sandbox::with_config(config);
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let state_root = sandbox.path().join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    (sandbox, project_root, state_root)
}

/// Create a UnixStream pair, returning (writer, reader, server_read, session).
fn chat_pair(
    name: &str,
) -> (
    super::SharedWriter,
    tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedReadHalf,
    super::SessionHandle,
) {
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let (server_read, server_write) = server.into_split();
    let writer = super::SharedWriter::new(server_write);
    let (client_read, _client_write) = client.into_split();
    let reader = tokio::io::BufReader::new(client_read);
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new(name.to_string()),
    ));
    (writer, reader, server_read, session)
}

#[test]
fn add_usage_stats_sums_baseline_and_prompt_totals() {
    let baseline = UsageStats {
        prompt_tokens: Some(10),
        completion_tokens: Some(20),
        cached_input_tokens: Some(30),
        cache_creation_input_tokens: Some(40),
        reasoning_tokens: Some(50),
        turns: 2,
    };
    let delta = UsageStats {
        prompt_tokens: Some(5),
        completion_tokens: Some(6),
        cached_input_tokens: Some(7),
        cache_creation_input_tokens: Some(8),
        reasoning_tokens: Some(9),
        turns: 1,
    };

    let total = add_usage_stats(&baseline, &delta);

    assert_eq!(total.prompt_tokens, Some(15));
    assert_eq!(total.completion_tokens, Some(26));
    assert_eq!(total.cached_input_tokens, Some(37));
    assert_eq!(total.cache_creation_input_tokens, Some(48));
    assert_eq!(total.reasoning_tokens, Some(59));
    assert_eq!(total.turns, 3);
}

#[test]
fn add_usage_stats_propagates_mixed_absence_and_presence() {
    let baseline = UsageStats {
        prompt_tokens: Some(10),
        completion_tokens: None,
        cached_input_tokens: Some(0),
        cache_creation_input_tokens: None,
        reasoning_tokens: Some(50),
        turns: 1,
    };
    let delta = UsageStats {
        prompt_tokens: None,
        completion_tokens: Some(6),
        cached_input_tokens: None,
        cache_creation_input_tokens: Some(8),
        reasoning_tokens: Some(0),
        turns: 1,
    };

    let total = add_usage_stats(&baseline, &delta);

    assert_eq!(total.prompt_tokens, Some(10));
    assert_eq!(total.completion_tokens, Some(6));
    assert_eq!(total.cached_input_tokens, Some(0));
    assert_eq!(total.cache_creation_input_tokens, Some(8));
    assert_eq!(total.reasoning_tokens, Some(50));
    assert_eq!(total.turns, 2);
}

#[test]
fn add_usage_stats_none_plus_none_stays_none() {
    let baseline = UsageStats {
        prompt_tokens: None,
        completion_tokens: None,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        reasoning_tokens: None,
        turns: 1,
    };
    let delta = UsageStats {
        prompt_tokens: None,
        completion_tokens: None,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        reasoning_tokens: None,
        turns: 2,
    };

    let total = add_usage_stats(&baseline, &delta);

    assert_eq!(total.prompt_tokens, None);
    assert_eq!(total.completion_tokens, None);
    assert_eq!(total.cached_input_tokens, None);
    assert_eq!(total.cache_creation_input_tokens, None);
    assert_eq!(total.reasoning_tokens, None);
    assert_eq!(total.turns, 3);
}

#[test]
fn usage_stats_from_state_reads_persisted_usage_and_defaults_missing_state() {
    let state = serde_json::json!({
        "prompt_tokens": 11,
        "completion_tokens": 12,
        "cached_input_tokens": 13,
        "cache_creation_input_tokens": 14,
        "reasoning_tokens": 15,
        "turns": 3
    });

    let stats = usage_stats_from_state(Some(&state));

    assert_eq!(stats.prompt_tokens, Some(11));
    assert_eq!(stats.completion_tokens, Some(12));
    assert_eq!(stats.cached_input_tokens, Some(13));
    assert_eq!(stats.cache_creation_input_tokens, Some(14));
    assert_eq!(stats.reasoning_tokens, Some(15));
    assert_eq!(stats.turns, 3);

    let stats = usage_stats_from_state(None);

    assert_eq!(stats.prompt_tokens, None);
    assert_eq!(stats.completion_tokens, None);
    assert_eq!(stats.cached_input_tokens, None);
    assert_eq!(stats.cache_creation_input_tokens, None);
    assert_eq!(stats.reasoning_tokens, None);
    assert_eq!(stats.turns, 0);
}

#[test]
fn resolve_chat_env_prefers_request_env() {
    let request = serde_json::json!({
        "env": {"GREMLINS_CHAT_ENV": "from-tui", "ignored": 42}
    });
    let base = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);

    let resolved = resolve_chat_env(&request, &base);

    assert_eq!(
        resolved.get("GREMLINS_CHAT_ENV").map(String::as_str),
        Some("from-tui")
    );
    assert!(!resolved.contains_key("ignored"));
    assert!(!resolved.contains_key("PATH"));
}

#[test]
fn resolve_chat_env_falls_back_to_base_snapshot() {
    let request = serde_json::json!({});
    let base = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);

    let resolved = resolve_chat_env(&request, &base);

    assert_eq!(resolved, base);
}

#[test]
fn resolve_filepath_source_values_resolves_relative_against_project_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("plan.md"), "plan").unwrap();

    let source = InputSources::new(HashMap::from([
        (
            "plan".to_string(),
            InputSource::new("plan".to_string(), vec!["filepath".to_string()], true, None).unwrap(),
        ),
        (
            "message".to_string(),
            InputSource::new(
                "message".to_string(),
                vec!["string".to_string()],
                true,
                None,
            )
            .unwrap(),
        ),
    ]));
    let inputs = HashMap::from([
        ("plan".to_string(), "plan.md".to_string()),
        ("message".to_string(), "keep me".to_string()),
    ]);

    let resolved = resolve_filepath_source_values(&source, &inputs, &project_root);

    assert_eq!(
        resolved.get("plan").map(String::as_str),
        Some(project_root.join("plan.md").to_string_lossy().as_ref())
    );
    assert_eq!(resolved.get("message").map(String::as_str), Some("keep me"));
}

#[test]
fn resolve_filepath_source_values_passes_absolute_values_through() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let absolute = tmp.path().join("plan.md");
    std::fs::write(&absolute, "absolute plan").unwrap();

    let source = InputSources::new(HashMap::from([(
        "plan".to_string(),
        InputSource::new("plan".to_string(), vec!["filepath".to_string()], true, None).unwrap(),
    )]));

    let with_absolute =
        HashMap::from([("plan".to_string(), absolute.to_string_lossy().into_owned())]);
    let resolved = resolve_filepath_source_values(&source, &with_absolute, &project_root);
    assert_eq!(
        resolved.get("plan").map(String::as_str),
        Some(absolute.to_string_lossy().as_ref())
    );
}

#[test]
fn resolve_filepath_source_values_preserves_mixed_inline_text() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let source = InputSources::new(HashMap::from([(
        "plan".to_string(),
        InputSource::new(
            "plan".to_string(),
            vec!["filepath".to_string(), "string".to_string()],
            true,
            None,
        )
        .unwrap(),
    )]));

    let inputs = HashMap::from([("plan".to_string(), "write more tests".to_string())]);
    let resolved = resolve_filepath_source_values(&source, &inputs, &project_root);

    assert_eq!(
        resolved.get("plan").map(String::as_str),
        Some("write more tests"),
        "mixed-type values that are not existing files must stay verbatim"
    );
}

#[test]
fn resolve_filepath_source_values_preserves_missing_relative() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let source = InputSources::new(HashMap::from([(
        "plan".to_string(),
        InputSource::new("plan".to_string(), vec!["filepath".to_string()], true, None).unwrap(),
    )]));

    let with_missing = HashMap::from([("plan".to_string(), "missing.md".to_string())]);
    let resolved = resolve_filepath_source_values(&source, &with_missing, &project_root);

    // The original value is preserved for the diagnostic...
    assert_eq!(resolved.get("plan").map(String::as_str), Some("missing.md"));
    // ...but validation checks existence only under `project_root`, so a
    // same-named file in the daemon cwd can never satisfy it.
    let error =
        crate::schemas::bootstrap::validate_source_values_under(&source, &resolved, &project_root)
            .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("expected an existing file, got \"missing.md\""),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn handle_launch_resolves_relative_filepath_against_project_root() {
    let sandbox = Sandbox::with_config(Some(r#"{"default-model": "cmd/echo"}"#));
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("plan.md"), "plan").unwrap();

    let definition_path = project_root.join(".gremlins").join("demo.json5");
    std::fs::create_dir_all(definition_path.parent().unwrap()).unwrap();
    std::fs::write(
        &definition_path,
        r#"{
  default_model: "cmd/echo",
  bootstrap: {
    source: {
      plan: { type: "filepath", optional: true },
    },
  },
  stages: [],
}
"#,
    )
    .unwrap();

    let (shutdown_tx, _) = watch::channel(false);
    let request = |plan: &str| {
        serde_json::json!({
            "op": "launch",
            "definition": definition_path.to_string_lossy(),
            "args": ["--plan", plan],
            "project_root": project_root.to_string_lossy(),
        })
    };

    // A relative `filepath` value that exists under `project_root` (but not
    // the daemon cwd) must validate and launch successfully.
    let resp = handle_launch(&request("plan.md"), sandbox.path(), &shutdown_tx).await;
    assert_eq!(
        resp.get("type").and_then(Value::as_str),
        Some("ok"),
        "relative filepath under project_root must launch: {resp}"
    );
    let id = resp["id"].as_str().unwrap().to_string();
    super::get_run_map().lock().unwrap().remove(&id);

    // A relative `filepath` value that exists nowhere under `project_root`
    // must fail validation with the existing error; the daemon cwd is not
    // allowed to satisfy it.
    let resp = handle_launch(&request("missing.md"), sandbox.path(), &shutdown_tx).await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    let message = resp
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("expected an existing file, got \"missing.md\""),
        "missing file diagnostic must preserve the caller's value: {message}"
    );
}

#[tokio::test]
async fn handle_artifact_returns_error_for_missing_id() {
    let resp = handle_artifact(&serde_json::json!({"op": "artifact"})).await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    assert!(resp
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|e| e.contains("missing 'id' field")));
}

#[tokio::test]
async fn handle_artifact_returns_error_for_invalid_id() {
    let resp = handle_artifact(&serde_json::json!({"op": "artifact", "id": "../etc"})).await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    assert!(resp
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|e| e.contains("invalid gremlin id")));
}

#[tokio::test]
async fn model_op_show_set_clear_round_trip() {
    let _sandbox = Sandbox::with_config(Some(
        r#"default-model: "cmd/default"
tui-model: "cmd/tui"
model-profiles:
  fast:
    model: cmd/echo
"#,
    ));

    let show = handle_model(&serde_json::json!({"op": "model"})).await;
    assert_eq!(show.get("type").and_then(Value::as_str), Some("ok"));
    assert_eq!(show["model"], "cmd/tui");
    assert_eq!(show["source"], "tui-model");

    let set = handle_model(&serde_json::json!({"op": "model", "model": "profile/fast"})).await;
    assert_eq!(set.get("type").and_then(Value::as_str), Some("ok"));
    assert_eq!(set["model"], "cmd/echo");
    assert_eq!(set["source"], "tui.json");

    let show_after_set = handle_model(&serde_json::json!({"op": "model"})).await;
    assert_eq!(show_after_set["model"], "cmd/echo");
    assert_eq!(show_after_set["source"], "tui.json");

    let clear = handle_model(&serde_json::json!({"op": "model", "clear": true})).await;
    assert_eq!(clear.get("type").and_then(Value::as_str), Some("ok"));
    assert_eq!(clear["model"], "cmd/tui");
    assert_eq!(clear["source"], "tui-model");

    let override_path = crate::config::resolve_user_config_root(None).join("tui.json");
    assert!(!override_path.exists());
}

#[tokio::test]
async fn model_op_invalid_spec_does_not_create_override() {
    let _sandbox = Sandbox::with_config(Some(r#"default-model: "cmd/default""#));
    let override_path = crate::config::resolve_user_config_root(None).join("tui.json");

    let resp = handle_model(&serde_json::json!({"op": "model", "model": "not-a-model"})).await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    assert!(!override_path.exists());
}

#[tokio::test]
async fn model_op_unknown_provider_does_not_create_override() {
    let _sandbox = Sandbox::with_config(Some(r#"default-model: "cmd/default""#));
    let override_path = crate::config::resolve_user_config_root(None).join("tui.json");

    let resp = handle_model(&serde_json::json!({"op": "model", "model": "fake/m"})).await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    assert!(!override_path.exists());
}

#[tokio::test]
async fn model_op_unknown_profile_does_not_create_override() {
    let _sandbox = Sandbox::with_config(Some(r#"default-model: "cmd/default""#));
    let override_path = crate::config::resolve_user_config_root(None).join("tui.json");

    let resp = handle_model(&serde_json::json!({"op": "model", "model": "profile/nope"})).await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    assert!(!override_path.exists());
}

#[test]
fn model_parser_reserves_profile_provider() {
    for spec in ["profile/foo", "model://profile/foo"] {
        let err = Client::parse(spec).unwrap_err();
        assert!(
            err.contains("reserved profile-reference prefix"),
            "{spec:?} -> {err:?}"
        );
    }

    let err = Client::parse("profile:foo").unwrap_err();
    assert!(err.contains("expected 'provider/model'"), "{err:?}");
}

#[tokio::test]
async fn handle_artifact_returns_unknown_gremlin_for_missing_state() {
    let _sandbox = Sandbox::new();
    let resp =
        handle_artifact(&serde_json::json!({"op": "artifact", "id": "gr-missing", "list": true}))
            .await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    assert!(resp
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|e| e.contains("unknown gremlin")));
}

#[tokio::test]
async fn handle_artifact_lists_keys_and_reads_content_from_persisted_state() {
    let _sandbox = Sandbox::new();
    let mut initial = serde_json::Map::new();
    initial.insert("id".into(), Value::String("gr-1".into()));
    let store =
        crate::executor::state::FileSystemStateStore::create("gr-1", &initial, false).unwrap();
    store
        .write_into_registry(
            &crate::artifacts::uri::Uri::parse("artifact://out").unwrap(),
            "hello from artifact",
        )
        .await
        .unwrap();

    let list_resp =
        handle_artifact(&serde_json::json!({"op": "artifact", "id": "gr-1", "list": true})).await;
    assert_eq!(list_resp.get("type").and_then(Value::as_str), Some("ok"));
    assert_eq!(
        list_resp
            .get("keys")
            .and_then(Value::as_array)
            .map(|keys| { keys.iter().filter_map(Value::as_str).collect::<Vec<_>>() }),
        Some(vec!["artifact://out"])
    );

    let content_resp = handle_artifact(
        &serde_json::json!({"op": "artifact", "id": "gr-1", "artifact": "artifact://out"}),
    )
    .await;
    assert_eq!(content_resp.get("type").and_then(Value::as_str), Some("ok"));
    assert_eq!(
        content_resp.get("content").and_then(Value::as_str),
        Some("hello from artifact")
    );
}

#[tokio::test]
async fn handle_artifact_reads_live_run_map_store() {
    let _sandbox = Sandbox::new();

    // Ephemeral store — its state lives in a TempDir, unreachable via
    // config::state_root(), so this exercises the run-map branch that makes
    // the op work for live/ephemeral chat gremlins.
    let store: Arc<dyn crate::executor::state::StateStore + Send + Sync> = Arc::new(
        crate::executor::state::FileSystemStateStore::create(
            "gr-live",
            &serde_json::Map::new(),
            /* ephemeral */ true,
        )
        .unwrap(),
    );
    store
        .write_into_registry(
            &crate::artifacts::uri::Uri::parse("artifact://live").unwrap(),
            "live artifact",
        )
        .await
        .unwrap();

    let channels = super::InteractiveChannels::new();
    let (interactive, _session) = channels.split();
    let (state_tx, _state_rx) = watch::channel(super::RunState {
        id: "gr-live".to_string(),
        status: "running".to_string(),
        stage: "starting".to_string(),
        started_at: String::new(),
    });
    let (stage_stream_broadcast, _) = broadcast::channel(1);

    let handle = super::RunHandle {
        cancel: super::CancelToken::new(),
        task: None,
        aborted: Arc::new(AtomicBool::new(false)),
        state_tx,
        stage_stream_broadcast,
        interactive,
        store: store.clone(),
        log_flush: None,
        log_sub: None,
    };
    super::get_run_map()
        .lock()
        .unwrap()
        .insert("gr-live".to_string(), handle);

    let list_resp =
        handle_artifact(&serde_json::json!({"op": "artifact", "id": "gr-live", "list": true}))
            .await;
    assert_eq!(list_resp.get("type").and_then(Value::as_str), Some("ok"));
    assert_eq!(
        list_resp
            .get("keys")
            .and_then(Value::as_array)
            .map(|keys| { keys.iter().filter_map(Value::as_str).collect::<Vec<_>>() }),
        Some(vec!["artifact://live"])
    );
    let content_resp = handle_artifact(
        &serde_json::json!({"op": "artifact", "id": "gr-live", "artifact": "artifact://live"}),
    )
    .await;
    assert_eq!(content_resp.get("type").and_then(Value::as_str), Some("ok"));
    assert_eq!(
        content_resp.get("content").and_then(Value::as_str),
        Some("live artifact")
    );

    // Clean up the run-map entry so it does not leak into other tests.
    super::get_run_map().lock().unwrap().remove("gr-live");
}

#[tokio::test]
async fn handle_artifact_returns_error_for_unbound_artifact() {
    let _sandbox = Sandbox::new();
    let mut initial = serde_json::Map::new();
    initial.insert("id".into(), Value::String("gr-2".into()));
    let _store =
        crate::executor::state::FileSystemStateStore::create("gr-2", &initial, false).unwrap();

    let resp = handle_artifact(
        &serde_json::json!({"op": "artifact", "id": "gr-2", "artifact": "artifact://missing"}),
    )
    .await;
    assert_eq!(resp.get("type").and_then(Value::as_str), Some("error"));
    assert!(resp
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|e| e.contains("artifact not bound")));
}

#[tokio::test]
async fn chat_turn_refreshes_env_and_falls_back_to_base_snapshot() {
    let (_sandbox, project_root, state_root) =
        sandbox_dirs(Some(r#"{"default-model": "cmd/echo"}"#));

    // `run_supervisor` captures this at daemon startup; establish the
    // same snapshot here so the absent-field fallback can be asserted.
    let base_env = super::base_process_env().clone();

    let (writer, mut reader, _server_read, session) = chat_pair("sess-test");

    let chat = |turn: &str, env: Option<serde_json::Value>| {
        let mut req = serde_json::json!({
            "op": "chat",
            "text": "hello",
            "turn": turn,
            "project_root": project_root.to_string_lossy(),
        });
        if let Some(env) = env {
            req["env"] = env;
        }
        req
    };

    // First contact: the request env seeds the newly created gremlin.
    super::handle_chat(
        &chat(
            "1",
            Some(serde_json::json!({"GREMLINS_CHAT_ENV_TEST": "first"})),
        ),
        &state_root,
        &writer,
        &session,
    )
    .await;
    let ack = read_until_type(&mut reader, "ok").await;
    let id = ack["id"].as_str().unwrap().to_string();
    {
        let env = wait_for_chat_gremlin(&session, &id, |g| g.env.clone()).await;
        assert_eq!(
            env.get("GREMLINS_CHAT_ENV_TEST").map(String::as_str),
            Some("first")
        );
    }

    // Reuse: a later turn's request env replaces the captured env.
    super::handle_chat(
        &chat(
            "2",
            Some(serde_json::json!({"GREMLINS_CHAT_ENV_TEST": "second"})),
        ),
        &state_root,
        &writer,
        &session,
    )
    .await;
    read_until_type(&mut reader, "ok").await;
    {
        let env = wait_for_chat_gremlin(&session, &id, |g| g.env.clone()).await;
        assert_eq!(
            env.get("GREMLINS_CHAT_ENV_TEST").map(String::as_str),
            Some("second")
        );
    }

    // Absent env: fall back to the daemon's base process snapshot.
    super::handle_chat(&chat("3", None), &state_root, &writer, &session).await;
    read_until_type(&mut reader, "ok").await;
    {
        let env = wait_for_chat_gremlin(&session, &id, |g| g.env.clone()).await;
        assert_eq!(
            env, base_env,
            "absent env must fall back to the daemon base process snapshot"
        );
    }

    super::teardown_session(&session, &state_root).await;

    {
        let s = session.lock().await;
        assert!(
            s.chat_ids.is_empty(),
            "teardown must drain session chat ids"
        );
        assert!(
            s.chat_gremlins.is_empty(),
            "teardown must drop parked chat gremlins"
        );
    }
    assert!(
        !super::get_run_map().lock().unwrap().contains_key(&id),
        "teardown must remove the chat gremlin from the run map"
    );
}

#[tokio::test]
async fn chat_reuse_rebuilds_client_after_model_change() {
    let (_sandbox, project_root, state_root) =
        sandbox_dirs(Some(r#"{"default-model": "cmd/echo"}"#));

    let (writer, mut reader, _server_read, session) = chat_pair("sess-model-reuse");

    let chat = |turn: &str| {
        serde_json::json!({
            "op": "chat",
            "text": "hello",
            "turn": turn,
            "project_root": project_root.to_string_lossy(),
        })
    };

    // First turn seeds the chat gremlin with the default model.
    super::handle_chat(&chat("1"), &state_root, &writer, &session).await;
    let ack = read_until_type(&mut reader, "ok").await;
    let id = ack["id"].as_str().unwrap().to_string();
    let before = wait_for_chat_gremlin(&session, &id, |g| g.client.clone()).await;
    assert_eq!(before, Client::parse("cmd/echo").unwrap());
    assert_eq!(
        wait_for_chat_gremlin(&session, &id, |g| g.state.read_str("model")).await,
        "cmd/echo"
    );

    // Change the model override; the reused gremlin must pick up the new
    // client handle on the next turn without being recreated.
    let set = handle_model(&serde_json::json!({ "op": "model", "model": "cmd/true" })).await;
    assert_eq!(set["model"], "cmd/true");
    assert_eq!(set["source"], "tui.json");

    super::handle_chat(&chat("2"), &state_root, &writer, &session).await;
    read_until_type(&mut reader, "ok").await;
    let after = wait_for_chat_gremlin(&session, &id, |g| g.client.clone()).await;
    assert_eq!(after, Client::parse("cmd/true").unwrap());
    assert_eq!(
        wait_for_chat_gremlin(&session, &id, |g| g.state.read_str("model")).await,
        "cmd/true"
    );

    super::teardown_session(&session, &state_root).await;
}

#[tokio::test]
async fn chat_model_reports_null_when_no_gremlin_exists() {
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new("sess-model-none".to_string()),
    ));

    let resp = handle_chat_model(&serde_json::json!({}), &session).await;

    assert_eq!(resp["type"], "ok");
    assert!(resp["model"].is_null());
}

#[tokio::test]
async fn chat_model_is_session_scoped_and_reads_parked_gremlin() {
    let (_sandbox, project_root, state_root) =
        sandbox_dirs(Some(r#"{"default-model": "cmd/echo"}"#));

    let (writer, mut reader, _server_read, session_a) = chat_pair("sess-model-a");
    let session_b: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new("sess-model-b".to_string()),
    ));

    super::handle_chat(
        &serde_json::json!({
            "op": "chat",
            "text": "hello",
            "turn": "1",
            "project_root": project_root.to_string_lossy(),
        }),
        &state_root,
        &writer,
        &session_a,
    )
    .await;
    let ack = read_until_type(&mut reader, "ok").await;
    let id = ack["id"].as_str().unwrap().to_string();
    assert_eq!(
        wait_for_chat_gremlin(&session_a, &id, |g| g.state.read_str("model")).await,
        "cmd/echo"
    );

    let resp_a = handle_chat_model(&serde_json::json!({}), &session_a).await;
    assert_eq!(resp_a["model"], "cmd/echo");

    // A session with no gremlin of its own must not observe session A's
    // model; the lookup is scoped to the requesting session.
    let resp_b = handle_chat_model(&serde_json::json!({}), &session_b).await;
    assert_eq!(resp_b["type"], "ok");
    assert!(resp_b["model"].is_null());

    super::teardown_session(&session_a, &state_root).await;
    super::teardown_session(&session_b, &state_root).await;
}

#[tokio::test]
async fn chat_model_reports_null_when_model_field_is_empty() {
    let (_sandbox, project_root, state_root) =
        sandbox_dirs(Some(r#"{"default-model": "cmd/echo"}"#));

    let (writer, mut reader, _server_read, session) = chat_pair("sess-model-empty");

    super::handle_chat(
        &serde_json::json!({
            "op": "chat",
            "text": "hello",
            "turn": "1",
            "project_root": project_root.to_string_lossy(),
        }),
        &state_root,
        &writer,
        &session,
    )
    .await;
    let ack = read_until_type(&mut reader, "ok").await;
    let id = ack["id"].as_str().unwrap().to_string();
    assert_eq!(
        wait_for_chat_gremlin(&session, &id, |g| g.state.read_str("model")).await,
        "cmd/echo"
    );

    {
        let s = session.lock().await;
        let gremlin = s.chat_gremlins.get(&id).unwrap();
        let fields = serde_json::json!({ "model": "" })
            .as_object()
            .unwrap()
            .clone();
        gremlin.state.patch(&[], &fields);
    }

    let resp = handle_chat_model(&serde_json::json!({}), &session).await;
    assert_eq!(resp["type"], "ok");
    assert!(resp["model"].is_null());

    super::teardown_session(&session, &state_root).await;
}

#[tokio::test]
async fn chat_model_falls_back_to_run_map_while_turn_is_in_flight() {
    let (_sandbox, project_root, state_root) =
        sandbox_dirs(Some(r#"{"default-model": "cmd/echo"}"#));

    let (writer, mut reader, _server_read, session) = chat_pair("sess-model-inflight");

    super::handle_chat(
        &serde_json::json!({
            "op": "chat",
            "text": "hello",
            "turn": "1",
            "project_root": project_root.to_string_lossy(),
        }),
        &state_root,
        &writer,
        &session,
    )
    .await;
    let ack = read_until_type(&mut reader, "ok").await;
    let id = ack["id"].as_str().unwrap().to_string();
    assert_eq!(
        wait_for_chat_gremlin(&session, &id, |g| g.state.read_str("model")).await,
        "cmd/echo"
    );

    // Simulate an in-flight turn: the gremlin leaves `chat_gremlins` but
    // its id remains on `current_turn`, and the run map still carries the
    // same store (a cloned Arc, so it survives the dropped `Gremlin`).
    let done_tx;
    {
        let mut s = session.lock().await;
        s.chat_gremlins.remove(&id);
        let (tx, rx) = watch::channel(false);
        done_tx = Arc::new(tx);
        s.current_turn = Some(super::CurrentTurn {
            id: id.clone(),
            turn: Some("1".to_string()),
            cancel: super::CancelToken::new(),
            soft_cancel: Arc::new(AtomicBool::new(false)),
            done: rx,
            done_tx: Arc::clone(&done_tx),
            interrupted: false,
        });
    }

    let resp = handle_chat_model(&serde_json::json!({}), &session).await;
    assert_eq!(resp["model"], "cmd/echo");

    {
        let mut s = session.lock().await;
        s.current_turn = None;
        s.chat_ids.clear();
    }
    super::cleanup_chat_gremlin(&id).await;
}

#[tokio::test]
async fn chat_stage_model_wins_over_default_model_by_stage() {
    let (_sandbox, project_root, state_root) = sandbox_dirs(Some(
        r#"default-model: "cmd/default"
default-model-by-stage:
  chat: cmd/echo
"#,
    ));

    let (writer, mut reader, _server_read, session) = chat_pair("sess-stage-model");

    let set = handle_model(&serde_json::json!({ "op": "model", "model": "cmd/true" })).await;
    assert_eq!(set["model"], "cmd/true");
    assert_eq!(set["source"], "tui.json");

    super::handle_chat(
        &serde_json::json!({
            "op": "chat",
            "text": "hello",
            "turn": "1",
            "project_root": project_root.to_string_lossy(),
        }),
        &state_root,
        &writer,
        &session,
    )
    .await;
    let ack = read_until_type(&mut reader, "ok").await;
    let id = ack["id"].as_str().unwrap().to_string();

    // The chat gremlin's definition carries the TUI-resolved model.
    // Chat definitions are never serialized as YAML, so we verify
    // through the state store.  The `model` field is patched by
    // handle_chat from the same `chat_model` value that set_pending
    // passes to next_stage, so this verifies both the state.json
    // entry and the stage's effective model (which is never the
    // default-model-by-stage fallback).
    let store = super::get_run_map()
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .store
        .clone();
    let state = store.read_str("model");
    assert_eq!(
        state, "cmd/true",
        "chat definition must carry the TUI-resolved model, not the default-model-by-stage entry"
    );

    super::teardown_session(&session, &state_root).await;
}

#[tokio::test]
async fn chat_resolves_profile_default_model() {
    let (_sandbox, project_root, state_root) = sandbox_dirs(Some(
        r#"default-model: "profile/fast"
model-profiles:
  fast:
    model: cmd/echo
"#,
    ));

    let (writer, mut reader, _server_read, session) = chat_pair("sess-profile-test");

    let request = serde_json::json!({
        "op": "chat",
        "text": "hello",
        "turn": "1",
        "project_root": project_root.to_string_lossy(),
    });

    super::handle_chat(&request, &state_root, &writer, &session).await;
    let ack = read_until_type(&mut reader, "ok").await;
    assert_eq!(ack["status"], "streaming");
    assert!(!ack["id"].as_str().unwrap().is_empty());

    super::teardown_session(&session, &state_root).await;
}

#[tokio::test]
async fn chat_reports_error_on_unknown_profile() {
    let (_sandbox, project_root, state_root) = sandbox_dirs(Some(
        r#"default-model: "profile/nope"
model-profiles:
  fast:
    model: cmd/echo
"#,
    ));

    let (writer, mut reader, _server_read, session) = chat_pair("sess-unknown-profile");

    let request = serde_json::json!({
        "op": "chat",
        "text": "hello",
        "turn": "1",
        "project_root": project_root.to_string_lossy(),
    });

    super::handle_chat(&request, &state_root, &writer, &session).await;
    let err = read_until_type(&mut reader, "error").await;
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("unknown model profile"),
        "expected unknown profile error, got: {err}"
    );

    super::teardown_session(&session, &state_root).await;
}

#[tokio::test]
async fn chat_reports_error_on_empty_profile_model() {
    let (_sandbox, project_root, state_root) = sandbox_dirs(Some(
        r#"default-model: "profile/empty"
model-profiles:
  empty:
    model: ""
"#,
    ));

    let (writer, mut reader, _server_read, session) = chat_pair("sess-empty-profile");

    let request = serde_json::json!({
        "op": "chat",
        "text": "hello",
        "turn": "1",
        "project_root": project_root.to_string_lossy(),
    });

    super::handle_chat(&request, &state_root, &writer, &session).await;
    let err = read_until_type(&mut reader, "error").await;
    assert!(
        err["message"].as_str().unwrap().contains("has no model"),
        "expected empty profile client error, got: {err}"
    );

    super::teardown_session(&session, &state_root).await;
}

#[tokio::test]
async fn stop_session_chat_gremlin_removes_ownership_and_cancels_turn() {
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new("sess-stop-test".to_string()),
    ));
    let cancel = super::CancelToken::new();
    let done_tx;
    {
        let mut s = session.lock().await;
        s.chat_ids.insert("chat-stop".to_string());
        let (tx, rx) = watch::channel(false);
        done_tx = Arc::new(tx);
        s.current_turn = Some(super::CurrentTurn {
            id: "chat-stop".to_string(),
            turn: Some("1".to_string()),
            cancel: cancel.clone(),
            soft_cancel: Arc::new(AtomicBool::new(false)),
            done: rx,
            done_tx: Arc::clone(&done_tx),
            interrupted: false,
        });
    }

    super::session_map()
        .lock()
        .unwrap()
        .insert("sess-stop-test".to_string(), Arc::clone(&session));

    super::stop_session_chat_gremlin("chat-stop").await;
    super::session_map()
        .lock()
        .unwrap()
        .remove("sess-stop-test");

    assert!(cancel.is_cancelled());
    let s = session.lock().await;
    assert!(!s.chat_ids.contains("chat-stop"));
    assert!(s.chat_gremlins.is_empty());
}

/// The chat gremlin must anchor against the caller-supplied project root:
/// it is recorded in `state.json` and `gremlin.cwd()` resolves to it even
/// though the gremlin has no worktree.
#[test]
fn create_chat_gremlin_seeds_project_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("operator-project");
    std::fs::create_dir_all(&project_root).unwrap();
    let state_root = tmp.path().join("state");
    std::fs::create_dir_all(&state_root).unwrap();

    let gremlin = create_chat_gremlin(
        &model("cmd/echo"),
        "you are helpful",
        &project_root,
        &HashMap::new(),
    )
    .unwrap();

    assert_eq!(
        gremlin.state.read_str("project_root"),
        project_root.to_string_lossy().into_owned(),
        "state.json must record the operator's project root"
    );
    assert_eq!(
        gremlin.cwd(),
        project_root,
        "gremlin.cwd() must resolve to the project root"
    );
    assert!(
        gremlin.workdir.is_none(),
        "chat gremlins must not have a worktree"
    );
}

/// The chat gremlin's `env` must be seeded from the resolved environment
/// map (the TUI's environment, or the daemon's base env when absent).
#[test]
fn create_chat_gremlin_seeds_environment() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("operator-project");
    std::fs::create_dir_all(&project_root).unwrap();
    let state_root = tmp.path().join("state");
    std::fs::create_dir_all(&state_root).unwrap();

    let env = HashMap::from([
        ("PATH".to_string(), "/custom/bin".to_string()),
        ("HOME".to_string(), "/home/operator".to_string()),
    ]);

    let gremlin =
        create_chat_gremlin(&model("cmd/echo"), "you are helpful", &project_root, &env).unwrap();

    assert_eq!(
        gremlin.env, env,
        "gremlin.env must be seeded from the resolved environment map"
    );
}

/// Dropping the debug abort guard must send `Quit` and reset the pause
/// token so an aborted debug connection never leaves a pipeline gremlin
/// paused indefinitely.
#[tokio::test]
async fn debug_abort_guard_sends_quit_and_resets_pause() {
    let channels = super::InteractiveChannels::new();
    let (handle, mut session) = channels.split();
    handle.pause.pause();

    let guard = super::DebugDetachGuard::new(handle.clone());
    drop(guard);

    assert!(!handle.pause.is_paused(), "guard drop must reset pause");
    let cmd = tokio::time::timeout(Duration::from_secs(1), session.cmd_rx.recv())
        .await
        .expect("guard drop should send Quit promptly")
        .expect("cmd channel closed before Quit arrived");
    assert!(matches!(cmd, super::InteractiveCommand::Quit));
}

/// EOF on the actual main session socket must still drain session
/// resources. Regression test for teardown being skipped after the
/// reader marked the session closed.
#[tokio::test]
async fn main_socket_eof_still_tears_down_session() {
    let sandbox = Sandbox::new();
    let state_root = sandbox.path().join("state");
    std::fs::create_dir_all(&state_root).unwrap();

    let session_id = "sess-eof-test".to_string();
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new(session_id.clone()),
    ));
    super::session_map()
        .lock()
        .unwrap()
        .insert(session_id.clone(), Arc::clone(&session));

    let listener = socket::bind_session_socket(&state_root, &session_id).unwrap();
    let client = socket::connect_session_socket(&state_root, &session_id)
        .await
        .unwrap();
    let (server, _addr) = listener.accept().await.unwrap();
    let (server_read, server_write) = server.into_split();
    let writer = super::SharedWriter::new(server_write);

    let session_for_task = Arc::clone(&session);
    let state_root_for_task = state_root.clone();
    let handle = tokio::spawn(async move {
        super::handle_session_main(server_read, writer, session_for_task, &state_root_for_task)
            .await;
    });

    drop(client);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !super::session_map()
                .lock()
                .unwrap()
                .contains_key(&session_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("session was not torn down after main socket EOF"));

    handle.await.unwrap();

    assert!(
        !socket::session_socket_path(&state_root, &session_id).exists(),
        "session socket file must be unlinked on main socket EOF"
    );
}

#[test]
fn chat_payload_serializes_tool_output_chunk() {
    let value = super::chat_payload(
        &Some("t-9".to_string()),
        serde_json::json!({"type": "tool_output_chunk", "id": "call_9", "name": "Bash", "call": "Bash(command=\"ls\")", "text": "hi\n"}),
    );
    assert_eq!(value["type"], "tool_output_chunk");
    assert_eq!(value["id"], "call_9");
    assert_eq!(value["name"], "Bash");
    assert_eq!(value["call"], "Bash(command=\"ls\")");
    assert_eq!(value["text"], "hi\n");
    assert_eq!(value["turn"], "t-9");
}

#[test]
fn chat_payload_serializes_tool_result() {
    let value = super::chat_payload(
        &Some("t-9".to_string()),
        serde_json::json!({"type": "tool_result", "id": "call_9", "name": "Grep", "call": "Grep(pattern=\"fn\")", "output": "x.rs:1:fn"}),
    );
    assert_eq!(value["type"], "tool_result");
    assert_eq!(value["id"], "call_9");
    assert_eq!(value["name"], "Grep");
    assert_eq!(value["call"], "Grep(pattern=\"fn\")");
    assert_eq!(value["output"], "x.rs:1:fn");
    assert_eq!(value["turn"], "t-9");
}

#[test]
fn interactive_event_to_stage_line_maps_agent_events() {
    let line = interactive_event_to_stage_line(&InteractiveEvent::StreamChunk {
        text: "hi".to_string(),
    })
    .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["kind"], "text");
    assert_eq!(v["text"], "hi");

    let line = interactive_event_to_stage_line(&InteractiveEvent::ReasoningChunk {
        text: "think".to_string(),
    })
    .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["kind"], "reasoning");
    assert_eq!(v["text"], "think");

    let line = interactive_event_to_stage_line(&InteractiveEvent::ToolResult {
        id: "call_1".to_string(),
        name: "Bash".to_string(),
        call: "Bash(command=\"ls\")".to_string(),
        output: "ok".to_string(),
    })
    .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["kind"], "tool_result");
    assert_eq!(v["name"], "Bash");

    // Lifecycle events are not stage content.
    assert!(interactive_event_to_stage_line(&InteractiveEvent::Ready { turn: 0 }).is_none());
    assert!(interactive_event_to_stage_line(&InteractiveEvent::Ended {
        reason: "resumed".to_string(),
    })
    .is_none());
}

#[test]
fn agent_stream_line_derives_kind_from_type() {
    assert_eq!(
        agent_stream_line(r#"{"type":"stream_chunk","text":"hi"}"#)["kind"],
        "text"
    );
    assert_eq!(
        agent_stream_line(r#"{"type":"reasoning_chunk","text":"x"}"#)["kind"],
        "reasoning"
    );
    assert_eq!(
        agent_stream_line(r#"{"type":"tool_result","output":"y"}"#)["kind"],
        "tool_result"
    );
    assert_eq!(
        agent_stream_line(r#"{"type":"stream_reset"}"#)["kind"],
        "stream_reset"
    );
    // Unrecognized event types fall back to `unknown`.
    assert_eq!(
        agent_stream_line(r#"{"type":"assistant","text":"z"}"#)["kind"],
        "unknown"
    );
    // Non-JSON input is tolerated.
    assert_eq!(agent_stream_line("not json")["kind"], "unknown");
}

#[test]
fn exec_stream_line_tags_all_as_stdout() {
    // Historical blobs have interleaved stdout/stderr without tagging;
    // we can't distinguish them, so everything is stdout.
    assert_eq!(exec_stream_line("plain output")["kind"], "stdout");
    assert_eq!(exec_stream_line("plain output")["text"], "plain output");
    assert_eq!(exec_stream_line("[stderr] boom")["kind"], "stdout");
}

#[test]
fn stage_line_envelope_flattens_payload() {
    let v = stage_line_envelope(serde_json::json!({"kind": "text", "text": "hi"}), 3);
    assert_eq!(v["type"], "stage_stream_line");
    assert_eq!(v["kind"], "text");
    assert_eq!(v["text"], "hi");
    assert_eq!(v["offset"], 3);
}

// ── handle_compact ──────────────────────────────────────────

#[tokio::test]
async fn handle_compact_empty_session_errors() {
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new("test-compact-empty".to_string()),
    ));
    let resp = handle_compact(&serde_json::Value::Null, &session).await;
    assert_eq!(resp["type"], "error");
    assert!(resp["message"]
        .as_str()
        .unwrap()
        .contains("no active chat session"));
}

#[tokio::test]
async fn handle_compact_parked_gremlin_no_history_returns_nothing_to_compact() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_root = tmp.path().join("proj");
    std::fs::create_dir_all(&project_root).unwrap();
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new("test-compact-parked".to_string()),
    ));
    let gremlin = create_chat_gremlin(
        &model("cmd/echo"),
        "helpful",
        &project_root,
        &HashMap::new(),
    )
    .unwrap();
    let id = gremlin.id.to_string();
    {
        let mut s = session.lock().await;
        s.chat_ids.insert(id.clone());
        s.chat_gremlins.insert(id.clone(), gremlin);
    }
    let resp = handle_compact(&serde_json::Value::Null, &session).await;
    // Fresh chat gremlin has no conversation history, so there is nothing
    // to compact.
    assert_eq!(resp["type"], "ok");
    assert_eq!(resp["compacted"], false);
    assert_eq!(resp["reason"], "nothing to compact");
}

#[tokio::test]
async fn handle_compact_id_in_ids_but_not_gremlins_errors() {
    let session: super::SessionHandle = Arc::new(tokio::sync::Mutex::new(
        super::ClientSession::new("test-compact-missing".to_string()),
    ));
    {
        let mut s = session.lock().await;
        s.chat_ids.insert("ghost-id".to_string());
        // chat_gremlins intentionally left empty
    }
    let resp = handle_compact(&serde_json::Value::Null, &session).await;
    assert_eq!(resp["type"], "error");
    assert!(resp["message"]
        .as_str()
        .unwrap()
        .contains("chat gremlin not found"));
}
