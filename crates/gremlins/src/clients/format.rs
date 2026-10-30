use rig_core::completion::message::{AssistantContent, Reasoning, Sealed, ToolCall};
use rig_core::completion::Message;
use rig_core::streaming::{Item, StreamEvent};

/// Truncate a string to at most `n` characters, collapsing line breaks and
/// appending an ellipsis when it is cut. Shared by log rendering and TUI
/// tool-call titles.
pub(crate) fn trunc(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        format!("{}...", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}

pub(crate) fn apply_chunk(
    chunk: Item<StreamEvent>,
    text: &mut String,
    reasoning: &mut String,
    reasoning_items: &mut Vec<Sealed<Reasoning>>,
    tool_calls: &mut Vec<ToolCall>,
) {
    match chunk {
        Item::Event(StreamEvent::End { content, .. }) => match content {
            AssistantContent::Text(t) => text.push_str(&t.text),
            AssistantContent::ToolCall(tc) => tool_calls.push(tc),
            AssistantContent::Reasoning(sealed) => reasoning_items.push(sealed),
            _ => {}
        },
        Item::Event(StreamEvent::Reasoning { text: r, .. }) => reasoning.push_str(&r),
        _ => {}
    }
}

/// Format a tool call for human display — name plus the key argument.
pub(crate) fn format_tool_call(tc: &ToolCall) -> String {
    format_tool_call_parts(&tc.function.name, &tc.function.arguments)
}

/// Format a tool call from its already-parsed name and arguments.
///
/// This is the single formatting routine shared by the FIFO tool-event path
/// and the live-output forwarder so both always render the same string.
pub(crate) fn format_tool_call_parts(name: &str, args: &serde_json::Value) -> String {
    // Extract the primary argument for common tools.
    let detail: Option<String> = match name {
        "Bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| format!("command={s:?}")),
        "Read" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| format!("file_path={s:?}")),
        "Write" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| format!("file_path={s:?}")),
        "Edit" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| format!("file_path={s:?}")),
        "Glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| format!("pattern={s:?}")),
        "Grep" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| format!("pattern={s:?}")),
        "WebSearch" => args
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| format!("url={s:?}")),
        "Landscape" => {
            let depth = args
                .get("depth")
                .and_then(|v| v.as_u64())
                .map(|d| format!("depth={d}"));
            let path = args
                .get("file_path")
                .and_then(|v| v.as_str())
                .map(|s| format!("file_path={s:?}"));
            match (path, depth) {
                (Some(p), Some(d)) => Some(format!("{p}, {d}")),
                (Some(p), None) => Some(p),
                _ => None,
            }
        }
        "Done" => args
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| format!("summary={:?}", trunc(s, 120))),
        "Task" => args
            .get("kind")
            .and_then(|v| v.as_str())
            .map(|s| format!("kind={s:?}")),
        _ => None,
    };

    // Collect secondary parameters for Read and Grep.
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = detail {
        parts.push(d);
    }
    if name == "Read" || name == "Grep" {
        parts.extend(tool_secondary_args_with(name, args, Some(TITLE_VALUE_CAP)));
    }

    if parts.is_empty() {
        name.to_string()
    } else {
        format!("{name}({})", parts.join(", "))
    }
}

pub(crate) fn assistant_tool_message(
    text: &str,
    reasoning_items: &[Sealed<Reasoning>],
    tool_calls: &[ToolCall],
    include_reasoning_items: bool,
) -> Message {
    let mut contents = Vec::new();
    // Omit structured reasoning items from persistent history unless the
    // backend needs them (Azure Responses `function_call` → `reasoning` id
    // linkage) or the message would otherwise be empty. A reasoning-only turn
    // has no text and no tool calls, so dropping its reasoning items would
    // force the empty-text fallback below and produce a degenerate assistant
    // turn in history.
    if include_reasoning_items || (text.is_empty() && tool_calls.is_empty()) {
        contents.extend(
            reasoning_items
                .iter()
                .cloned()
                .map(AssistantContent::Reasoning),
        );
    }
    if !text.is_empty() {
        contents.push(AssistantContent::text(text.to_string()));
    }
    for tc in tool_calls {
        contents.push(AssistantContent::ToolCall(tc.clone()));
    }
    if contents.is_empty() {
        contents.push(AssistantContent::text(String::new()));
    }
    Message::Assistant {
        id: None,
        content: contents,
    }
}

