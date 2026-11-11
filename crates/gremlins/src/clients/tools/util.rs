/// Escape newlines/carriage-returns and truncate for compact display.
pub(crate) fn preview_str(s: &str, max_chars: usize) -> String {
    let truncated: String = s.chars().take(max_chars).collect();
    let escaped = truncated.replace('\n', "\\n").replace('\r', "\\r");
    if s.chars().count() > max_chars {
        format!("{escaped}…")
    } else {
        escaped
    }
}

/// Classify a tool result as "ok" or "error" for audit logging.
pub(crate) fn result_status(res: &str) -> &'static str {
    if res.starts_with("Error:") || res.starts_with("[exit") || res.starts_with("[timeout]") {
        "error"
    } else {
        "ok"
    }
}

/// Run a blocking closure on the tokio blocking thread pool.
pub(crate) async fn blocking_string(f: impl FnOnce() -> String + Send + 'static) -> String {
    match tokio::task::spawn_blocking(f).await {
        Ok(s) => s,
        Err(e) => format!("Error: {e}"),
    }
}

/// Parse tool-call arguments from a JSON string.
pub(crate) fn parse_args(args_json: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(args_json).map_err(|e| format!("Error: invalid arguments: {e}"))
}

/// Extract a required string field from parsed JSON args.
pub(crate) fn req_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Error: missing '{key}'"))
}

/// Return a prefix of `s` that fits within `max_bytes`, at a valid UTF-8
/// character boundary.
pub(crate) fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(max_bytes);
    let mut used = 0usize;
    for c in s.chars() {
        let cl = c.len_utf8();
        if used + cl > max_bytes {
            break;
        }
        out.push(c);
        used += cl;
    }
    out
}
