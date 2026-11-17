use serde_json::{Map, Value};

/// Normalize supported Edit shorthands, leaving other shapes to schema validation.
pub(super) fn repair_args(args: &mut Value, preview_chars: usize) -> Result<bool, String> {
    let Some(obj) = args.as_object_mut() else {
        return Ok(false);
    };
    if let Some(edits) = obj.get_mut("edits") {
        let mut changed = false;
        if let Some(text) = edits.as_str() {
            *edits = serde_json::from_str(text)
                .map_err(|error| quoted_edits_error(text, &error, preview_chars))?;
            changed = true;
        }
        if edits.get("old_string").is_some() {
            *edits = Value::Array(vec![edits.take()]);
            changed = true;
        }
        return Ok(changed);
    }

    // Older calls put the replacement and optional scope alongside file_path.
    if !obj.contains_key("old_string") || !obj.contains_key("new_string") {
        return Ok(false);
    }
    let mut single = Map::new();
    for key in ["old_string", "new_string", "start_line", "end_line"] {
        if let Some(value) = obj.remove(key) {
            single.insert(key.into(), value);
        }
    }
    obj.insert("edits".into(), Value::Array(vec![Value::Object(single)]));
    Ok(true)
}

fn quoted_edits_error(text: &str, error: &serde_json::Error, preview_chars: usize) -> String {
    const RETRY: &str = "\"; pass the array directly, not as a quoted string";

    let prefix = format!("Error: Edit.edits: {error} near \"");
    let radius = preview_chars.saturating_sub(prefix.chars().count() + RETRY.len()) / 2;
    let offset = json_error_offset(text, error);
    let left = context_bytes(text[..offset].chars().rev(), radius);
    let right = context_bytes(text[offset..].chars(), radius);
    let excerpt = text[offset - left..offset + right].escape_debug();
    format!("{prefix}{excerpt}{RETRY}")
}

/// Serde reports a one-based byte column in the decoded JSON text.
fn json_error_offset(text: &str, error: &serde_json::Error) -> usize {
    if error.is_eof() {
        return text.len();
    }
    let line_start: usize = text
        .split_inclusive('\n')
        .take(error.line().saturating_sub(1))
        .map(str::len)
        .sum();
    text.floor_char_boundary(line_start + error.column().saturating_sub(1))
}

/// Count source bytes that fit on one side of the fault after escaping.
fn context_bytes(chars: impl Iterator<Item = char>, budget: usize) -> usize {
    chars
        .scan(0, |width, c| {
            *width += c.escape_debug().len();
            (*width <= budget).then_some(c.len_utf8())
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_edits_excerpt_scales_with_preview_budget() {
        let padding = "界".repeat(100);
        let edits = format!(r#"["{padding}" "{padding}"]"#);
        let lengths = [160, 240].map(|budget| {
            let mut args = serde_json::json!({"edits": edits});
            let error = repair_args(&mut args, budget).unwrap_err();
            let length = error.chars().count();
            assert!(length <= budget, "{error}");
            assert!(error.ends_with("pass the array directly, not as a quoted string"));
            length
        });
        assert!(lengths[0] < lengths[1]);
    }
}
