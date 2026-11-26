//! Real-binary passive watch and exact-input workflow fixtures.
#![cfg(unix)]

use gremlins::executor::socket;
use serde_json::{json, Value};
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(150);
type Peer = BufReader<UnixStream>;

struct Fixture {
    root: tempfile::TempDir,
    listener: UnixListener,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("watch-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(root.path().join("state")).unwrap();
        let listener = socket::bind_socket(&root.path().join("state")).unwrap();
        Self { root, listener }
    }

    fn spawn(&self, args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_gremlins"))
            .args(args)
            .env_clear()
            .env("HOME", self.root.path())
            .env("GREMLINS_SANDBOX_ROOT", self.root.path())
            .current_dir(self.root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    async fn request(&self, expected: Value) -> Peer {
        let (stream, _) = timeout(LIMIT, self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut peer = BufReader::new(stream);
        assert_eq!(read(&mut peer).await, json!({"op":"version"}));
        write(
            &mut peer,
            &json!({"type":"ok", "version":gremlins::VERSION}),
        )
        .await;
        assert_eq!(read(&mut peer).await, expected);
        peer
    }

    async fn no_retry(&self) {
        assert!(
            timeout(QUIET, self.listener.accept()).await.is_err(),
            "must not reconnect, broaden filters, or select a sibling"
        );
    }
}

fn request(id: &str, stage: Option<&str>, invocation: Option<&str>) -> Value {
    let mut value = json!({"op":"watch", "scope":"run", "id":id, "replay":true});
    if let Some(stage) = stage {
        value["stage"] = json!(stage);
    }
    if let Some(invocation) = invocation {
        value["stage_run_id"] = json!(invocation);
    }
    value
}

fn ack(request: &Value) -> Value {
    json!({"type":"watched", "scope":"run", "id":request["id"],
        "stage":request["stage"], "stage_run_id":request["stage_run_id"], "replay":true})
}

fn checkpoint(request: &Value, complete: bool) -> Value {
    json!({"type":"replay_checkpoint", "scope":"run", "id":request["id"],
        "stage":request["stage"], "stage_run_id":request["stage_run_id"],
        "complete":complete, "through_sequence":42, "evicted_through_sequence":3})
}

fn boundary(run: &str, invocation: &str, kind: &str, active: bool) -> Value {
    json!({"type":"stage_boundary", "run_id":run, "stage":"work",
        "stage_run_id":invocation, "stage_type":kind, "active":active})
}

fn chunk(run: &str, invocation: &str, text: &str) -> Value {
    json!({"type":"stream_chunk", "run_id":run, "stage":"work",
        "stage_run_id":invocation, "text":text, "is_stderr":false,
        "sequence":17, "future_field":{"preserve":true}})
}

async fn read(peer: &mut Peer) -> Value {
    timeout(LIMIT, socket::read_json_line(peer))
        .await
        .unwrap()
        .unwrap()
        .expect("expected protocol frame")
}

async fn write(peer: &mut Peer, value: &Value) {
    timeout(LIMIT, socket::write_json_line(peer.get_mut(), value))
        .await
        .unwrap()
        .unwrap();
}

async fn send(peer: &mut Peer, frames: &[Value]) {
    for frame in frames {
        write(peer, frame).await;
    }
}

async fn closed(peer: &mut Peer) {
    assert_eq!(
        timeout(LIMIT, socket::read_json_line(peer))
            .await
            .unwrap()
            .unwrap(),
        None,
        "passive close must not write attach, input, detach, pause, or resume"
    );
}

async fn held(child: &mut Child, peer: &mut Peer) {
    assert!(timeout(QUIET, socket::read_json_line(peer)).await.is_err());
    assert!(child.try_wait().unwrap().is_none(), "watch ended too early");
}

async fn finish(child: Child, success: bool) -> Output {
    let output = timeout(LIMIT, child.wait_with_output())
        .await
        .expect("CLI did not exit promptly")
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_frames(output: &Output, frames: &[Value]) {
    let expected: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
    assert_eq!(
        String::from_utf8(output.stdout.clone()).unwrap(),
        expected,
        "watch must preserve JSONL, source IDs, unknown fields, and repeated records"
    );
}

#[tokio::test]
async fn malformed_filters_fail_before_connecting() {
    for flag in ["--stage", "--stage-run-id"] {
        for value in ["", "   "] {
            let fixture = Fixture::new();
            let child = fixture.spawn(&["watch", "parent", flag, value, "--snapshot"]);
            let output = finish(child, false).await;
            assert!(String::from_utf8_lossy(&output.stderr).contains("must be a nonempty string"));
            fixture.no_retry().await;
        }
    }
}

#[tokio::test]
async fn unspecified_filters_accept_missing_or_null_in_ack_and_checkpoint() {
    for omitted in [false, true] {
        for stage in [None, Some("work")] {
            let fixture = Fixture::new();
            let mut args = vec!["watch", "parent", "--snapshot"];
            if let Some(stage) = stage {
                args.extend(["--stage", stage]);
            }
            let child = fixture.spawn(&args);
            let req = request("parent", stage, None);
            let mut peer = fixture.request(req.clone()).await;
            let mut frames = vec![ack(&req), checkpoint(&req, true)];
            if omitted {
                for frame in &mut frames {
                    let object = frame.as_object_mut().unwrap();
                    object.remove("stage_run_id");
                    if stage.is_none() {
                        object.remove("stage");
                    }
                }
            }
            send(&mut peer, &frames).await;
            assert_frames(&finish(child, true).await, &frames);
            closed(&mut peer).await;
        }
    }
}

#[tokio::test]
async fn complete_empty_snapshot_is_distinct_from_incomplete_history() {
    let fixture = Fixture::new();
    let child = fixture.spawn(&["watch", "parent", "--stage", "absent", "--snapshot"]);
    let req = request("parent", Some("absent"), None);
    let mut peer = fixture.request(req.clone()).await;
    let frames = vec![ack(&req), checkpoint(&req, true)];
    send(&mut peer, &frames).await;
    assert_frames(&finish(child, true).await, &frames);
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn closing_stdout_releases_idle_watch_without_control_traffic() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&["watch", "parent"]);
    let req = request("parent", None, None);
    let mut peer = fixture.request(req.clone()).await;
    send(&mut peer, &[ack(&req), checkpoint(&req, true)]).await;
    held(&mut child, &mut peer).await;
    drop(child.stdout.take());
    finish(child, true).await;
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn parent_snapshot_keeps_parallel_source_ids_gap_and_repeated_stage_until_checkpoint() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&["watch", "parent", "--snapshot"]);
    let req = request("parent", None, None);
    let mut peer = fixture.request(req.clone()).await;
    let mut frames = vec![
        ack(&req),
        json!({"type":"history_gap", "scope":"run", "id":"parent",
            "through_sequence":42, "evicted_through_sequence":3}),
        boundary("parent:left", "A", "agent", true),
        boundary("parent:right", "E", "exec", true),
        chunk("parent:left", "A", "agent answer"),
        json!({"type":"exec_output", "run_id":"parent:right", "stage":"work",
            "stage_run_id":"E", "text":"exec output", "is_stderr":true}),
        boundary("parent:left", "A", "agent", false),
        json!({"type":"run_completed", "run_id":"parent:left"}),
        boundary("parent:left", "B", "agent", true),
    ];
    send(&mut peer, &frames).await;
    held(&mut child, &mut peer).await;
    frames.push(checkpoint(&req, false));
    write(&mut peer, frames.last().unwrap()).await;
    // Neither the server socket nor listener is closed to make snapshot exit.
    let output = finish(child, true).await;
    assert_frames(&output, &frames);
    closed(&mut peer).await;
    fixture.no_retry().await;
}

// The mock supervisor filters a catalog, rather than feeding unrelated events
// to the CLI (filtering belongs to the server, not the JSONL renderer).
fn matching(req: &Value, catalog: &[Value]) -> Vec<Value> {
    catalog
        .iter()
        .filter(|frame| {
            frame["run_id"] == req["id"]
                && (req["stage"].is_null() || frame["stage"] == req["stage"])
                && (req["stage_run_id"].is_null() || frame["stage_run_id"] == req["stage_run_id"])
        })
        .cloned()
        .collect()
}

#[tokio::test]
async fn exact_follow_replays_then_lives_without_siblings_and_closes_at_invocation_eof() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&[
        "watch",
        "parent:left",
        "--stage",
        "work",
        "--stage-run-id",
        "A",
    ]);
    let req = request("parent:left", Some("work"), Some("A"));
    let mut peer = fixture.request(req.clone()).await;
    let replay = matching(
        &req,
        &[
            boundary("parent:left", "A", "agent", true),
            chunk("parent:left", "A", "same text"),
            chunk("parent:left", "B", "sibling secret"),
            chunk("parent:right", "A", "parallel sibling secret"),
            chunk("other-run", "A", "other run secret"),
        ],
    );
    let mut frames = vec![ack(&req)];
    frames.extend(replay);
    frames.push(checkpoint(&req, true));
    send(&mut peer, &frames).await;
    held(&mut child, &mut peer).await;
    let live = matching(
        &req,
        &[
            chunk("parent:left", "B", "replacement secret"),
            chunk("parent:left", "A", "same text"),
            boundary("parent:left", "A", "agent", false),
        ],
    );
    send(&mut peer, &live).await;
    frames.extend(live);
    peer.get_mut().shutdown().await.unwrap();
    let output = finish(child, true).await;
    assert_frames(&output, &frames);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn stage_name_watch_survives_one_invocation_ending() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&["watch", "run", "--stage", "work"]);
    let req = request("run", Some("work"), None);
    let mut peer = fixture.request(req.clone()).await;
    let mut frames = vec![
        ack(&req),
        boundary("run", "A", "agent", true),
        checkpoint(&req, true),
        boundary("run", "A", "agent", false),
    ];
    send(&mut peer, &frames).await;
    held(&mut child, &mut peer).await;
    let later = vec![
        boundary("run", "B", "agent", true),
        chunk("run", "B", "later invocation"),
        boundary("run", "B", "agent", false),
        json!({"type":"run_completed", "run_id":"run"}),
    ];
    send(&mut peer, &later).await;
    frames.extend(later);
    peer.get_mut().shutdown().await.unwrap();
    assert_frames(&finish(child, true).await, &frames);
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn interrupted_watcher_only_sends_version_and_watch() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&["watch", "run"]);
    let req = request("run", None, None);
    let mut peer = fixture.request(req.clone()).await;
    send(&mut peer, &[ack(&req), checkpoint(&req, true)]).await;
    held(&mut child, &mut peer).await;
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = finish(child, false).await;
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(output.status.signal(), Some(libc::SIGINT));
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn malformed_denied_and_wrong_acknowledgments_fail_without_broadening() {
    for case in [
        "malformed",
        "denied",
        "type",
        "scope",
        "id",
        "replay",
        "stage",
        "stage_run_id",
        "missing_filter",
    ] {
        let fixture = Fixture::new();
        let child = fixture.spawn(&[
            "watch",
            "run",
            "--stage",
            "work",
            "--stage-run-id",
            "A",
            "--snapshot",
        ]);
        let req = request("run", Some("work"), Some("A"));
        let mut peer = fixture.request(req.clone()).await;
        let mut frame = ack(&req);
        match case {
            "malformed" => peer.get_mut().write_all(b"not-json\n").await.unwrap(),
            "denied" => {
                write(
                    &mut peer,
                    &json!({"type":"error", "message":"fixture denied"}),
                )
                .await
            }
            "missing_filter" => {
                frame.as_object_mut().unwrap().remove("stage_run_id");
                write(&mut peer, &frame).await;
            }
            key => {
                frame[key] = if key == "replay" {
                    json!(false)
                } else {
                    json!("wrong")
                };
                write(&mut peer, &frame).await;
            }
        }
        let output = finish(child, false).await;
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if case == "malformed" {
                "malformed watch frame"
            } else if case == "denied" {
                "fixture denied"
            } else {
                "unexpected watch acknowledgment"
            }),
            "{case}: {error}"
        );
        closed(&mut peer).await;
        fixture.no_retry().await;
    }
}

