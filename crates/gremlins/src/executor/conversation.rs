//! Budget-driven compaction for chat conversation artifacts.
//!
//! The persisted `artifact://conversation.json` is always a full verbatim
//! transcript. When chat history is read back for a new turn, [`compact`]
//! produces a smaller *working* history: the most recent completed turn is
//! retained verbatim, while the older prefix is mechanically distilled into
//! structured memory (file facts, command results, per-turn ledger/Done
//! summaries) and an "already explored" manifest.

use indexmap::IndexSet;
use rig_core::completion::message::{AssistantContent, ToolCall, ToolResultContent, UserContent};
use rig_core::completion::Message;

use crate::clients::tools;

/// Maximum bytes kept from a `Read` result for a file fact.
const FILE_EXCERPT_BYTES: usize = 2048;
/// Maximum bytes kept from a command/pattern result tail.
const RESULT_TAIL_BYTES: usize = 2048;
/// Maximum bytes kept from a ledger line or `Done` summary.
const TURN_SUMMARY_BYTES: usize = 2048;

#[derive(Default)]
struct FileFact {
    path: String,
    excerpt: String,
}

#[derive(Default)]
struct ResultFact {
    key: String,
    status: String,
    output_tail: String,
}

#[derive(Default)]
struct Facts {
    file_facts: Vec<FileFact>,
    results: Vec<ResultFact>,
    summaries: Vec<String>,
    file_keys: IndexSet<String>,
    result_keys: IndexSet<String>,
}

impl Facts {
    fn record_call(&mut self, name: &str, args: &serde_json::Value, output: &str) {
        match name {
            "Read" => {
                if let Some(path) = str_arg(args, "file_path") {
                    self.file_keys.insert(path.clone());
                    self.file_facts.push(FileFact {
                        path,
                        excerpt: head_bytes(output, FILE_EXCERPT_BYTES),
                    });
                }
            }
            "Write" | "Edit" => {
                if let Some(path) = str_arg(args, "file_path") {
                    self.file_keys.insert(path.clone());
                    self.file_facts.push(FileFact {
                        path,
                        excerpt: head_bytes(output, RESULT_TAIL_BYTES),
                    });
                }
            }
            "Bash" => {
                if let Some(command) = str_arg(args, "command") {
                    let key = format!("Bash: {command}");
                    self.result_keys.insert(key.clone());
                    self.results.push(ResultFact {
                        key,
                        status: tools::result_status(output).to_string(),
                        output_tail: tail_bytes(output, RESULT_TAIL_BYTES),
                    });
                }
            }
            "Grep" | "Glob" => {
                if let Some(pattern) = str_arg(args, "pattern") {
                    let key = format!("{name}: {pattern}");
                    self.result_keys.insert(key.clone());
                    self.results.push(ResultFact {
                        key,
                        status: tools::result_status(output).to_string(),
                        output_tail: tail_bytes(output, RESULT_TAIL_BYTES),
                    });
                }
            }
            "Done" => {
                if let Some(summary) = str_arg(args, "summary") {
                    self.summaries
                        .push(head_bytes(summary.trim(), TURN_SUMMARY_BYTES));
                }
            }
            _ => {}
        }
    }
}

/// Reduce a chat history to a budgeted working history.
///
/// A budget of `0` disables proactive compaction. Histories that already fit
/// the budget are returned untouched. Otherwise the oldest completed turns are
/// compacted into text-only user/assistant blocks and the most recent
/// completed turn (extended backward while it stays within budget) is retained
/// verbatim.
pub(crate) fn compact(history: &[Message], budget_bytes: u64) -> Vec<Message> {
    if budget_bytes == 0 || history.is_empty() {
        return history.to_vec();
    }

    if serialized_len(history) <= budget_bytes as usize {
        return history.to_vec();
    }

    let boundaries = round_boundaries(history);
    if boundaries.len() <= 1 {
        // Only one completed turn (or an unpartitionable transcript): keep it
        // verbatim and let the hard prompt ceiling evaluate it as-is.
        return history.to_vec();
    }

    // The most recent completed turn always survives verbatim, even if it
    // alone exceeds the soft budget.
    let newest = *boundaries.last().expect("boundaries is non-empty");
    let mut cut = newest;

    // Walk candidates oldest-first so the verbatim window extends as far back
    // as the soft budget allows.
    for &candidate in &boundaries[..boundaries.len() - 1] {
        if candidate == 0 {
            continue;
        }
        let working = build_working_history(history, candidate);
        if serialized_len(&working) <= budget_bytes as usize {
            cut = candidate;
            break;
        }
    }

    build_working_history(history, cut)
}

