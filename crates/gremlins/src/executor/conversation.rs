//! Budget-driven compaction for chat conversation artifacts.
//!
//! The persisted `artifact://conversation.json` is always a full verbatim
//! transcript. When chat history is read back for a new turn, [`compact`]
//! produces a smaller *working* history: the most recent completed chat turn
//! is retained verbatim, while the older prefix is mechanically distilled into
//! structured memory (file facts, command results, per-turn ledger/Done
//! summaries) and an "already explored" manifest.

use indexmap::{IndexMap, IndexSet};
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
    /// Paths with summarized file-content excerpts (not operation
    /// acknowledgements or failed reads).
    file_keys: IndexSet<String>,
    /// Command/pattern cache keys with summarized results.
    result_keys: IndexSet<String>,
    /// Paths with a successful whole-file `Read` in this scope.
    covered_file_keys: IndexSet<String>,
    /// Paths mutated by a successful `Write`/`Edit` in this scope. These
    /// invalidate any earlier excerpt when chunks are merged.
    invalidated_file_keys: IndexSet<String>,
}

impl Facts {
    fn record_call(&mut self, name: &str, args: &serde_json::Value, output: &str) {
        match name {
            "Read" => {
                if let Some(path) = str_arg(args, "file_path") {
                    if is_content_output(output) {
                        self.upsert_file_fact(path.clone(), head_bytes(output, FILE_EXCERPT_BYTES));
                        if is_full_read(args) {
                            self.covered_file_keys.insert(path);
                        }
                    }
                }
            }
            "Write" | "Edit" => {
                if let Some(path) = str_arg(args, "file_path") {
                    if is_successful_mutation(output) {
                        self.invalidate_file_path(&path);
                    }
                }
            }
            "Bash" => {
                if let Some(command) = str_arg(args, "command") {
                    let key = format!("Bash: {command}");
                    self.upsert_result_fact(
                        key,
                        tools::result_status(output).to_string(),
                        bash_result_tail(output, RESULT_TAIL_BYTES),
                    );
                }
            }
            "Grep" | "Glob" => {
                if let Some(key) = search_result_key(name, args) {
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

    /// Store one content excerpt per path; a later successful read replaces
    /// the earlier excerpt and moves it to the newest position, so budget
    /// trimming drops the road-too-stale copy first.
    fn upsert_file_fact(&mut self, path: String, excerpt: String) {
        self.invalidated_file_keys.shift_remove(&path);
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

    fn invalidate_file_path(&mut self, path: &str) {
        self.file_facts.retain(|fact| fact.path != path);
        self.file_keys.shift_remove(path);
        self.covered_file_keys.shift_remove(path);
        self.invalidated_file_keys.insert(path.to_string());
    }

    /// Merge chronological chunks. Chunk invalidations retire older excerpts
    /// before newer excerpts are merged on top.
    fn merge(&mut self, other: Facts) {
        for path in &other.invalidated_file_keys {
            self.file_facts.retain(|fact| &fact.path != path);
            self.file_keys.shift_remove(path);
            self.invalidated_file_keys.insert(path.clone());
        }
        for fact in other.file_facts {
            self.upsert_file_fact(fact.path, fact.excerpt);
        }
        for fact in other.results {
            self.upsert_result_fact(fact.key, fact.status, fact.output_tail);
        }
        self.summaries.extend(other.summaries);
    }
}

/// Reduce a chat history to a budgeted working history.
///
/// A budget of `0` disables proactive compaction. `request_overhead_bytes` is
/// the serialized size of the non-history portion of the request (the current
/// prompt, system preamble, tool definitions, and envelope fields) with the
/// empty `messages` array excluded, so the soft trigger is evaluated against
/// the normalized serialized request rather than the bare transcript.
/// Histories that already fit the remaining budget are returned untouched.
/// Otherwise the oldest completed chat turns are compacted into text-only
/// user/assistant blocks and the most recent completed chat turn survives
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

    let boundaries = chat_turn_boundaries(history);
    if boundaries.len() <= 1 {
        // Only one completed chat turn (or an unpartitionable transcript):
        // keep it verbatim and let the hard prompt ceiling evaluate it as-is.
        return history.to_vec();
    }

    let message_lens: Vec<usize> = history.iter().map(serialized_message_len).collect();
    let ack =
        assistant_text("Prior context acknowledged. The retained verbatim conversation follows.");
    let ack_len = serialized_message_len(&ack);
    let empty_memory_msg_len = serialized_message_len(&Message::user(String::new()));

    let suffix_lens: Vec<usize> = boundaries
        .iter()
        .map(|&cut| range_serialized_len(cut, history.len(), &message_lens))
        .collect();
    let window_keys = window_keys_by_boundary(history, &boundaries);

    // `older` is the compacted prefix. It is built incrementally as the cut
    // advances, so facts are extracted from each archived chunk exactly once.
    let mut older = Facts::default();

    for (index, &candidate) in boundaries.iter().enumerate() {
        if candidate == 0 {
            continue;
        }

        if index > 0 {
            let chunk_start = boundaries[index - 1];
            older.merge(extract_facts(&history[chunk_start..candidate]));
        }

        // The assembled transcript is linear in the memory message size:
        // adding the memory text only replaces the empty user message with a
        // longer one. If even the empty memory message cannot fit this
        // retained window, no memory can; try a later cut (a smaller verbatim
        // window) so the required manifest has room.
        let suffix_len = suffix_lens[index];
        let empty_working_len = empty_memory_msg_len + ack_len + suffix_len + 2;
        if empty_working_len > available {
            continue;
        }

        let memory_budget = available - empty_working_len;
        if let Some(memory) = fit_memory(
            &older,
            &window_keys[index],
            memory_budget,
            suffix_len,
            ack_len,
            available,
        ) {
            return assemble_working(memory, &history[candidate..]);
        }
    }

    history.to_vec()
}

fn fit_memory(
    older: &Facts,
    current: &WindowKeys,
    mut memory_budget: usize,
    suffix_len: usize,
    ack_len: usize,
    available: usize,
) -> Option<String> {
    loop {
        let memory = render_memory(older, current, memory_budget)?;
        let memory_msg_len = serialized_message_len(&Message::user(memory.clone()));
        let working_len = memory_msg_len + ack_len + suffix_len + 2;
        if working_len <= available {
            return Some(memory);
        }

        if memory_budget == 0 {
            return None;
        }

        // JSON escaping can make the wrapped memory a few bytes larger than
        // the raw string we budgeted. Shrink by the observed overflow.
        let overflow = working_len.saturating_sub(available);
        if overflow >= memory_budget {
            memory_budget = 0;
        } else {
            memory_budget -= overflow;
        }

        // Guarantee progress when the current render already truncated the
        // memory to (or below) the budget but the wrapper still overflowed.
        let memory_len = memory.len();
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
// Chat-turn boundaries
// ---------------------------------------------------------------------------

/// A cut is valid only at the start of a completed chat turn: the operator's
/// prompt for one chat invocation. Ledger messages, artifact reminders, and
/// completion nudges are injected by the agent loop between tool batches and
/// do not start a new chat turn.
fn chat_turn_boundaries(history: &[Message]) -> Vec<usize> {
    let mut boundaries = Vec::new();
    if !history.is_empty() {
        boundaries.push(0);
    }
    for (index, message) in history.iter().enumerate().skip(1) {
        if is_chat_turn_start(message) {
            boundaries.push(index);
        }
    }
    boundaries
}

fn is_chat_turn_start(message: &Message) -> bool {
    is_text_user_message(message) && !is_synthetic_round_prompt(&user_text(message))
}

fn is_synthetic_round_prompt(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("Actions taken this turn:")
        || text.starts_with("Done was rejected because it was combined with other tool calls.")
        || text.starts_with("Done rejected:")
        || text.starts_with("The following expected output file(s) were not written:")
        || text.starts_with("You produced text but no tool calls.")
        || text == "Continue."
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

fn is_error_output(output: &str) -> bool {
    output.starts_with("Error:")
        || output.starts_with("[exit")
        || output.starts_with("[timeout]")
        || output.starts_with("Tool output exceeded the limit")
}

fn is_content_output(output: &str) -> bool {
    !is_error_output(output)
}

fn is_full_read(args: &serde_json::Value) -> bool {
    let offset = args
        .get("offset")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    offset == 0 && args.get("limit").is_none()
}

fn is_successful_mutation(output: &str) -> bool {
    output.trim() == "OK"
}

fn search_result_key(name: &str, args: &serde_json::Value) -> Option<String> {
    let pattern = str_arg(args, "pattern")?;
    let path = args
        .get("path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(".");

    match name {
        "Grep" => {
            let glob = args
                .get("glob")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("");
            let max_matches = args
                .get("max_matches")
                .and_then(|value| value.as_u64())
                .unwrap_or(2000);
            Some(format!(
                "Grep: pattern={pattern}, path={path}, glob={glob}, max_matches={max_matches}"
            ))
        }
        "Glob" => Some(format!("Glob: pattern={pattern}, path={path}")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Window keys (authoritative verbatim evidence)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileCoverage {
    Full,
    Invalidated,
}

#[derive(Default)]
struct WindowKeys {
    /// Paths whose full contents are present verbatim in the window.
    file_keys: IndexSet<String>,
    /// Command/pattern cache keys whose results are present verbatim.
    result_keys: IndexSet<String>,
    /// Decisive per-path file events in this window. A later partial read
    /// does not shadow a full read, but a successful mutation does.
    coverage: IndexMap<String, FileCoverage>,
}

fn window_keys_by_boundary(history: &[Message], boundaries: &[usize]) -> Vec<WindowKeys> {
    let mut windows = Vec::with_capacity(boundaries.len());
    let mut suffix = WindowKeys::default();

    for index in (0..boundaries.len()).rev() {
        let start = boundaries[index];
        let end = boundaries.get(index + 1).copied().unwrap_or(history.len());
        let chunk = window_keys_for_range(&history[start..end]);
        suffix = prepend_window_chunk(chunk, suffix);
        windows.push(WindowKeys {
            file_keys: suffix.file_keys.clone(),
            result_keys: suffix.result_keys.clone(),
            coverage: IndexMap::new(),
        });
    }

    windows.reverse();
    windows
}

fn window_keys_for_range(messages: &[Message]) -> WindowKeys {
    let mut keys = WindowKeys::default();
    scan_window_chunk(messages, &mut keys);
    keys
}

fn scan_window_chunk(messages: &[Message], keys: &mut WindowKeys) {
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
                record_window_call(
                    keys,
                    &tool_call.function.name,
                    &tool_call.function.arguments,
                    output,
                );
            }

            index = cursor;
            continue;
        }

        index += 1;
    }
}

fn record_window_call(keys: &mut WindowKeys, name: &str, args: &serde_json::Value, output: &str) {
    match name {
        "Read" => {
            if let Some(path) = str_arg(args, "file_path") {
                if is_content_output(output) && is_full_read(args) {
                    keys.coverage.insert(path.clone(), FileCoverage::Full);
                    keys.file_keys.insert(path);
                }
            }
        }
        "Write" | "Edit" => {
            if let Some(path) = str_arg(args, "file_path") {
                if is_successful_mutation(output) {
                    keys.coverage
                        .insert(path.clone(), FileCoverage::Invalidated);
                    keys.file_keys.shift_remove(&path);
                }
            }
        }
        "Bash" => {
            if let Some(command) = str_arg(args, "command") {
                keys.result_keys.insert(format!("Bash: {command}"));
            }
        }
        "Grep" | "Glob" => {
            if let Some(key) = search_result_key(name, args) {
                keys.result_keys.insert(key);
            }
        }
        _ => {}
    }
}

/// Combine an older chunk with the newer suffix to its right. Decisive file
/// events in the suffix win; otherwise the older chunk's event carries.
fn prepend_window_chunk(chunk: WindowKeys, suffix: WindowKeys) -> WindowKeys {
    let mut combined = WindowKeys::default();

    for (path, coverage) in chunk.coverage {
        if !suffix.coverage.contains_key(&path) {
            combined.coverage.insert(path.clone(), coverage);
            if coverage == FileCoverage::Full {
                combined.file_keys.insert(path);
            }
        }
    }
    for (path, coverage) in suffix.coverage {
        if coverage == FileCoverage::Full {
            combined.file_keys.insert(path.clone());
        }
        combined.coverage.insert(path, coverage);
    }

    let mut result_keys = chunk.result_keys;
    for key in suffix.result_keys {
        result_keys.insert(key);
    }
    combined.result_keys = result_keys;

    combined
}

// ---------------------------------------------------------------------------
// Memory rendering
// ---------------------------------------------------------------------------

/// Render the compacted-memory preamble. Returns `None` when the required
/// metadata (preamble plus both manifest tiers) cannot fit, so the caller
/// can try a retained window with more room instead of accepting an
/// incomplete memory that omits the manifest.
///
/// Evidence entries are emitted newest-first; older entries are trimmed when
/// the budget is tight.
fn render_memory(older: &Facts, current: &WindowKeys, max_bytes: usize) -> Option<String> {
    let preamble = "Conversation memory (compacted from earlier turns).\n";
    let manifest = render_manifest(older, current);

    let required_len = preamble.len() + manifest.len();
    if required_len > max_bytes {
        return None;
    }

    let evidence_budget = max_bytes - required_len;
    let evidence = render_evidence(older, current, evidence_budget);

    let mut memory = String::with_capacity(required_len + evidence.len());
    memory.push_str(preamble);
    memory.push_str(&evidence);
    memory.push_str(&manifest);
    Some(memory)
}

fn render_manifest(older: &Facts, current: &WindowKeys) -> String {
    let mut manifest = String::new();
    manifest.push_str("\n## Already explored\n");
    manifest.push_str("Authoritative (present verbatim below — do not re-read):\n");

    let mut wrote_authoritative = false;
    for path in &current.file_keys {
        manifest.push_str(&format!("- path: {path}\n"));
        wrote_authoritative = true;
    }
    for key in &current.result_keys {
        manifest.push_str(&format!("- key: {key}\n"));
        wrote_authoritative = true;
    }
    if !wrote_authoritative {
        manifest.push_str("- (none)\n");
    }

    manifest.push_str(
        "\nSummarized (older compacted context — re-read when exact content is needed):\n",
    );

    let mut wrote_summarized = false;
    for path in &older.file_keys {
        if !current.file_keys.contains(path) {
            manifest.push_str(&format!("- path: {path}\n"));
            wrote_summarized = true;
        }
    }
    for key in &older.result_keys {
        if !current.result_keys.contains(key) {
            manifest.push_str(&format!("- key: {key}\n"));
            wrote_summarized = true;
        }
    }
    if !wrote_summarized {
        manifest.push_str("- (none)\n");
    }

    manifest
}

fn render_evidence(older: &Facts, current: &WindowKeys, mut budget: usize) -> String {
    let mut out = String::new();

    let mut file_entries = Vec::new();
    for fact in older.file_facts.iter().rev() {
        if !current.file_keys.contains(&fact.path) {
            file_entries.push(format!(
                "- path: {}\n  excerpt: {}\n",
                fact.path, fact.excerpt
            ));
        }
    }
    push_evidence_section(
        &mut out,
        &mut budget,
        "\n## File facts (summarized)\n",
        &file_entries,
    );

    let mut result_entries = Vec::new();
    for result in older.results.iter().rev() {
        if !current.result_keys.contains(&result.key) {
            result_entries.push(format!(
                "- key: {}\n  status: {}\n  output-tail: {}\n",
                result.key, result.status, result.output_tail
            ));
        }
    }
    push_evidence_section(
        &mut out,
        &mut budget,
        "\n## Command and pattern results (summarized)\n",
        &result_entries,
    );

    let summaries: Vec<String> = older
        .summaries
        .iter()
        .rev()
        .map(|summary| format!("- {summary}\n"))
        .collect();
    push_evidence_section(
        &mut out,
        &mut budget,
        "\n## Earlier turn summaries\n",
        &summaries,
    );

    out
}

fn push_evidence_section(out: &mut String, budget: &mut usize, header: &str, entries: &[String]) {
    if entries.is_empty() {
        return;
    }

    // Newest entries are first; once one entry no longer fits, trim the older
    // entries behind it rather than dropping newer evidence first.
    let header_len = header.len();
    if header_len > *budget {
        return;
    }

    let mut entry_budget = *budget - header_len;
    let mut selected = Vec::new();
    for entry in entries {
        if entry.len() <= entry_budget {
            selected.push(entry.clone());
            entry_budget -= entry.len();
        } else {
            break;
        }
    }

    if selected.is_empty() {
        return;
    }

    out.push_str(header);
    *budget -= header_len;
    for entry in selected {
        out.push_str(&entry);
        *budget -= entry.len();
    }
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

fn serialized_message_len(message: &Message) -> usize {
    serde_json::to_string(message)
        .map(|json| json.len())
        .unwrap_or(usize::MAX)
}

fn range_serialized_len(start: usize, end: usize, message_lens: &[usize]) -> usize {
    let count = end - start;
    if count == 0 {
        return 2;
    }
    let body: usize = message_lens[start..end].iter().sum();
    // A compact JSON array: `[` + elements joined by `,` + `]`.
    2 + body + count - 1
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

/// Keep the last `max` bytes of a Bash result, preserving the leading
/// `[exit N]` marker for failing commands so the exit code survives even when
/// the output tail is truncated.
fn bash_result_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }

    let tail = tail_bytes(text, max);
    if !text.starts_with("[exit") || tail.starts_with("[exit") {
        return tail;
    }

    let marker_end = text.find('\n').unwrap_or(text.len());
    let marker = &text[..marker_end];
    let marker_len = marker.len();
    if marker_len >= max {
        return head_bytes(marker, max);
    }

    let tail_budget = max - marker_len - 1;
    let mut preserved = String::with_capacity(max);
    preserved.push_str(marker);
    preserved.push('\n');
    preserved.push_str(&tail_bytes(text, tail_budget));
    preserved
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

    /// A realistic two-chat-turn archive. The first turn has two tool batches
    /// (Read then Bash) before Done; the second has one Bash batch before
    /// Done. Ledger messages are model rounds inside each chat turn, not new
    /// chat-turn boundaries.
    fn two_turn_history() -> Vec<Message> {
        let read_call = tool_call("read_1", "Read", serde_json::json!({"file_path": "a.txt"}));
        let bash_call = tool_call(
            "bash_1",
            "Bash",
            serde_json::json!({"command": "cargo check"}),
        );
        let done_1 = tool_call(
            "done_1",
            "Done",
            serde_json::json!({"summary": "read a.txt"}),
        );
        let bash_2 = tool_call(
            "bash_2",
            "Bash",
            serde_json::json!({"command": "cargo test"}),
        );
        let done_2 = tool_call("done_2", "Done", serde_json::json!({"summary": "finished"}));

        vec![
            user_prompt("read a.txt and run cargo check"),
            assistant_with_tools(&[read_call]),
            tool_result("Read", "read_1", "hello from a.txt"),
            user_prompt("Actions taken this turn:\n- Read a.txt\n"),
            assistant_with_tools(&[bash_call]),
            tool_result("Bash", "bash_1", "[exit 1]\ncargo failed"),
            user_prompt("Actions taken this turn:\n- Bash cargo check (failed)\n"),
            assistant_with_tools(&[done_1]),
            tool_result("Done", "done_1", "Done accepted."),
            user_prompt("now run cargo test"),
            assistant_with_tools(&[bash_2]),
            tool_result("Bash", "bash_2", "[exit 0]\nok"),
            user_prompt("Actions taken this turn:\n- Bash cargo test\n"),
            assistant_with_tools(&[done_2]),
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
    fn partitioning_retains_most_recent_chat_turn_verbatim() {
        let history = two_turn_history();
        let boundaries = chat_turn_boundaries(&history);
        assert_eq!(boundaries, vec![0, 9]);

        let cut = 9;
        let working = compact(&history, serialized_len(&history) as u64 - 1, 0);
        assert!(working.len() < history.len());
        assert_eq!(
            &working[working.len() - (history.len() - cut)..],
            &history[cut..]
        );
        // The latest chat prompt is retained, not a trailing ledger.
        assert_eq!(
            &working[working.len() - (history.len() - cut)],
            &history[cut]
        );
    }

    #[test]
    fn compacted_memory_contains_keyed_file_facts_and_results() {
        let history = two_turn_history();
        let older = extract_facts(&history[..9]);
        let current = window_keys_for_range(&history[9..]);
        let text = render_memory(&older, &current, 4096).expect("manifest must fit");

        assert!(
            text.contains("Conversation memory"),
            "missing memory: {text}"
        );
        assert!(text.contains("path: a.txt"), "missing file fact: {text}");
        assert!(
            text.contains("Bash: cargo check"),
            "missing older result key: {text}"
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
        let working = compact(&history, serialized_len(&history) as u64 - 1, 0);
        let text = user_text(&working[0]);

        let authoritative = text.find("Authoritative").unwrap();
        let summarized = text.find("Summarized").unwrap();
        assert!(authoritative < summarized);

        // The newest turn's Bash result is retained verbatim behind the memory
        // block, so it must be authoritative rather than summarized.
        let authoritative_tier = &text[authoritative..summarized];
        assert!(authoritative_tier.contains("Bash: cargo test"));
        assert!(!authoritative_tier.contains("path: a.txt"));

        let summarized_tier = &text[summarized..];
        assert!(summarized_tier.contains("path: a.txt"));
        assert!(summarized_tier.contains("Bash: cargo check"));
    }

    #[test]
    fn output_has_valid_transcript_shape() {
        let history = two_turn_history();
        let working = compact(&history, serialized_len(&history) as u64 - 1, 0);

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
        assert!(!is_synthetic_round_prompt(&user_text(&working[2])));
    }

    #[test]
    fn chat_turn_boundaries_skip_synthetic_round_prompts() {
        let history = two_turn_history();
        let boundaries = chat_turn_boundaries(&history);

        assert_eq!(boundaries, vec![0, 9]);
        for boundary in boundaries.iter().skip(1) {
            assert!(
                *boundary == 0 || is_chat_turn_start(&history[*boundary]),
                "boundary {boundary} is not a chat-turn start"
            );
        }
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
    fn search_keys_include_scope_and_limits() {
        let mut facts = Facts::default();
        facts.record_call(
            "Grep",
            &serde_json::json!({"pattern": "TODO", "path": "src"}),
            "src/a.rs:1:TODO",
        );
        facts.record_call(
            "Grep",
            &serde_json::json!({"pattern": "TODO", "path": "tests"}),
            "tests/a.rs:1:TODO",
        );
        facts.record_call(
            "Grep",
            &serde_json::json!({"pattern": "TODO", "path": "src", "max_matches": 5}),
            "src/a.rs:1:TODO",
        );
        facts.record_call(
            "Glob",
            &serde_json::json!({"pattern": "*.rs", "path": "src"}),
            "src/a.rs",
        );
        facts.record_call(
            "Glob",
            &serde_json::json!({"pattern": "*.rs", "path": "tests"}),
            "tests/a.rs",
        );

        assert_eq!(facts.results.len(), 5);
        assert_eq!(facts.result_keys.len(), 5);
        assert!(facts
            .result_keys
            .contains("Grep: pattern=TODO, path=src, glob=, max_matches=2000"));
        assert!(facts
            .result_keys
            .contains("Grep: pattern=TODO, path=tests, glob=, max_matches=2000"));
        assert!(facts
            .result_keys
            .contains("Grep: pattern=TODO, path=src, glob=, max_matches=5"));
        assert!(facts.result_keys.contains("Glob: pattern=*.rs, path=src"));
        assert!(facts.result_keys.contains("Glob: pattern=*.rs, path=tests"));
    }

    #[test]
    fn long_failing_bash_tail_preserves_exit_code() {
        let mut output = String::from("[exit 7]\n");
        output.push_str(&"x".repeat(RESULT_TAIL_BYTES + 500));

        let tail = bash_result_tail(&output, RESULT_TAIL_BYTES);
        assert!(
            tail.starts_with("[exit 7]\n"),
            "exit marker must survive truncation: {tail:?}"
        );
        assert!(tail.len() <= RESULT_TAIL_BYTES);
        assert!(!tail.contains('x') || tail.ends_with('x'));

        // Timeout output is already tiny and does not need marker surgery.
        let timeout = "[timeout]";
        assert_eq!(bash_result_tail(timeout, RESULT_TAIL_BYTES), timeout);
    }

    #[test]
    fn partial_and_failed_reads_are_not_authoritative() {
        let partial_read = tool_call(
            "read_partial",
            "Read",
            serde_json::json!({"file_path": "a.txt", "limit": 10}),
        );
        let failed_read = tool_call(
            "read_failed",
            "Read",
            serde_json::json!({"file_path": "missing.txt"}),
        );

        let partial_history = vec![
            user_prompt("read a.txt"),
            assistant_with_tools(&[partial_read]),
            tool_result("Read", "read_partial", "partial contents"),
        ];
        let facts = extract_facts(&partial_history);
        assert!(!facts.covered_file_keys.contains("a.txt"));
        assert!(facts.file_keys.contains("a.txt"));

        let failed_history = vec![
            user_prompt("read missing"),
            assistant_with_tools(&[failed_read]),
            tool_result("Read", "read_failed", "Error: no such file"),
        ];
        let facts = extract_facts(&failed_history);
        assert!(facts.file_facts.is_empty());
        assert!(!facts.covered_file_keys.contains("missing.txt"));
        assert!(!facts.file_keys.contains("missing.txt"));
    }

    #[test]
    fn mutations_invalidate_file_coverage() {
        let read_call = tool_call("read_1", "Read", serde_json::json!({"file_path": "a.txt"}));
        let write_call = tool_call(
            "write_1",
            "Write",
            serde_json::json!({"file_path": "a.txt", "content": "changed"}),
        );

        let history = vec![
            user_prompt("read then write a.txt"),
            assistant_with_tools(&[read_call]),
            tool_result("Read", "read_1", "old contents"),
            assistant_with_tools(&[write_call]),
            tool_result("Write", "write_1", "OK"),
        ];
        let facts = extract_facts(&history);
        assert!(!facts.covered_file_keys.contains("a.txt"));
        assert!(!facts.file_keys.contains("a.txt"));
        assert!(facts.invalidated_file_keys.contains("a.txt"));
    }

    #[test]
    fn window_keys_do_not_cover_partial_reads_or_mutations() {
        let full_read = tool_call("read_1", "Read", serde_json::json!({"file_path": "a.txt"}));
        let partial_read = tool_call(
            "read_2",
            "Read",
            serde_json::json!({"file_path": "b.txt", "limit": 5}),
        );
        let write_call = tool_call(
            "write_1",
            "Write",
            serde_json::json!({"file_path": "c.txt", "content": "x"}),
        );

        let history = vec![
            user_prompt("inspect files"),
            assistant_with_tools(&[full_read, partial_read, write_call]),
            tool_result("Read", "read_1", "full contents"),
            tool_result("Read", "read_2", "partial"),
            tool_result("Write", "write_1", "OK"),
        ];
        let keys = window_keys_for_range(&history);
        assert!(keys.file_keys.contains("a.txt"));
        assert!(!keys.file_keys.contains("b.txt"));
        assert!(!keys.file_keys.contains("c.txt"));
    }

    #[test]
    fn render_memory_requires_manifest_and_keeps_newest_evidence() {
        let old_fact = FileFact {
            path: "old.txt".to_string(),
            excerpt: format!("OLD_CONTENT\n{}", "o".repeat(2048)),
        };
        let new_fact = FileFact {
            path: "new.txt".to_string(),
            excerpt: format!("NEW_CONTENT\n{}", "n".repeat(2048)),
        };
        let older = Facts {
            file_facts: vec![old_fact, new_fact],
            file_keys: IndexSet::from(["old.txt".to_string(), "new.txt".to_string()]),
            ..Facts::default()
        };
        let current = WindowKeys::default();

        // 4096 bytes cannot hold both 2 KiB excerpts plus the manifest; the
        // newer excerpt must win and both manifest tiers must survive.
        let memory = render_memory(&older, &current, 4096).expect("manifest must fit");
        assert!(
            memory.contains("NEW_CONTENT"),
            "newest evidence lost: {memory}"
        );
        assert!(
            !memory.contains("OLD_CONTENT"),
            "oldest evidence retained: {memory}"
        );
        assert!(memory.contains("## Already explored"));
        assert!(memory.contains("Authoritative"));
        assert!(memory.contains("Summarized"));

        // A very tight budget still renders the required metadata.
        let memory = render_memory(&older, &current, 512).expect("manifest must fit");
        assert!(memory.contains("## Already explored"));
        assert!(!memory.contains("NEW_CONTENT"));
        assert!(!memory.contains("OLD_CONTENT"));
    }

    #[test]
    fn memory_rendering_respects_byte_cap() {
        let history = two_turn_history();
        let older = extract_facts(&history[..9]);
        let current = window_keys_for_range(&history[9..]);

        // The manifest cannot fit in 64 bytes.
        assert!(render_memory(&older, &current, 64).is_none());

        let cap = 4096;
        let memory = render_memory(&older, &current, cap).expect("manifest must fit");
        assert!(
            memory.len() <= cap,
            "memory len {} exceeds {cap}",
            memory.len()
        );

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
    fn range_serialized_len_matches_json_serialization() {
        let history = two_turn_history();
        let message_lens: Vec<usize> = history.iter().map(serialized_message_len).collect();

        for start in 0..=history.len() {
            for end in start..=history.len() {
                assert_eq!(
                    range_serialized_len(start, end, &message_lens),
                    serde_json::to_string(&history[start..end]).unwrap().len()
                );
            }
        }
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
