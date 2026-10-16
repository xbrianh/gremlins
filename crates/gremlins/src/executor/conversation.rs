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
                    self.upsert_file_fact(path, head_bytes(output, FILE_EXCERPT_BYTES));
                }
            }
            "Write" | "Edit" => {
                if let Some(path) = str_arg(args, "file_path") {
                    self.upsert_file_fact(path, head_bytes(output, RESULT_TAIL_BYTES));
                }
            }
            "Bash" => {
                if let Some(command) = str_arg(args, "command") {
                    let key = format!("Bash: {command}");
                    self.upsert_result_fact(
                        key,
                        tools::result_status(output).to_string(),
                        tail_bytes(output, RESULT_TAIL_BYTES),
                    );
                }
            }
            "Grep" | "Glob" => {
                if let Some(pattern) = str_arg(args, "pattern") {
                    let key = format!("{name}: {pattern}");
                    self.upsert_result_fact(
                        key,
                        tools::result_status(output).to_string(),
                        tail_bytes(output, RESULT_TAIL_BYTES),
                    );
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

    /// Store one fact per path; a later read/write of the same path replaces
    /// the earlier excerpt and moves it to the newest position, so budget
    /// trimming drops the road-too-stale copy first.
    fn upsert_file_fact(&mut self, path: String, excerpt: String) {
        if let Some(position) = self.file_facts.iter().position(|fact| fact.path == path) {
            let mut fact = self.file_facts.remove(position);
            fact.excerpt = excerpt;
            self.file_facts.push(fact);
        } else {
            self.file_facts.push(FileFact {
                path: path.clone(),
                excerpt,
            });
        }
        self.file_keys.insert(path);
    }

    /// Store one cached result per command/pattern; the most recent exit status
    /// and output tail win.
    fn upsert_result_fact(&mut self, key: String, status: String, output_tail: String) {
        if let Some(position) = self.results.iter().position(|fact| fact.key == key) {
            let mut fact = self.results.remove(position);
            fact.status = status;
            fact.output_tail = output_tail;
            self.results.push(fact);
        } else {
            self.results.push(ResultFact {
                key: key.clone(),
                status,
                output_tail,
            });
        }
        self.result_keys.insert(key);
    }
}

/// Reduce a chat history to a budgeted working history.
///
/// A budget of `0` disables proactive compaction. `request_overhead_bytes` is
/// the estimated size of non-history serialized request content (the current
/// prompt, system preamble, tool definitions, and other request fields), so
/// the soft trigger is evaluated against the assembled request rather than the
/// bare transcript. Histories that already fit the remaining budget are
/// returned untouched. Otherwise the oldest completed turns are compacted into
/// text-only user/assistant blocks and the most recent completed turn survives
/// verbatim.
pub(crate) fn compact(
    history: &[Message],
    budget_bytes: u64,
    request_overhead_bytes: usize,
) -> Vec<Message> {
    if budget_bytes == 0 || history.is_empty() {
        return history.to_vec();
    }

    let available = budget_bytes.saturating_sub(request_overhead_bytes as u64) as usize;
    let original_len = serialized_len(history);
    if original_len <= available {
        return history.to_vec();
    }

    let boundaries = round_boundaries(history);
    if boundaries.len() <= 1 {
        // Only one completed turn (or an unpartitionable transcript): keep it
        // verbatim and let the hard prompt ceiling evaluate it as-is.
        return history.to_vec();
    }

    // The most recent completed turn always survives verbatim, even if it
    // alone exceeds the soft budget. Walk candidates oldest-first so the
    // verbatim window extends as far back as the remaining budget allows.
    let mut best_effort: Option<Vec<Message>> = None;
    let mut best_effort_len = usize::MAX;

    for &candidate in &boundaries {
        if candidate == 0 {
            // A zero cut would put the entire history in the verbatim window
            // and only *add* the fixed memory preface.
            continue;
        }

        let working = build_working_history(history, candidate, available);
        let working_len = serialized_len(&working);
        if working_len <= available {
            return working;
        }
        if working_len < best_effort_len {
            best_effort_len = working_len;
            best_effort = Some(working);
        }
    }

    // No compacted form fits the remaining budget. Prefer the smallest
    // compacted form if it is actually smaller than the verbatim input;
    // otherwise leave the input untouched rather than growing a just-over
    // threshold transcript toward the hard prompt ceiling.
    if let Some(working) = best_effort {
        if best_effort_len < original_len {
            return working;
        }
    }
    history.to_vec()
}

/// Partition at `cut` and prepend a compacted-memory block. The memory is a
/// text user message followed by a text-only assistant acknowledgement, so the
/// working transcript starts with a user turn and never has two consecutive
/// text user messages in the compacted prefix.
fn build_working_history(history: &[Message], cut: usize, available_bytes: usize) -> Vec<Message> {
    let window = &history[cut..];
    let (older, current) = facts_for(history, cut);

    let empty = assemble_working(String::new(), window);
    let empty_len = serialized_len(&empty);
    if empty_len >= available_bytes {
        // Even an empty memory block would not fit this window under the soft
        // budget; return the smallest representable form for this cut.
        return empty;
    }

    let mut memory_budget = available_bytes - empty_len;
    loop {
        let memory = render_memory(&older, &current, memory_budget);
        let memory_len = memory.len();
        let working = assemble_working(memory, window);
        let len = serialized_len(&working);
        if len <= available_bytes {
            return working;
        }

        if memory_budget == 0 {
            return empty;
        }

        // JSON escaping can make the wrapped memory a few bytes larger than
        // the raw string we budgeted. Shrink by the observed overflow.
        let overflow = len.saturating_sub(available_bytes);
        if overflow >= memory_budget {
            memory_budget = 0;
        } else {
            memory_budget -= overflow;
        }

        // Guarantee progress when the current render already truncated the
        // memory to (or below) the budget but the wrapper still overflowed.
        if memory_budget >= memory_len && memory_len > 0 {
            memory_budget = memory_len - 1;
        }
    }
}

fn assemble_working(memory: String, window: &[Message]) -> Vec<Message> {
    let mut working = Vec::with_capacity(2 + window.len());
    working.push(Message::user(memory));
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

fn facts_for(history: &[Message], cut: usize) -> (Facts, Facts) {
    let mut older = extract_facts(&history[..cut]);
    let current = extract_facts(&history[cut..]);

    // One key must not appear in both manifest tiers. Anything still verbatim
    // in the retained window is already authoritative; keep it out of the
    // summarized tier and the older excerpts so a follow-up turn gets a single
    // instruction for that path/command.
    let current_file_keys = current.file_keys.clone();
    let current_result_keys = current.result_keys.clone();

    older
        .file_facts
        .retain(|fact| !current_file_keys.contains(&fact.path));
    older.file_keys = std::mem::take(&mut older.file_keys)
        .into_iter()
        .filter(|path| !current_file_keys.contains(path))
        .collect();
    older
        .results
        .retain(|fact| !current_result_keys.contains(&fact.key));
    older.result_keys = std::mem::take(&mut older.result_keys)
        .into_iter()
        .filter(|key| !current_result_keys.contains(key))
        .collect();

    (older, current)
}

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

/// Render the compacted-memory preamble, bounded to `max_bytes`. Entries are
/// emitted oldest-first and stop as soon as the next entry would exceed the
/// budget; later entries are dropped rather than emitting a partial fact.
fn render_memory(older: &Facts, current: &Facts, max_bytes: usize) -> String {
    let mut memory = String::new();
    let mut budget = max_bytes;

    if !push_budgeted(
        &mut memory,
        &mut budget,
        "Conversation memory (compacted from earlier turns).\n",
    ) {
        return memory;
    }

    if !older.file_facts.is_empty() {
        if !push_budgeted(&mut memory, &mut budget, "\n## File facts (summarized)\n") {
            return memory;
        }
        for fact in &older.file_facts {
            if !push_budgeted(
                &mut memory,
                &mut budget,
                &format!("- path: {}\n  excerpt: {}\n", fact.path, fact.excerpt),
            ) {
                return memory;
            }
        }
    }

    if !older.results.is_empty() {
        if !push_budgeted(
            &mut memory,
            &mut budget,
            "\n## Command and pattern results (summarized)\n",
        ) {
            return memory;
        }
        for result in &older.results {
            if !push_budgeted(
                &mut memory,
                &mut budget,
                &format!(
                    "- key: {}\n  status: {}\n  output-tail: {}\n",
                    result.key, result.status, result.output_tail
                ),
            ) {
                return memory;
            }
        }
    }

    if !older.summaries.is_empty() {
        if !push_budgeted(&mut memory, &mut budget, "\n## Earlier turn summaries\n") {
            return memory;
        }
        for summary in &older.summaries {
            if !push_budgeted(&mut memory, &mut budget, &format!("- {summary}\n")) {
                return memory;
            }
        }
    }

    if !push_budgeted(&mut memory, &mut budget, "\n## Already explored\n") {
        return memory;
    }
    if !push_budgeted(
        &mut memory,
        &mut budget,
        "Authoritative (present verbatim below — do not re-read):\n",
    ) {
        return memory;
    }
    for path in &current.file_keys {
        if !push_budgeted(&mut memory, &mut budget, &format!("- path: {path}\n")) {
            return memory;
        }
    }
    for key in &current.result_keys {
        if !push_budgeted(&mut memory, &mut budget, &format!("- key: {key}\n")) {
            return memory;
        }
    }
    if current.file_keys.is_empty()
        && current.result_keys.is_empty()
        && !push_budgeted(&mut memory, &mut budget, "- (none)\n")
    {
        return memory;
    }

    if !push_budgeted(
        &mut memory,
        &mut budget,
        "\nSummarized (older compacted context — re-read when exact content is needed):\n",
    ) {
        return memory;
    }
    for path in &older.file_keys {
        if !push_budgeted(&mut memory, &mut budget, &format!("- path: {path}\n")) {
            return memory;
        }
    }
    for key in &older.result_keys {
        if !push_budgeted(&mut memory, &mut budget, &format!("- key: {key}\n")) {
            return memory;
        }
    }
    if older.file_keys.is_empty()
        && older.result_keys.is_empty()
        && !push_budgeted(&mut memory, &mut budget, "- (none)\n")
    {
        return memory;
    }

    memory
}

fn push_budgeted(out: &mut String, budget: &mut usize, text: &str) -> bool {
    if text.len() > *budget {
        return false;
    }
    out.push_str(text);
    *budget -= text.len();
    true
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
        let working = compact(&history, 20_000_000, 0);
        assert_eq!(working, history);
    }

    #[test]
    fn zero_budget_disables_compaction() {
        let history = two_turn_history();
        let working = compact(&history, 0, 0);
        assert_eq!(working, history);
    }

    #[test]
    fn request_overhead_shrinks_the_history_budget() {
        let history = two_turn_history();
        let history_len = serialized_len(&history) as u64;

        // History alone fits the soft threshold...
        assert_eq!(compact(&history, history_len, 0), history);

        // ...but the same history plus 1 byte of non-history request content no
        // longer does, so compaction must produce something smaller.
        let working = compact(&history, history_len, 1);
        assert!(serialized_len(&working) < serialized_len(&history));
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

        let working = compact(&history, 512, 0);
        assert!(working.len() < history.len());
        assert_eq!(
            &working[working.len() - (history.len() - cut)..],
            &history[cut..]
        );
    }

    #[test]
    fn compacted_memory_contains_keyed_file_facts_and_results() {
        let history = two_turn_history();

        // Render the prefix directly for cut 9 (newest completed turn
        // verbatim). The older prefix then contains both the Read fact and the
        // Bash result, so both must appear as keyed summarized memory.
        let (older, current) = facts_for(&history, 9);
        let text = render_memory(&older, &current, 4096);

        assert!(
            text.contains("Conversation memory"),
            "missing memory: {text}"
        );
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
        let budget = serialized_len(&build_working_history(&history, 6, usize::MAX - 1)) as u64;
        assert!(budget < serialized_len(&history) as u64);

        let working = compact(&history, budget, 0);
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
        let working = compact(&history, 700, 0);

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
        assert!(
            working.first().is_some_and(is_text_user_message),
            "working history must start with a user text message"
        );
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
    fn repeated_calls_upsert_by_key() {
        let mut facts = Facts::default();
        facts.record_call("Read", &serde_json::json!({"file_path": "a.txt"}), "first");
        facts.record_call("Read", &serde_json::json!({"file_path": "a.txt"}), "second");
        facts.record_call(
            "Bash",
            &serde_json::json!({"command": "cargo check"}),
            "[exit 0]\nfirst",
        );
        facts.record_call(
            "Bash",
            &serde_json::json!({"command": "cargo check"}),
            "[exit 1]\nsecond",
        );

        assert_eq!(facts.file_facts.len(), 1);
        assert_eq!(facts.file_facts[0].excerpt, "second");
        assert_eq!(facts.file_keys.len(), 1);

        assert_eq!(facts.results.len(), 1);
        assert_eq!(facts.results[0].status, "error");
        assert_eq!(facts.results[0].output_tail, "[exit 1]\nsecond");
        assert_eq!(facts.result_keys.len(), 1);
    }

    #[test]
    fn existing_window_keys_are_removed_from_summarized_tier() {
        let history = two_turn_history();
        let (older, current) = facts_for(&history, 6);

        // Only the Bash round is verbatim; the Read fact belongs to the older
        // prefix and must stay summarized.
        assert!(current.file_keys.is_empty());
        assert!(current.result_keys.contains("Bash: cargo check"));

        assert!(older.file_keys.contains("a.txt"));
        assert!(!current.file_keys.contains("a.txt"));
        let file_facts: Vec<_> = older.file_facts.iter().map(|f| f.path.as_str()).collect();
        assert!(file_facts.contains(&"a.txt"));
        assert!(!older.result_keys.contains("Bash: cargo check"));
        assert!(current.result_keys.contains("Bash: cargo check"));
    }

    #[test]
    fn memory_rendering_respects_byte_cap() {
        let history = two_turn_history();
        let (older, current) = facts_for(&history, 9);
        let cap = 64;
        let memory = render_memory(&older, &current, cap);
        assert!(
            memory.len() <= cap,
            "memory len {} exceeds {cap}",
            memory.len()
        );

        // The cap is applied to the raw text, not to the serialized wrapper.
        let working = assemble_working(memory, &history[9..]);
        assert!(serialized_len(&working) > 0);
    }

    #[test]
    fn compact_returns_original_when_every_compaction_is_larger() {
        // A single-turn history cannot be cut: compaction would only prepend
        // the fixed memory preface.
        let history = vec![
            user_prompt("hi"),
            assistant_with_tools(&[]),
            tool_result("Done", "done", "ok"),
        ];
        let compacted = compact(&history, 10_000, 0);
        assert_eq!(compacted, history);
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