pub(crate) fn key_arg(args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        for k in ["file_path", "command", "pattern", "summary", "url", "kind"] {
            if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    String::new()
}

/// Upper bound for long string values in TUI tool-call titles.
///
/// Log lines apply their own 200-char budget in `enrich_log_line`, so this cap
/// is a display-only concern for the comparatively narrow tool-slot titles.
const TITLE_VALUE_CAP: usize = 120;

/// Collect non-empty secondary parameters for a tool invocation, without
/// display truncation.
///
/// Returns ordered `key=value` strings for parameters beyond the primary
/// argument that help operators understand the tool invocation at a glance.
/// Absent, zero, or empty values are omitted. Callers that need a display cap
/// (such as TUI titles) should use [`tool_secondary_args_with`] instead.
fn tool_secondary_args(name: &str, args: &serde_json::Value) -> Vec<String> {
    tool_secondary_args_with(name, args, None)
}

/// Collect non-empty secondary parameters for a tool invocation.
///
/// When `string_value_cap` is `Some(n)`, string values longer than `n`
/// characters are truncated with [`trunc`] before being debug-quoted. This is
/// used for compact TUI titles; logging passes `None` and applies its own
/// whole-line budget afterward.
fn tool_secondary_args_with(
    name: &str,
    args: &serde_json::Value,
    string_value_cap: Option<usize>,
) -> Vec<String> {
    let mut extras = Vec::new();

    let string_arg = |value: &str| match string_value_cap {
        Some(cap) => format!("{:?}", trunc(value, cap)),
        None => format!("{value:?}"),
    };

    match name {
        "Read" => {
            if let Some(v) = args
                .get("offset")
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
            {
                extras.push(format!("offset={v}"));
            }
            if let Some(v) = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
            {
                extras.push(format!("limit={v}"));
            }
        }
        "Bash" | "WebSearch" => {
            if let Some(v) = args
                .get("timeout")
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
            {
                extras.push(format!("timeout={v}"));
            }
        }
        "Grep" => {
            if let Some(v) = args
                .get("max_matches")
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
            {
                extras.push(format!("max_matches={v}"));
            }
            if let Some(v) = args
                .get("glob")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                extras.push(format!("glob={}", string_arg(v)));
            }
            if let Some(v) = args
                .get("path")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                extras.push(format!("path={}", string_arg(v)));
            }
            if let Some(v) = args
                .get("context_before")
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
            {
                extras.push(format!("context_before={v}"));
            }
            if let Some(v) = args
                .get("context_after")
                .and_then(|v| v.as_u64())
                .filter(|v| *v > 0)
            {
                extras.push(format!("context_after={v}"));
            }
        }
        "Glob" => {
            if let Some(v) = args
                .get("path")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                extras.push(format!("path={}", string_arg(v)));
            }
        }
        _ => {}
    }

    extras
}

/// Enrich the log line with secondary arguments beyond what `key_arg` returns.
///
/// `key_arg` picks a single primary key; this function appends any additional
/// parameters that help operators understand the tool invocation at a glance.
pub(crate) fn enrich_log_line(name: &str, args: &serde_json::Value) -> String {
    let primary = key_arg(args);
    let extras = tool_secondary_args(name, args);

    if extras.is_empty() {
        return primary;
    }

    let suffix = extras.join(" ");

    const CAP: usize = 200;
    const SPACE: usize = 1;
    const QUOTE_OVERHEAD: usize = 2; // opening and closing " from :?
    const TRUNC_MARKER: usize = 3; // "..." appended by trunc() when it cuts

    let suffix_len = suffix.len();

    if suffix_len + SPACE + QUOTE_OVERHEAD >= CAP {
        // The suffix alone fills the budget — render it behind a quoted "..."
        // placeholder and truncate so the whole line still fits within CAP.
        let max_suffix = CAP.saturating_sub(QUOTE_OVERHEAD + TRUNC_MARKER + SPACE + TRUNC_MARKER);
        let suffix = trunc(&suffix, max_suffix);
        return format!("\"...\" {}", suffix);
    }

    let max_primary = CAP - suffix_len - SPACE - QUOTE_OVERHEAD - TRUNC_MARKER;
    let primary = trunc(&primary, max_primary);
    format!("{primary:?} {suffix}")
}

/// Compact label for a tool call in the per-turn action ledger.
#[allow(dead_code)]
pub(crate) fn ledger_key_arg(args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        for k in [
            "file_path",
            "command",
            "pattern",
            "summary",
            "path",
            "kind",
            "url",
        ] {
            if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return truncate_ledger_value(v);
                }
            }
        }
    }
    String::new()
}

pub(crate) fn truncate_ledger_value(v: &str) -> String {
    const MAX: usize = 80;
    // Collapse embedded line breaks so one action is always one ledger line.
    let v: String = v
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let chars: Vec<char> = v.chars().collect();
    if chars.len() <= MAX {
        return v;
    }
    let head: String = chars[..40].iter().collect();
    let tail: String = chars[chars.len() - 39..].iter().collect();
    format!("{head}…{tail}")
}
