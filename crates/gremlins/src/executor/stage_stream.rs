//! Stage-aware live stream — server-side handler for the `stage_stream`
//! session op.
//!
//! Agent stages replay `artifacts/stream-{stage}.jsonl`; exec stages replay
//! `command_logs/exec-{stage}.log`; every other stage falls back to the
//! operational log. Live following subscribes to the run handle's
//! stage-stream broadcast, or the log broadcast for the operational
//! fallback.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::broadcast;

use crate::clients::interactive::InteractiveEvent;
use crate::config;
use crate::executor::socket;
use crate::executor::state;
use crate::executor::supervisor::{
    error_response, get_run_map, resolve_store_for_id, SharedWriter,
};

// ---------------------------------------------------------------------------
// StageStreamKind
// ---------------------------------------------------------------------------

/// Which kind of stage output a `stage_stream` follow should serve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StageStreamKind {
    /// Agent stage: replay `artifacts/stream-{stage}.jsonl`.
    Agent,
    /// Exec stage: replay `command_logs/exec-{stage}.log`.
    Exec,
    /// Bootstrap / between stages / terminal: fall back to the operational log.
    Other,
}

/// Maximum lines of history to replay before switching to live following.
const STAGE_STREAM_MAX_HISTORY: usize = 500;

// ---------------------------------------------------------------------------
// handle_stage_stream
// ---------------------------------------------------------------------------

/// Emit a stage-stream payload wrapped in the `stage_stream_line` envelope.
/// Returns `false` when the write fails (client disconnected).
macro_rules! stage_line {
    ($writer:expr, $offset:expr, $value:expr) => {{
        let payload = $crate::executor::stage_stream::stage_line_envelope($value, $offset);
        if $writer.write_json_line(&payload).await.is_err() {
            return;
        }
        $offset += 1;
    }};
}

