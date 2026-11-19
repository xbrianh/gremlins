//! Shared attachment handler: observe or control one execution on one
//! gremlin.
//!
//! Available on the control socket (`attach` op) and the verified session
//! auxiliary socket (`attach` as first op). Subscribes before acknowledging
//! observation or requesting a pause.
//!
//! ## Scopes
//!
//! - **`run`** — follows successive stage boundaries for the inline log.
//!   Displays `StageBoundary`, `StageTransition`, `RunCompleted`, etc.
//! - **`conversation`** — pinned to one execution (one agent-loop
//!   invocation). Only `conversation`-scoped attachments can inject text.
//!
//! ## Commands
//!
//! | Op          | Scope         | Effect |
//! |-------------|---------------|--------|
//! | `message`   | conversation  | Inject text, release pause, resume. |
//! | `talk`      | conversation  | Inject text, run one turn, re-enter interactive. |
//! | `continue`  | conversation  | Run one turn, re-enter interactive. |
//! | `bail`      | conversation  | Terminate with error. |
//! | `quit`      | conversation  | Release pause, resume autonomous. |
//! | `detach`    | run           | Release the lease, close this attachment. |
//!
//! Multiple passive readers, at most one controller per execution.
//! Passive display streams are bounded, lag-tolerant; a gap report lets
//! the viewer know output was missed. Command acknowledgments, readiness,
//! and terminal outcomes are not lossy.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;

use crate::clients::interactive::{
    AttachError, DebugAttachment, EndSignal, InteractiveCommand, InteractiveHandle,
};
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket;
use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run_lag_tolerant, EventKind, Recv};
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::watch;

use super::debug::{check_agent_stage, send_debug_status};
use super::error_response;
use super::run_registry::{RunRegistry, RunState, StoreHandle};

// ── Wire types ──────────────────────────────────────────────────────────

/// What a passive `run`-scope subscriber receives.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RunEvent {
    StageBoundary {
        id: String,
        stage: String,
        stage_type: String,
        active: bool,
    },
    StageTransition {
        id: String,
        stage: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sub_stage: Option<String>,
    },
    RunCompleted {
        id: String,
        exit_code: i32,
    },
    RunFailed {
        id: String,
        exit_code: i32,
        error: Option<String>,
    },
    RunStopped {
        id: String,
    },
    Lagged {
        skipped: u64,
    },
}

impl RunEvent {
    fn from_kind(kind: &EventKind) -> Option<Self> {
        match kind {
            EventKind::StageBoundary {
                id,
                stage,
                stage_type,
                active,
            } => Some(Self::StageBoundary {
                id: id.clone(),
                stage: stage.clone(),
                stage_type: stage_type.clone(),
                active: *active,
            }),
            EventKind::StageTransition {
                id,
                stage,
                sub_stage,
            } => Some(Self::StageTransition {
                id: id.clone(),
                stage: stage.clone(),
                sub_stage: sub_stage.clone(),
            }),
            EventKind::RunCompleted { id, exit_code } => Some(Self::RunCompleted {
                id: id.clone(),
                exit_code: *exit_code,
            }),
            EventKind::RunFailed {
                id,
                exit_code,
                error,
            } => Some(Self::RunFailed {
                id: id.clone(),
                exit_code: *exit_code,
                error: error.clone(),
            }),
            EventKind::RunStopped { id } => Some(Self::RunStopped { id: id.clone() }),
            _ => None,
        }
    }
}

// ── Entry points ───────────────────────────────────────────────────────

