//! Acceptance tests: inspect requests sent by resumed workers, not just resolvers.
#![allow(clippy::await_holding_lock)]

use super::{spawn_test_actor, TestRegistry};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::supervisor::{handle_resume, test_provider::pending_request};
use crate::test_support::Sandbox;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

const ID: &str = "gr-resume-dispatch";

fn read_json(path: impl AsRef<std::path::Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn sandbox(provider: &TcpListener) -> Sandbox {
    let sandbox = Sandbox::with_providers(
        &json!({"openai": {
            "api-key": "test",
            "base-url": format!("http://{}/v1", provider.local_addr().unwrap())
        }})
        .to_string(),
    );
    fs::write(
        sandbox.join("config/settings.yaml"),
        format!("model-profiles:\n  replacement:\n    model: openai/{}\n    task-models:\n      Scout: openai/{}\n", model(provider, "replacement"), model(provider, "replacement-scout")),
    )
    .unwrap();
    crate::config::init_global().unwrap();
    sandbox
}

// Backend instances are pooled by model URI across tests, not provider settings.
fn model(provider: &TcpListener, name: &str) -> String {
    format!("{name}-{}", provider.local_addr().unwrap().port())
}

fn agent(name: &str) -> Value {
    json!({
        "name": name, "type": "agent", "model": "openai/old-stage",
        "task-models": {"Scout": "openai/old-stage-scout", "Implement": "openai/old-stage-implement"},
        "prompt": ["Complete the dispatch acceptance check."]
    })
}

fn seed(sandbox: &Sandbox, stage: Value) -> PathBuf {
    let project = sandbox.join("project");
    fs::create_dir_all(project.join(".gremlins")).unwrap();
    let source = project.join(".gremlins/dispatch.json5");
    // Resume must use the persisted program, never this source.
    fs::write(&source, "not a definition").unwrap();
    let dir = sandbox.join(format!("state/{ID}-0001"));
    fs::create_dir_all(dir.join("artifacts")).unwrap();
    fs::write(
        dir.join("definition.json"),
        serde_json::to_vec(&json!({
            "__gremlins_expanded__": true,
            "default_model": "openai/old-default",
            "default_task_models": {"Scout": "openai/old-default-scout", "Implement": "openai/old-default-implement"},
            "stages": [stage]
        }))
        .unwrap(),
    )
    .unwrap();
    state::write_state(
        &dir,
        json!({
            "id": ID, "kind": "dispatch", "project_root": project,
            "definition_path": source, "workdir": "", "status": "stopped",
            "stage": "enclosing", "attempt": "original", "stage_inputs": {},
            "enclosing_model": "openai/old-inherited",
            "enclosing_task_models": {"Scout": "openai/old-inherited-scout", "Implement": "openai/old-inherited-implement"}
        })
        .as_object()
        .unwrap(),
    )
    .unwrap();
    fs::write(dir.join("finished"), "original").unwrap();
    dir
}

async fn resume(registry: &TestRegistry) {
    let response =
        handle_resume(&json!({"id": ID, "model": "profile/replacement"}), registry).await;
    assert_eq!(response["type"], "ok", "{response}");
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

// OpenAI uses the Responses API, including for tool-call streaming.
// Closing each HTTP connection avoids depending on keep-alive request parsing.
async fn tool_response(mut stream: TcpStream, name: &str, args: Value) {
    static NEXT_CALL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call_id = format!(
        "call-{}",
        NEXT_CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let item = json!({
        "type": "function_call", "id": format!("fc_{call_id}"),
        "call_id": call_id, "name": name, "arguments": args.to_string(),
        "status": "completed"
    });
    let events = [
        json!({"type": "response.output_item.added", "sequence_number": 1,
            "output_index": 0, "item": {"type": "function_call",
                "id": item["id"], "call_id": item["call_id"], "name": name,
                "arguments": "", "status": "in_progress"}}),
        json!({"type": "response.function_call_arguments.delta", "sequence_number": 2,
            "output_index": 0, "item_id": item["id"], "delta": args.to_string()}),
        json!({"type": "response.output_item.done", "sequence_number": 3,
            "output_index": 0, "item": item}),
        json!({"type": "response.completed", "sequence_number": 4,
            "response": {"id": "resp_test", "object": "response", "model": "test",
                "status": "completed", "output": [item],
                "usage": {"input_tokens": 4, "output_tokens": 5, "total_tokens": 9}}}),
    ];
    let body: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn done(stream: TcpStream) {
    tool_response(stream, "Done", json!({"summary": "dispatch complete"})).await;
}

async fn nested_replacement_dispatch(container: &str) {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = sandbox(&provider);
    // Leaf-only matching and first-prefix matching must not select these decoys.
    fs::write(
        sandbox.join("config/settings.yaml"),
        json!({"model-profiles": {"replacement": {
            "model": format!("openai/{}", model(&provider, "fallback")),
            "model-by-stage": {
                "leaf": format!("openai/{}", model(&provider, "bare-leaf")),
                "enclosing/*": format!("openai/{}", model(&provider, "short-prefix")),
                "enclosing/left/*": format!("openai/{}", model(&provider, "left-prefix")),
                "enclosing/right/*": format!("openai/{}", model(&provider, "right-prefix")),
                "enclosing/left/leaf": format!("openai/{}", model(&provider, "exact"))
            }
        }}})
        .to_string(),
    )
    .unwrap();
    crate::config::init_global().unwrap();
    let mut exact = agent("leaf");
    exact["prompt"] = json!(["NESTED_EXACT_MARKER"]);
    let mut left_prefix = agent("other");
    left_prefix["prompt"] = json!(["NESTED_LEFT_PREFIX_MARKER"]);
    let mut right_prefix = agent("leaf");
    right_prefix["prompt"] = json!(["NESTED_RIGHT_PREFIX_MARKER"]);
    let dir = seed(
        &sandbox,
        json!({
            "name": "enclosing", "type": container, "model": "openai/old-enclosing",
            "body": [
                {"name": "left", "type": "sequence", "body": [exact, left_prefix]},
                {"name": "right", "type": "sequence", "body": [right_prefix]}
            ]
        }),
    );
    let (registry, _shutdown) = spawn_test_actor().await;
    resume(&registry).await;

    // In parallel, hold both branches' first requests to prove child dispatch.
    let mut requests = vec![pending_request(&provider).await];
    if container == "parallel" {
        requests.push(pending_request(&provider).await);
    }
    let mut seen = Vec::new();
    for _ in 0..3 {
        let (request, stream) = if let Some(request) = requests.pop() {
            request
        } else {
            pending_request(&provider).await
        };
        let body = request.to_string();
        let expected = if body.contains("NESTED_EXACT_MARKER") {
            "exact"
        } else if body.contains("NESTED_LEFT_PREFIX_MARKER") {
            "left-prefix"
        } else if body.contains("NESTED_RIGHT_PREFIX_MARKER") {
            "right-prefix"
        } else {
            panic!("unexpected nested dispatch: {request}");
        };
        assert_eq!(request["model"], model(&provider, expected), "{request}");
        seen.push(expected);
        done(stream).await;
    }
    seen.sort_unstable();
    assert_eq!(seen, ["exact", "left-prefix", "right-prefix"]);
    drained(&registry).await;
    assert_eq!(read_json(dir.join("state.json"))["status"], "done");
}

#[tokio::test]
async fn resume_nested_sequence_dispatches_exact_and_longest_prefix_structural_paths() {
    nested_replacement_dispatch("sequence").await;
}

#[tokio::test]
async fn resume_nested_parallel_workers_dispatch_exact_and_longest_prefix_structural_paths() {
    nested_replacement_dispatch("parallel").await;
}

#[tokio::test]
async fn resume_profile_dispatches_mapped_and_unmapped_tasks_ignoring_old_pins() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let selected = model(&provider, "replacement");
    let scout_model = model(&provider, "replacement-scout");
    let sandbox = sandbox(&provider);
    let dir = seed(
        &sandbox,
        json!({
            "name": "enclosing", "type": "sequence", "model": "openai/old-enclosing",
            "task-models": {"Scout": "openai/old-enclosing-scout", "Implement": "openai/old-enclosing-implement"},
            "body": [agent("work")]
        }),
    );
    let (registry, _shutdown) = spawn_test_actor().await;
    resume(&registry).await;

    let (parent, stream) = pending_request(&provider).await;
    assert_eq!(parent["model"], selected);
    tool_response(
        stream,
        "Task",
        json!({"kind": "Scout", "prompt": "SCOUT_DISPATCH_MARKER"}),
    )
    .await;
    let (scout, stream) = pending_request(&provider).await;
    assert_eq!(scout["model"], scout_model, "{scout}");
    assert!(scout.to_string().contains("SCOUT_DISPATCH_MARKER"));
    done(stream).await;

    let (parent, stream) = pending_request(&provider).await;
    assert_eq!(parent["model"], selected);
    tool_response(
        stream,
        "Task",
        json!({"kind": "Implement", "prompt": "IMPLEMENT_DISPATCH_MARKER"}),
    )
    .await;
    let (implement, stream) = pending_request(&provider).await;
    // An unmapped kind uses the selected parent, not an old default or pin.
    assert_eq!(implement["model"], selected, "{implement}");
    assert!(implement.to_string().contains("IMPLEMENT_DISPATCH_MARKER"));
    done(stream).await;
    let (parent, stream) = pending_request(&provider).await;
    assert_eq!(parent["model"], selected);
    done(stream).await;
    drained(&registry).await;
    assert_eq!(read_json(dir.join("state.json"))["status"], "done");
    assert_eq!(
        read_json(dir.join("definition.json"))["resolved_model_selection"]["task_models"],
        json!({"Scout": format!("openai/{scout_model}")})
    );
}

#[tokio::test]
async fn resume_parallel_children_actually_dispatch_with_replacement_selection() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let selected = model(&provider, "replacement");
    let scout_model = model(&provider, "replacement-scout");
    let sandbox = sandbox(&provider);
    let dir = seed(
        &sandbox,
        json!({
            "name": "enclosing", "type": "parallel", "model": "openai/old-enclosing",
            "task-models": {"Scout": "openai/old-parallel-scout", "Implement": "openai/old-parallel-implement"},
            "body": [agent("left"), agent("right")]
        }),
    );
    let (registry, _shutdown) = spawn_test_actor().await;
    resume(&registry).await;

    // Hold both requests before replying: both child workers must really run.
    let (left, left_stream) = pending_request(&provider).await;
    let (right, right_stream) = pending_request(&provider).await;
    assert_eq!(left["model"], selected, "{left}");
    assert_eq!(right["model"], selected, "{right}");
    // Inspect snapshots while both workers are held; success cleans child state.
    for branch in ["left", "right"] {
        let child = Gremlin::from(&format!("{ID}:{branch}")).unwrap();
        assert_eq!(
            read_json(child.state.state_dir().join("definition.json"))["resolved_model_selection"]
                ["task_models"],
            json!({"Scout": format!("openai/{scout_model}")})
        );
    }
    // Exercise task selection inside a spawned parallel worker as well.
    tool_response(
        left_stream,
        "Task",
        json!({"kind": "Scout", "prompt": "PARALLEL_SCOUT_MARKER"}),
    )
    .await;
    let (scout, stream) = pending_request(&provider).await;
    assert_eq!(scout["model"], scout_model, "{scout}");
    assert!(scout.to_string().contains("PARALLEL_SCOUT_MARKER"));
    done(stream).await;
    let (child, stream) = pending_request(&provider).await;
    assert_eq!(child["model"], selected);
    done(stream).await;
    done(right_stream).await;
    drained(&registry).await;
    assert_eq!(read_json(dir.join("state.json"))["status"], "done");
    for branch in ["left", "right"] {
        assert!(Gremlin::from(&format!("{ID}:{branch}")).is_err());
    }
}
