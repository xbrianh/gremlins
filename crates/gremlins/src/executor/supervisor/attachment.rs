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
//! | `detach`    | run           | Close this attachment, keep observing. |
//!
//! Multiple passive readers, at most one controller per execution.
//! Passive display streams are bounded, lag-tolerant; a gap report lets
//! the viewer know output was missed. Command acknowledgments, readiness,
//! and terminal outcomes are not lossy.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;

use crate::clients::interactive::{DebugOutcome, InteractiveCommand};
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket;
use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run_lag_tolerant, EventKind, Recv};

use super::debug::{check_agent_stage, send_debug_status};
use super::error_response;
use super::run_registry::RunRegistry;

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
        EventKind::StreamChunk { .. }
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

    // 4. Validate active leaf — only control-mode needs agent stage.
    if mode != "passive" {
        let stage_check = tokio::task::spawn_blocking(move || check_agent_stage(&store))
            .await
            .map_err(|e| e.to_string())
            .and_then(|result| result);
        if let Err(e) = stage_check {
            let resp = error_response(&format!("gremlin {id}: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    }

    // 5. Subscribe to run events before acknowledging.
    let (stream_sub, mut evt_rx) = subscribe_run_lag_tolerant(id, 256);
    let _stream_guard = BusSubGuard::new(stream_sub);

    // 6. Emit attachment acknowledgment with execution identity.
    let mut caps = vec!["observe"];
    if mode != "passive" {
        caps.extend(["message", "talk", "continue", "bail", "quit", "detach"]);
    }
    let ack = serde_json::json!({
        "type": "attached",
        "scope": "conversation",
        "id": id,
        "nonce": execution_nonce,
        "capabilities": caps,
    });
    if writer.write_json_line(&ack).await.is_err() {
        return;
    }

    if mode == "passive" {
        handle_passive_conversation(id, &mut reader, writer, &mut evt_rx).await;
        return;
    }

    // 7. Active mode: request the pause and wait for ready.
    //    run_state is used only in the select to detect run-stop;
    //    the select uses bare `return` in the run-stop branch so the
    //    watch::Ref never enters the output type (keeps it Send).
    let mut attachment = match target.attach() {
        Ok(a) => a,
        Err(message) => {
            let _ = writer.write_json_line(&error_response(message)).await;
            return;
        }
    };
    // The pause is now pending. Report progress so a client can tell the
    // request was registered before it waits for `ready`.
    send_debug_status(writer, "waiting_for_ready").await;

    let ready = tokio::select! {
        biased;
        _ = socket::read_json_line(&mut reader) => return,
        _ = run_state.wait_for(|state| state.status != "running") => return,
        ready = &mut attachment.ready => ready,
    };
    // run_state is consumed by the select — no lock held across await.

    if ready.is_err() {
        let _ = writer
            .write_json_line(&error_response("agent exited before accepting pause"))
            .await;
        return;
    }

    // 8. Ready — start bidirectional command loop.
    let ready_payload = serde_json::json!({
        "type": "ready",
        "scope": "conversation",
        "id": id,
        "nonce": execution_nonce,
    });
    if writer.write_json_line(&ready_payload).await.is_err() {
        return;
    }

    // 9. Command loop.
    loop {
        tokio::select! {
            biased;
            result = socket::read_json_line(&mut reader) => {
                let Ok(Some(cmd)) = result else {
                    // EOF / disconnect: send Quit to release the pause
                    // deterministically, then wait for the outcome.
                    let _ = attachment.try_send(InteractiveCommand::Quit);
                    break;
                };
                let op = cmd.get("op").and_then(Value::as_str).unwrap_or("");
                // Client-supplied request id for command correlation.
                let req_id = cmd.get("req_id").and_then(Value::as_str).map(|s| s.to_string());
                let command = match op {
                    "message" => {
                        let text = cmd["text"].as_str().unwrap_or("").to_string();
                        InteractiveCommand::Message(text)
                    }
                    "talk" => {
                        let text = cmd["text"].as_str().unwrap_or("").to_string();
                        InteractiveCommand::Inject(text)
                    }
                    "continue" => InteractiveCommand::RunTurn,
                    "bail" => {
                        InteractiveCommand::Bail(cmd["reason"].as_str().unwrap_or("operator bailed").into())
                    }
                    "quit" => InteractiveCommand::Quit,
                    "detach" => {
                        // Detach: release pause, resume autonomous, close attachment.
                        let _ = attachment.try_send(InteractiveCommand::Quit);
                        let detach_frame = serde_json::json!({
                            "type": "detached",
                            "scope": "conversation",
                            "id": id,
                        });
                        let _ = writer.write_json_line(&detach_frame).await;
                        break;
                    }
                    _ => {
                        let resp = error_response(&format!("unknown attach op: {op:?}"));
                        if writer.write_json_line(&resp).await.is_err() { break; }
                        continue;
                    }
                };
                // Never wait for the agent: EOF must still release the slot.
                if let Err(err) = attachment.try_send(command) {
                    use tokio::sync::mpsc::error::TrySendError;
                    let reason = match err {
                        TrySendError::Full(_) => "command queue is full; command not accepted",
                        TrySendError::Closed(_) => "attachment is ending; command not accepted",
                    };
                    if writer.write_json_line(&error_response(reason)).await.is_err() { break; }
                } else if let Some(ref rid) = req_id {
                    // Report delivery after the loop has accepted the text.
                    let ack = serde_json::json!({
                        "type": "command_ack",
                        "scope": "conversation",
                        "req_id": rid,
                    });
                    if writer.write_json_line(&ack).await.is_err() { break; }
                }
            }
            result = evt_rx.recv() => {
                match result {
                    Recv::Event(dispatch) => {
                        let kind = dispatch.kind();
                        let mut payload = conversation_display_event(kind);
                        // Tag every display frame with scope/nonce so clients
                        // can distinguish display events from control replies.
                        if let Some(ref mut p) = payload {
                            if let Some(obj) = p.as_object_mut() {
                                obj.insert("scope".into(), "conversation".into());
                                if obj.get("type").and_then(Value::as_str) == Some("ready") {
                                    // display Ready carries turn, control Ready has id+nonce.
                                }
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
            ended = &mut attachment.ended => {
                let outcome = ended.unwrap_or_else(|_| DebugOutcome::Ended {
                    reason: "agent exited".into(),
                });
                let mut frame = serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = frame.as_object_mut() {
                    obj.insert("scope".into(), "conversation".into());
                }
                let _ = writer.write_json_line(&frame).await;
                break;
            }
        }
    }
}

/// Passive conversation: stream events until the execution ends.
async fn handle_passive_conversation(
    _id: &str,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
    evt_rx: &mut gremlins_magic_bus::ReceiverHalf<EventKind>,
) {
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
