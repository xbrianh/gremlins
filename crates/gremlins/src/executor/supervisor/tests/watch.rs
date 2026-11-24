//! Socket-level watch routing and read-only correlation filtering.
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use gremlins_magic_bus::{emit_corr_event, emit_corr_event_async, CorrId, EventKind};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::{create_chat_gremlin, model, spawn_test_actor, TestRegistry};
use crate::executor::gremlin::Gremlin;
use crate::executor::socket;
use crate::executor::supervisor::{handle_connection, session::handle_session_aux, CancelToken};

const LIMIT: Duration = Duration::from_secs(5);
const SESSION: &str = "sess-watch-tests";

struct Peer {
    reader: BufReader<UnixStream>,
    task: JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Peer {
    async fn open(registry: &TestRegistry, aux: bool, request: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watch.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        // Bound kernel buffering so the unread-socket lag test is deterministic
        // without retaining hundreds of MiB in the global event journal.
        let send_buffer: libc::c_int = 4096;
        // SAFETY: server owns a valid socket; the option points to a live c_int.
        let result = unsafe {
            libc::setsockopt(
                server.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&send_buffer as *const libc::c_int).cast(),
                std::mem::size_of_val(&send_buffer) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        let registry = Arc::clone(registry);
        let task = tokio::spawn(async move {
            if aux {
                let (read, write) = server.into_split();
                handle_session_aux(read, write, SESSION.into(), registry).await;
            } else {
                handle_connection(server, registry, Default::default()).await;
            }
        });
        let mut peer = Self {
            reader: BufReader::new(client),
            task,
            _dir: dir,
        };
        peer.send(request).await;
        peer
    }

    async fn watch(registry: &TestRegistry, aux: bool, id: &str, filter: Value) -> Self {
        let mut request = json!({"op":"watch", "id":id, "session_id":SESSION});
        request
            .as_object_mut()
            .unwrap()
            .extend(filter.as_object().unwrap().clone());
        let mut peer = Self::open(registry, aux, request).await;
        let ack = peer.frame().await;
        assert_eq!(ack["type"], "watched", "{ack}");
        assert_eq!(ack["id"], id);
        assert_eq!(
            ack["replay"],
            json!(filter["replay"].as_bool().unwrap_or(false))
        );
        assert_eq!(ack["stage"], filter["stage"]);
        assert_eq!(ack["stage_run_id"], filter["stage_run_id"]);
        peer
    }

    async fn send(&mut self, request: Value) {
        socket::write_json_line(self.reader.get_mut(), &request)
            .await
            .unwrap();
    }

    async fn frame(&mut self) -> Value {
        timeout(LIMIT, socket::read_json_line(&mut self.reader))
            .await
            .unwrap()
            .unwrap()
            .expect("watch closed unexpectedly")
    }

    async fn no_frame(&mut self) {
        assert!(
            timeout(
                Duration::from_millis(50),
                socket::read_json_line(&mut self.reader)
            )
            .await
            .is_err(),
            "foreign event produced a frame or closed the watch"
        );
    }

    async fn closed(&mut self) {
        assert!(timeout(LIMIT, socket::read_json_line(&mut self.reader))
            .await
            .unwrap()
            .unwrap()
            .is_none());
        timeout(LIMIT, &mut self.task).await.unwrap().unwrap();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn live(id: &str) -> (TestRegistry, Gremlin, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let gremlin = create_chat_gremlin(
        id,
        &model("cmd/echo"),
        "helpful",
        dir.path(),
        &Default::default(),
    )
    .unwrap();
    let (registry, _) = spawn_test_actor().await;
    let gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
        .await;
    (registry, gremlin, dir)
}

fn corr(id: &str, stage: &str, execution: &str) -> CorrId {
    CorrId::new()
        .with_run(id)
        .with_stage(stage)
        .with_stage_run_id(execution)
}

fn tool() -> EventKind {
    EventKind::ToolResult {
        id: "repeated-tool-id".into(),
        name: "Bash".into(),
        call: "pwd".into(),
        output: "/work".into(),
    }
}

fn boundary(id: &str, stage: &str, execution: &str, kind: &str) -> EventKind {
    EventKind::StageBoundary {
        id: id.into(),
        stage: stage.into(),
        stage_run_id: execution.into(),
        stage_type: kind.into(),
        active: true,
    }
}

fn assert_wire(frame: &Value, identity: &CorrId, kind: &EventKind) -> u64 {
    let mut expected = serde_json::to_value(kind).unwrap();
    // Run lifecycle frames use the attachment schema (not the daemon schema).
    if matches!(kind, EventKind::RunStopped { .. }) {
        expected = json!({"type":"run_stopped", "id":identity.run().unwrap()});
    }
    expected["run_id"] = json!(identity.run());
    if let Some(stage) = identity.stage() {
        if expected
            .get("stage")
            .is_some_and(|payload| payload != stage)
        {
            expected["correlation_stage"] = json!(stage);
        } else {
            expected["stage"] = json!(stage);
        }
    }
    expected["stage_run_id"] = json!(identity.stage_run_id());
    let sequence = frame["sequence"].as_u64().expect("wire sequence");
    expected["sequence"] = json!(sequence);
    assert_eq!(frame, &expected);
    assert!(frame.get("execution_id").is_none());
    assert!(frame.get("stage_id").is_none());
    sequence
}

#[tokio::test]
async fn control_and_verified_aux_filter_interleaved_runs_stages_and_iterations() {
    let id = "watch-interleaved";
    let (registry, _gremlin, _dir) = live(id).await;
    let mut run = Peer::watch(&registry, false, id, json!({})).await;
    let mut stage = Peer::watch(&registry, true, id, json!({"stage":"worker"})).await;
    let mut first_execution =
        Peer::watch(&registry, false, id, json!({"stage_run_id":"iteration-1"})).await;
    let mut execution =
        Peer::watch(&registry, false, id, json!({"stage_run_id":"iteration-2"})).await;
    let mut exact = Peer::watch(
        &registry,
        true,
        id,
        json!({"stage":"worker", "stage_run_id":"iteration-2"}),
    )
    .await;
    let child = format!("{id}:child");
    let events = vec![
        // Explicit absence checks below cover both foreign output and terminals.
        (
            corr("watch-other-run", "worker", "iteration-2"),
            EventKind::RunStopped {
                id: "watch-other-run".into(),
            },
        ),
        (
            corr(id, "worker", "iteration-1"),
            boundary(id, "worker", "iteration-1", "agent"),
        ),
        (corr("watch-other-run", "worker", "iteration-2"), tool()),
        (corr(id, "parallel", "parallel-1"), tool()),
        (corr(id, "worker", "iteration-1"), tool()),
        (
            corr(id, "worker", "iteration-2"),
            boundary(id, "worker", "iteration-2", "exec"),
        ),
        (corr(&child, "worker", "iteration-2"), tool()),
        (
            corr(&child, "worker", "iteration-2"),
            EventKind::RunStopped { id: child.clone() },
        ),
        (corr(id, "worker", "iteration-2"), tool()),
        (
            corr(id, "worker", "iteration-2"),
            EventKind::ExecOutput {
                id: id.into(),
                text: "exec stderr\n".into(),
                is_stderr: true,
                ts: "now".into(),
            },
        ),
        (
            corr(id, "parallel", "parallel-1"),
            EventKind::StreamChunk {
                text: "parallel still live".into(),
            },
        ),
        // An ordered barrier for the first-invocation watch: after the
        // same-name successor, its next frame must still be iteration-1.
        // The stage-name watch must receive both invocations and this marker.
        (
            corr(id, "worker", "iteration-1"),
            EventKind::StreamChunk {
                text: "first invocation still selected".into(),
            },
        ),
    ];
    let mut previous = 0;
    for (identity, kind) in events {
        emit_corr_event(&identity, kind.clone());
        if identity.run() == Some("watch-other-run") {
            for peer in [
                &mut run,
                &mut stage,
                &mut first_execution,
                &mut execution,
                &mut exact,
            ] {
                peer.no_frame().await;
            }
            continue;
        }
        let frame = run.frame().await;
        let sequence = assert_wire(&frame, &identity, &kind);
        assert!(sequence > previous);
        previous = sequence;
        if identity.stage() == Some("worker") {
            assert_eq!(
                assert_wire(&stage.frame().await, &identity, &kind),
                sequence
            );
        }
        if identity.stage_run_id() == Some("iteration-1") {
            assert_eq!(
                assert_wire(&first_execution.frame().await, &identity, &kind),
                sequence
            );
        }
        if identity.stage_run_id() == Some("iteration-2") {
            assert_eq!(
                assert_wire(&execution.frame().await, &identity, &kind),
                sequence
            );
            assert_eq!(
                assert_wire(&exact.frame().await, &identity, &kind),
                sequence
            );
        }
    }
    // A read-only command is a final socket barrier: no successor frame may
    // precede its rejection, and the first-invocation watch is still open.
    first_execution.send(json!({"op":"input"})).await;
    let rejected = first_execution.frame().await;
    assert_eq!(rejected["type"], "error", "{rejected}");
    assert!(rejected["message"]
        .as_str()
        .unwrap()
        .contains("watch is read-only"));
    first_execution.closed().await;

    // A parent terminal is visible to matching filters and closes each stream.
    let identity = corr(id, "worker", "iteration-2");
    let terminal = EventKind::RunStopped { id: id.into() };
    emit_corr_event(&identity, terminal.clone());
    for peer in [&mut run, &mut stage, &mut execution, &mut exact] {
        assert_wire(&peer.frame().await, &identity, &terminal);
        peer.closed().await;
    }
    registry.stop_run(id).await.unwrap();
}

#[tokio::test]
async fn control_and_verified_aux_replay_retained_events_once_before_live_barrier() {
    for aux in [false, true] {
        let id = format!("watch-replay-{aux}");
        let (registry, _gremlin, _dir) = live(&id).await;
        let identity = corr(&id, "leaf", "nested-iteration");
        let retained = [
            EventKind::StageTransition {
                id: id.clone(),
                stage: "outer".into(),
                sub_stage: Some("leaf".into()),
            },
            tool(),
            tool(), // Identical payloads are distinct retained events, not duplicates.
        ];
        for kind in &retained {
            emit_corr_event(&identity, kind.clone());
        }
        let mut peer = Peer::watch(
            &registry,
            aux,
            &id,
            json!({"replay":true, "stage":"leaf", "stage_run_id":"nested-iteration"}),
        )
        .await;
        // Emitting after the acknowledgment exercises the history/live cut even
        // if the server is still writing retained frames to the real socket.
        let live = EventKind::StreamChunk {
            text: "exactly-once live barrier".into(),
        };
        emit_corr_event(&identity, retained[0].clone());
        emit_corr_event(&identity, live.clone());
        let mut first = peer.frame().await;
        // The shared journal may already be incomplete because of other tests.
        let gap = if first["type"] == "history_gap" {
            let gap = first;
            first = peer.frame().await;
            Some(gap)
        } else {
            None
        };
        let mut previous = assert_wire(&first, &identity, &retained[0]);
        assert_eq!(first["stage"], "outer");
        assert_eq!(first["correlation_stage"], "leaf");
        for kind in &retained[1..] {
            let sequence = assert_wire(&peer.frame().await, &identity, kind);
            assert!(
                sequence > previous,
                "retained event duplicated or reordered"
            );
            previous = sequence;
        }
        let checkpoint = peer.frame().await;
        assert_eq!(checkpoint["type"], "replay_checkpoint", "{checkpoint}");
        assert_eq!(checkpoint["id"], id);
        assert_eq!(checkpoint["scope"], "run");
        assert_eq!(checkpoint["stage"], "leaf");
        assert_eq!(checkpoint["stage_run_id"], "nested-iteration");
        assert_eq!(checkpoint["complete"], gap.is_none());
        let through = checkpoint["through_sequence"].as_u64().unwrap();
        assert!(through >= previous);
        if let Some(gap) = gap {
            assert_eq!(gap["through_sequence"], checkpoint["through_sequence"]);
            assert_eq!(
                gap["evicted_through_sequence"],
                checkpoint["evicted_through_sequence"]
            );
        }
        // The nested transition must preserve its payload stage in live output
        // too, with a fresh sequence strictly beyond the replay cut.
        let nested = peer.frame().await;
        let nested_sequence = assert_wire(&nested, &identity, &retained[0]);
        assert!(nested_sequence > through);
        assert_eq!(nested["stage"], "outer");
        assert_eq!(nested["correlation_stage"], "leaf");
        // No repeated retained event or second checkpoint may precede this.
        assert!(assert_wire(&peer.frame().await, &identity, &live) > nested_sequence);
        let terminal = EventKind::RunStopped { id: id.clone() };
        emit_corr_event(&identity, terminal.clone());
        // A second ordered barrier also rules out duplicate live delivery.
        assert_wire(&peer.frame().await, &identity, &terminal);
        peer.closed().await;
        registry.stop_run(&id).await.unwrap();
    }
}

#[tokio::test]
async fn invalid_sessions_are_rejected_before_watch_routing() {
    let (registry, _gremlin, _dir) = live("watch-session-validation").await;
    for session in [
        None,
        Some(json!(null)),
        Some(json!(42)),
        Some(json!("../invalid")),
        Some(json!("sess-other")),
    ] {
        let mut request = json!({"op":"watch", "id":"watch-session-validation"});
        if let Some(session) = session {
            request["session_id"] = session;
        }
        let mut peer = Peer::open(&registry, true, request).await;
        let error = peer.frame().await;
        assert_eq!(error["type"], "error", "{error}");
        assert!(error.to_string().contains("session"), "{error}");
        peer.closed().await;
    }
    registry.stop_run("watch-session-validation").await.unwrap();
}

#[tokio::test]
async fn malformed_filters_and_retired_keys_reject_before_registration() {
    let (registry, _gremlin, _dir) = live("watch-filter-validation").await;
    let mut invalid = vec![
        json!({"id":null}),
        json!({"id":""}),
        json!({"id":"../bad"}),
        json!({"scope":"conversation"}),
        json!({"scope":3}),
        json!({"target":{}}),
        json!({"replay":"yes"}),
        json!({"stage_name":"worker"}),
        json!({"stage_run":"iteration-1"}),
        json!({"run_id":"another-run"}),
        json!({"unknown_selector":null}),
    ];
    for key in ["stage", "stage_run_id"] {
        for value in [json!(null), json!(""), json!(4), json!([]), json!({})] {
            let mut filter = json!({});
            filter[key] = value;
            invalid.push(filter);
        }
    }
    for aux in [false, true] {
        for filter in &invalid {
            let mut request =
                json!({"op":"watch", "id":"watch-filter-validation", "session_id":SESSION});
            request
                .as_object_mut()
                .unwrap()
                .extend(filter.as_object().unwrap().clone());
            let mut peer = Peer::open(&registry, aux, request).await;
            assert_eq!(peer.frame().await["type"], "error", "{filter}");
            peer.closed().await;
        }
        for key in ["stage_id", "execution_id"] {
            for nested in [false, true] {
                for replacement in [false, true] {
                    // Nonexistent run proves validation precedes run resolution/registration.
                    let mut request =
                        json!({"op":"watch", "id":"watch-not-registered", "session_id":SESSION});
                    let target = if nested {
                        request["target"] = json!({});
                        &mut request["target"]
                    } else {
                        &mut request
                    };
                    target[key] = json!(null);
                    if replacement {
                        target["stage_run_id"] = json!("new-id");
                    }
                    let mut peer = Peer::open(&registry, aux, request).await;
                    let error = peer.frame().await;
                    assert_eq!(error["type"], "error");
                    assert!(error.to_string().contains(key), "{error}");
                    assert!(error.to_string().contains("stage_run_id"), "{error}");
                    peer.closed().await;
                }
            }
        }
        let mut missing = Peer::open(
            &registry,
            aux,
            json!({"op":"watch", "id":"watch-not-registered", "session_id":SESSION}),
        )
        .await;
        assert!(missing.frame().await.to_string().contains("not running"));
        missing.closed().await;
    }
    registry.stop_run("watch-filter-validation").await.unwrap();
}

#[tokio::test]
async fn live_only_watch_commands_disconnect_without_input_or_pausing_other_viewers() {
    let id = "watch-readonly";
    let (registry, mut gremlin, _dir) = live(id).await;
    let mut session = gremlin.next_interactive_session();
    let identity = corr(id, "agent", "live-execution");
    emit_corr_event(
        &identity,
        EventKind::StreamChunk {
            text: "before registration".into(),
        },
    );
    let mut survivor = Peer::watch(&registry, true, id, json!({})).await;
    for aux in [false, true] {
        let mut viewer = Peer::watch(&registry, aux, id, json!({})).await;
        viewer
            .send(json!({"op":"message", "text":"must not deliver", "req_id":"forbidden"}))
            .await;
        let error = viewer.frame().await;
        assert_eq!(error["type"], "error");
        assert!(error.to_string().contains("read-only"));
        viewer.closed().await;
        assert!(!session.accept_pause());
        assert!(timeout(LIMIT, session.next_command())
            .await
            .unwrap()
            .is_none());
        let kind = EventKind::StreamChunk {
            text: format!("survivor after {aux}"),
        };
        emit_corr_event(&identity, kind.clone());
        // Neither old history nor a replay checkpoint may precede live output.
        assert_wire(&survivor.frame().await, &identity, &kind);
    }
    let terminal = EventKind::RunStopped { id: id.into() };
    emit_corr_event(&identity, terminal.clone());
    assert_wire(&survivor.frame().await, &identity, &terminal);
    survivor.closed().await;
    registry.stop_run(id).await.unwrap();
}

#[tokio::test]
async fn socket_watch_lag_does_not_backpressure_producer() {
    let id = "watch-socket-lag";
    let (registry, _gremlin, _dir) = live(id).await;
    let mut viewer = Peer::watch(&registry, true, id, json!({})).await;
    let identity = corr(id, "agent", "lag-execution");
    // 32 KiB exceeds the explicitly bounded socket send buffer. While the
    // writer blocks on the first frame, 320 events overflow its 256-slot queue.
    // Keep the retained journal footprint near 10 MiB rather than 300+ MiB.
    let kind = EventKind::StreamChunk {
        text: "x".repeat(32 * 1024),
    };
    timeout(LIMIT, async {
        for _ in 0..320 {
            emit_corr_event_async(&identity, kind.clone()).await;
        }
    })
    .await
    .expect("watch backpressured producer");
    let mut lagged = false;
    for _ in 0..321 {
        let frame = viewer.frame().await;
        if frame["type"] == "lagged" {
            assert!(frame["skipped"].as_u64().unwrap() > 0);
            lagged = true;
            break;
        }
        assert_eq!(frame["type"], "stream_chunk");
    }
    assert!(lagged, "blocked socket never reported lag");
    viewer.send(json!({"op":"detach"})).await;
    // Drain pending display before the command error and EOF.
    loop {
        if viewer.frame().await["type"] == "error" {
            break;
        }
    }
    viewer.closed().await;
    registry.stop_run(id).await.unwrap();
}
