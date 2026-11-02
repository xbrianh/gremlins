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
///
/// What a title shows is a property of the tool, so both the primary detail and
/// the decision to append secondaries come from the handler registry rather
/// than from a name table here.
pub(crate) fn format_tool_call_parts(name: &str, args: &serde_json::Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(detail) = super::tools::title_detail(name, args) {
        parts.push(detail);
    }
    if super::tools::title_uses_secondaries(name) {
        parts.extend(super::tools::secondary_args(
            name,
            args,
            Some(TITLE_VALUE_CAP),
        ));
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

/// Enrich the log line with secondary arguments beyond what `key_arg` returns.
///
/// `key_arg` picks a single primary key; this function appends any additional
/// parameters that help operators understand the tool invocation at a glance.
pub(crate) fn enrich_log_line(name: &str, args: &serde_json::Value) -> String {
    let primary = key_arg(args);
    let extras = super::tools::secondary_args(name, args, None);

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
