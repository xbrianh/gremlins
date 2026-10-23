//! Stage-aware live stream — server-side handler for the `stage_stream`
//! session op.
//!
//! Agent stages replay `artifacts/stream-{stage}.jsonl`; exec stages replay
//! `command_logs/exec-{stage}.log`; every other stage falls back to the
//! operational log. Live following subscribes to both the run handle's
//! stage-stream broadcast (for agent/exec content and stage boundaries) and
//! its log broadcast (for the operational fallback).

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
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

/// Streaming op: serve a stage-aware live stream for a gremlin.
///
/// Agent stages replay `artifacts/stream-{stage}.jsonl`; exec stages replay
/// `command_logs/exec-{stage}.log`; every other stage falls back to the
/// operational log. Live following subscribes to the run handle's stage-stream
/// broadcast and log broadcast simultaneously, so stage boundaries are
/// observed even while the current content source is the operational log.
///
/// When a `stage_boundary` line arrives, the handler reclassifies the current
/// stage, replays the tail of the new blob, and then continues following on
/// the already-open receivers — no queued live event is discarded.
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

    let state = store.state_tree();
    let stage_name = state
        .get("sub_stage")
        .and_then(|v| v.as_str())
        .or_else(|| state.get("stage").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    // Terminal runs retain their last stage name; #2056 wants the
    // operational log (and final diagnostics) there instead.
    let status = state.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let kind = if !status.is_empty() && status != "running" {
        StageStreamKind::Other
    } else {
        classify_stage_stream(&store, &stage_name).await
    };

    serve_stage_stream_loop(id, &store, follow, kind, &stage_name, &mut reader, writer).await;
}