/// Partition at `cut` and prepend a compacted-memory block. The memory is a
/// text user message followed by a text-only assistant acknowledgement, so the
/// working transcript starts with a user turn and never has two consecutive
/// text user messages in the compacted prefix.
fn build_working_history(history: &[Message], cut: usize) -> Vec<Message> {
    let window = &history[cut..];
    let mut working = Vec::with_capacity(2 + window.len());
    working.push(Message::user(render_memory(&history[..cut], window)));
    working.push(assistant_text(
        "Prior context acknowledged. The retained verbatim conversation follows.",
    ));
    working.extend(window.iter().cloned());
    working
}

// ---------------------------------------------------------------------------
// Round boundaries
// ---------------------------------------------------------------------------

/// A cut is valid only at the start of a model round: a user text message
/// (initial prompt, ledger, reminder, or nudge), never a tool result or an
/// assistant message. Cutting before a tool result could start the verbatim
/// window with an orphaned result; cutting before a user text message keeps the
/// preceding assistant/tool-result block in the compacted prefix.
fn round_boundaries(history: &[Message]) -> Vec<usize> {
    let mut boundaries = Vec::new();
    if !history.is_empty() {
        boundaries.push(0);
    }
    for (index, message) in history.iter().enumerate().skip(1) {
        if is_text_user_message(message) {
            boundaries.push(index);
        }
    }
    boundaries
}

