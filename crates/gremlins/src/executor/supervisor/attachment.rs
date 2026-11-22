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

use crate::clients::interactive::{AttachError, DebugAttachment, EndSignal, InteractiveCommand};
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket;
use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run_lag_tolerant, EventKind, Recv};
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::watch;

use super::debug::{check_agent_stage, send_debug_status};
use super::error_response;
use super::run_registry::{RunRegistry, RunState};

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
    let end_signal = target.end_signal();

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

    // Between-turn chat reservations have no live stage to validate yet.
    if mode != "passive"
        && (!debugger_between_turn(&target) || store.state_tree()["kind"] != "chat")
    {
        let checked_store = store.clone();
        let checked = tokio::task::spawn_blocking(move || check_agent_stage(&checked_store))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        if let Err(error) = checked {
            let _ = writer
                .write_json_line(&error_response(&format!("gremlin {id}: {error}")))
                .await;
            return;
        }
    }

    // 5. Subscribe to run events before acknowledging.
    let (stream_sub, mut evt_rx) = subscribe_run_lag_tolerant(id, 256);
    let _stream_guard = BusSubGuard::new(stream_sub);

    // 6. Emit attachment acknowledgment with execution identity and a
    //    stable execution nonce. Capabilities are computed from the actual
    //    target's backend capability, not merely its stage type.
    let mut caps = vec!["observe"];
    if mode != "passive" && target.accepts_commands() {
        caps.extend(["message", "talk", "continue", "bail", "quit", "detach"]);
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

    // Preserve pause-on-attach: existing debuggers wait for readiness before
    // sending their first command. Only chat registration gaps reserve entry.
    let lease = if debugger_between_turn(&target) {
        handle.attach_next_turn_debugger(execution_id.clone())
    } else {
        target.attach()
    };
    let mut attachment = match lease {
        Ok(lease) => lease,
        Err(error) => {
            report_attach_error(writer, error).await;
            return;
        }
    };
    if !wait_for_ready(
        &mut attachment,
        &mut run_state,
        &mut reader,
        writer,
        id,
        execution_nonce,
    )
    .await
    {
        return;
    }

    loop {
        tokio::select! {
            biased;
            result = socket::read_json_line(&mut reader) => {
                match result {
                    Ok(Some(cmd)) => {
                        let op = cmd.get("op").and_then(Value::as_str).unwrap_or("");
                        let req_id = cmd.get("req_id").and_then(Value::as_str).map(|s| s.to_string());
                        let should_break = dispatch_attach_command(
                            &cmd, op, req_id, id, writer, &attachment,
                        ).await;
                        if should_break {
                            break;
                        }
                    }
                    _ => {
                        // EOF, disconnect, or a malformed frame: release any
                        // pending lease and close the attachment.
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
            // so the debugger sees the same outcome as the chat rather than
            // a bare socket EOF.
            outcome = &mut attachment.ended => {
                let mut frame = match outcome {
                    Ok(outcome) => {
                        serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null)
                    }
                    Err(_) => serde_json::json!({
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

async fn dispatch_attach_command(
    cmd: &Value,
    op: &str,
    req_id: Option<String>,
    id: &str,
    writer: &SharedWriter,
    attachment: &DebugAttachment,
) -> bool {
    let command = match op {
        "message" => InteractiveCommand::Message(
            cmd["text"].as_str().unwrap_or("").into(),
            req_id.clone(),
            None,
        ),
        "talk" => InteractiveCommand::Inject(cmd["text"].as_str().unwrap_or("").into()),
        "continue" => InteractiveCommand::RunTurn,
        "bail" => {
            InteractiveCommand::Bail(cmd["reason"].as_str().unwrap_or("operator bailed").into())
        }
        "quit" => InteractiveCommand::Quit,
        "detach" => {
            let _ = writer
                .write_json_line(&serde_json::json!({
                    "type": "detached", "scope": "conversation", "id": id,
                }))
                .await;
            return true;
        }
        _ => {
            return writer
                .write_json_line(&error_response(&format!("unknown attach op: {op:?}")))
                .await
                .is_err();
        }
    };
    // Never wait for incorporation here: keep reading pipelined commands and
    // disconnects, preserving the existing enqueue acknowledgement contract.
    let response = match attachment.try_send(command) {
        Ok(()) => match req_id {
            Some(req_id) => {
                serde_json::json!({"type": "command_ack", "scope": "conversation", "req_id": req_id})
            }
            None => return false,
        },
        Err(error) => {
            use tokio::sync::mpsc::error::TrySendError;
            error_response(match error {
                TrySendError::Full(_) => "command queue is full; command not accepted",
                TrySendError::Closed(_) => "attachment is ending; command not accepted",
            })
        }
    };
    writer.write_json_line(&response).await.is_err()
}

fn report_attach_message(error: AttachError) -> &'static str {
    match error {
        AttachError::NoLongerActive => "agent stage is no longer active",
        AttachError::Stale => "execution has been replaced; target is stale",
        AttachError::Busy => "busy",
    }
}

async fn report_attach_error(writer: &SharedWriter, error: AttachError) {
    let _ = writer
        .write_json_line(&error_response(report_attach_message(error)))
        .await;
}

fn debugger_between_turn(target: &crate::clients::interactive::DebugTarget) -> bool {
    // Registration-only handles are observation-live before the first turn,
    // but have no execution to attach to. Ended executions may reserve the
    // next chat turn; a live target (including read-only snapshots) may not.
    target.is_registration_only() || target.end_signal().is_ended()
}

/// Readiness owns its run-stop future; whichever branch loses is dropped.
async fn wait_for_ready(
    lease: &mut DebugAttachment,
    run_state: &mut watch::Receiver<RunState>,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
    id: &str,
    nonce: u64,
) -> bool {
    send_debug_status(writer, "waiting_for_ready").await;
    let ready = tokio::select! {
        biased;
        frame = socket::read_json_line(reader) => {
            if matches!(frame, Ok(Some(_))) {
                let _ = writer.write_json_line(&error_response(
                    "command received before ready; attachment closed",
                )).await;
            }
            return false;
        }
        _ = async { let _ = run_state.wait_for(|state| state.status != "running").await; } => false,
        result = &mut lease.ready => result.is_ok(),
    };
    if !ready {
        let _ = writer
            .write_json_line(&error_response("agent exited before accepting pause"))
            .await;
        return false;
    }
    writer
        .write_json_line(&serde_json::json!({
            "type": "ready", "scope": "conversation", "id": id, "nonce": nonce,
        }))
        .await
        .is_ok()
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
    end_signal: EndSignal,
) {
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

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::clients::interactive::{AttachError, InteractiveSessionFactory};

    #[tokio::test]
    async fn pipelined_message_and_detach_are_both_acknowledged() {
        let (mut factory, handle) = InteractiveSessionFactory::for_chat();
        let _session = factory.next_session();
        let attachment = handle.target().attach().unwrap();
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, write) = server.into_split();
        let writer = SharedWriter::new(write);
        let mut reader = BufReader::new(client);
        let message = serde_json::json!({"text": "correction"});
        assert!(
            !dispatch_attach_command(
                &message,
                "message",
                Some("request-1".into()),
                "chat",
                &writer,
                &attachment,
            )
            .await
        );
        assert!(
            dispatch_attach_command(&Value::Null, "detach", None, "chat", &writer, &attachment,)
                .await
        );
        let ack = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(ack["type"], "command_ack");
        assert_eq!(ack["req_id"], "request-1");
        let detached = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(detached["type"], "detached");
    }

    #[tokio::test]
    async fn registration_target_routes_debugger_to_first_writable_turn() {
        let (mut factory, handle) = InteractiveSessionFactory::for_chat();
        let registration_target = handle.target();
        assert!(debugger_between_turn(&registration_target));

        let reservation = handle.attach_next_turn_debugger(None).unwrap();
        let mut first_turn = factory.next_session();
        assert!(first_turn.accept_pause());
        assert!(reservation.try_send(InteractiveCommand::RunTurn).is_ok());
        assert!(matches!(
            first_turn.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
    }

    #[tokio::test]
    async fn live_targets_and_corrections_cannot_reserve_successors() {
        let (mut factory, handle) = InteractiveSessionFactory::for_chat();
        let mut turn = factory.next_session();
        let live = handle.target();
        assert!(!debugger_between_turn(&live));
        assert!(matches!(
            handle.attach_next_turn_debugger(None),
            Err(AttachError::Busy)
        ));
        assert!(live.attach().is_ok());

        let readonly = live.read_only();
        assert!(!debugger_between_turn(&readonly));
        assert!(matches!(
            handle.attach_next_turn_debugger(None),
            Err(AttachError::Busy)
        ));
        assert!(!turn.accept_pause());
    }
}