/// Read the first request from an attachment stream and dispatch based on
/// scope. Called from both the control socket and the session aux socket.
pub(crate) async fn handle_attach(
    request: &Value,
    registry: &Arc<RunRegistry>,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        let resp = error_response("missing 'id' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    if validate_gremlin_id(id).is_err() {
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let scope = request
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("run");

    match scope {
        "conversation" => handle_conversation_attach(request, id, registry, reader, writer).await,
        _ => handle_run_attach(request, id, registry, reader, writer).await,
    }
}

// ── Run-scope (passive, stage-boundary follow) ──────────────────────────

async fn handle_run_attach(
    _request: &Value,
    id: &str,
    registry: &Arc<RunRegistry>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    // Resolve the target in the registry so finished/stale gremlins get a
    // specific error before any acknowledgment.
    let store = registry.get_run_store(id).await;
    if store.is_none() {
        let resp = error_response(&format!("gremlin {id} is not running"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let (sub, mut bus_rx) = subscribe_run_lag_tolerant(id, 256);
    let _stream_guard = BusSubGuard::new(sub);

    // Emit a status frame so the caller knows the attachment is live.
    let status = serde_json::json!({
        "type": "attached",
        "id": id,
        "scope": "run",
        "capabilities": ["observe"],
    });
    if writer.write_json_line(&status).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            result = bus_rx.recv() => {
                match result {
                    Recv::Event(dispatch) => {
                        let kind = dispatch.into_kind();
                        // Filter: only run-lifecycle events for this id.
                        if event_run_id_matches(&kind, id) {
                            if let Some(evt) = RunEvent::from_kind(&kind) {
                                let value = match serde_json::to_value(&evt) {
                                    Ok(v) => v,
                                    Err(_) => continue,
                                };
                                if writer.write_json_line(&value).await.is_err() {
                                    break;
                                }
                            }
                        }
                        // RunCompleted, RunFailed, RunStopped → terminal.
                        if matches!(
                            kind,
                            EventKind::RunCompleted { .. }
                                | EventKind::RunFailed { .. }
                                | EventKind::RunStopped { .. }
                        ) {
                            break;
                        }
                    }
                    Recv::Lagged(skipped) => {
                        let payload = serde_json::to_value(&RunEvent::Lagged { skipped })
                            .unwrap_or(serde_json::Value::Null);
                        if writer.write_json_line(&payload).await.is_err() {
                            break;
                        }
                    }
                    Recv::Closed => break,
                }
            }
            _ = socket::read_json_line(&mut reader) => {
                // Client sent something — ignore (passive only) or use
                // for detach in future.
                break;
            }
        }
    }
}

// ── Conversation-scope (interactive, can inject text) ──────────────────

/// Returns `true` when `kind` carries an id matching `target`.
/// Every `EventKind` variant must appear in one arm — nothing falls through.
fn event_run_id_matches(kind: &EventKind, target: &str) -> bool {
    match kind {
        EventKind::RunStarted { id, .. }
        | EventKind::RunCompleted { id, .. }
        | EventKind::RunFailed { id, .. }
        | EventKind::RunStopped { id }
        | EventKind::RunRemoved { id }
        | EventKind::TokenUsage { id, .. }
        | EventKind::StageTransition { id, .. }
        | EventKind::Bail { id, .. }
        | EventKind::LogLine { id, .. }
        | EventKind::StageBoundary { id, .. }
        | EventKind::ExecOutput { id, .. }
        | EventKind::SkillLoaded { id, .. }
        | EventKind::OperatorMessage { id, .. }
        | EventKind::PromptRequest { id, .. } => id == target,
        // Variants that carry no per-run id: match everything.
        EventKind::TurnStarted { .. }
        | EventKind::StreamChunk { .. }
        | EventKind::ReasoningChunk { .. }
        | EventKind::TurnComplete { .. }
        | EventKind::ToolOutputChunk { .. }
        | EventKind::ToolResult { .. }
        | EventKind::StreamReset
        | EventKind::Ready { .. }
        | EventKind::Done { .. }
        | EventKind::Ended { .. }
        | EventKind::Error { .. }
        | EventKind::SystemWarning { .. }
        | EventKind::TaskProgress { .. } => true,
    }
}

pub(crate) async fn handle_conversation_attach(
    request: &Value,
    id: &str,
    registry: &Arc<RunRegistry>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    // 1. Resolve the interactive handle from the registry.
    let Some((handle, store, mut run_state)) = registry.get_run_interactive(id).await else {
        let resp = error_response(&format!("gremlin {id} is not running"));
        let _ = writer.write_json_line(&resp).await;
        return;
    };

    // 2. Snapshot target and nonce before any async work.
    let target = handle.target();
    let execution_nonce = target.nonce();
    let execution_id = target.execution_id().map(|s| s.to_string());
    // Capture the end signal for this execution so a passive observer can
    // stop when the execution ends even if no terminal event is emitted on
    // the bus (e.g. the run is cancelled before `Done`/`Ended`).
    let end_signal = handle.end_signal();

    // 3. Check if the caller wants passive observation only. Conversation
    //    scope is interactive by default: a client that names no mode expects
    //    the operator commands advertised in the `attached` ack.
    //
    //    NOTE: the default changed from "passive" to "control" as part of
    //    the debug→attach cutover (the TUI and CLI debug clients both send
    //    no explicit mode and expect to control). Any /watch-style observer
    //    or third-party client that omits `mode` must now
    //    send `"mode": "passive"` explicitly, or it will be rejected
    //    when the target is not an agent leaf.
    let mode = request
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("control");

    // 4. Defer stage validation: passive mode skips entirely; control mode
    //    checks only when the client actually sends a command (message/talk/
    //    continue/bail/quit). This keeps observation zero-cost and lets
    //    the attached ack go out immediately without blocking.

    // 5. Subscribe to run events before acknowledging.
    let (stream_sub, mut evt_rx) = subscribe_run_lag_tolerant(id, 256);
    let _stream_guard = BusSubGuard::new(stream_sub);

    // 6. Emit attachment acknowledgment with execution identity and a
    //    stable execution nonce. Capabilities are computed from the actual
    //    target: agent leaves advertise control, everything else observation
    //    only.
    let mut caps = vec!["observe"];
    if mode != "passive" {
        let can_control = tokio::task::spawn_blocking({
            let store = store.clone();
            move || check_agent_stage(&store).is_ok()
        })
        .await
        .unwrap_or(false);
        if can_control {
            caps.extend(["message", "talk", "continue", "bail", "quit", "detach"]);
        }
    }
    let ack = serde_json::json!({
        "type": "attached",
        "scope": "conversation",
        "id": id,
        "execution_id": execution_id,
        "nonce": execution_nonce,
        "capabilities": caps,
    });
    if writer.write_json_line(&ack).await.is_err() {
        return;
    }

    if mode == "passive" {
        handle_passive_conversation(id, &mut reader, writer, &mut evt_rx, end_signal).await;
        return;
    }

    // 7. Control mode: stream display events and accept commands on the
    //    same socket. No pause is requested at attach time — the agent
    //    keeps running. Only `message` / `talk` / `continue` / `bail` /
    //    `quit` acquire a temporary lease on the captured execution.

    // The active lease, if any. Dropping releases the pause slot. A debugger
    // command acquires a lease that crosses turn boundaries and is reused for
    // every subsequent command; a `message` acquires a one-shot lease (unless a
    // debugger lease is already held) and releases it after delivery.
    let mut active_lease: Option<crate::clients::interactive::DebugAttachment> = None;

    loop {
        tokio::select! {
            biased;
            result = socket::read_json_line(&mut reader) => {
                match result {
                    Ok(Some(cmd)) => {
                        let op = cmd.get("op").and_then(Value::as_str).unwrap_or("");
                        let req_id = cmd.get("req_id").and_then(Value::as_str).map(|s| s.to_string());
                        let should_break = dispatch_attach_command(
                            &cmd,
                            op,
                            req_id,
                            id,
                            execution_nonce,
                            execution_id.as_deref(),
                            &handle,
                            &store,
                            &mut run_state,
                            &mut reader,
                            &mut evt_rx,
                            writer,
                            &mut active_lease,
                        )
                        .await;
                        if should_break {
                            break;
                        }
                    }
                    _ => {
                        // EOF, disconnect, or a malformed frame: release any
                        // pending lease and close the attachment.
                        drop(active_lease.take());
                        break;
                    }
                }
            }
            result = evt_rx.recv() => {
                match result {
                    Recv::Event(dispatch) => {
                        let kind = dispatch.kind();
                        let mut payload = conversation_display_event(kind);
                        // Tag every display frame with scope so clients can
                        // distinguish display events from control replies.
                        if let Some(ref mut p) = payload {
                            if let Some(obj) = p.as_object_mut() {
                                obj.insert("scope".into(), "conversation".into());
                            }
                        }
                        if let Some(p) = payload {
                            if writer.write_json_line(&p).await.is_err() { break; }
                        }
                    }
                    Recv::Lagged(skipped) => {
                        let payload = serde_json::json!({
                            "type": "lagged",
                            "scope": "conversation",
                            "skipped": skipped,
                        });
                        if writer.write_json_line(&payload).await.is_err() { break; }
                    }
                    Recv::Closed => break,
                }
            }
            // Report the classified terminal outcome when the execution ends
            // while a lease is held, so the debugger sees the same outcome as
            // the chat rather than a bare socket EOF. Pending when no lease is
            // held (a passive observer only follows the display stream).
            outcome = async {
                match active_lease.as_mut() {
                    Some(lease) => (&mut lease.ended).await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                let mut frame = match outcome {
                    Some(outcome) => {
                        serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null)
                    }
                    None => serde_json::json!({
                        "type": "ended",
                        "reason": "agent exited",
                    }),
                };
                if let Some(obj) = frame.as_object_mut() {
                    obj.insert("scope".into(), "conversation".into());
                    obj.entry("id").or_insert_with(|| serde_json::Value::String(id.into()));
                }
                let _ = writer.write_json_line(&frame).await;
                break;
            }
        }
    }
}

// ── Control-mode command dispatch ──────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn dispatch_attach_command(
    cmd: &Value,
    op: &str,
    req_id: Option<String>,
    id: &str,
    execution_nonce: u64,
    execution_id: Option<&str>,
    handle: &InteractiveHandle,
    store: &StoreHandle,
    run_state: &mut watch::Receiver<RunState>,
    reader: &mut BufReader<OwnedReadHalf>,
    evt_rx: &mut gremlins_magic_bus::ReceiverHalf<EventKind>,
    writer: &SharedWriter,
    active_lease: &mut Option<DebugAttachment>,
) -> bool {
    match op {
        "message" => {
            let text = cmd["text"].as_str().unwrap_or("").to_string();

            // Validate the leaf is an agent stage (deferred from attach time).
            if let Err(e) = run_stage_check(store).await {
                let resp = error_response(&format!("gremlin {id}: {e}"));
                let _ = writer.write_json_line(&resp).await;
                return false;
            }

            // Reuse a held debugger lease; otherwise acquire a one-shot
            // message lease that we release after delivery so the agent
            // resumes autonomously.
            let acquired_for_message = if active_lease.is_none() {
                let lease = match acquire_lease(
                    handle,
                    execution_nonce,
                    run_state,
                    reader,
                    writer,
                    false,
                    id,
                    execution_nonce,
                )
                .await
                {
                    Some(lease) => lease,
                    None => return false,
                };
                *active_lease = Some(lease);
                true
            } else {
                false
            };

            // Deliver the correction with a delivery one-shot.
            let (delivery_tx, delivery_rx) = tokio::sync::oneshot::channel();
            let send_req_id = req_id.clone();
            let send_result = active_lease
                .as_mut()
                .unwrap()
                .try_send(InteractiveCommand::Message(
                    text,
                    send_req_id,
                    Some(delivery_tx),
                ));
            if let Err(err) = send_result {
                use tokio::sync::mpsc::error::TrySendError;
                let reason = match err {
                    TrySendError::Full(_) => "command queue is full; message not delivered",
                    TrySendError::Closed(_) => "attachment is ending; message not delivered",
                };
                let _ = writer.write_json_line(&error_response(reason)).await;
                if acquired_for_message {
                    drop(active_lease.take());
                }
                return false;
            }

            // Wait for incorporation while staying responsive to the socket
            // and display stream.
            let mut delivery_task = tokio::spawn(delivery_rx);
            let delivered = loop {
                tokio::select! {
                    biased;
                    result = socket::read_json_line(reader) => {
                        match result {
                            Ok(Some(frame)) => {
                                // A command arriving mid-delivery is not what
                                // the plan wants: the correction must resolve
                                // exactly. Queueing another control op while
                                // incorporation is outstanding would leave an
                                // ambiguous owner.
                                let _ = writer.write_json_line(&error_response(
                                    "busy delivering correction",
                                )).await;
                                let _ = frame;
                            }
                            _ => {
                                // EOF before incorporation: withdraw and
                                // report non-delivery.
                                drop(active_lease.take());
                                let _ = writer.write_json_line(&serde_json::json!({
                                    "type": "correction_dropped",
                                    "id": id,
                                    "reason": "disconnected before incorporation",
                                })).await;
                                return false;
                            }
                        }
                    }
                    result = evt_rx.recv() => {
                        // Forward display events while waiting for
                        // incorporation so a slow viewer never blocks the
                        // agent and the owner keeps seeing the stream.
                        match result {
                            Recv::Event(dispatch) => {
                                let kind = dispatch.kind();
                                if let Some(p) = conversation_display_event(kind) {
                                    if writer.write_json_line(&p).await.is_err() {
                                        drop(active_lease.take());
                                        return false;
                                    }
                                }
                            }
                            Recv::Lagged(skipped) => {
                                let payload = serde_json::json!({
                                    "type": "lagged",
                                    "scope": "conversation",
                                    "skipped": skipped,
                                });
                                if writer.write_json_line(&payload).await.is_err() {
                                    drop(active_lease.take());
                                    return false;
                                }
                            }
                            Recv::Closed => {
                                drop(active_lease.take());
                                return false;
                            }
                        }
                    }
                    delivered = &mut delivery_task => {
                        break matches!(delivered, Ok(Ok(())));
                    }
                }
            };

            if delivered {
                let mut payload = serde_json::json!({
                    "type": "delivered",
                    "scope": "conversation",
                    "id": id,
                    "nonce": execution_nonce,
                });
                if let Some(ref rid) = req_id {
                    payload["req_id"] = serde_json::Value::String(rid.clone());
                }
                if let Some(eid) = execution_id {
                    payload["execution_id"] = serde_json::Value::String(eid.to_string());
                }
                let _ = writer.write_json_line(&payload).await;
            } else {
                let mut payload = serde_json::json!({
                    "type": "correction_dropped",
                    "id": id,
                    "reason": "agent exited before incorporation",
                });
                if let Some(eid) = execution_id {
                    payload["execution_id"] = serde_json::Value::String(eid.to_string());
                }
                let _ = writer.write_json_line(&payload).await;
            }

            if acquired_for_message {
                drop(active_lease.take());
            }
        }
        "detach" => {
            // Release any lease and close the attachment.
            drop(active_lease.take());
            let detach_frame = serde_json::json!({
                "type": "detached",
                "scope": "conversation",
                "id": id,
            });
            let _ = writer.write_json_line(&detach_frame).await;
            return true;
        }
        "talk" | "continue" | "bail" | "quit" => {
            // Validate the leaf is an agent stage (deferred from attach time).
            if let Err(e) = run_stage_check(store).await {
                let resp = error_response(&format!("gremlin {id}: {e}"));
                let _ = writer.write_json_line(&resp).await;
                return false;
            }

            if active_lease.is_none() {
                let lease = match acquire_lease(
                    handle,
                    execution_nonce,
                    run_state,
                    reader,
                    writer,
                    true,
                    id,
                    execution_nonce,
                )
                .await
                {
                    Some(lease) => lease,
                    None => return false,
                };
                *active_lease = Some(lease);
            }

            let command = match op {
                "talk" => {
                    let text = cmd["text"].as_str().unwrap_or("").to_string();
                    InteractiveCommand::Inject(text)
                }
                "bail" => InteractiveCommand::Bail(
                    cmd["reason"].as_str().unwrap_or("operator bailed").into(),
                ),
                "quit" => InteractiveCommand::Quit,
                _ => InteractiveCommand::RunTurn,
            };
            if let Err(err) = active_lease.as_mut().unwrap().try_send(command) {
                use tokio::sync::mpsc::error::TrySendError;
                let reason = match err {
                    TrySendError::Full(_) => "command queue is full; command not accepted",
                    TrySendError::Closed(_) => "attachment is ending; command not accepted",
                };
                let _ = writer.write_json_line(&error_response(reason)).await;
            }

            // Keep the lease so the debugger can send more commands. The
            // agent loop re-enters the interactive loop after one turn.
        }
        _ => {
            let resp = error_response(&format!("unknown attach op: {op:?}"));
            let _ = writer.write_json_line(&resp).await;
        }
    }
    false
}

async fn run_stage_check(store: &StoreHandle) -> Result<(), String> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || check_agent_stage(&store))
        .await
        .map_err(|e| e.to_string())
        .and_then(|result| result)
}

