use super::tool_specs::tool_param_schema;

/// Build a short summary of the fields present in a mixed-mode edit object,
/// so the error message shows what the model actually sent.
fn describe_edit_provided_fields(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in &["start_line", "end_line", "old_string", "new_string"] {
        if let Some(v) = obj.get(*key) {
            let display = match v {
                serde_json::Value::String(s) => {
                    if s.len() > 40 {
                        format!("{}<{} chars>", key, s.len())
                    } else {
                        format!("{key}={s:?}")
                    }
                }
                other => format!("{key}={other}"),
            };
            parts.push(display);
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" (provided: {})", parts.join(", "))
    }
}

/// Check that all required tool arguments are present and have the correct types.
/// Returns an error message string on failure, or `None` if valid.
pub(crate) fn validate_tool_args(name: &str, args: &serde_json::Value) -> Option<String> {
    let schema = tool_param_schema(name)?;
    for (field, expected_type, required) in &schema {
        match args.get(field) {
            None if *required => {
                let expected: Vec<String> = schema
                    .iter()
                    .map(|(f, t, required)| {
                        if *required {
                            format!("{f}: {t}")
                        } else {
                            format!("{f}: {t} (optional)")
                        }
                    })
                    .collect();
                let found: Vec<String> = args
                    .as_object()
                    .map(|o| o.keys().map(|k| format!("{k}: …")).collect())
                    .unwrap_or_default();
                let found_str = if found.is_empty() {
                    "<not an object>".into()
                } else {
                    format!("{{{}}}", found.join(", "))
                };
                let mut msg = format!(
                    "Error: missing '{field}' for {name} tool. \
                     Expected: {{{}}}. Got: {found_str}",
                    expected.join(", ")
                );
                // If file_path is missing for Edit and any edit item has a
                // file_path key, the model likely nested it — give a targeted hint.
                if name == "Edit" && *field == "file_path" {
                    if let Some(edits) = args.get("edits").and_then(|v| v.as_array()) {
                        let misplaced = edits.iter().any(|e| {
                            e.as_object()
                                .map(|o| o.contains_key("file_path"))
                                .unwrap_or(false)
                        });
                        if misplaced {
                            msg.push_str(
                                " (Note: file_path belongs at the top level, not inside individual edits — all edits target the same file)",
                            );
                        }
                    }
                }
                return Some(msg);
            }
            None => continue,
            Some(val) => {
                let ok = match *expected_type {
                    "string" => val.is_string(),
                    "array" => val.is_array(),
                    "integer" => val.is_number(),
                    "object" => val.is_object(),
                    _ => true,
                };
                if !ok {
                    let actual = if val.is_string() {
                        "string"
                    } else if val.is_array() {
                        "array"
                    } else if val.is_number() {
                        "number"
                    } else if val.is_object() {
                        "object"
                    } else if val.is_null() {
                        "null"
                    } else if val.is_boolean() {
                        "bool"
                    } else {
                        "?"
                    };
                    return Some(format!(
                        "Error: '{field}' for {name} must be {expected_type}, got {actual}"
                    ));
                }
            }
        }
    }
    // Nested constraints for tools with structured array items.
    if name == "Edit" {
        if let Some(edits) = args.get("edits").and_then(|v| v.as_array()) {
            if edits.is_empty() {
                return Some("Error: 'edits' array must not be empty for Edit tool".into());
            }
            for (i, edit) in edits.iter().enumerate() {
                let obj = match edit.as_object() {
                    Some(o) => o,
                    None => {
                        return Some(format!(
                            "Error: edit {} in 'edits' must be an object for Edit tool",
                            i + 1
                        ));
                    }
                };
                // 'replacement' (the old range-mode field) is removed — reject it
                // explicitly even when old_string/new_string are also present,
                // so legacy mixed-mode requests don't silently apply new_string.
                if obj.contains_key("replacement") {
                    let provided = describe_edit_provided_fields(obj);
                    return Some(format!(
                        "Error: edit {} has 'replacement' which is no longer supported — use 'new_string' instead{}",
                        i + 1,
                        provided
                    ));
                }
                if !obj.contains_key("old_string") {
                    return Some(format!(
                        "Error: edit {} missing 'old_string' for Edit tool",
                        i + 1
                    ));
                }
                if !obj.contains_key("new_string") {
                    return Some(format!(
                        "Error: edit {} missing 'new_string' for Edit tool",
                        i + 1
                    ));
                }
                // 'start_line'/'end_line' are optional scope fields;
                // both accept any integer (zero clamps to 1 in the executor),
                // end_line may appear without start_line (scope defaults to
                // line 1), and start_line > end_line is accepted (the executor
                // clamps the byte window).
                let has_start = obj.contains_key("start_line");
                let has_end = obj.contains_key("end_line");
                if has_start {
                    match obj.get("start_line").and_then(|v| v.as_u64()) {
                        Some(_) => {}
                        None => {
                            return Some(format!(
                                "Error: edit {} 'start_line' must be an integer for Edit tool",
                                i + 1
                            ));
                        }
                    }
                }
                if has_end {
                    match obj.get("end_line").and_then(|v| v.as_u64()) {
                        Some(_) => {}
                        None => {
                            return Some(format!(
                                "Error: edit {} 'end_line' must be an integer for Edit tool",
                                i + 1
                            ));
                        }
                    }
                }
            }
        }
    }
    None
}