fn is_text_user_message(message: &Message) -> bool {
    match message {
        Message::User { content } => {
            content.iter().any(|c| matches!(c, UserContent::Text(_)))
                && !content
                    .iter()
                    .any(|c| matches!(c, UserContent::ToolResult(_)))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Mechanical extraction
// ---------------------------------------------------------------------------

fn extract_facts(messages: &[Message]) -> Facts {
    let mut facts = Facts::default();
    let mut index = 0;

    while index < messages.len() {
        if let Message::Assistant { content, .. } = &messages[index] {
            let tool_calls: Vec<ToolCall> = content
                .iter()
                .filter_map(|item| match item {
                    AssistantContent::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            if tool_calls.is_empty() {
                index += 1;
                continue;
            }

            // Tool results are the consecutive user tool-result messages that
            // immediately follow the assistant tool-use message.
            let mut outputs = Vec::new();
            let mut cursor = index + 1;
            while cursor < messages.len() {
                match tool_result_text(&messages[cursor]) {
                    Some(text) => {
                        outputs.push(text);
                        cursor += 1;
                    }
                    None => break,
                }
            }

            for (position, tool_call) in tool_calls.iter().enumerate() {
                let output = outputs.get(position).map(String::as_str).unwrap_or("");
                facts.record_call(
                    &tool_call.function.name,
                    &tool_call.function.arguments,
                    output,
                );
            }

            index = cursor;
            continue;
        }

        if is_text_user_message(&messages[index]) {
            let text = user_text(&messages[index]);
            if text.starts_with("Actions taken this turn:") {
                facts
                    .summaries
                    .push(head_bytes(text.trim(), TURN_SUMMARY_BYTES));
            }
        }

        index += 1;
    }

    facts
}

fn tool_result_text(message: &Message) -> Option<String> {
    let Message::User { content } = message else {
        return None;
    };

    let mut saw_result = false;
    let mut text = String::new();
    for item in content {
        if let UserContent::ToolResult(result) = item {
            saw_result = true;
            for part in &result.content {
                if let ToolResultContent::Text(part) = part {
                    text.push_str(&part.text);
                }
            }
        }
    }

    saw_result.then_some(text)
}

fn user_text(message: &Message) -> String {
    match message {
        Message::User { content } => content
            .iter()
            .filter_map(|item| match item {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

fn str_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

// ---------------------------------------------------------------------------
// Memory rendering
// ---------------------------------------------------------------------------

fn render_memory(prefix: &[Message], window: &[Message]) -> String {
    let older = extract_facts(prefix);
    let current = extract_facts(window);

    let mut memory = String::new();
    memory.push_str("Conversation memory (compacted from earlier turns).\n");

    if !older.file_facts.is_empty() {
        memory.push_str("\n## File facts (summarized)\n");
        for fact in &older.file_facts {
            memory.push_str(&format!(
                "- path: {}\n  excerpt: {}\n",
                fact.path, fact.excerpt
            ));
        }
    }

    if !older.results.is_empty() {
        memory.push_str("\n## Command and pattern results (summarized)\n");
        for result in &older.results {
            memory.push_str(&format!(
                "- key: {}\n  status: {}\n  output-tail: {}\n",
                result.key, result.status, result.output_tail
            ));
        }
    }

    if !older.summaries.is_empty() {
        memory.push_str("\n## Earlier turn summaries\n");
        for summary in &older.summaries {
            memory.push_str(&format!("- {summary}\n"));
        }
    }

    memory.push_str("\n## Already explored\n");
    memory.push_str("Authoritative (present verbatim below — do not re-read):\n");
    for path in &current.file_keys {
        memory.push_str(&format!("- path: {path}\n"));
    }
    for key in &current.result_keys {
        memory.push_str(&format!("- key: {key}\n"));
    }
    if current.file_keys.is_empty() && current.result_keys.is_empty() {
        memory.push_str("- (none)\n");
    }

    memory.push_str(
        "\nSummarized (older compacted context — re-read when exact content is needed):\n",
    );
    for path in &older.file_keys {
        memory.push_str(&format!("- path: {path}\n"));
    }
    for key in &older.result_keys {
        memory.push_str(&format!("- key: {key}\n"));
    }
    if older.file_keys.is_empty() && older.result_keys.is_empty() {
        memory.push_str("- (none)\n");
    }

    memory
}

fn assistant_text(text: &str) -> Message {
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::text(text.to_string())],
    }
}

// ---------------------------------------------------------------------------
// Byte helpers
// ---------------------------------------------------------------------------

fn serialized_len(messages: &[Message]) -> usize {
    serde_json::to_string(messages)
        .map(|json| json.len())
        .unwrap_or(usize::MAX)
}

/// Keep the first `max` bytes of `text`, cutting on a UTF-8 char boundary.
fn head_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Keep the last `max` bytes of `text`, cutting on a UTF-8 char boundary.
fn tail_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::{CallId, ToolFunction, ToolName};

    fn user_prompt(text: &str) -> Message {
        Message::user(text.to_string())
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall::from_dual_wire(
            "",
            id,
            ToolFunction::new(ToolName::new(name.to_string()).unwrap(), args),
        )
    }

    fn assistant_with_tools(calls: &[ToolCall]) -> Message {
        Message::Assistant {
            id: None,
            content: calls
                .iter()
                .cloned()
                .map(AssistantContent::ToolCall)
                .collect(),
        }
    }

    fn tool_result(name: &str, call_id: &str, output: &str) -> Message {
        Message::tool_result(
            CallId::from_wire(call_id.to_string()),
            ToolName::new(name.to_string()).unwrap(),
            output.to_string(),
        )
    }

    fn two_turn_history() -> Vec<Message> {
        let read_call = tool_call("read_1", "Read", serde_json::json!({"file_path": "a.txt"}));
        let done_call = tool_call(
            "done_1",
            "Done",
            serde_json::json!({"summary": "read a.txt"}),
        );
        let bash_call = tool_call(
            "bash_1",
            "Bash",
            serde_json::json!({"command": "cargo check"}),
        );
        let final_done = tool_call("done_2", "Done", serde_json::json!({"summary": "finished"}));

        vec![
            user_prompt("find a.txt"),
            assistant_with_tools(&[read_call]),
            tool_result("Read", "read_1", "hello from a.txt"),
            user_prompt("Actions taken this turn:\n- Read a.txt\n"),
            assistant_with_tools(&[done_call]),
            tool_result("Done", "done_1", "Done accepted."),
            user_prompt("Actions taken this turn:\n- Done\n"),
            assistant_with_tools(&[bash_call]),
            tool_result("Bash", "bash_1", "[exit 1]\ncargo failed"),
            user_prompt("Actions taken this turn:\n- Bash cargo check (failed)\n"),
            assistant_with_tools(&[final_done]),
            tool_result("Done", "done_2", "Done accepted."),
        ]
    }

    #[test]
    fn under_budget_is_untouched() {
        let history = two_turn_history();
        let working = compact(&history, 20_000_000);
        assert_eq!(working, history);
    }

    #[test]
    fn zero_budget_disables_compaction() {
        let history = two_turn_history();
        let working = compact(&history, 0);
        assert_eq!(working, history);
    }

    #[test]
    fn partitioning_retains_most_recent_turn_verbatim() {
        let history = two_turn_history();

        // First find the start of the final completed turn: the final
        // `Actions taken this turn:` ledger message.
        let cut = history
            .iter()
            .rposition(|message| {
                matches!(message, Message::User { content }
                    if content.iter().any(|c| matches!(c, UserContent::Text(t)
                        if t.text == "Actions taken this turn:\n- Bash cargo check (failed)\n")))
            })
            .unwrap();

        let working = compact(&history, 512);
        assert!(working.len() < history.len());
        assert_eq!(
            &working[working.len() - (history.len() - cut)..],
            &history[cut..]
        );
    }

    #[test]
    fn compacted_memory_contains_keyed_file_facts_and_results() {
        let history = two_turn_history();
        let working = compact(&history, 700);
        let text = user_text(&working[0]);

        assert!(text.contains("path: a.txt"), "missing file fact: {text}");
        assert!(
            text.contains("Bash: cargo check"),
            "missing result key: {text}"
        );
        assert!(text.contains("status: error"), "missing status: {text}");
        assert!(
            text.contains("do not re-read"),
            "missing authoritative tier: {text}"
        );
        assert!(
            text.contains("re-read when exact content is needed"),
            "missing summarized tier: {text}"
        );
    }

    #[test]
    fn manifest_distinguishes_authoritative_from_summarized() {
        let history = two_turn_history();

        // Choose a budget that retains the Bash + final-Done rounds verbatim.
        // Boundary 6 is the `Actions taken this turn: - Done` user message, so
        // the verbatim window from there includes the Bash call and its result.
        let budget = serialized_len(&build_working_history(&history, 6)) as u64;
        assert!(budget < serialized_len(&history) as u64);

        let working = compact(&history, budget);
        let text = user_text(&working[0]);

        let authoritative = text.find("Authoritative").unwrap();
        let summarized = text.find("Summarized").unwrap();
        assert!(authoritative < summarized);

        // The Bash command is retained verbatim behind the memory block, so it
        // must be authoritative rather than summarized.
        let authoritative_tier = &text[authoritative..summarized];
        assert!(authoritative_tier.contains("Bash: cargo check"));
        assert!(!authoritative_tier.contains("path: a.txt"));

        let summarized_tier = &text[summarized..];
        assert!(summarized_tier.contains("path: a.txt"));
    }

    #[test]
    fn output_has_valid_transcript_shape() {
        let history = two_turn_history();
        let working = compact(&history, 700);

        // No assistant tool-use may appear without a following tool result.
        for (index, message) in working.iter().enumerate() {
            if let Message::Assistant { content, .. } = message {
                let has_tool_call = content
                    .iter()
                    .any(|item| matches!(item, AssistantContent::ToolCall(_)));
                if has_tool_call {
                    assert!(
                        index + 1 < working.len()
                            && tool_result_text(&working[index + 1]).is_some(),
                        "assistant tool-use at {index} has no paired tool result"
                    );
                }
            }
        }

        // The compacted prefix is a text user turn, a text-only assistant
        // acknowledgement, then the verbatim window starts on a text user
        // message (never an orphaned tool result).
        assert!(is_text_user_message(&working[0]));
        assert!(working
            .get(1)
            .is_some_and(|m| matches!(m, Message::Assistant { .. })));
        assert!(working.get(2).is_some_and(is_text_user_message));
    }

    #[test]
    fn round_boundaries_only_cut_before_text_user_messages() {
        let history = two_turn_history();
        let boundaries = round_boundaries(&history);
        for boundary in &boundaries {
            assert!(
                *boundary == 0 || is_text_user_message(&history[*boundary]),
                "boundary {boundary} is not a text user message"
            );
        }
        assert!(boundaries.len() >= 3);
    }

    #[test]
    fn byte_helpers_cut_on_char_boundaries() {
        assert_eq!(head_bytes("abcdef", 3), "abc");
        assert_eq!(tail_bytes("abcdef", 3), "def");
        // "é" is two bytes in UTF-8.
        assert_eq!(head_bytes("aé", 2), "a");
        assert_eq!(tail_bytes("éa", 2), "a");
    }
}
