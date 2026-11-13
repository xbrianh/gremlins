//! Client-side replay and dedup for the `/watch` stream pane.
//!
//! When a watch follow starts, this module first subscribes to the live
//! `stage_stream` socket (so no output is lost), then replays the tail of
//! the stage-appropriate blob (agent stream, exec log, or operational log)
//! to backfill history, builds a dedup fingerprint set, and then drains
//! the buffered live stream. Lines already replayed are skipped so the
//! watch pane never shows duplicates.
//!
//! The dedup checkpoint is cleared on every `stage_boundary` so one stage's
//! history cannot suppress another stage's identical output.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::tui::client::DaemonClient;
use crate::tui::screen::classify_stream_value;
use crate::tui::AbortOnDrop;

// ── Stage-type classification ──────────────────────────────────

/// Walk a definition's `stages` array looking for `name`; return its `type`.
fn find_in_tree(stages: &serde_json::Value, name: &str) -> Option<String> {
    let children = stages.as_array()?;
    for stage in children {
        if stage.get("name").and_then(|v| v.as_str()) == Some(name) {
            return stage.get("type").and_then(|v| v.as_str()).map(String::from);
        }
    }
    for stage in children {
        if let Some(body) = stage.get("body") {
            if let Some(found) = find_in_tree(body, name) {
                return Some(found);
            }
        }
    }
    None
}

// ── Blob-key helpers ───────────────────────────────────────────

/// Sanitize a stage name for use in a blob key (matches
/// `core::proc::sanitize_log_filename`).
fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' => c,
            _ => '_',
        })
        .collect::<String>()
        .replace("..", "__")
}

// ── Dedup fingerprints ─────────────────────────────────────────

/// Compute a dedup fingerprint from a decoded stage_stream_line envelope.
fn fingerprint(env: &serde_json::Value) -> String {
    let kind = env.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let text = env.get("text").and_then(|v| v.as_str()).unwrap_or("");
    format!("{kind}:{text}")
}

/// Compute a dedup fingerprint from a live EventKind JSON value.
///
/// Returns `None` for structural events (`done`, `stream_reset`,
/// `stage_boundary`, `turn_complete`, `tool_output_chunk`, `tool_result`)
/// — they carry no text content worth deduplicating.
fn live_fingerprint(raw: &serde_json::Value) -> Option<String> {
    // Structural events carry no text content worth deduplicating.
    match raw.get("type").and_then(|v| v.as_str()) {
        Some("done")
        | Some("stream_reset")
        | Some("stage_boundary")
        | Some("turn_complete")
        | Some("tool_output_chunk")
        | Some("tool_result") => return None,
        _ => {}
    }
    // Delegate kind mapping to the canonical classify_stream_value.
    let kind = classify_stream_value(raw)?;
    let text = raw
        .get("text")
        .or(raw.get("line"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Some(format!("{kind}:{text}"))
}

// ── Agent stream decoding ──────────────────────────────────────

/// Format a `tool_use` content block into the canonical display string
/// (e.g. `Bash(command="ls")`).
fn format_tool_use_str(block: &serde_json::Value) -> String {
    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
    match block.get("input") {
        Some(serde_json::Value::Object(map)) if !map.is_empty() => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, value)| {
                    format!("{key}={}", serde_json::to_string(value).unwrap_or_default())
                })
                .collect();
            format!("{name}({})", parts.join(", "))
        }
        Some(serde_json::Value::String(s)) => format!("{name}({s})"),
        _ => name.to_string(),
    }
}

/// Decode one raw line from an `artifacts/stream-*.jsonl` blob into
/// `stage_stream_line` envelope(s).
fn decode_agent_stream_line_to_envelopes(line: &str) -> Vec<serde_json::Value> {
    let parsed = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v,
        Err(_) => {
            return vec![serde_json::json!({
                "type": "stage_stream_line",
                "kind": "log",
                "text": line,
            })];
        }
    };
    match parsed.get("type").and_then(|v| v.as_str()) {
        // Transcript records (persisted format).
        Some("assistant") => {
            if let Some(content) = parsed
                .pointer("/message/content")
                .and_then(|v| v.as_array())
            {
                return content
                    .iter()
                    .filter_map(|block| match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => Some(serde_json::json!({
                            "type": "stage_stream_line",
                            "kind": "text",
                            "text": block.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                        })),
                        Some("tool_use") => {
                            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let call = format_tool_use_str(block);
                            Some(serde_json::json!({
                                "type": "stage_stream_line",
                                "kind": "tool_call",
                                "id": id,
                                "name": name,
                                "call": call,
                                "text": call,
                            }))
                        }
                        _ => None,
                    })
                    .collect();
            }
            vec![serde_json::json!({
                "type": "stage_stream_line",
                "kind": "log",
                "text": line,
            })]
        }
        Some("user") => {
            if let Some(content) = parsed
                .pointer("/message/content")
                .and_then(|v| v.as_array())
            {
                return content
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                            let id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let text = block.get("content").and_then(|v| v.as_str()).unwrap_or("");
                            Some(serde_json::json!({
                                "type": "stage_stream_line",
                                "kind": "tool_result",
                                "id": id,
                                "name": "",
                                "call": "",
                                "output": text,
                                "text": text,
                            }))
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            vec![serde_json::json!({
                "type": "stage_stream_line",
                "kind": "log",
                "text": line,
            })]
        }
        Some("reminder") => vec![serde_json::json!({
            "type": "stage_stream_line",
            "kind": "log",
            "text": line,
        })],
        _ => vec![serde_json::json!({
            "type": "stage_stream_line",
            "kind": "log",
            "text": line,
        })],
    }
}