#[tokio::test]
async fn documented_snapshot_exact_send_watch_response_and_ended_rejection_workflow() {
    let fixture = Fixture::new();
    let snapshot = fixture.spawn(&["watch", "parent", "--snapshot"]);
    let req = request("parent", None, None);
    let mut peer = fixture.request(req.clone()).await;
    let frames = vec![
        ack(&req),
        boundary("parent:left", "old", "agent", false),
        boundary("parent:right", "E", "exec", true),
        boundary("parent:left", "A", "agent", true),
        boundary("parent:left", "B", "agent", true),
        checkpoint(&req, true),
    ];
    send(&mut peer, &frames).await;
    let output = finish(snapshot, true).await;
    closed(&mut peer).await;
    let snapshot: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let target = snapshot
        .iter()
        .find(|frame| {
            frame["type"] == "stage_boundary"
                && frame["active"] == true
                && frame["stage_type"] == "agent"
        })
        .unwrap();
    let run = target["run_id"].as_str().unwrap();
    let invocation = target["stage_run_id"].as_str().unwrap();
    assert_eq!((run, invocation), ("parent:left", "A"));
    let exec = snapshot
        .iter()
        .find(|frame| frame["stage_type"] == "exec")
        .unwrap();
    assert_eq!(exec["run_id"], "parent:right");
    assert_ne!(target["run_id"], exec["run_id"]);

    let mut debug = fixture.spawn(&["debug", run, invocation]);
    debug
        .stdin
        .take()
        .unwrap()
        .write_all(b"one exact message\n")
        .await
        .unwrap();
    let (stream, _) = timeout(LIMIT, fixture.listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut input = BufReader::new(stream);
    assert_eq!(read(&mut input).await, json!({"op":"version"}));
    write(
        &mut input,
        &json!({"type":"ok", "version":gremlins::VERSION}),
    )
    .await;
    let bind = read(&mut input).await;
    let bind_id = bind["req_id"].as_str().unwrap();
    assert_eq!(
        bind,
        json!({"op":"input", "id":run, "stage_run_id":invocation, "req_id":bind_id})
    );
    write(
        &mut input,
        &json!({"type":"input_bound", "id":run, "stage_run_id":invocation, "req_id":bind_id}),
    )
    .await;
    let message = read(&mut input).await;
    let message_id = message["req_id"].as_str().unwrap();
    assert_ne!(bind_id, message_id);
    assert_eq!(
        message,
        json!({"cmd":"message", "text":"one exact message", "req_id":message_id})
    );
    write(&mut input, &json!({"type":"queued", "req_id":message_id})).await;
    held(&mut debug, &mut input).await;
    write(
        &mut input,
        &json!({"type":"delivered", "req_id":message_id}),
    )
    .await;
    finish(debug, true).await;
    closed(&mut input).await;

    let watch = fixture.spawn(&["watch", run, "--stage-run-id", invocation]);
    let req = request(run, None, Some(invocation));
    let mut peer = fixture.request(req.clone()).await;
    let response = vec![
        ack(&req),
        checkpoint(&req, true),
        chunk(run, invocation, "tagged response"),
        boundary(run, invocation, "agent", false),
    ];
    send(&mut peer, &response).await;
    peer.get_mut().shutdown().await.unwrap();
    assert_frames(&finish(watch, true).await, &response);
    closed(&mut peer).await;

    // B remains writable, but rejection of ended A must never retarget B.
    let mut rejected = fixture.spawn(&["debug", run, invocation]);
    rejected
        .stdin
        .take()
        .unwrap()
        .write_all(b"must not reach sibling B\n")
        .await
        .unwrap();
    let (stream, _) = timeout(LIMIT, fixture.listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut input = BufReader::new(stream);
    assert_eq!(read(&mut input).await, json!({"op":"version"}));
    write(
        &mut input,
        &json!({"type":"ok", "version":gremlins::VERSION}),
    )
    .await;
    let bind = read(&mut input).await;
    assert_eq!(
        bind,
        json!({"op":"input", "id":run, "stage_run_id":invocation, "req_id":bind["req_id"]})
    );
    write(
        &mut input,
        &json!({"type":"error", "id":run, "stage_run_id":invocation,
        "req_id":bind["req_id"], "code":"ended", "message":"A ended; B remains active"}),
    )
    .await;
    let output = finish(rejected, false).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("ended"));
    closed(&mut input).await;
    fixture.no_retry().await;
}
