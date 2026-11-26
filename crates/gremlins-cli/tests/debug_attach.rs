//! Real-binary Unix fixtures for exact, input-only `debug run A` sessions.
#![cfg(unix)]

use gremlins::executor::socket;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
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
    listener: Arc<UnixListener>,
}

impl Fixture {
    fn new() -> Self {
        // Bind the child's resolved state root directly. Never mutate the test
        // process environment: fixtures can run concurrently, even on macOS.
        let root = tempfile::Builder::new()
            .prefix("debug-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(root.path().join("state")).unwrap();
        let listener = socket::bind_socket(&root.path().join("state")).unwrap();
        Self {
            root,
            listener: Arc::new(listener),
        }
    }

    fn command(&self) -> Command {
        self.program(env!("CARGO_BIN_EXE_gremlins"))
    }

    fn program(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", self.root.path())
            .env("GREMLINS_SANDBOX_ROOT", self.root.path())
            .current_dir(self.root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    fn spawn(&self) -> Child {
        self.command().args(["debug", "run", "A"]).spawn().unwrap()
    }

    async fn request(&self) -> (Peer, Value) {
        let (stream, _) = timeout(LIMIT, self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut peer = BufReader::new(stream);
        assert_eq!(read(&mut peer).await, json!({"op":"version"}));
        write(&mut peer, json!({"type":"ok", "version":gremlins::VERSION})).await;
        let request = read(&mut peer).await;
        let req_id = request["req_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .expect("bind req_id");
        assert!(
            !req_id.contains("debug"),
            "debug leaked into wire correlation ID"
        );
        assert_eq!(
            request,
            json!({"op":"input", "id":"run", "stage_run_id":"A", "req_id":req_id})
        );
        (peer, request)
    }

    async fn bound(&self) -> (Peer, String) {
        let (mut peer, request) = self.request().await;
        write(&mut peer, ack(&request)).await;
        (peer, request["req_id"].as_str().unwrap().to_owned())
    }

    // A has ended; B is now the available execution of the same stage.
    // Unlike a reason string, this server can actually bind B and deliver input.
    fn replacement_b(&self) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Vec<Value>>>) {
        let listener = Arc::clone(&self.listener);
        let traffic = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&traffic);
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                observed
                    .lock()
                    .unwrap()
                    .push(json!({"connection":"replacement B"}));
                let mut peer = BufReader::new(stream);
                while let Ok(Ok(Some(frame))) =
                    timeout(LIMIT, socket::read_json_line(&mut peer)).await
                {
                    observed.lock().unwrap().push(frame.clone());
                    let reply = if frame["op"] == "version" {
                        json!({"type":"ok", "version":gremlins::VERSION})
                    } else if frame["op"] == "input" {
                        if frame["stage_run_id"] == "A" {
                            json!({"type":"ended", "id":"run", "stage_run_id":"A", "req_id":frame["req_id"], "reason":"A ended; B is current"})
                        } else {
                            json!({"type":"input_bound", "id":"run", "stage_run_id":"B", "req_id":frame["req_id"]})
                        }
                    } else {
                        json!({"type":"delivered", "req_id":frame["req_id"]})
                    };
                    if socket::write_json_line(peer.get_mut(), &reply)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        (task, traffic)
    }

    async fn no_retry(&self) {
        assert!(
            timeout(QUIET, self.listener.accept()).await.is_err(),
            "unexpected replacement connection (B must not be selected)"
        );
    }
}

async fn assert_no_replacement(
    (task, traffic): (tokio::task::JoinHandle<()>, Arc<Mutex<Vec<Value>>>),
) {
    tokio::time::sleep(QUIET).await;
    task.abort();
    let _ = task.await;
    let traffic = traffic.lock().unwrap();
    assert!(
        traffic.is_empty(),
        "unexpected traffic to replacement B: {traffic:?}"
    );
}

fn ack(request: &Value) -> Value {
    json!({"type":"input_bound", "id":request["id"], "stage_run_id":request["stage_run_id"], "req_id":request["req_id"]})
}

async fn read(peer: &mut Peer) -> Value {
    timeout(LIMIT, socket::read_json_line(peer))
        .await
        .unwrap()
        .unwrap()
        .expect("expected protocol frame")
}

async fn write(peer: &mut Peer, frame: Value) {
    timeout(LIMIT, socket::write_json_line(peer.get_mut(), &frame))
        .await
        .unwrap()
        .unwrap();
}

async fn receipt(peer: &mut Peer, command: &Value, kind: &str) {
    write(
        peer,
        json!({"type":kind, "req_id":command["req_id"], "message":"fixture rejected command"}),
    )
    .await;
}

fn command(frame: Value, expected: Value, ids: &mut HashSet<String>) -> Value {
    let id = frame["req_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .expect("command req_id")
        .to_owned();
    assert!(
        !id.contains("debug"),
        "debug leaked into wire correlation ID: {id}"
    );
    assert!(ids.insert(id), "req_id must be unique within connection");
    let mut payload = frame.clone();
    payload.as_object_mut().unwrap().remove("req_id");
    assert_eq!(
        payload, expected,
        "no legacy op, mode, selectors, or watch/attach traffic"
    );
    frame
}

async fn closed(peer: &mut Peer) {
    assert_eq!(
        timeout(LIMIT, socket::read_json_line(peer))
            .await
            .unwrap()
            .unwrap(),
        None,
        "local close must not send resume, bail, quit, or other controls"
    );
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

async fn held(child: &mut Child, peer: &mut Peer) {
    assert!(
        timeout(QUIET, socket::read_json_line(peer)).await.is_err(),
        "must await terminal receipt before next command or close"
    );
    assert!(child.try_wait().unwrap().is_none(), "queued is not success");
}

#[tokio::test]
async fn binding_rejection_matrix_never_sends_messages_or_retries() {
    for case in [
        "controller_busy",
        "denied",
        "read_only",
        "ended",
        "id",
        "stage_run_id",
        "req_id",
    ] {
        let fixture = Fixture::new();
        let mut child = fixture.spawn();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"must not reach A or B\n").await.unwrap();
        let (mut peer, request) = fixture.request().await;
        let reply = if matches!(case, "id" | "stage_run_id" | "req_id") {
            let mut reply = ack(&request);
            reply[case] = json!("B");
            reply
        } else if case == "ended" {
            json!({"type":"ended", "id":"run", "stage_run_id":"A", "req_id":request["req_id"], "reason":"ended"})
        } else {
            json!({"type":"error", "code":case, "message":case, "req_id":request["req_id"]})
        };
        write(&mut peer, reply).await;
        let output = finish(child, false).await;
        if matches!(case, "controller_busy" | "denied" | "read_only") {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(case),
                "missing rejection reason {case}: {stderr}"
            );
        }
        closed(&mut peer).await;
        // No replacement connection may follow a rejected binding.
        fixture.no_retry().await;
        drop(stdin);
    }
}

#[tokio::test]
async fn run_only_is_usage_error_without_connecting() {
    let fixture = Fixture::new();
    let child = fixture.command().args(["debug", "run"]).spawn().unwrap();
    let output = finish(child, false).await;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Usage:"), "{stderr}");
    assert!(stderr.contains("STAGE_RUN_ID"), "{stderr}");
    fixture.no_retry().await;
}

#[tokio::test]
async fn eof_waits_for_delivered_not_queued() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"message\n").await.unwrap();
    drop(stdin);
    let (mut peer, bind_id) = fixture.bound().await;
    let mut ids = HashSet::from([bind_id]);
    let message = command(
        read(&mut peer).await,
        json!({"cmd":"message", "text":"message"}),
        &mut ids,
    );
    receipt(&mut peer, &message, "queued").await;
    held(&mut child, &mut peer).await;
    receipt(&mut peer, &message, "delivered").await;
    finish(child, true).await;
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn messages_and_controls_preserve_order_and_continue_keeps_session_open() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn();
    let mut stdin = child.stdin.take().unwrap();
    let (mut peer, bind_id) = fixture.bound().await;
    let mut ids = HashSet::from([bind_id]);
    for (line, expected) in [
        ("first\n", json!({"cmd":"message","text":"first"})),
        ("second\n", json!({"cmd":"message","text":"second"})),
        ("/pause\n", json!({"cmd":"pause"})),
        ("/step\n", json!({"cmd":"step"})),
        ("/continue\n", json!({"cmd":"resume"})),
    ] {
        stdin.write_all(line.as_bytes()).await.unwrap();
        let frame = command(read(&mut peer).await, expected, &mut ids);
        receipt(&mut peer, &frame, "queued").await;
        held(&mut child, &mut peer).await;
        receipt(&mut peer, &frame, "delivered").await;
    }
    held(&mut child, &mut peer).await;
    stdin.write_all(b"/exit\n").await.unwrap();
    finish(child, true).await;
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn resume_can_unblock_queued_message_and_exit_drains_all_receipts() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn();
    let mut stdin = child.stdin.take().unwrap();
    // All lines exist before binding completes; none may be drained or lost.
    stdin
        .write_all(b"message\n/continue\n/exit\n")
        .await
        .unwrap();
    let (mut peer, bind_id) = fixture.bound().await;
    let mut ids = HashSet::from([bind_id]);
    let message = command(
        read(&mut peer).await,
        json!({"cmd":"message", "text":"message"}),
        &mut ids,
    );
    receipt(&mut peer, &message, "queued").await;
    let resume = command(read(&mut peer).await, json!({"cmd":"resume"}), &mut ids);
    receipt(&mut peer, &resume, "delivered").await;
    held(&mut child, &mut peer).await;
    receipt(&mut peer, &message, "delivered").await;
    finish(child, true).await;
    closed(&mut peer).await;
}

#[tokio::test]
async fn batched_pause_continue_preserves_wire_order_without_receipts() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"/pause\n/continue\n/exit\n")
        .await
        .unwrap();
    drop(stdin);
    let (mut peer, bind_id) = fixture.bound().await;
    let mut ids = HashSet::from([bind_id]);
    let pause = command(read(&mut peer).await, json!({"cmd":"pause"}), &mut ids);
    // Deliberately withhold even a queued receipt until both controls arrive.
    let resume = command(read(&mut peer).await, json!({"cmd":"resume"}), &mut ids);
    held(&mut child, &mut peer).await;
    receipt(&mut peer, &resume, "delivered").await;
    held(&mut child, &mut peer).await;
    receipt(&mut peer, &pause, "delivered").await;
    finish(child, true).await;
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn closing_drains_final_receipt_before_ended_or_disconnect() {
    for close in ["eof", "exit", "quit"] {
        for terminal in ["ended", "disconnect"] {
            for outcome in ["delivered", "dropped", "error", "missing"] {
                let fixture = Fixture::new();
                let mut child = fixture.spawn();
                let mut stdin = child.stdin.take().unwrap();
                let input = match close {
                    "eof" => "message\n",
                    "exit" => "message\n/exit\n",
                    _ => "/quit\n",
                };
                stdin.write_all(input.as_bytes()).await.unwrap();
                drop(stdin);
                let (mut peer, bind_id) = fixture.bound().await;
                let expected = if close == "quit" {
                    json!({"cmd":"bail", "reason":"operator stopped"})
                } else {
                    json!({"cmd":"message", "text":"message"})
                };
                let frame = command(
                    read(&mut peer).await,
                    expected,
                    &mut HashSet::from([bind_id]),
                );
                receipt(&mut peer, &frame, "queued").await;
                held(&mut child, &mut peer).await;
                let mut bytes = Vec::new();
                if outcome != "missing" {
                    bytes.extend(serde_json::to_vec(&json!({"type":outcome, "req_id":frame["req_id"], "message":"fixture final rejection"})).unwrap());
                    bytes.push(b'\n');
                }
                if terminal == "ended" {
                    bytes.extend(serde_json::to_vec(&json!({"type":"ended", "id":"run", "stage_run_id":"A", "reason":"A ended"})).unwrap());
                    bytes.push(b'\n');
                }
                // One write makes delivered+ended available to the reader together.
                peer.get_mut().write_all(&bytes).await.unwrap();
                if terminal == "disconnect" {
                    // Half-close the server's output: still observe any forbidden input.
                    peer.get_mut().shutdown().await.unwrap();
                }
                let output = finish(child, outcome == "delivered").await;
                if outcome != "delivered" {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let reason = match outcome {
                        "dropped" => "not delivered",
                        "error" => "fixture final rejection",
                        _ if terminal == "ended" => "A ended",
                        _ => "disconnected",
                    };
                    assert!(
                        stderr.contains(reason),
                        "{close}/{terminal}/{outcome}: {stderr}"
                    );
                }
                closed(&mut peer).await;
                fixture.no_retry().await;
            }
        }
    }
}

#[tokio::test]
async fn quit_bails_with_reason_and_exits_only_after_receipt() {
    for (line, reason) in [
        ("/quit\n", "operator stopped"),
        ("/quit fixture reason\n", "fixture reason"),
    ] {
        let fixture = Fixture::new();
        let mut child = fixture.spawn();
        let mut stdin = child.stdin.take().unwrap();
        let (mut peer, bind_id) = fixture.bound().await;
        stdin.write_all(line.as_bytes()).await.unwrap();
        let frame = command(
            read(&mut peer).await,
            json!({"cmd":"bail", "reason":reason}),
            &mut HashSet::from([bind_id]),
        );
        receipt(&mut peer, &frame, "queued").await;
        held(&mut child, &mut peer).await;
        receipt(&mut peer, &frame, "delivered").await;
        finish(child, true).await;
        closed(&mut peer).await;
        fixture.no_retry().await;
    }
}

#[tokio::test]
async fn exit_empty_lines_and_eof_are_local_only() {
    for input in ["", "\n  \n", "/exit\n"] {
        let fixture = Fixture::new();
        let mut child = fixture.spawn();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        drop(stdin);
        let (mut peer, _) = fixture.bound().await;
        finish(child, true).await;
        closed(&mut peer).await;
        fixture.no_retry().await;
    }
}

#[tokio::test]
async fn terminal_failures_never_retarget_replacement_b() {
    for case in ["dropped", "error", "ended", "disconnect", "wrong_receipt"] {
        let fixture = Fixture::new();
        let mut child = fixture.spawn();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"for A\n").await.unwrap();
        drop(stdin); // A one-line EOF must still drain the queued receipt.
        let (mut peer, bind_id) = fixture.bound().await;
        let frame = command(
            read(&mut peer).await,
            json!({"cmd":"message", "text":"for A"}),
            &mut HashSet::from([bind_id]),
        );
        receipt(&mut peer, &frame, "queued").await;
        held(&mut child, &mut peer).await;
        let replacement = fixture.replacement_b();
        match case {
            "disconnect" => {
                drop(peer);
                let output = finish(child, false).await;
                assert!(String::from_utf8_lossy(&output.stderr).contains("disconnected"));
            }
            _ => {
                match case {
                    "ended" => write(&mut peer, json!({"type":"ended", "id":"run", "stage_run_id":"A", "reason":"replaced by B"})).await,
                    "wrong_receipt" => write(&mut peer, json!({"type":"delivered", "req_id":"B"})).await,
                    kind => receipt(&mut peer, &frame, kind).await,
                }
                let output = finish(child, false).await;
                let stderr = String::from_utf8_lossy(&output.stderr);
                let expected = match case {
                    "ended" => "replaced by B",
                    "wrong_receipt" => "mismatched input receipt",
                    "dropped" => "not delivered",
                    _ => "fixture rejected command",
                };
                assert!(stderr.contains(expected), "{stderr}");
                closed(&mut peer).await;
            }
        }
        assert_no_replacement(replacement).await;
        fixture.no_retry().await;
    }
}

