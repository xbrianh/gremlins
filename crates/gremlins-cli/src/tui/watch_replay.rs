//! Client-side replay and dedup for the `/watch` stream pane.
//!
//! When a watch follow starts, this module replays the tail of the
//! stage-appropriate blob (agent stream, exec log, or operational log),
//! builds a dedup fingerprint set, and then attaches to the live
//! `stage_stream` socket. Lines already replayed are skipped by the
//! live follower so the watch pane never shows duplicates.

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

/// Build a dedup fingerprint from an old-style `stage_stream_line`
/// envelope value.
fn fingerprint(env: &serde_json::Value) -> String {
    let kind = env.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let text = env.get("text").and_then(|v| v.as_str()).unwrap_or("");
    format!("{kind}:{text}")
}

/// Compute a dedup fingerprint from a raw line (either an EventKind
/// JSON value or an old-style `stage_stream_line` envelope).
///
/// Structural events (`done`, `stream_reset`, etc.) always return
/// `None` — they carry no text content worth deduplicating.
fn live_fingerprint(raw: &serde_json::Value) -> Option<String> {
    if raw.get("type").and_then(|v| v.as_str()) == Some("stage_stream_line") {
        let kind = raw.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let text = raw.get("text").and_then(|v| v.as_str()).unwrap_or("");
        return Some(format!("{kind}:{text}"));
    }
    // Structural events carry no text content worth deduplicating.
    // Match raw type tags directly rather than classified kinds to avoid
    // vocabulary mismatches (e.g. classify_stream_value maps
    // "tool_output_chunk" → "tool_output").
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
/// `stage_stream_line` envelope(s). Replicates the old daemon-side
/// `agent_stream_lines` so replay looks identical to live streaming.
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
    let kind = match parsed.get("type").and_then(|v| v.as_str()) {
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
            return vec![serde_json::json!({
                "type": "stage_stream_line",
                "kind": "log",
                "text": line,
            })];
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
            return vec![serde_json::json!({
                "type": "stage_stream_line",
                "kind": "log",
                "text": line,
            })];
        }
        Some("reminder") => "log",
        // Legacy live-event schema compatibility.
        Some("stream_chunk") => "text",
        Some("reasoning_chunk") => "reasoning",
        Some("tool_output_chunk") => "tool_output",
        Some("tool_result") => "tool_result",
        Some("turn_complete") => "turn_complete",
        Some("done") => "done",
        Some("stream_reset") => "stream_reset",
        _ => "log",
    };
    let text = parsed
        .get("text")
        .or(parsed.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or(line);
    vec![serde_json::json!({
        "type": "stage_stream_line",
        "kind": kind,
        "text": text,
    })]
}

// ── Entry point ────────────────────────────────────────────────

/// Replay the tail of the stage-appropriate blob, build a dedup set,
/// then follow the live stage stream (falling back to operational log).
///
/// All output is sent through `log_tx` tagged with `follow_id` and
/// `generation` so the event loop ignores messages from superseded
/// follows.
pub(crate) async fn replay_and_dedup(
    client: Arc<DaemonClient>,
    follow_id: String,
    current_stage: String,
    generation: u64,
    log_tx: mpsc::UnboundedSender<serde_json::Value>,
) {
    let mut dedup: HashSet<String> = HashSet::new();

    // ── Blob selection ──────────────────────────────────────
    let blob = if current_stage.is_empty() {
        "log".to_string()
    } else if let Ok(def) = client.read_definition(&follow_id).await {
        let stage_type = def
            .get("stages")
            .and_then(|stages| find_in_tree(stages, &current_stage));
        match stage_type.as_deref() {
            Some("agent") => {
                format!("artifacts/stream-{}.jsonl", safe_name(&current_stage))
            }
            Some("exec") => {
                format!("command_logs/exec-{}.log", safe_name(&current_stage))
            }
            _ => "log".to_string(),
        }
    } else {
        "log".to_string()
    };

    // ── Replay ──────────────────────────────────────────────
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

    // ── Live following ──────────────────────────────────────
    let stage_result = client.follow_stage_stream(&follow_id).await;
    match stage_result {
        Ok((mut rx, handle)) => {
            let _guard = AbortOnDrop(handle);
            while let Some(raw) = rx.recv().await {
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
            let _ = log_tx.send(serde_json::json!({
                "type": "view_error",
                "id": follow_id,
                "generation": generation,
                "message": "stage stream closed",
            }));
        }
        Err(_stage_err) => {
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
                    let _ = log_tx.send(serde_json::json!({
                        "type": "view_error",
                        "id": follow_id,
                        "generation": generation,
                        "message": "log stream closed",
                    }));
                }
                Err(e) => {
                    let _ = log_tx.send(serde_json::json!({
                        "type": "view_error",
                        "id": follow_id,
                        "generation": generation,
                        "message": e,
                    }));
                }
            }
        }
    }
}
