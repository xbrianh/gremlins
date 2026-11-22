use serde_json::Value;

/// Parse a stream-json line and extract state updates.
/// Returns None if the line is not valid JSON or not a relevant event.
pub(crate) fn decode_line(line: &[u8]) -> Option<Value> {
    let evt: Value = serde_json::from_slice(line).ok()?;
    if evt.is_object() {
        Some(evt)
    } else {
        None
    }
}

/// Extract cost_usd, result_text, is_error, and api_error_status from a result event.
pub(crate) fn extract_state(evt: &Value, state: &mut StreamState) {
    if evt.get("type").and_then(|v| v.as_str()) != Some("result") {
        return;
    }
    if let Some(cost) = evt
        .get("total_cost_usd")
        .or_else(|| evt.get("cost_usd"))
        .and_then(|v| v.as_f64())
    {
        state.cost_usd = Some(cost);
    }
    if let Some(result) = evt.get("result").and_then(|v| v.as_str()) {
        state.result_text = Some(result.to_string());
    }
    state.is_error = evt
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    state.api_error_status = evt
        .get("api_error_status")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
}

#[derive(Debug, Default)]
pub(crate) struct StreamState {
    pub cost_usd: Option<f64>,
    pub(crate) result_text: Option<String>,
    pub(crate) is_error: bool,
    pub(crate) api_error_status: Option<i32>,
}

/// Emit a stream-json event to stderr (via `log`).
/// When `on_log` is provided, it is also called with each formatted log line.
/// The caller owns bus emission and captures the producer's execution identity.
pub(crate) fn emit_event(
    prefix: &str,
    evt: &Value,
    on_log: Option<&(dyn Fn(String) + Send + Sync)>,
) {
    use super::format::trunc;

    let log_info = |msg: String| {
        log::info!("{msg}");
        if let Some(ref cb) = on_log {
            cb(msg);
        }
    };
    let log_warn = |msg: String| {
        log::warn!("{msg}");
        if let Some(ref cb) = on_log {
            cb(msg);
        }
    };

    let evt_type = evt.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match evt_type {
        "system" => {
            if evt.get("subtype").and_then(|v| v.as_str()) == Some("init") {
                let model = evt.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                let cwd = evt.get("cwd").and_then(|v| v.as_str()).unwrap_or("?");
                let msg = format!(
                    "{}using client model={} cwd={} reasoning_effort={}",
                    prefix, model, cwd, "default"
                );
                log_info(msg);
            }
        }
        "assistant" => {
            if let Some(content) = evt
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for c in content {
                    match c.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            let msg = format!("{}text: {}", prefix, trunc(text, 200));
                            log_info(msg);
                        }
                        Some("thinking") => {
                            let thinking = c.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                            let msg = format!("{}think: {}", prefix, trunc(thinking, 200));
                            log_info(msg);
                        }
                        Some("tool_use") => {
                            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let arg = tool_arg(c);
                            let msg = format!("{}tool: {} {}", prefix, name, trunc(&arg, 200));
                            log_info(msg);
                        }
                        _ => {}
                    }
                }
            }
        }
        "user" => {
            if let Some(content) = evt
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for c in content {
                    if c.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        let is_error = c.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                        let body = tool_result_body(c.get("content").unwrap_or(&Value::Null));
                        if is_error {
                            let msg = format!("{}result ERROR: {}", prefix, trunc(&body, 200));
                            log_warn(msg);
                        } else {
                            let msg = format!("{}result: {}", prefix, trunc(&body, 200));
                            log_info(msg);
                        }
                    }
                }
            }
        }
        "result" => {
            let cost = evt
                .get("total_cost_usd")
                .or_else(|| evt.get("cost_usd"))
                .and_then(|v| v.as_f64());
            let cost_str = cost.map_or("?".to_string(), |c| format!("{c}"));
            let subtype = evt.get("subtype").and_then(|v| v.as_str()).unwrap_or("?");
            let turns = evt
                .get("num_turns")
                .and_then(|v| v.as_i64())
                .map_or("?".to_string(), |t| t.to_string());
            let msg = format!("{prefix}final: subtype={subtype} turns={turns} cost={cost_str}");
            log_info(msg);
        }
        _ => {}
    }
}

fn tool_arg(c: &Value) -> String {
    let inp = c.get("input").and_then(|v| v.as_object());
    if let Some(inp) = inp {
        for k in &["file_path", "command", "pattern", "url", "output_file"] {
            if let Some(v) = inp.get(*k).and_then(|v| v.as_str()) {
                return v.to_string();
            }
        }
    }
    String::new()
}

fn tool_result_body(body: &Value) -> String {
    match body {
        Value::Array(arr) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|p| {
                    p.get("text")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
            parts.join(" ")
        }
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn forwards_each_formatted_line_to_the_producer_callback() {
        let lines = Mutex::new(Vec::new());
        let on_log = |line| lines.lock().unwrap().push(line);
        for evt in [
            serde_json::json!({"type": "system", "subtype": "init", "model": "model", "cwd": "/work"}),
            serde_json::json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "hello"},
                {"type": "thinking", "thinking": "consider"},
                {"type": "tool_use", "name": "Read", "input": {"file_path": "file.rs"}}
            ]}}),
            serde_json::json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "is_error": true, "content": "failed"}
            ]}}),
            serde_json::json!({"type": "result", "subtype": "success", "num_turns": 1, "total_cost_usd": 0.5}),
        ] {
            emit_event("[run] ", &evt, Some(&on_log));
        }
        assert_eq!(
            *lines.lock().unwrap(),
            [
                "[run] using client model=model cwd=/work reasoning_effort=default",
                "[run] text: hello",
                "[run] think: consider",
                "[run] tool: Read file.rs",
                "[run] result ERROR: failed",
                "[run] final: subtype=success turns=1 cost=0.5",
            ]
        );
    }
}