#[tokio::test]
async fn final_rejection_precedes_session_failure_with_stdin_open() {
    for terminal in ["ended", "disconnect"] {
        let fixture = Fixture::new();
        let mut child = fixture.spawn();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"message\n").await.unwrap();
        let (mut peer, _) = fixture.bound().await;
        let frame = read(&mut peer).await;
        let mut bytes = serde_json::to_vec(&json!({
            "type":"error", "req_id":frame["req_id"], "message":"actual command rejection"
        }))
        .unwrap();
        bytes.push(b'\n');
        if terminal == "ended" {
            bytes.extend(
                serde_json::to_vec(&json!({
                    "type":"ended", "reason":"later session termination"
                }))
                .unwrap(),
            );
            bytes.push(b'\n');
        }
        peer.get_mut().write_all(&bytes).await.unwrap();
        if terminal == "disconnect" {
            peer.get_mut().shutdown().await.unwrap();
        }
        let output = finish(child, false).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("actual command rejection"), "{stderr}");
        closed(&mut peer).await;
        drop(stdin);
    }
}

#[tokio::test]
async fn excessive_outstanding_commands_fail_without_waiting_for_receipts() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn();
    let mut stdin = child.stdin.take().unwrap();
    let (peer, _) = fixture.bound().await;
    stdin
        .write_all("message\n".repeat(129).as_bytes())
        .await
        .unwrap();
    let output = finish(child, false).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("outstanding input limit"));
    // Failure is explicit, not a wait for a resume hidden behind the limit.
    drop(stdin);
    drop(peer);
    fixture.no_retry().await;
}