// ── Entry point ────────────────────────────────────────────────

/// Subscribe to the live stage stream, replay blob history, then drain
/// the buffered live stream with deduplication.
///
/// All output is sent through `log_tx` tagged with `follow_id` and
/// `generation` so the event loop ignores messages from superseded
/// follows.
///
/// The dedup checkpoint is cleared on `stage_boundary` events so one
/// stage's output never suppresses another stage's identical output.
pub(crate) async fn replay_and_dedup(
    client: Arc<DaemonClient>,
    follow_id: String,
    _current_stage: String,
    replay_stage: String,
    generation: u64,
    log_tx: mpsc::UnboundedSender<serde_json::Value>,
) {
    let send_error = |msg: &str| {
        let _ = log_tx.send(serde_json::json!({
            "type": "view_error",
            "id": follow_id,
            "generation": generation,
            "message": msg,
        }));
    };

    // ── Subscribe first so no live output is lost ─────────
    let stage_result = client.follow_stage_stream(&follow_id).await;

    // ── Blob selection ────────────────────────────────────
    let blob = if replay_stage.is_empty() {
        "log".to_string()
    } else if let Ok(def) = client.read_definition(&follow_id).await {
        let stage_type = def
            .get("stages")
            .and_then(|stages| find_in_tree(stages, &replay_stage));
        match stage_type.as_deref() {
            Some("agent") => {
                format!("artifacts/stream-{}.jsonl", safe_name(&replay_stage))
            }
            Some("exec") => {
                format!("command_logs/exec-{}.log", safe_name(&replay_stage))
            }
            _ => "log".to_string(),
        }
    } else {
        "log".to_string()
    };

    let mut dedup: HashSet<String> = HashSet::new();

    // ── Replay blob history ───────────────────────────────
    if let Ok(lines) = client.read_blob(&follow_id, &blob, 500).await {
        'replay: for line in lines {
            let envelopes: Vec<serde_json::Value> = if blob.starts_with("artifacts/stream-") {
                // Agent stream blob — each line is a JSON record.
                decode_agent_stream_line_to_envelopes(&line)
            } else if blob.starts_with("command_logs/exec-") {
                // Exec log — raw text lines.
                vec![serde_json::json!({
                    "type": "stage_stream_line",
                    "kind": "stdout",
                    "text": line,
                })]
            } else {
                // Operational log — strip "] " timestamp prefix.
                let text = line.split_once("] ").map(|(_, msg)| msg).unwrap_or(&line);
                vec![serde_json::json!({
                    "type": "stage_stream_line",
                    "kind": "log",
                    "text": text,
                })]
            };

            for env in envelopes {
                dedup.insert(fingerprint(&env));
                let tagged = serde_json::json!({
                    "type": "view_log",
                    "id": follow_id,
                    "generation": generation,
                    "line": env.to_string(),
                });
                if log_tx.send(tagged).is_err() {
                    break 'replay;
                }
            }
        }
    }

    // ── Drain buffered live stream ────────────────────────
    match stage_result {
        Ok((mut rx, handle)) => {
            let _guard = AbortOnDrop(handle);
            while let Some(raw) = rx.recv().await {
                // Clear dedup on stage boundaries so one stage's history
                // cannot suppress another stage's identical output.
                if raw.get("type").and_then(|v| v.as_str()) == Some("stage_boundary") {
                    dedup.clear();
                }

                // Filter unclassified events — lifecycle traffic like
                // `stage_transition` must not reach the watch pane.
                if classify_stream_value(&raw).is_none() {
                    continue;
                }

                // Skip lines whose fingerprint was already replayed.
                if let Some(fp) = live_fingerprint(&raw) {
                    if dedup.contains(&fp) {
                        continue;
                    }
                }
                let tagged = serde_json::json!({
                    "type": "view_log",
                    "id": follow_id,
                    "generation": generation,
                    "line": raw.to_string(),
                });
                if log_tx.send(tagged).is_err() {
                    break;
                }
            }
            send_error("stage stream closed");
        }
        Err(_) => {
            // Fall back to operational log.
            match client.follow_log(&follow_id).await {
                Ok((mut rx, handle)) => {
                    let _guard = AbortOnDrop(handle);
                    while let Some(raw) = rx.recv().await {
                        let Some(line) = raw.get("line").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let tagged = serde_json::json!({
                            "type": "view_log",
                            "id": follow_id,
                            "generation": generation,
                            "line": line,
                        });
                        if log_tx.send(tagged).is_err() {
                            break;
                        }
                    }
                    send_error("log stream closed");
                }
                Err(e) => {
                    send_error(&e);
                }
            }
        }
    }
}