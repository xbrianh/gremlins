//! End-to-end command dispatch and immutable selection history across restarts.
#![allow(clippy::await_holding_lock)]

use super::resume_context::{drained, launch, read_json, source, stopped};
use super::spawn_test_actor;
use crate::config;
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::supervisor::handle_resume;
use crate::test_support::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn model(log: &Path, label: &str) -> String {
    format!(
        "cmd/sh -c 'cat >/dev/null; echo {label} >> {}; echo done'",
        log.display()
    )
}

fn settings(sandbox: &Sandbox, log: &Path, edited: bool) {
    let label = if edited { "edited" } else { "B" };
    fs::create_dir_all(sandbox.join("config")).unwrap();
    fs::write(
        sandbox.join("config/settings.yaml"),
        serde_json::to_vec(&json!({
            "model-profiles": {
                "A": {"model": model(log, "A")},
                "B": {"model": model(log, label), "model-by-stage": {
                    "prefix-exact": model(log, &format!("{label}-exact")),
                    "pre*": model(log, &format!("{label}-short")),
                    "prefix*": model(log, &format!("{label}-long"))
                }},
                "C": {"model": model(log, "C")}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    config::init_global().unwrap();
}

fn rewind(dir: &Path) {
    stopped(dir);
    let mut data = read_json(dir.join("state.json"));
    data["stage"] = json!("gate");
    state::write_state(dir, data.as_object().unwrap()).unwrap();
}

async fn run(id: &str, selection: Option<&str>, stage: Option<&str>) {
    // A fresh actor models daemon restart; no previous live handle survives.
    let (registry, _shutdown) = spawn_test_actor().await;
    let mut request = json!({"id": id});
    if let Some(selection) = selection {
        request["model"] = json!(selection);
    }
    if let Some(stage) = stage {
        request["stage"] = json!(stage);
    }
    let response = handle_resume(&request, &registry).await;
    assert_eq!(response["type"], "ok", "{response}");
    drained(&registry, id).await;
}

fn assert_history(dir: &Path, snapshots: &[&[u8]]) {
    let data = read_json(dir.join("state.json"));
    let history = data["definition_history"].as_array().unwrap();
    assert_eq!(history.len(), snapshots.len());
    let state = Gremlin::from(data["id"].as_str().unwrap()).unwrap().state;
    for (index, (entry, expected)) in history.iter().zip(snapshots).enumerate() {
        assert_eq!(
            state
                .read_definition_revision(entry["id"].as_str().unwrap())
                .unwrap(),
            *expected
        );
        if index == 0 {
            assert!(entry["predecessor"].is_null());
        } else {
            assert_eq!(entry["predecessor"], history[index - 1]["id"]);
        }
    }
    assert_eq!(data["definition_revision"], history.last().unwrap()["id"]);
}

#[tokio::test]
async fn ordinary_resume_dispatch_preserves_revision_chain() {
    profile_history(false).await;
}

#[tokio::test]
async fn rewind_resume_dispatch_preserves_revision_chain_and_parent() {
    profile_history(true).await;
}

async fn profile_history(fork_c: bool) {
    let sandbox = Sandbox::new();
    let log = sandbox.join("dispatch.log");
    let gate = sandbox.join("ready");
    settings(&sandbox, &log, false);
    let agent = |name: &str| json!({"name": name, "type": "agent", "model": model(&log, "old-stage"), "prompt": ["Finish."]});
    fs::create_dir_all(sandbox.join("project/.gremlins")).unwrap();
    fs::write(sandbox.join("project/.gremlins/Finish."), "Finish.").unwrap();
    let path = source(
        &sandbox,
        json!({"default_model": model(&log, "old-default"), "stages": [
            {"name": "gate", "type": "exec", "options": {"cmds": [format!("test -f {}", gate.display())]}},
            {"name": "enclosing", "type": "sequence", "model": model(&log, "old-enclosing"), "body": [
                agent("prefix-exact"), agent("prefix-leaf"), agent("pre-leaf"), agent("unmatched")
            ]}
        ]}),
    );
    let (registry, shutdown) = spawn_test_actor().await;
    let id = launch(&registry, &path, Some("profile/A")).await;
    drop(shutdown);
    let dir = Gremlin::from(&id).unwrap().state.state_dir();
    // Model a pre-revision run, including noncanonical snapshot whitespace.
    let mut legacy = read_json(dir.join("state.json"));
    legacy
        .as_object_mut()
        .unwrap()
        .remove("definition_revision");
    legacy.as_object_mut().unwrap().remove("definition_history");
    state::write_state(&dir, legacy.as_object().unwrap()).unwrap();
    fs::remove_dir_all(dir.join("definition-revisions")).unwrap();
    let a = format!(
        "{}\n\n",
        serde_json::to_string_pretty(&read_json(dir.join("definition.json"))).unwrap()
    )
    .into_bytes();
    fs::write(dir.join("definition.json"), &a).unwrap();
    fs::write(&gate, "ready").unwrap();
    run(&id, Some("profile/B"), None).await;
    let b = fs::read(dir.join("definition.json")).unwrap();
    assert_history(&dir, &[&a, &b]);
    let history = dir.join("artifacts/acceptance-history");
    fs::write(&history, b"historical bytes\n\0").unwrap();
    let expected = "B-exact\nB-long\nB-short\nB\n";
    assert_eq!(fs::read_to_string(&log).unwrap(), expected);

    settings(&sandbox, &log, true);
    rewind(&dir);
    run(&id, None, None).await;
    assert_eq!(fs::read(dir.join("definition.json")).unwrap(), b);
    fs::write(sandbox.join("config/settings.yaml"), "{}\n").unwrap();
    config::init_global().unwrap();
    rewind(&dir);
    run(&id, None, None).await;
    assert_eq!(fs::read(dir.join("definition.json")).unwrap(), b);
    assert_eq!(fs::read_to_string(&log).unwrap(), expected.repeat(3));

    settings(&sandbox, &log, false);
    rewind(&dir);
    let parent_state = fs::read(dir.join("state.json")).unwrap();
    run(&id, Some("profile/C"), fork_c.then_some("gate")).await;
    let fork = Gremlin::from(&id).unwrap().state.state_dir();
    if fork_c {
        assert_ne!(fork, dir);
        assert_eq!(fs::read(dir.join("state.json")).unwrap(), parent_state);
        assert_eq!(fs::read(dir.join("definition.json")).unwrap(), b);
        assert_history(&dir, &[&a, &b]);
    } else {
        assert_eq!(fork, dir);
    }
    assert_eq!(fs::read(&history).unwrap(), b"historical bytes\n\0");
    let c = fs::read(fork.join("definition.json")).unwrap();
    let selected: Value = serde_json::from_slice(&c).unwrap();
    assert_eq!(
        selected["resolved_model_selection"]["requested"],
        "profile/C"
    );
    assert_history(&fork, &[&a, &b, &c]);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("{}{}", expected.repeat(3), "C\n".repeat(4))
    );

    fs::write(sandbox.join("config/settings.yaml"), "{}\n").unwrap();
    config::init_global().unwrap();
    rewind(&fork);
    run(&id, None, None).await;
    assert_eq!(fs::read(fork.join("definition.json")).unwrap(), c);
    assert_history(&fork, &[&a, &b, &c]);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("{}{}", expected.repeat(3), "C\n".repeat(8))
    );
    assert_eq!(fs::read(&history).unwrap(), b"historical bytes\n\0");
    if fork_c {
        assert_eq!(fs::read(dir.join("state.json")).unwrap(), parent_state);
        assert_eq!(fs::read(dir.join("definition.json")).unwrap(), b);
    }

    rewind(&fork);
    let concrete = model(&log, "concrete");
    run(&id, Some(&concrete), None).await;
    let snapshot = fs::read(fork.join("definition.json")).unwrap();
    rewind(&fork);
    run(&id, None, None).await;
    assert_eq!(fs::read(fork.join("definition.json")).unwrap(), snapshot);
    assert!(fs::read_to_string(&log)
        .unwrap()
        .ends_with(&"concrete\n".repeat(8)));
}