#[tokio::test]
async fn oversized_stdin_line_fails_clearly() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn();
    let mut stdin = child.stdin.take().unwrap();
    let (mut peer, _) = fixture.bound().await;
    stdin.write_all(&vec![b'x'; 64 * 1024 + 1]).await.unwrap();
    let output = finish(child, false).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("stdin line exceeds"));
    closed(&mut peer).await;
    drop(stdin);
}

#[tokio::test]
async fn idle_ended_is_failure_even_with_stdin_open() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn();
    let stdin = child.stdin.take().unwrap();
    let (mut peer, _) = fixture.bound().await;
    let replacement = fixture.replacement_b();
    write(
        &mut peer,
        json!({"type":"ended", "id":"run", "stage_run_id":"A", "reason":"A ended; B is current"}),
    )
    .await;
    let output = finish(child, false).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("A ended; B is current"));
    closed(&mut peer).await;
    assert_no_replacement(replacement).await;
    fixture.no_retry().await;
    drop(stdin);
}

#[tokio::test]
async fn documented_printf_pipeline_delivers_before_success() {
    let fixture = Fixture::new();
    let mut shell = fixture.program("/bin/sh");
    // Pass the binary as a positional argument, not interpolated shell text.
    let mut child = shell
        .args([
            "-c",
            "printf '%s\\n' 'message' | \"$1\" debug run A",
            "debug-pipeline",
            env!("CARGO_BIN_EXE_gremlins"),
        ])
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let (mut peer, bind_id) = fixture.bound().await;
    let frame = command(
        read(&mut peer).await,
        json!({"cmd":"message", "text":"message"}),
        &mut HashSet::from([bind_id]),
    );
    receipt(&mut peer, &frame, "queued").await;
    held(&mut child, &mut peer).await;
    receipt(&mut peer, &frame, "delivered").await;
    finish(child, true).await;
    closed(&mut peer).await;
    fixture.no_retry().await;
}

#[tokio::test]
async fn help_documents_exact_input_and_local_exit() {
    let fixture = Fixture::new();
    let output = finish(
        fixture.command().args(["debug", "--help"]).spawn().unwrap(),
        true,
    )
    .await;
    let help = String::from_utf8_lossy(&output.stdout);
    for term in [
        "STAGE_RUN_ID",
        "input",
        "/pause",
        "/step",
        "/continue",
        "/quit",
        "/exit",
        "EOF",
    ] {
        assert!(help.contains(term), "missing {term}: {help}");
    }
    assert!(!help.contains("debugger mode"), "{help}");
    fixture.no_retry().await;
}