/// Streaming op: serve a stage-aware live stream for a gremlin.
///
/// Agent stages replay `artifacts/stream-{stage}.jsonl`; exec stages replay
/// `command_logs/exec-{stage}.log`; every other stage falls back to the
/// operational log. Live following subscribes to the run handle's stage-stream
/// broadcast, or the log broadcast for the operational fallback.
///
/// When a `stage_boundary` line arrives on the broadcast, the handler
/// reclassifies the current stage, switches subscriptions, and replays the
/// tail of the new blob so the client sees the stage transition.
pub(crate) async fn handle_stage_stream(
    request: &Value,
    _state_root: &Path,
    mut reader: BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        let resp = error_response("missing 'id' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    if crate::executor::gremlin::validate_gremlin_id(id).is_err() {
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let store = match resolve_store_for_id(id) {
        Ok(store) => store,
        Err(resp) => {
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    let follow = request
        .get("follow")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Classify the gremlin's current stage so we can pick the right source.
    let stage_name = store
        .state_tree()
        .get("stage")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = classify_stage_stream(&store, &stage_name).await;

    serve_stage_stream_loop(id, &store, follow, kind, &stage_name, &mut reader, writer).await;
}

/// Core loop: replay history + follow live, with stage-boundary re-evaluation.
async fn serve_stage_stream_loop(
    id: &str,
    store: &Arc<dyn state::StateStore + Send + Sync>,
    follow: bool,
    mut kind: StageStreamKind,
    stage_name: &str,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let mut offset: u64 = 0;

    loop {
        // Subscribe *before* replaying history so lines emitted during replay
        // are buffered and not lost.
        let mut rx = if follow {
            let map = get_run_map().lock().unwrap();
            map.get(id).map(|h| match kind {
                StageStreamKind::Other => h.log_broadcast.subscribe(),
                StageStreamKind::Agent | StageStreamKind::Exec => {
                    h.stage_stream_broadcast.subscribe()
                }
            })
        } else {
            None
        };

        // 1. Replay history from the stage-appropriate blob.
        let safe_name = crate::core::proc::sanitize_log_filename(stage_name);
        let content = match kind {
            StageStreamKind::Agent => {
                read_store_blob(store, &format!("artifacts/stream-{safe_name}.jsonl")).await
            }
            StageStreamKind::Exec => {
                read_store_blob(store, &format!("command_logs/exec-{safe_name}.log")).await
            }
            StageStreamKind::Other => read_store_blob(store, "log").await,
        };

        // Tail only the last STAGE_STREAM_MAX_HISTORY lines to avoid
        // reading an arbitrarily large blob into memory.
        let lines: Vec<&str> = content.lines().collect();
        let tail = tail_lines(&lines, STAGE_STREAM_MAX_HISTORY);
        for line in tail {
            let payload = match kind {
                StageStreamKind::Agent => agent_stream_line(line),
                StageStreamKind::Exec => exec_stream_line(line),
                StageStreamKind::Other => {
                    serde_json::json!({"kind": "log", "text": line})
                }
            };
            stage_line!(writer, offset, payload);
        }

        // 2. If following, drain only for Agent (where broadcast lines match
        //    the file 1:1); for Exec/Other the broadcast format differs from
        //    the blob format, so draining would drop real live lines.
        if let Some(ref mut rx) = rx {
            if matches!(kind, StageStreamKind::Agent) {
                while rx.try_recv().is_ok() {}
            }

            loop {
                tokio::select! {
                    result = rx.recv() => {
                        match result {
                            Ok(line) => {
                                // Check for stage_boundary sentinel.
                                if let Some(boundary) =
                                    parse_stage_boundary(&line)
                                {
                                    let new_kind = boundary_kind(&boundary);
                                    if new_kind != kind {
                                        kind = new_kind;
                                        // Break out of the inner loop to
                                        // re-subscribe and replay the new
                                        // blob.
                                        break;
                                    }
                                    // Same kind — forward the boundary as-is.
                                    stage_line!(writer, offset, boundary);
                                    continue;
                                }

                                match kind {
                                    StageStreamKind::Other => {
                                        stage_line!(writer, offset, serde_json::json!({
                                            "kind": "log",
                                            "text": line
                                        }));
                                    }
                                    StageStreamKind::Agent
                                    | StageStreamKind::Exec => {
                                        stage_line!(writer, offset, live_stage_line(&line));
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                stage_line!(writer, offset, serde_json::json!({
                                    "kind": "lagged",
                                    "skipped": n
                                }));
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                    _ = socket::read_json_line(reader) => { return; }
                }
            }
        } else {
            return;
        }
    }
}

/// Return the last `n` lines from `lines`.
fn tail_lines<'a>(lines: &'a [&'a str], n: usize) -> &'a [&'a str] {
    if lines.len() <= n {
        lines
    } else {
        &lines[lines.len() - n..]
    }
}

// ---------------------------------------------------------------------------
// stage_boundary handling
// ---------------------------------------------------------------------------

/// Try to parse a broadcast line as a `stage_boundary` sentinel.
/// Returns the parsed object when found, or `None`.
fn parse_stage_boundary(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("kind").and_then(|v| v.as_str()) == Some("stage_boundary") {
        Some(v)
    } else {
        None
    }
}

/// Derive a `StageStreamKind` from a stage_boundary payload.
fn boundary_kind(boundary: &Value) -> StageStreamKind {
    match boundary.get("stage_type").and_then(|v| v.as_str()) {
        Some("agent") => StageStreamKind::Agent,
        Some("exec") => StageStreamKind::Exec,
        _ => StageStreamKind::Other,
    }
}

/// Send a stage_boundary sentinel into the stage-stream broadcast when a
/// stage transition occurs. Called from the gremlin run loop.
pub(crate) fn send_stage_boundary(
    tx: &Option<broadcast::Sender<String>>,
    stage_name: &str,
    stage_type: &str,
) {
    if let Some(ref tx) = tx {
        let payload = serde_json::json!({
            "kind": "stage_boundary",
            "stage": stage_name,
            "stage_type": stage_type,
        })
        .to_string();
        let _ = tx.send(payload);
    }
}

// ---------------------------------------------------------------------------
// Envelope / translation helpers
// ---------------------------------------------------------------------------

/// Wrap a stage-stream payload object in the `stage_stream_line` envelope,
/// flattening its fields alongside `type` and `offset`.
pub(crate) fn stage_line_envelope(value: Value, offset: u64) -> Value {
    let mut obj = match value {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("text".to_string(), other);
            map
        }
    };
    obj.insert(
        "type".to_string(),
        Value::String("stage_stream_line".to_string()),
    );
    obj.insert("offset".to_string(), Value::from(offset));
    Value::Object(obj)
}

/// Parse a live stage-stream broadcast line (already JSON) into a value.
/// Plain-string log lines fall back to a `text` payload.
fn live_stage_line(line: &str) -> Value {
    serde_json::from_str::<Value>(line)
        .unwrap_or_else(|_| serde_json::json!({"kind": "text", "text": line}))
}

/// Convert one line of the agent raw stream (`artifacts/stream-*.jsonl`) into a
/// stage-stream payload, deriving `kind` from the event `type` field.
pub(crate) fn agent_stream_line(line: &str) -> Value {
    let parsed = serde_json::from_str::<Value>(line).unwrap_or(Value::Null);
    let kind = match parsed.get("type").and_then(|v| v.as_str()) {
        Some("stream_chunk") => "text",
        Some("reasoning_chunk") => "reasoning",
        Some("tool_result") => "tool_result",
        Some("tool_output_chunk") => "tool_output",
        Some("turn_complete") => "turn_complete",
        Some("done") => "done",
        Some("stream_reset") => "stream_reset",
        _ => "unknown",
    };
    let mut obj = match parsed {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    obj.insert("kind".to_string(), Value::String(kind.to_string()));
    Value::Object(obj)
}

/// Convert one line of an exec command log (`command_logs/exec-*.log`) into a
/// stage-stream payload.
///
/// The historical blob intermixes stdout and stderr without tagging, so all
/// lines are classified as `"stdout"`. Live output is correctly tagged at
/// source by the executor layer.
pub(crate) fn exec_stream_line(line: &str) -> Value {
    serde_json::json!({"kind": "stdout", "text": line})
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Classify the gremlin's current stage for stage-stream routing.
async fn classify_stage_stream(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    stage_name: &str,
) -> StageStreamKind {
    if stage_name.is_empty() {
        return StageStreamKind::Other;
    }
    let default_model = config::global_config()
        .ok()
        .and_then(|c| c.default_model().map(String::from));
    let store_for_read = Arc::clone(store);
    let stage_name_owned = stage_name.to_string();
    let stage_type = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut blob = store_for_read
            .open("definition.yaml", state::BlobMode::Read)
            .ok()?;
        let mut bytes = Vec::new();
        blob.read_to_end(&mut bytes).ok()?;
        let def = crate::definition::StaticDefinition::from_expanded_bytes(
            &bytes,
            None,
            default_model.as_deref(),
        )
        .ok()?;
        def.stages
            .iter()
            .find(|s| s.name() == stage_name_owned.as_str())
            .map(|s| s.stage_type().to_string())
    })
    .await
    .ok()
    .flatten();
    match stage_type.as_deref() {
        Some("agent") => StageStreamKind::Agent,
        Some("exec") => StageStreamKind::Exec,
        _ => StageStreamKind::Other,
    }
}

/// Read a store blob into a string, returning empty when it is absent.
async fn read_store_blob(store: &Arc<dyn state::StateStore + Send + Sync>, name: &str) -> String {
    if !store.exists(name) {
        return String::new();
    }
    match store.open(name, state::BlobMode::Read) {
        Ok(mut blob) => tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut s = String::new();
            blob.read_to_string(&mut s).ok();
            s
        })
        .await
        .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Agent event → stage-stream line translation
// ---------------------------------------------------------------------------

/// Spawn a task that translates agent `InteractiveEvent`s into stage-stream
/// JSON lines and forwards them to the stage-stream broadcast.
pub(crate) fn spawn_stage_stream_forwarder(
    mut evt_rx: broadcast::Receiver<InteractiveEvent>,
    stage_stream_tx: broadcast::Sender<String>,
) {
    tokio::spawn(async move {
        loop {
            match evt_rx.recv().await {
                Ok(event) => {
                    if let Some(line) = interactive_event_to_stage_line(&event) {
                        let _ = stage_stream_tx.send(line);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Convert an agent `InteractiveEvent` into a stage-stream JSON line.
///
/// Returns `None` for interactive lifecycle events (`Ready`/`Ended`) that are
/// not stage content.
pub(crate) fn interactive_event_to_stage_line(event: &InteractiveEvent) -> Option<String> {
    use serde_json::json;
    match event {
        InteractiveEvent::ReasoningChunk { text } => {
            Some(json!({"kind": "reasoning", "text": text}).to_string())
        }
        InteractiveEvent::StreamChunk { text } => {
            Some(json!({"kind": "text", "text": text}).to_string())
        }
        InteractiveEvent::ToolOutputChunk {
            id,
            name,
            call,
            text,
        } => Some(
            json!({"kind": "tool_output", "id": id, "name": name, "call": call, "text": text})
                .to_string(),
        ),
        InteractiveEvent::ToolResult {
            id,
            name,
            call,
            output,
        } => Some(
            json!({"kind": "tool_result", "id": id, "name": name, "call": call, "output": output})
                .to_string(),
        ),
        InteractiveEvent::TurnComplete {
            turn,
            text,
            tool_calls,
            stats: _,
            totals: _,
        } => Some(
            json!({"kind": "turn_complete", "turn": turn, "text": text, "tool_calls": tool_calls})
                .to_string(),
        ),
        InteractiveEvent::Done {
            text,
            summary,
            usage: _,
        } => Some(json!({"kind": "done", "text": text, "summary": summary}).to_string()),
        InteractiveEvent::StreamReset => Some(json!({"kind": "stream_reset"}).to_string()),
        // Ready/Ended are interactive lifecycle events, not stage content.
        InteractiveEvent::Ready { .. } | InteractiveEvent::Ended { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interactive_event_to_stage_line_maps_agent_events() {
        let line = interactive_event_to_stage_line(&InteractiveEvent::StreamChunk {
            text: "hi".to_string(),
        })
        .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "text");
        assert_eq!(v["text"], "hi");

        let line = interactive_event_to_stage_line(&InteractiveEvent::ReasoningChunk {
            text: "think".to_string(),
        })
        .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "reasoning");
        assert_eq!(v["text"], "think");

        let line = interactive_event_to_stage_line(&InteractiveEvent::ToolResult {
            id: "call_1".to_string(),
            name: "Bash".to_string(),
            call: "Bash(command=\"ls\")".to_string(),
            output: "ok".to_string(),
        })
        .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "tool_result");
        assert_eq!(v["name"], "Bash");

        // Lifecycle events are not stage content.
        assert!(interactive_event_to_stage_line(&InteractiveEvent::Ready { turn: 0 }).is_none());
        assert!(interactive_event_to_stage_line(&InteractiveEvent::Ended {
            reason: "resumed".to_string(),
        })
        .is_none());
    }

    #[test]
    fn agent_stream_line_derives_kind_from_type() {
        assert_eq!(
            agent_stream_line(r#"{"type":"stream_chunk","text":"hi"}"#)["kind"],
            "text"
        );
        assert_eq!(
            agent_stream_line(r#"{"type":"reasoning_chunk","text":"x"}"#)["kind"],
            "reasoning"
        );
        assert_eq!(
            agent_stream_line(r#"{"type":"tool_result","output":"y"}"#)["kind"],
            "tool_result"
        );
        assert_eq!(
            agent_stream_line(r#"{"type":"stream_reset"}"#)["kind"],
            "stream_reset"
        );
        // Unrecognized event types fall back to `unknown`.
        assert_eq!(
            agent_stream_line(r#"{"type":"assistant","text":"z"}"#)["kind"],
            "unknown"
        );
        // Non-JSON input is tolerated.
        assert_eq!(agent_stream_line("not json")["kind"], "unknown");
    }

    #[test]
    fn exec_stream_line_tags_all_as_stdout() {
        // Historical blobs have interleaved stdout/stderr without tagging;
        // we can't distinguish them, so everything is stdout.
        assert_eq!(exec_stream_line("plain output")["kind"], "stdout");
        assert_eq!(exec_stream_line("plain output")["text"], "plain output");
        assert_eq!(exec_stream_line("[stderr] boom")["kind"], "stdout");
    }

    #[test]
    fn stage_line_envelope_flattens_payload() {
        let v = stage_line_envelope(json!({"kind": "text", "text": "hi"}), 3);
        assert_eq!(v["type"], "stage_stream_line");
        assert_eq!(v["kind"], "text");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["offset"], 3);
    }

    #[test]
    fn tail_lines_truncates() {
        let lines: Vec<&str> = vec!["a", "b", "c", "d", "e"];
        assert_eq!(tail_lines(&lines, 3), &["c", "d", "e"]);
        assert_eq!(tail_lines(&lines, 10), &["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn parse_stage_boundary_detects_sentinel() {
        let v = parse_stage_boundary(
            r#"{"kind":"stage_boundary","stage":"implement","stage_type":"agent"}"#,
        );
        assert!(v.is_some());
        assert_eq!(v.unwrap()["stage"], "implement");

        assert!(parse_stage_boundary(r#"{"kind":"text","text":"hi"}"#).is_none());
        assert!(parse_stage_boundary("not json").is_none());
    }

    #[test]
    fn boundary_kind_maps_types() {
        assert_eq!(
            boundary_kind(&json!({"kind":"stage_boundary","stage_type":"agent"})),
            StageStreamKind::Agent
        );
        assert_eq!(
            boundary_kind(&json!({"kind":"stage_boundary","stage_type":"exec"})),
            StageStreamKind::Exec
        );
        assert_eq!(
            boundary_kind(&json!({"kind":"stage_boundary","stage_type":"bootstrap"})),
            StageStreamKind::Other
        );
    }
}
