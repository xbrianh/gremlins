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

/// Emit a stream-json event to stderr in the standard format.
pub(crate) fn emit_event(prefix: &str, evt: &Value) {
    use crate::clients::log_util::trunc;

    let evt_type = evt.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match evt_type {
        "system" => {
            if evt.get("subtype").and_then(|v| v.as_str()) == Some("init") {
                let model = evt.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                let cwd = evt.get("cwd").and_then(|v| v.as_str()).unwrap_or("?");
                log::info!(
                    "{}init model={} cwd={} reasoning_effort={}",
                    prefix,
                    model,
                    cwd,
                    "default"
                );
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
                            log::info!("{}text: {}", prefix, trunc(text, 200));
                        }
                        Some("thinking") => {
                            let thinking = c.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                            log::info!("{}think: {}", prefix, trunc(thinking, 200));
                        }
                        Some("tool_use") => {
                            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let arg = tool_arg(c);
                            log::info!("{}tool: {} {}", prefix, name, trunc(&arg, 200));
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
                            log::warn!("{}result ERROR: {}", prefix, trunc(&body, 200));
                        } else {
                            log::info!("{}result: {}", prefix, trunc(&body, 200));
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
            log::info!("{prefix}final: subtype={subtype} turns={turns} cost={cost_str}");
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
