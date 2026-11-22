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
    let (mut target, between_turn) = handle.attachment_target();
    #[cfg(test)]
    setup_gate::wait(id).await;
    let execution_nonce = target.nonce();
    let mut execution_id = target.execution_id().map(|s| s.to_string());
    // This signal belongs to the snapshot, not a later debugger-bound target:
    // a between-turn handoff may replace the execution while readiness awaits.
    // Capture the end signal for this execution so a passive observer can
    // stop when the execution ends even if no terminal event is emitted on
    // the bus (e.g. the run is cancelled before `Done`/`Ended`).
    let mut end_signal = target.end_signal();

    // Observation never owns a pause. Only explicit debugger intent may
    // reserve entry into a future chat turn.
    let mode = request
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("control");

    // Observation requires a captured execution, including before the first turn.
    if mode != "debugger" && between_turn {
        return;
    }

    // Between-turn chat reservations have no live stage to validate yet.
    if mode == "debugger" && (!between_turn || store.state_tree()["kind"] != "chat") {
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
    let ack_end = (!(mode == "debugger" && between_turn)).then_some(&end_signal);
    if !write_view(writer, &ack, ack_end).await {
        return;
    }

    #[cfg(test)]
    receiver_gate::wait(id).await;

    if mode == "passive" {
        handle_passive_conversation(
            execution_id.as_deref(),
            &mut reader,
            writer,
            &mut evt_rx,
            end_signal,
        )
        .await;
        return;
    }

    // Preserve pause-on-attach: existing debuggers wait for readiness before
    // sending their first command. Only chat registration gaps reserve entry.
    let mut attachment = None;
    if mode == "debugger" {
        let lease = if between_turn {
            handle.attach_next_turn_debugger(execution_id.clone())
        } else {
            target.attach_debugger()
        };
        let mut lease = match lease {
            Ok(lease) => lease,
            Err(error) => {
                report_attach_error(writer, error).await;
                return;
            }
        };
        // TOCTOU: do not wait on a stale ended signal when reserving a future
        // turn; the bound target is selected only after readiness completes.
        let readiness_end = (!between_turn).then(|| target.end_signal());
        let readiness = wait_for_ready_view(
            &mut lease,
            &mut run_state,
            &mut reader,
            writer,
            id,
            readiness_end.as_ref(),
            Some((&mut evt_rx, execution_id.as_deref())),
        )
        .await;
        target = match readiness {
            Ok(target) => target,
            Err(failure) => {
                drop(lease);
                report_readiness_failure(writer, failure, None).await;
                return;
            }
        };
        // Only an explicit between-turn debugger reservation may bind a new
        // execution. Ordinary viewers and corrections retain their snapshot.
        execution_id = target.execution_id().map(str::to_owned);
        end_signal = target.end_signal();
        attachment = Some(lease);
    }

    loop {
        tokio::select! {
            biased;
            result = socket::read_json_line(&mut reader) => {
                match result {
                    Ok(Some(cmd)) => {
                        let op = cmd.get("op").and_then(Value::as_str).unwrap_or("");
                        let req_id = cmd.get("req_id").and_then(Value::as_str).map(|s| s.to_string());
                        if op == "detach" {
                            let _ = write_view(writer, &serde_json::json!({
                                "type": "detached", "scope": "conversation", "id": id,
                            }), Some(&end_signal)).await;
                            break;
                        }
                        if end_signal.is_ended() {
                            // A retained debugger may still be awaiting its
                            // classified outcome, but cannot command an ended
                            // execution or reopen its observation.
                            continue;
                        }
                        if !matches!(op, "message" | "talk" | "continue" | "bail" | "quit") {
                            if !write_view(writer, &error_response("unknown attach op"), Some(&end_signal)).await { break; }
                            continue;
                        }
                        let temporary = attachment.is_none() && op == "message";
                        if attachment.is_none() {
                            let checked_store = store.clone();
                            let checked = tokio::task::spawn_blocking(move || check_agent_stage(&checked_store))
                                .await.map_err(|error| error.to_string()).and_then(|result| result);
                            if let Err(error) = checked {
                                let _ = writer.write_json_line(&error_response(&error)).await;
                                continue;
                            }
                            let lease = if op == "message" { target.attach() } else { target.attach_debugger() };
                            let mut lease = match lease {
                                Ok(lease) => lease,
                                Err(error) => { report_attach_error(writer, error).await; continue; }
                            };
                            if let Err(failure) = wait_for_ready_view(&mut lease, &mut run_state, &mut reader, writer, id, Some(&end_signal), Some((&mut evt_rx, execution_id.as_deref()))).await {
                                drop(lease);
                                let dropped = (op == "message").then(|| message_outcome(false, id, target.nonce(), target.execution_id(), req_id.as_deref()));
                                report_readiness_failure(writer, failure, dropped).await;
                                break;
                            }
                            attachment = Some(lease);
                        }
                        let should_break = if op == "message" {
                            deliver_message(&cmd, req_id, id, target.nonce(), target.execution_id(),
                                writer, &mut reader, &mut run_state, &end_signal, &mut attachment, temporary, &mut evt_rx).await
                        } else {
                            tokio::select! {
                                result = dispatch_attach_command(&cmd, op, req_id, id, writer, attachment.as_ref().unwrap()) => result,
                                _ = end_signal.wait() => true,
                            }
                        };
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
            _ = end_signal.wait(), if attachment.is_none() => break,
            result = evt_rx.recv(), if !end_signal.is_ended() => {
                match result {
                    Recv::Event(dispatch) => {
                        if let Some(p) = matching_display(&dispatch, execution_id.as_deref()) {
                            if !write_view(writer, &p, Some(&end_signal)).await
                                && !(end_signal.is_ended() && attachment.is_some()) { break; }
                        }
                    }
                    Recv::Lagged(skipped) => {
                        let payload = serde_json::json!({
                            "type": "lagged",
                            "scope": "conversation",
                            "skipped": skipped,
                        });
                        if !write_view(writer, &payload, Some(&end_signal)).await
                            && !(end_signal.is_ended() && attachment.is_some()) { break; }
                    }
                    Recv::Closed => break,
                }
            }
            // Report the classified terminal outcome when the execution ends
            // so the debugger sees the same outcome as the chat rather than
            // a bare socket EOF.
            outcome = async {
                match attachment.as_mut() {
                    Some(lease) => (&mut lease.ended).await,
                    None => std::future::pending().await,
                }
            } => {
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
                // Classification may follow the execution's end signal. Give
                // its final control reply a bounded flush, without reopening
                // observation or waiting indefinitely for a stalled viewer.
                tokio::select! {
                    _ = writer.write_json_line(&frame) => {},
                    _ = socket::read_json_line(&mut reader) => {},
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                }
                break;
            }
        }
    }
}

// ── Control-mode command dispatch ──────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn deliver_message(
    cmd: &Value,
    req_id: Option<String>,
    id: &str,
    nonce: u64,
    execution_id: Option<&str>,
    writer: &SharedWriter,
    reader: &mut BufReader<OwnedReadHalf>,
    run_state: &mut watch::Receiver<RunState>,
    end: &EndSignal,
    attachment: &mut Option<DebugAttachment>,
    temporary: bool,
    evt_rx: &mut gremlins_magic_bus::ReceiverHalf<EventKind>,
) -> bool {
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    if let Err(error) = attachment
        .as_ref()
        .unwrap()
        .try_send(InteractiveCommand::Message(
            cmd["text"].as_str().unwrap_or("").into(),
            req_id.clone(),
            Some(tx),
        ))
    {
        let reason = match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                "command queue is full; message not delivered"
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                "attachment is ending; message not delivered"
            }
        };
        if temporary {
            drop(attachment.take());
        }
        return !write_view(writer, &error_response(reason), Some(end)).await;
    }
    let delivered = loop {
        tokio::select! {
            biased;
            result = &mut rx => break result.is_ok(),
            frame = socket::read_json_line(reader) => {
                if !matches!(frame, Ok(Some(_))) {
                    if temporary { drop(attachment.take()); }
                    return true;
                }
                #[cfg(test)]
                delivery_busy_gate::wait(id).await;
                let frame = error_response("busy delivering correction");
                let write = write_view(writer, &frame, Some(end));
                tokio::pin!(write);
                let written = tokio::select! {
                    biased;
                    result = &mut rx => {
                        if temporary { drop(attachment.take()); }
                        let _ = write.await;
                        break result.is_ok();
                    }
                    written = &mut write => written,
                };
                if !written {
                    if end.is_ended() { break false; }
                    if temporary { drop(attachment.take()); }
                    return true;
                }
            }
            _ = end.wait() => break false,
            event = evt_rx.recv() => {
                #[cfg(test)]
                delivery_display_gate::wait(id).await;
                let display = forward_display(event, execution_id, writer, Some(end));
                tokio::pin!(display);
                let forwarded = tokio::select! {
                    biased;
                    result = &mut rx => {
                        // Keep the in-flight write intact, but release control at
                        // incorporation rather than after socket backpressure.
                        if temporary { drop(attachment.take()); }
                        let _ = display.await;
                        break result.is_ok();
                    }
                    forwarded = &mut display => forwarded,
                };
                if !forwarded {
                    if end.is_ended() { break false; }
                    if temporary { drop(attachment.take()); }
                    return true;
                }
            }
            // Discard the watch Ref before select holds an output across socket I/O.
            _ = futures::FutureExt::map(run_state.wait_for(|state| state.status != "running"), |_| ()) => break false,
        }
    };
    // Display or busy-reply I/O can observe the end after incorporation was confirmed.
    // Ending observation must not retract an already-sent command outcome.
    let delivered = delivered || rx.try_recv().is_ok();
    if temporary {
        drop(attachment.take());
    }
    let frame = message_outcome(delivered, id, nonce, execution_id, req_id.as_deref());
    if delivered {
        // Confirmation is a command outcome, not an observation. Expiration
        // cannot retract it; bound only the flush after execution has ended.
        let write = writer.write_json_line(&frame);
        tokio::pin!(write);
        let written = tokio::select! {
            biased;
            result = &mut write => result.is_ok(),
            _ = end.wait() => {
                tokio::select! {
                    biased;
                    result = &mut write => result.is_ok(),
                    _ = writer.closed() => false,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => false,
                }
            }
            _ = writer.closed() => false,
        };
        return !written || end.is_ended();
    }
    tokio::select! {
        _ = writer.write_json_line(&frame) => {},
        _ = socket::read_json_line(reader) => {},
        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
    }
    true
}