/// Acquire a lease on the target captured at attach time and wait for the
/// agent to accept the pause. Returns `None` when the caller has already
/// reported a terminal error frame.
#[allow(clippy::too_many_arguments)]
async fn acquire_lease(
    handle: &InteractiveHandle,
    captured_nonce: u64,
    run_state: &mut watch::Receiver<RunState>,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
    debugger: bool,
    id: &str,
    ready_nonce: u64,
) -> Option<DebugAttachment> {
    let current_target = handle.target();
    if current_target.nonce() != captured_nonce {
        let _ = writer
            .write_json_line(&error_response(
                "execution has been replaced since attach; retry",
            ))
            .await;
        return None;
    }

    let mut lease = match if debugger {
        current_target.attach_debugger()
    } else {
        current_target.attach()
    } {
        Ok(a) => a,
        Err(err) => {
            let msg = match err {
                AttachError::NoLongerActive => "agent stage is no longer active",
                AttachError::Stale => "execution has been replaced; target is stale",
                AttachError::Busy => "busy",
            };
            let _ = writer.write_json_line(&error_response(msg)).await;
            return None;
        }
    };

    if debugger {
        send_debug_status(writer, "waiting_for_ready").await;
    }

    // Spawn the run-stop wait into a oneshot so the watch::Ref never crosses
    // an await point (watch::Ref is !Send).
    let (run_stop_tx, run_stop_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut rs = run_state.clone();
        tokio::spawn(async move {
            let _ = rs.wait_for(|s| s.status != "running").await;
            let _ = run_stop_tx.send(());
        });
    }

    let ready = tokio::select! {
        biased;
        _ = socket::read_json_line(reader) => {
            // EOF while waiting for Ready: drop the pending lease before the
            // agent can observe the pause.
            drop(lease);
            return None;
        }
        _ = run_stop_rx => {
            let _ = writer.write_json_line(
                &error_response("run stopped before agent accepted pause"),
            ).await;
            return None;
        }
        r = &mut lease.ready => r,
    };

    if ready.is_err() {
        let _ = writer
            .write_json_line(&error_response("agent exited before accepting pause"))
            .await;
        return None;
    }

    // Ready — report it, then retain the lease.
    let ready_payload = serde_json::json!({
        "type": "ready",
        "scope": "conversation",
        "id": id,
        "nonce": ready_nonce,
    });
    if writer.write_json_line(&ready_payload).await.is_err() {
        return None;
    }

    Some(lease)
}

