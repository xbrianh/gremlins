//! Interactive debug handler (`handle_debug` op).

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;
use tokio::sync::broadcast;

use crate::clients::interactive::{InteractiveCommand, InteractiveEvent};
use crate::config;
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket;
use crate::executor::state;

use super::ops::resolve_store_for_id;
use super::{error_response, get_run_map, DebugDetachGuard, SharedWriter};

pub(crate) async fn send_debug_status(writer: &SharedWriter, stage: &str) {
    let payload = serde_json::json!({"type": "status", "stage": stage});
    let _ = writer.write_json_line(&payload).await;
}

pub(crate) async fn handle_debug(
    request: &Value,
    _state_root: &Path,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    send_debug_status(writer, "validating_id").await;

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

    // Resolve a StateStore: for live gremlins (including ephemeral) use the
    // handle stored in the RunHandle; otherwise open the persistent store
    // under `config::state_root()`.
    let store = match resolve_store_for_id(id) {
        Ok(store) => store,
        Err(resp) => {
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // Validate that the gremlin is currently in an agent stage.
    // Read the current stage name from the state tree and cross-reference
    // with the definition snapshot to check its type.
    send_debug_status(writer, "checking_stage_type").await;
    {
        let stage_name = store
            .state_tree()
            .get("stage")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if stage_name.is_empty() {
            let resp = error_response(&format!("gremlin {id} has no recorded stage"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }

        let default_model = config::global_config()
            .ok()
            .and_then(|c| c.default_model().map(String::from));
        let store_for_read = Arc::clone(&store);
        let stage_name_for_def = stage_name.clone();
        let is_agent = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut blob = match store_for_read.open("definition.json", state::BlobMode::Read) {
                Ok(b) => b,
                Err(e) => return Err(format!("failed to read definition snapshot: {e}")),
            };
            let mut bytes = Vec::new();
            blob.read_to_end(&mut bytes)
                .map_err(|e| format!("failed to read definition snapshot: {e}"))?;
            let def = crate::definition::StaticDefinition::from_expanded_bytes(
                &bytes,
                None,
                default_model.as_deref(),
            )
            .map_err(|e| format!("failed to load definition: {e}"))?;
            Ok(def
                .stages
                .iter()
                .any(|s| s.name() == stage_name_for_def.as_str() && s.stage_type() == "agent"))
        })
        .await;

        let is_agent = match is_agent {
            Ok(Ok(agent)) => agent,
            Ok(Err(e)) => {
                let resp = error_response(&format!("gremlin {id}: {e}"));
                let _ = writer.write_json_line(&resp).await;
                return;
            }
            Err(e) => {
                let resp = error_response(&format!("gremlin {id}: {e}"));
                let _ = writer.write_json_line(&resp).await;
                return;
            }
        };

        if !is_agent {
            let resp = error_response(&format!(
                "gremlin {id} is not in an agent stage (current: {stage_name})"
            ));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    }

    // Get the interactive handle from the run map.
    let interactive_handle = {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|h| h.interactive.clone())
    };

    let interactive_handle = match interactive_handle {
        Some(h) => h,
        None => {
            send_debug_status(writer, "not_running").await;
            let resp = error_response(&format!("gremlin {id} is not running"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    send_debug_status(writer, "got_handle").await;

    // Subscribe to interactive events *before* triggering pause so we don't miss
    // the Ready broadcast.
    let mut evt_rx = interactive_handle.evt_tx.subscribe();

    // Signal the agent loop to pause at its next yield point.
    let mut detach_guard = DebugDetachGuard::new(interactive_handle.clone());
    interactive_handle.pause.pause();
    send_debug_status(writer, "sent_pause_signal").await;
    send_debug_status(writer, "waiting_for_ready").await;

    // Wait for Ready from the agent loop, racing against cmd_tx.closed().
    let cmd_tx_closed = interactive_handle.cmd_tx.closed();
    tokio::pin!(cmd_tx_closed);
    let ready = loop {
        tokio::select! {
            result = evt_rx.recv() => {
                match result {
                    Ok(InteractiveEvent::Ready { turn: _turn }) => {
                        break true;
                    }
                    Ok(InteractiveEvent::Ended { reason: _reason }) => {
                        break false;
                    }
                    Ok(_other) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Lagged(_n)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break false;
                    }
                }
            }
            _ = &mut cmd_tx_closed => {
                break false;
            }
            result = socket::read_json_line(&mut reader) => {
                // Client sent a message before Ready — decode it.
                // If it's a quit, propagate to the agent so it doesn't stay paused.
                if let Ok(Some(cmd)) = result {
                    let op = cmd.get("op").and_then(|v| v.as_str());
                    if op == Some("quit") {
                        let _ = interactive_handle
                            .cmd_tx
                            .send(InteractiveCommand::Quit)
                            .await;
                    }
                }
                break false;
            }
        }
    };

    if !ready {
        interactive_handle.pause.reset();
        detach_guard.disarm();
        return;
    }

    // Send ready to the client.
    let ready_payload = serde_json::json!({
        "type": "ready",
        "id": id,
    });
    if writer.write_json_line(&ready_payload).await.is_err() {
        let _ = interactive_handle
            .cmd_tx
            .send(InteractiveCommand::Quit)
            .await;
        interactive_handle.pause.reset();
        detach_guard.disarm();
        return;
    }
    log::debug!("handle_debug: ready sent, entering command loop for {id}");

    // Bidirectional loop: read commands from client, forward to agent.
    loop {
        macro_rules! cmd {
            ($cmd:expr) => {
                if interactive_handle.cmd_tx.send($cmd).await.is_err() {
                    break;
                }
            };
        }
        tokio::select! {
            // Read from the client.
            result = socket::read_json_line(&mut reader) => {
                match result {
                    Ok(Some(cmd)) => {
                        let op = cmd.get("op").and_then(|v| v.as_str()).unwrap_or("");
                        match op {
                            "talk" => {
                                let text = cmd.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                cmd!(InteractiveCommand::Inject(text.to_string()));
                            }
                            "continue" => {
                                cmd!(InteractiveCommand::RunTurn);
                            }
                            "bail" => {
                                let reason = cmd.get("reason").and_then(|v| v.as_str()).unwrap_or("operator bailed");
                                cmd!(InteractiveCommand::Bail(reason.to_string()));
                            }
                            "quit" => {
                                cmd!(InteractiveCommand::Quit);
                            }
                            _ => {
                                let resp = error_response(&format!("unknown debug op: {op:?}"));
                                let _ = writer.write_json_line(&resp).await;
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        // Client disconnected — send Quit so the agent resumes.
                        let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                        break;
                    }
                }
            }
            // Read events from the agent loop.
            result = evt_rx.recv() => {
                macro_rules! emit {
                    ($payload:expr) => {
                        if writer.write_json_line(&$payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    };
                }
                match result {
                    Ok(InteractiveEvent::Ready { .. }) => {
                        // Agent re-entered interactive mode (after RunOneTurn).
                    }
                    Ok(InteractiveEvent::TurnComplete { turn, text, tool_calls, stats, totals }) => {
                        emit!(serde_json::json!({"type": "turn_complete", "turn": turn, "text": text, "tool_calls": tool_calls, "stats": stats, "totals": totals}));
                    }
                    Ok(InteractiveEvent::Done { text, usage, .. }) => {
                        emit!(serde_json::json!({"type": "done", "text": text, "usage": usage}));
                    }
                    Ok(InteractiveEvent::Ended { reason }) => {
                        let payload = serde_json::json!({"type": "ended", "reason": reason});
                        let _ = writer.write_json_line(&payload).await;
                        break;
                    }
                    Ok(InteractiveEvent::StreamChunk { text }) => {
                        emit!(serde_json::json!({"type": "stream_chunk", "text": text}));
                    }
                    Ok(InteractiveEvent::ReasoningChunk { text }) => {
                        emit!(serde_json::json!({"type": "reasoning_chunk", "text": text}));
                    }
                    Ok(InteractiveEvent::ToolOutputChunk { id, name, call, text }) => {
                        emit!(serde_json::json!({"type": "tool_output_chunk", "id": id, "name": name, "call": call, "text": text}));
                    }
                    Ok(InteractiveEvent::StreamReset) => {
                        emit!(serde_json::json!({"type": "stream_reset"}));
                    }
                    Ok(InteractiveEvent::ToolResult { id, name, call, output }) => {
                        emit!(serde_json::json!({"type": "tool_result", "id": id, "name": name, "call": call, "output": output}));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // Clear the pause signal so the agent loop resumes normal operation.
    detach_guard.disarm();
    interactive_handle.pause.reset();
}