fn message_outcome(
    delivered: bool,
    id: &str,
    nonce: u64,
    execution_id: Option<&str>,
    req_id: Option<&str>,
) -> Value {
    serde_json::json!({
        "type": if delivered { "delivered" } else { "correction_dropped" },
        "scope": "conversation", "id": id, "nonce": nonce,
        "execution_id": execution_id, "req_id": req_id,
    })
}

async fn dispatch_attach_command(
    cmd: &Value,
    op: &str,
    req_id: Option<String>,
    id: &str,
    writer: &SharedWriter,
    attachment: &DebugAttachment,
) -> bool {
    let command = match op {
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
    // Debugger commands acknowledge enqueue; messages use incorporation above.
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

enum ReadinessFailure {
    Closed,
    Ended,
    Error(Value),
}

async fn report_readiness_failure(
    writer: &SharedWriter,
    failure: ReadinessFailure,
    dropped: Option<Value>,
) {
    let frame = match failure {
        ReadinessFailure::Closed => return,
        ReadinessFailure::Ended => {
            dropped.unwrap_or_else(|| error_response("agent exited before accepting pause"))
        }
        ReadinessFailure::Error(frame) => frame,
    };
    let _ = writer.write_json_line(&frame).await;
}

/// Readiness owns its run-stop future; whichever branch loses is dropped.
/// Failures are reported by the caller only after releasing the lease.
#[cfg(test)]
async fn wait_for_ready(
    lease: &mut DebugAttachment,
    run_state: &mut watch::Receiver<RunState>,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
    id: &str,
    end: Option<&EndSignal>,
) -> Result<crate::clients::interactive::DebugTarget, ReadinessFailure> {
    wait_for_ready_view(lease, run_state, reader, writer, id, end, None).await
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_ready_view(
    lease: &mut DebugAttachment,
    run_state: &mut watch::Receiver<RunState>,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
    id: &str,
    end: Option<&EndSignal>,
    mut display: Option<(
        &mut gremlins_magic_bus::ReceiverHalf<EventKind>,
        Option<&str>,
    )>,
) -> Result<crate::clients::interactive::DebugTarget, ReadinessFailure> {
    if end.is_some_and(EndSignal::is_ended) {
        return Err(ReadinessFailure::Ended);
    }
    let status_written = tokio::select! {
        _ = send_debug_status(writer, "waiting_for_ready") => true,
        _ = async {
            match end {
                Some(end) => end.wait().await,
                None => std::future::pending().await,
            }
        } => false,
    };
    if !status_written {
        return Err(if end.is_some_and(EndSignal::is_ended) {
            ReadinessFailure::Ended
        } else {
            ReadinessFailure::Closed
        });
    }
    let ready = loop {
        let ready = tokio::select! {
            biased;
            frame = socket::read_json_line(reader) => {
                if matches!(frame, Ok(Some(_))) {
                    return Err(ReadinessFailure::Error(error_response(
                        "command received before ready; attachment closed",
                    )));
                }
                return Err(ReadinessFailure::Closed);
            }
            _ = async { let _ = run_state.wait_for(|state| state.status != "running").await; } => false,
            _ = async {
                match end {
                    Some(end) => end.wait().await,
                    None => std::future::pending().await,
                }
            } => false,
            _ = &mut lease.ended => false,
            result = &mut lease.ready => result.is_ok(),
            event = async {
                match display.as_mut() {
                    Some((rx, _)) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let execution_id = display.as_ref().and_then(|(_, id)| *id);
                if !forward_display(event, execution_id, writer, end).await {
                    return Err(if end.is_some_and(EndSignal::is_ended) {
                        ReadinessFailure::Ended
                    } else {
                        ReadinessFailure::Closed
                    });
                }
                continue;
            }
        };
        break ready;
    };
    if !ready {
        return Err(ReadinessFailure::Ended);
    }
    let target = match lease.bound_target() {
        Ok(target) => target,
        Err(error) => {
            return Err(ReadinessFailure::Error(error_response(
                report_attach_message(error),
            )));
        }
    };
    if !write_view(
        writer,
        &serde_json::json!({
            "type": "ready", "scope": "conversation", "id": id, "nonce": target.nonce(),
        }),
        end,
    )
    .await
    {
        return Err(if end.is_some_and(EndSignal::is_ended) {
            ReadinessFailure::Ended
        } else {
            ReadinessFailure::Closed
        });
    }
    Ok(target)
}

/// Passive conversation: stream events until the execution ends.
///
/// Socket EOF or the latched snapshot end signal closes the viewer, even
/// without a terminal bus event. Buffered successor events never retarget it.
async fn handle_passive_conversation(
    execution_id: Option<&str>,
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
            biased;
            _ = &mut end => break,
            _ = socket::read_json_line(reader) => break,
            result = evt_rx.recv() => {
                if !forward_display(result, execution_id, writer, Some(&end_signal)).await {
                    break;
                }
            }

        }
    }
}

// All viewer phases use this exact execution match. Missing execution identity
// is never a wildcard: registration-only viewers must not follow the next turn.
fn matching_display(
    dispatch: &gremlins_magic_bus::DispatchEvent<EventKind>,
    execution_id: Option<&str>,
) -> Option<Value> {
    let expected = execution_id?;
    if dispatch.corr_id().execution_id() != Some(expected) {
        return None;
    }
    let mut frame = conversation_display_event(dispatch.kind())?;
    if matches!(dispatch.kind(), EventKind::Ready { .. }) {
        frame["type"] = serde_json::json!("execution_ready");
        frame["id"] = serde_json::json!(dispatch.corr_id().run());
        frame["execution_id"] = serde_json::json!(expected);
    }
    frame
        .as_object_mut()?
        .insert("scope".into(), "conversation".into());
    Some(frame)
}

async fn write_view(writer: &SharedWriter, frame: &Value, end: Option<&EndSignal>) -> bool {
    tokio::select! {
        biased;
        _ = async {
            match end {
                Some(end) => end.wait().await,
                None => std::future::pending().await,
            }
        } => false,
        result = writer.write_json_line(frame) => result.is_ok(),
        _ = writer.closed() => false,
    }
}

async fn forward_display(
    event: Recv<EventKind>,
    execution_id: Option<&str>,
    writer: &SharedWriter,
    end: Option<&EndSignal>,
) -> bool {
    let frame = match event {
        Recv::Event(dispatch) => matching_display(&dispatch, execution_id),
        Recv::Lagged(skipped) => Some(serde_json::json!({
            "type": "lagged", "scope": "conversation", "skipped": skipped,
        })),
        Recv::Closed => return false,
    };
    match frame {
        Some(frame) => write_view(writer, &frame, end).await,
        None => true,
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

/// Deterministically hold a subscribed viewer before it consumes bus events.
/// Run-keyed and one-shot so unrelated viewers/tests remain independent.
#[cfg(test)]
pub(crate) mod setup_gate {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::oneshot;

    type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
    static GATES: OnceLock<Mutex<HashMap<String, Gate>>> = OnceLock::new();

    pub(crate) fn install(id: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        assert!(GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(id.into(), (entered_tx, release_rx))
            .is_none());
        (entered_rx, release_tx)
    }

    pub(super) async fn wait(id: &str) {
        let gate = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(id);
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}

#[cfg(test)]
pub(crate) mod receiver_gate {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::oneshot;

    type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
    static GATES: OnceLock<Mutex<HashMap<String, Gate>>> = OnceLock::new();

    pub(crate) fn install(id: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        assert!(GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(id.into(), (entered_tx, release_rx))
            .is_none());
        (entered_rx, release_tx)
    }

    pub(super) async fn wait(id: &str) {
        let gate = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(id);
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}

#[cfg(test)]
pub(crate) mod delivery_display_gate {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::oneshot;

    type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
    static GATES: OnceLock<Mutex<HashMap<String, Gate>>> = OnceLock::new();

    pub(crate) fn install(id: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        assert!(GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(id.into(), (entered_tx, release_rx))
            .is_none());
        (entered_rx, release_tx)
    }

    pub(super) async fn wait(id: &str) {
        let gate = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(id);
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}

/// Hold a selected busy reply before writing it, so confirmation and end can race it.
#[cfg(test)]
pub(crate) mod delivery_busy_gate {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::oneshot;

    type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
    static GATES: OnceLock<Mutex<HashMap<String, Gate>>> = OnceLock::new();

    pub(crate) fn install(id: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        assert!(GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(id.into(), (entered_tx, release_rx))
            .is_none());
        (entered_rx, release_tx)
    }

    pub(super) async fn wait(id: &str) {
        let gate = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(id);
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::clients::interactive::{AttachError, InteractiveSessionFactory};

    #[tokio::test]
    async fn pipelined_continue_and_detach_are_both_acknowledged() {
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
                "continue",
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
    async fn readiness_stops_on_execution_end_while_run_remains_running() {
        let (mut factory, handle) = InteractiveSessionFactory::for_chat();
        let _terminal = factory.supervise_next_turn();
        let session = factory.next_session();
        let target = handle.target();
        let mut lease = target.attach_debugger().unwrap();
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (read, write) = server.into_split();
        let writer = SharedWriter::new(write);
        let mut reader = BufReader::new(read);
        let (_state_tx, mut state) = watch::channel(RunState {
            status: "running".into(),
            id: "chat".into(),
            stage: String::new(),
            started_at: String::new(),
        });
        drop(session);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_ready(
                &mut lease,
                &mut state,
                &mut reader,
                &writer,
                "chat",
                Some(&target.end_signal()),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(ReadinessFailure::Ended)));
        drop(client);
    }

    #[tokio::test]
    async fn ready_lease_precedes_queued_foreign_display() {
        let (mut factory, handle) = InteractiveSessionFactory::for_chat();
        let mut session = factory.next_session();
        let target = handle.target();
        let mut lease = target.attach_debugger().unwrap();
        assert!(session.accept_pause());
        // Readiness is acknowledged when the agent polls its command inbox.
        assert!(futures::poll!(Box::pin(session.next_command())).is_pending());
        let (_client, server) = tokio::net::UnixStream::pair().unwrap();
        let (read, write) = server.into_split();
        let writer = SharedWriter::new(write);
        let mut reader = BufReader::new(read);
        let (_state_tx, mut state) = watch::channel(RunState {
            status: "running".into(),
            id: "readiness-priority".into(),
            stage: String::new(),
            started_at: String::new(),
        });
        let (sub, mut events) = subscribe_run_lag_tolerant("readiness-priority", 256);
        let _guard = BusSubGuard::new(sub);
        for _ in 0..16 {
            gremlins_magic_bus::emit_corr_event(
                &gremlins_magic_bus::CorrId::new()
                    .with_run("readiness-priority")
                    .with_execution_id("foreign"),
                EventKind::StreamChunk {
                    text: "foreign".into(),
                },
            );
        }
        let end = target.end_signal();
        let result = wait_for_ready_view(
            &mut lease,
            &mut state,
            &mut reader,
            &writer,
            "readiness-priority",
            Some(&end),
            Some((&mut events, target.execution_id())),
        )
        .await;
        assert!(result.is_ok());
        // An already-ready lease must not drain even one queued display event.
        for _ in 0..16 {
            assert!(events.try_recv().is_ok());
        }
    }

    #[tokio::test]
    async fn registration_target_routes_debugger_to_first_writable_turn() {
        let (mut factory, handle) = InteractiveSessionFactory::for_chat();
        let (_, between_turn) = handle.attachment_target();
        assert!(between_turn);

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
        let (live, between_turn) = handle.attachment_target();
        assert!(!between_turn);
        assert!(matches!(
            handle.attach_next_turn_debugger(None),
            Err(AttachError::Busy)
        ));
        assert!(live.attach().is_ok());

        let readonly = live.read_only();
        assert!(!readonly.accepts_commands());
        assert!(matches!(
            handle.attach_next_turn_debugger(None),
            Err(AttachError::Busy)
        ));
        assert!(!turn.accept_pause());
    }
}