/// Passive conversation: stream events until the execution ends.
///
/// The event loop exits on a terminal bus event (`Done`/`Ended`), on socket
/// EOF, or when `end_signal` fires — the last covering executions that end
/// without ever emitting a terminal event (e.g. cancellation).
async fn handle_passive_conversation(
    _id: &str,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
    evt_rx: &mut gremlins_magic_bus::ReceiverHalf<EventKind>,
    end_signal: Option<EndSignal>,
) {
    // When the slot is already gone (`end_signal` is `None`), drain
    // remaining events synchronously then return — no racing against the
    // immediate-completion future in the select loop.
    let end_signal = match end_signal {
        Some(signal) => signal,
        None => loop {
            match evt_rx.recv().await {
                Recv::Event(dispatch) => {
                    let kind = dispatch.into_kind();
                    let payload = conversation_display_event(&kind);
                    if let Some(p) = payload {
                        if writer.write_json_line(&p).await.is_err() {
                            return;
                        }
                    }
                    if matches!(kind, EventKind::Done { .. } | EventKind::Ended { .. }) {
                        return;
                    }
                }
                Recv::Lagged(skipped) => {
                    let payload = serde_json::json!({
                        "type": "lagged",
                        "skipped": skipped,
                    });
                    if writer.write_json_line(&payload).await.is_err() {
                        return;
                    }
                }
                Recv::Closed => return,
            }
        },
    };

    // Pinned once so its registration is not dropped between iterations: an
    // end signal that fires while an event arm is running must still complete
    // the future. `EndSignal` latches, so an execution that ended before this
    // future was first polled still completes immediately.
    let end = end_signal.wait();
    tokio::pin!(end);

    loop {
        tokio::select! {
            result = evt_rx.recv() => {
                match result {
                    Recv::Event(dispatch) => {
                        let kind = dispatch.into_kind();
                        let payload = conversation_display_event(&kind);
                        if let Some(p) = payload {
                            if writer.write_json_line(&p).await.is_err() { break; }
                        }
                        // Terminal: Done or Ended.
                        if matches!(kind, EventKind::Done { .. } | EventKind::Ended { .. }) {
                            break;
                        }
                    }
                    Recv::Lagged(skipped) => {
                        let payload = serde_json::json!({
                            "type": "lagged",
                            "skipped": skipped,
                        });
                        if writer.write_json_line(&payload).await.is_err() { break; }
                    }
                    Recv::Closed => break,
                }
            }
            // The execution ended (return, error, cancellation, or a
            // successor execution started) without a terminal bus event.
            _ = &mut end => break,
            _ = socket::read_json_line(reader) => break,
        }
    }
}