/// Core loop: replay history + follow live, with stage-boundary re-evaluation.
async fn serve_stage_stream_loop(
    id: &str,
    store: &Arc<dyn state::StateStore + Send + Sync>,
    follow: bool,
    mut kind: StageStreamKind,
    initial_stage_name: &str,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let mut offset: u64 = 0;
    let mut stage_name = initial_stage_name.to_string();
    let mut dedupe = ReplayDedupe::default();

    // Subscribe to both broadcasts *before* replaying history. The stage
    // broadcast carries content for agent/exec stages plus stage-boundary
    // sentinels; the log broadcast carries operational-log fallback content.
    let stage_rx = if follow {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|h| h.stage_stream_broadcast.subscribe())
    } else {
        None
    };
    let log_rx = if follow {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|h| h.log_broadcast.subscribe())
    } else {
        None
    };

    if !replay_source(store, kind, &stage_name, writer, &mut offset, &mut dedupe).await {
        return;
    }

    let Some(stage_rx) = stage_rx else {
        return;
    };
    let Some(log_rx) = log_rx else {
        return;
    };
    let mut stage_rx = Some(stage_rx);
    let mut log_rx = Some(log_rx);

    let mut stage_closed = false;
    let mut log_closed = false;

    loop {
        // Drain events that arrived while we were replaying. Do not discard
        // them; deduplicate only records whose fingerprint is covered by the
        // replay checkpoint, and always process queued stage boundaries.
        if let Some(stage_rx) = stage_rx.as_mut() {
            while let Ok(line) = stage_rx.try_recv() {
                if !process_stage_line(
                    store,
                    &mut kind,
                    &mut stage_name,
                    &line,
                    writer,
                    &mut offset,
                    &mut dedupe,
                )
                .await
                {
                    return;
                }
            }
        }
        if let Some(log_rx) = log_rx.as_mut() {
            while let Ok(line) = log_rx.try_recv() {
                if !process_log_line(kind, &line, writer, &mut offset, &mut dedupe).await {
                    return;
                }
            }
        }

        tokio::select! {
            result = recv_stage(&mut stage_rx), if !stage_closed => {
                match result {
                    Ok(line) => {
                        if !process_stage_line(
                            store,
                            &mut kind,
                            &mut stage_name,
                            &line,
                            writer,
                            &mut offset,
                            &mut dedupe,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        if !send_stage_line(
                            writer,
                            &mut offset,
                            serde_json::json!({ "kind": "lagged", "skipped": n }),
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        stage_closed = true;
                        stage_rx = None;
                        if log_closed {
                            return;
                        }
                        // The run ended. If an inactive boundary was not
                        // observed, force the operational-log fallback so the
                        // final diagnostics are still replayed.
                        if !matches!(kind, StageStreamKind::Other) {
                            kind = StageStreamKind::Other;
                            stage_name.clear();
                            if !replay_source(
                                store,
                                kind,
                                &stage_name,
                                writer,
                                &mut offset,
                                &mut dedupe,
                            )
                            .await
                            {
                                return;
                            }
                        }
                    }
                }
            }
            result = recv_log(&mut log_rx), if !log_closed => {
                match result {
                    Ok(line) => {
                        if !process_log_line(kind, &line, writer, &mut offset, &mut dedupe).await {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        if matches!(kind, StageStreamKind::Other)
                            && !send_stage_line(
                                writer,
                                &mut offset,
                                serde_json::json!({"kind": "lagged", "skipped": n}),
                            )
                            .await
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        log_closed = true;
                        log_rx = None;
                        if stage_closed {
                            return;
                        }
                    }
                }
            }
            _ = socket::read_json_line(reader) => { return; }
        }
    }
}

async fn recv_stage(
    rx: &mut Option<broadcast::Receiver<String>>,
) -> Result<String, broadcast::error::RecvError> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_log(
    rx: &mut Option<broadcast::Receiver<String>>,
) -> Result<String, broadcast::error::RecvError> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Process one line from the stage-stream broadcast.
///
/// Stage boundaries reclassify the current source and trigger a replay of the
/// newly active blob. Content lines are forwarded only for agent/exec stages,
/// and only when they were not already replayed.
#[allow(clippy::too_many_arguments)]
async fn process_stage_line(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    kind: &mut StageStreamKind,
    stage_name: &mut String,
    line: &str,
    writer: &SharedWriter,
    offset: &mut u64,
    dedupe: &mut ReplayDedupe,
) -> bool {
    if let Some(boundary) = parse_stage_boundary(line) {
        let new_kind = boundary_kind(&boundary);
        let new_stage = boundary_stage(&boundary);
        if new_kind != *kind || new_stage != *stage_name {
            *kind = new_kind;
            *stage_name = new_stage;
            if !send_stage_line(writer, offset, boundary).await {
                return false;
            }
            return replay_source(store, *kind, stage_name, writer, offset, dedupe).await;
        }
        // Duplicate boundary for the same source — nothing to replay or show.
        return true;
    }

    if matches!(*kind, StageStreamKind::Agent | StageStreamKind::Exec) {
        let value = live_stage_line(line);
        if !dedupe.is_replayed(&value) {
            return send_stage_line(writer, offset, value).await;
        }
    }
    true
}

/// Process one line from the operational-log broadcast.
async fn process_log_line(
    kind: StageStreamKind,
    line: &str,
    writer: &SharedWriter,
    offset: &mut u64,
    dedupe: &mut ReplayDedupe,
) -> bool {
    if matches!(kind, StageStreamKind::Other) {
        let value = serde_json::json!({"kind": "log", "text": line});
        if !dedupe.is_replayed(&value) {
            return send_stage_line(writer, offset, value).await;
        }
    }
    true
}

/// Send a stage-stream payload wrapped in the `stage_stream_line` envelope.
/// Returns `false` when the write fails (client disconnected).
async fn send_stage_line(writer: &SharedWriter, offset: &mut u64, value: Value) -> bool {
    let payload = stage_line_envelope(value, *offset);
    if writer.write_json_line(&payload).await.is_err() {
        return false;
    }
    *offset += 1;
    true
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Replay the tail of the blob selected by `kind`/`stage_name`.
async fn replay_source(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    kind: StageStreamKind,
    stage_name: &str,
    writer: &SharedWriter,
    offset: &mut u64,
    dedupe: &mut ReplayDedupe,
) -> bool {
    let safe_name = crate::core::proc::sanitize_log_filename(stage_name);
    let blob = match kind {
        StageStreamKind::Agent => format!("artifacts/stream-{safe_name}.jsonl"),
        StageStreamKind::Exec => format!("command_logs/exec-{safe_name}.log"),
        StageStreamKind::Other => "log".to_string(),
    };

    let lines = read_store_blob_tail(store, &blob, STAGE_STREAM_MAX_HISTORY).await;
    dedupe.reset();
    for line in lines {
        let payloads: Vec<Value> = match kind {
            StageStreamKind::Agent => agent_stream_lines(&line),
            StageStreamKind::Exec => vec![exec_stream_line(&line)],
            StageStreamKind::Other => vec![serde_json::json!({"kind": "log", "text": line})],
        };
        for payload in payloads {
            dedupe.insert(&payload);
            if !send_stage_line(writer, offset, payload).await {
                return false;
            }
        }
    }
    true
}

/// Read the last `max_lines` lines of a store blob without loading the whole
/// file into memory. Reads backward in fixed-size chunks and trims to the
/// requested tail before decoding.
async fn read_store_blob_tail(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    name: &str,
    max_lines: usize,
) -> Vec<String> {
    if max_lines == 0 || !store.exists(name) {
        return Vec::new();
    }

    let store = Arc::clone(store);
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let mut blob = match store.open(&name, state::BlobMode::Read) {
            Ok(blob) => blob,
            Err(_) => return Vec::new(),
        };
        read_tail_lines(&mut *blob, max_lines).unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

fn read_tail_lines<R: Read + Seek + ?Sized>(
    blob: &mut R,
    max_lines: usize,
) -> std::io::Result<Vec<String>> {
    let len = blob.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(Vec::new());
    }

    const CHUNK: usize = 8192;
    let mut pos = len;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut newline_count = 0usize;

    // Walk backward until we have seen one more newline than the requested
    // number of lines, so the earliest (partial) line can be trimmed.
    while pos > 0 && newline_count <= max_lines {
        let want = CHUNK.min(pos as usize);
        let start = pos - want as u64;
        blob.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; want];
        blob.read_exact(&mut buf)?;
        newline_count += buf.iter().filter(|&&b| b == b'\n').count();
        chunks.push(buf);
        pos = start;
    }

    chunks.reverse();
    let mut tail = chunks.concat();

    // If we read more than `max_lines` newlines, drop everything up to and
    // including the first newline in the assembled tail. That removes the
    // partial earliest line and leaves exactly the last `max_lines` lines.
    if newline_count > max_lines {
        let mut skip = newline_count - max_lines;
        let mut idx = 0;
        while skip > 0 && idx < tail.len() {
            if tail[idx] == b'\n' {
                skip -= 1;
            }
            idx += 1;
        }
        if idx > 0 {
            tail.drain(..idx);
        }
    }

    let text = String::from_utf8_lossy(&tail).into_owned();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    Ok(lines)
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

/// Extract the stage identity from a stage_boundary payload.
fn boundary_stage(boundary: &Value) -> String {
    boundary
        .get("stage")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Send a stage_boundary sentinel into the stage-stream broadcast when a
/// stage transition occurs. An empty `stage_name` denotes the inactive
/// operational-log fallback (bootstrap, between stages, or terminal).
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
            "active": !stage_name.is_empty(),
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

/// Convert one line of the agent raw stream (`artifacts/stream-*.jsonl`) into
/// stage-stream payloads.
///
/// The persisted raw stream uses turn-level transcript records rather than the
/// live delta schema: `assistant` records carry text/tool-use blocks and
/// `user` records carry tool-result blocks under `message.content`.
pub(crate) fn agent_stream_lines(line: &str) -> Vec<Value> {
    let parsed = match serde_json::from_str::<Value>(line) {
        Ok(parsed) => parsed,
        Err(_) => return vec![serde_json::json!({"kind": "unknown", "text": line})],
    };

    match parsed.get("type").and_then(|v| v.as_str()) {
        Some("assistant") => decode_assistant_content(&parsed),
        Some("user") => decode_user_content(&parsed),
        Some("reminder") => vec![serde_json::json!({
            "kind": "log",
            "text": parsed.get("message").and_then(|v| v.as_str()).unwrap_or("")
        })],
        // Backward compatibility with the previous live-event schema.
        Some("stream_chunk") => vec![serde_json::json!({
            "kind": "text",
            "text": parsed.get("text").and_then(|v| v.as_str()).unwrap_or("")
        })],
        Some("reasoning_chunk") => vec![serde_json::json!({
            "kind": "reasoning",
            "text": parsed.get("text").and_then(|v| v.as_str()).unwrap_or("")
        })],
        Some("tool_output_chunk") => vec![serde_json::json!({
            "kind": "tool_output",
            "id": parsed.get("id").cloned().unwrap_or(Value::Null),
            "name": parsed.get("name").cloned().unwrap_or(Value::Null),
            "call": parsed.get("call").cloned().unwrap_or(Value::Null),
            "text": parsed.get("text").cloned().unwrap_or(Value::Null),
        })],
        Some("tool_result") => vec![serde_json::json!({
            "kind": "tool_result",
            "id": parsed.get("id").cloned().unwrap_or(Value::Null),
            "name": parsed.get("name").cloned().unwrap_or(Value::Null),
            "call": parsed.get("call").cloned().unwrap_or(Value::Null),
            "output": parsed.get("output").cloned().unwrap_or(Value::Null),
        })],
        Some("turn_complete") => vec![serde_json::json!({
            "kind": "turn_complete",
            "turn": parsed.get("turn").cloned().unwrap_or(Value::Null),
            "text": parsed.get("text").cloned().unwrap_or(Value::Null),
            "tool_calls": parsed.get("tool_calls").cloned().unwrap_or(Value::Null),
        })],
        Some("done") => vec![serde_json::json!({
            "kind": "done",
            "text": parsed.get("text").cloned().unwrap_or(Value::Null),
            "summary": parsed.get("summary").cloned().unwrap_or(Value::Null),
        })],
        Some("stream_reset") => vec![serde_json::json!({"kind": "stream_reset"})],
        _ => vec![serde_json::json!({"kind": "unknown", "text": line})],
    }
}

/// Convert one line of the agent raw stream into a single stage-stream payload
/// (the first decoded payload for transcript records with multiple blocks).
#[cfg(test)]
pub(crate) fn agent_stream_line(line: &str) -> Value {
    agent_stream_lines(line)
        .into_iter()
        .next()
        .unwrap_or_else(|| serde_json::json!({"kind": "unknown", "text": line}))
}

fn decode_assistant_content(record: &Value) -> Vec<Value> {
    let Some(content) = record.pointer("/message/content").and_then(Value::as_array) else {
        return vec![serde_json::json!({
            "kind": "unknown",
            "text": record.to_string()
        })];
    };

    let mut out = Vec::new();
    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => out.push(serde_json::json!({
                "kind": "text",
                "text": block.get("text").and_then(|v| v.as_str()).unwrap_or("")
            })),
            Some("tool_use") => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let call = format_tool_use(block);
                out.push(serde_json::json!({
                    "kind": "tool_call",
                    "id": id,
                    "name": name,
                    "call": call,
                    "text": call,
                }));
            }
            _ => {}
        }
    }

    if out.is_empty() {
        vec![serde_json::json!({
            "kind": "unknown",
            "text": record.to_string()
        })]
    } else {
        out
    }
}

fn decode_user_content(record: &Value) -> Vec<Value> {
    let Some(content) = record.pointer("/message/content").and_then(Value::as_array) else {
        return vec![serde_json::json!({
            "kind": "unknown",
            "text": record.to_string()
        })];
    };

    let mut out = Vec::new();
    for block in content {
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
            let id = block
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let text = block.get("content").and_then(|v| v.as_str()).unwrap_or("");
            out.push(serde_json::json!({
                "kind": "tool_result",
                "id": id,
                "name": "",
                "call": "",
                "output": text,
                "text": text,
            }));
        }
    }

    if out.is_empty() {
        vec![serde_json::json!({
            "kind": "unknown",
            "text": record.to_string()
        })]
    } else {
        out
    }
}