/// Convert a conversation-relevant [`EventKind`] into a display frame.
fn conversation_display_event(kind: &EventKind) -> Option<Value> {
    match kind {
        EventKind::StreamChunk { text } => {
            Some(serde_json::json!({"type":"stream_chunk","text": text}))
        }
        EventKind::ReasoningChunk { text } => {
            Some(serde_json::json!({"type":"reasoning_chunk","text": text}))
        }
        EventKind::TurnComplete {
            turn,
            text,
            tool_calls,
            stats,
            totals,
            ..
        } => Some(serde_json::json!({
            "type": "turn_complete",
            "turn": turn,
            "text": text,
            "tool_calls": tool_calls,
            "stats": stats,
            "totals": totals,
        })),
        EventKind::ToolOutputChunk {
            id,
            name,
            call,
            text,
        } => Some(serde_json::json!({
            "type": "tool_output_chunk",
            "id": id,
            "name": name,
            "call": call,
            "text": text,
        })),
        EventKind::ToolResult {
            id,
            name,
            call,
            output,
        } => Some(serde_json::json!({
            "type": "tool_result",
            "id": id,
            "name": name,
            "call": call,
            "output": output,
        })),
        EventKind::OperatorMessage { id, text } => Some(serde_json::json!({
            "type": "operator_message",
            "id": id,
            "text": text,
        })),
        EventKind::StreamReset => Some(serde_json::json!({"type": "stream_reset"})),
        EventKind::Ready { turn } => Some(serde_json::json!({"type": "ready", "turn": turn})),
        EventKind::Done {
            text,
            summary,
            usage,
        } => Some(serde_json::json!({
            "type": "done",
            "text": text,
            "summary": summary,
            "usage": usage,
        })),
        EventKind::Ended { reason } => Some(serde_json::json!({"type": "ended", "reason": reason})),
        EventKind::Error { message, turn } => Some(serde_json::json!({
            "type": "error",
            "message": message,
            "turn": turn,
        })),
        EventKind::SystemWarning { message, turn } => Some(serde_json::json!({
            "type": "system_warning",
            "message": message,
            "turn": turn,
        })),
        _ => None,
    }
}