fn format_tool_use(block: &Value) -> String {
    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
    match block.get("input") {
        Some(Value::Object(map)) if !map.is_empty() => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{key}={}", compact_json(value)))
                .collect();
            format!("{name}({})", parts.join(", "))
        }
        Some(Value::String(s)) => format!("{name}({s})"),
        _ => name.to_string(),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
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

// ---------------------------------------------------------------------------
// Replay deduplication
// ---------------------------------------------------------------------------

/// Set of fingerprints for records already replayed from a blob. Live
/// broadcast events that arrive during replay are checked against this
/// checkpoint rather than being discarded.
#[derive(Default)]
struct ReplayDedupe {
    fingerprints: HashSet<String>,
}

impl ReplayDedupe {
    fn reset(&mut self) {
        self.fingerprints.clear();
    }

    fn insert(&mut self, payload: &Value) {
        self.fingerprints.insert(payload_fingerprint(payload));
    }

    fn is_replayed(&self, payload: &Value) -> bool {
        self.fingerprints.contains(&payload_fingerprint(payload))
    }
}

fn payload_fingerprint(payload: &Value) -> String {
    let kind = payload.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "reasoning" | "text" | "log" | "stdout" | "stderr" | "tool_call" => {
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            format!("{kind}:{text}")
        }
        "tool_output" | "tool_result" => {
            if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    return format!("{kind}:{id}");
                }
            }
            let text = payload
                .get("text")
                .or_else(|| payload.get("output"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{kind}:{text}")
        }
        "turn_complete" => {
            let turn = payload
                .get("turn")
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
                .unwrap_or_default();
            format!("turn_complete:{turn}")
        }
        "done" => {
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            format!("done:{text}")
        }
        _ => serde_json::to_string(payload).unwrap_or_default(),
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
            stats,
            totals,
        } => Some(
            json!({"kind": "turn_complete", "turn": turn, "text": text, "tool_calls": tool_calls, "stats": stats, "totals": totals})
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
    fn agent_stream_line_decodes_persisted_transcript_records() {
        assert_eq!(
            agent_stream_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#
            )["kind"],
            "text"
        );
        assert_eq!(
            agent_stream_line(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call_1","name":"Bash","input":{"cmd":"ls"}}]}}"#
            )["kind"],
            "tool_call"
        );
        assert_eq!(
            agent_stream_line(
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","content":"ok"}]}}"#
            )["kind"],
            "tool_result"
        );
        assert_eq!(
            agent_stream_line(r#"{"type":"reminder","message":"nudge"}"#)["kind"],
            "log"
        );
    }

    #[test]
    fn agent_stream_line_keeps_legacy_live_event_compatibility() {
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
    fn agent_stream_lines_decodes_multiple_assistant_blocks() {
        let lines = agent_stream_lines(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"c1","name":"Bash","input":{"cmd":"ls"}}]}}"#,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "text");
        assert_eq!(lines[0]["text"], "hi");
        assert_eq!(lines[1]["kind"], "tool_call");
        assert_eq!(lines[1]["name"], "Bash");
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
    fn parse_stage_boundary_detects_sentinel() {
        let v = parse_stage_boundary(
            r#"{"kind":"stage_boundary","stage":"implement","stage_type":"agent"}"#,
        );
        assert!(v.is_some());
        let v = v.unwrap();
        assert_eq!(v["stage"], "implement");
        assert_eq!(boundary_stage(&v), "implement");

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

    #[test]
    fn read_tail_lines_keeps_only_requested_tail() {
        let mut data = Vec::new();
        for i in 0..10 {
            data.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        let mut cursor = std::io::Cursor::new(data);
        let tail = read_tail_lines(&mut cursor, 3).unwrap();
        assert_eq!(tail, vec!["line 7", "line 8", "line 9"]);
    }

    #[test]
    fn read_tail_lines_handles_no_trailing_newline() {
        let data = b"one\ntwo\nthree";
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let tail = read_tail_lines(&mut cursor, 2).unwrap();
        assert_eq!(tail, vec!["two", "three"]);
    }

    #[test]
    fn payload_fingerprint_matches_replayed_and_live_exec_lines() {
        let replayed = exec_stream_line("hello");
        let live = serde_json::json!({"kind": "stdout", "text": "hello", "ts": 123});
        assert_eq!(payload_fingerprint(&replayed), payload_fingerprint(&live));
    }
}
