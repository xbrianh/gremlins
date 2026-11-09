//! Cut-point helpers, compaction orchestration state, local-compaction
//! helpers, and the on-demand `CompactionReport` return type.
//!
//! Cut-point functions operate purely on [`Message`] slices — they have no
//! dependency on [`Backend`] or any Anthropic-specific type.
//!
//! Local-compaction helpers (`compact_local_raw`, `compact_local_with_retry`,
//! and their truncation/validation support) live here so that
//! [`crate::conversation::standard::StandardConversation`] can call them
//! when the backend does not support server-side compaction.

use std::fmt;

use rig_core::completion::message::{AssistantContent, UserContent};
use rig_core::completion::{CompletionRequest, CompletionResponse, Message};
use serde::Serialize;

use crate::clients::model::GremlinsModel;
use crate::clients::protocol::CompactionBlock;

/// Compute a cut index so `to_compact = &history[..cut]` and
/// `kept = &history[cut..]`. The cut lands on a completed-turn boundary:
/// `keep_recent` whole user/assistant exchanges are kept verbatim and
/// `to_compact` never ends in an unanswered tool call.
pub(crate) fn compaction_cut_index(history: &[Message], keep_recent: usize) -> usize {
    let mut cut = history.len();
    let mut kept_turns = 0;
    while cut > 0 && kept_turns < keep_recent {
        // Skip trailing user tool_result messages belonging to the
        // preceding turn.
        while cut > 0 && is_user_tool_result(&history[cut - 1]) {
            cut -= 1;
        }
        // Step back over this turn's user opener and its assistant/tool
        // messages to the previous completed-turn boundary.
        cut = previous_turn_boundary(history, cut);
        kept_turns += 1;
    }
    cut
}

/// Walk backward from `cut` (exclusive) to the start of the preceding
/// completed user+assistant turn, skipping assistant/tool messages.
fn previous_turn_boundary(history: &[Message], mut cut: usize) -> usize {
    // Step back over a chain of (assistant, user tool_result) pairs until
    // we hit a user message that is NOT a tool result (the turn opener).
    loop {
        // Skip the assistant message.
        if cut > 0 && matches!(history[cut - 1], Message::Assistant { .. }) {
            cut -= 1;
        } else {
            break;
        }
        // Skip any user tool_result messages after the assistant.
        while cut > 0 && is_user_tool_result(&history[cut - 1]) {
            cut -= 1;
        }
    }
    // Step back over the user opener.
    if cut > 0 && matches!(history[cut - 1], Message::User { .. }) {
        cut -= 1;
    }
    cut
}

/// True when the message is a user message containing exclusively tool
/// results (no mixed text + tool-result blocks).
fn is_user_tool_result(msg: &Message) -> bool {
    match msg {
        Message::User { content } => {
            !content.is_empty()
                && content
                    .iter()
                    .all(|c| matches!(c, UserContent::ToolResult(_)))
        }
        _ => false,
    }
}

// ── Compaction report ────────────────────────────────────────────────

/// Which compaction strategy was used.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) enum CompactionStrategy {
    Server,
    Local,
}

/// Result of an on-demand compaction call.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompactionReport {
    /// Which strategy was used.
    pub strategy: CompactionStrategy,
    /// How many messages were replaced.
    pub messages_compacted: usize,
    /// How many messages were kept verbatim.
    pub messages_kept: usize,
    /// Input tokens consumed by the compaction call itself.
    pub compaction_input_tokens: Option<u64>,
    /// Output tokens produced by the compaction call.
    pub compaction_output_tokens: Option<u64>,
    /// The signed compaction block (server compaction only).
    pub compaction_block: Option<CompactionBlock>,
}

// ── Local compaction ─────────────────────────────────────────────────

/// Default instructions for local compaction when none are configured.
const DEFAULT_LOCAL_COMPACTION_PROMPT: &str = "\
Summarize the conversation above. Preserve:
- Key decisions and the reasoning behind them
- Entity names, field names, and data structures agreed upon
- File paths that were created or modified
- Rejected approaches and why
- The user's latest open request or question
Write a single concise paragraph. Do not call any tools.";

/// Output token budget for the local compaction completion.
const LOCAL_COMPACTION_MAX_TOKENS: u64 = 2048;

/// Maximum characters for a single tool-result text block in the
/// summarization request. Beyond this, the content is truncated with a
/// marker.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Maximum number of summarization attempts per compaction trigger.
const MAX_COMPACTION_ATTEMPTS: usize = 2;

/// Retry delay between compaction attempts.
const COMPACTION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

// ── File path extraction for summaries ───────────────────────────────

/// Extract file paths from tool-use content blocks in `to_compact`.
/// Returns `(read_paths, modified_paths)` (sorted, deduplicated).
fn extract_file_ops(messages: &[Message]) -> (Vec<String>, Vec<String>) {
    let mut reads: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();

    for msg in messages {
        if let Message::Assistant { content, .. } = msg {
            for block in content {
                if let AssistantContent::ToolCall(tc) = block {
                    let path = tc
                        .function
                        .arguments
                        .get("file_path")
                        .or_else(|| tc.function.arguments.get("path"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    if let Some(p) = path {
                        match tc.function.name.as_str() {
                            "Read" | "Glob" | "Grep" | "Landscape" => {
                                if !reads.contains(&p) {
                                    reads.push(p);
                                }
                            }
                            "Write" | "Edit" if !modified.contains(&p) => {
                                modified.push(p);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    reads.sort();
    reads.dedup();
    modified.sort();
    modified.dedup();

    (reads, modified)
}

/// Format extracted file paths as a structured annotation block for the
/// summary. `read_files` and `modified_files` are expected to be
/// pre-sorted and deduplicated by [`extract_file_ops`].
fn format_file_ops(read_files: &[String], modified_files: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        parts.push(format!("Read: {}", read_files.join(", ")));
    }
    if !modified_files.is_empty() {
        parts.push(format!("Modified: {}", modified_files.join(", ")));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\n\nFiles:\n{}", parts.join("\n"))
    }
}

// ── Tool-result truncation ───────────────────────────────────────────

/// Truncate tool-result content blocks in `messages` for the
/// summarization request. Returns a new vec with truncated messages.
fn truncate_for_summarization(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|msg| match msg {
            Message::User { content } => Message::User {
                content: content
                    .iter()
                    .map(|c| match c {
                        UserContent::ToolResult(tr) => {
                            UserContent::ToolResult(rig_core::completion::message::ToolResult {
                                content: truncate_tool_result_content(&tr.content),
                                ..tr.clone()
                            })
                        }
                        other => other.clone(),
                    })
                    .collect(),
            },
            other => other.clone(),
        })
        .collect()
}

/// Truncate `ToolResultContent` entries that exceed
/// `TOOL_RESULT_MAX_CHARS`. Only text content is truncated; non-text
/// blocks pass through unchanged.
fn truncate_tool_result_content(
    content: &[rig_core::completion::message::ToolResultContent],
) -> Vec<rig_core::completion::message::ToolResultContent> {
    use rig_core::completion::message::{Text, ToolResultContent};
    content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(t) => {
                if t.text.len() > TOOL_RESULT_MAX_CHARS {
                    // Find a char boundary at or before the limit to avoid
                    // a panic when the byte-index slice lands in the middle
                    // of a multi-byte UTF-8 sequence.
                    let cutoff = t.text.floor_char_boundary(TOOL_RESULT_MAX_CHARS);
                    let truncated = &t.text[..cutoff];
                    let remaining = t.text.len() - cutoff;
                    ToolResultContent::Text(Text {
                        text: format!("{truncated}…\n[{remaining} more chars truncated]"),
                        ..t.clone()
                    })
                } else {
                    c.clone()
                }
            }
            other => other.clone(),
        })
        .collect()
}

// ── Defensive failure checks ─────────────────────────────────────────

/// Validate that a summarization response is safe to use as a compaction
/// summary. Returns `Some(reason)` if the response should be discarded.
fn validate_summarization_response(response: &CompletionResponse) -> Option<&'static str> {
    // Reject tool-use responses — summarization must be prose-only.
    let has_tool_use = response
        .choice
        .iter()
        .any(|c| matches!(c, AssistantContent::ToolCall(_)));
    if has_tool_use {
        return Some("summarization response contained tool calls");
    }

    None
}

/// Check whether the summary text looks truncated (terminated mid-word
/// or at an unnatural boundary). Only flags explicit truncation markers
/// and mid-word endings — open-ended text finishing with a period,
/// quote, backtick, or colon is treated as valid.
fn summary_looks_truncated(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    // Common truncation markers.
    if trimmed.ends_with('…') || trimmed.ends_with("...") {
        return true;
    }
    // Mid-word ending: the final byte is an alphanumeric continuation
    // character (letter, digit, or underscore) with no preceding
    // sentence-final punctuation nearby. A summary that trails off in
    // the middle of a word is almost certainly truncated.
    !trimmed.ends_with('.')
        && !trimmed.ends_with('?')
        && !trimmed.ends_with('!')
        && !trimmed.ends_with('"')
        && !trimmed.ends_with('`')
        && !trimmed.ends_with(':')
        && !trimmed.ends_with(';')
        && !trimmed.ends_with(')')
        && !trimmed.ends_with(']')
        && !trimmed.ends_with('}')
        && !trimmed.ends_with('\'')
}

// ── Local compaction ─────────────────────────────────────────────────

/// Why a local compaction attempt failed.
#[derive(Debug)]
pub(crate) enum CompactError {
    /// The model call itself failed (network, timeout, 5xx). Worth
    /// retrying.
    Transient(String),
    /// The response was rejected by validation (tool calls, empty
    /// summary, truncated). Retrying won't help.
    Fatal(String),
}

impl fmt::Display for CompactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompactError::Transient(msg) | CompactError::Fatal(msg) => msg.fmt(f),
        }
    }
}

/// Core single-turn summarization — raw call, no retry, no file-path
/// enrichment. This is the inner function wrapped by
/// [`compact_local_with_retry`].
pub(crate) async fn compact_local_raw(
    model: &GremlinsModel,
    to_compact: &[Message],
    instructions: Option<&str>,
    system_prompt: Option<&str>,
) -> Result<(Message, Option<u64>, Option<u64>), CompactError> {
    let summarization_instructions = instructions.unwrap_or(DEFAULT_LOCAL_COMPACTION_PROMPT);

    // Build a system preamble: the conversation's system prompt first
    // (so the model has role/constraint context), then the summarization
    // instructions.
    let preamble = match system_prompt {
        Some(sys) if !sys.is_empty() => {
            format!("{sys}\n\n{summarization_instructions}")
        }
        _ => summarization_instructions.to_string(),
    };

    // Truncate large tool-result content blocks before passing to the
    // model — the model needs the gist, not the verbatim content.
    let truncated = truncate_for_summarization(to_compact);

    // Build a CompletionRequest with the preamble carrying both the
    // conversation system context and the summarization instructions.
    // `to_compact` is set as the message history and a short user
    // message triggers the summarization turn. Order: preamble →
    // history → trigger.
    let request = CompletionRequest::new(Message::user("Summarize the conversation above."))
        .preamble(preamble)
        .messages(truncated)
        .max_tokens(LOCAL_COMPACTION_MAX_TOKENS);

    // Unary call — the summary is at most a few paragraphs.
    let response = match model.call(request).await {
        Ok(r) => r,
        Err(e) => {
            return Err(CompactError::Transient(format!(
                "Local compaction call failed: {e}"
            )));
        }
    };

    // Defensive: reject responses with tool calls.
    if let Some(reason) = validate_summarization_response(&response) {
        return Err(CompactError::Fatal(format!(
            "Local compaction rejected: {reason}"
        )));
    }

    // Extract the assistant text and token usage.
    let summary: String = response
        .choice
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    if summary.is_empty() {
        return Err(CompactError::Fatal(
            "Local compaction produced empty summary".into(),
        ));
    }

    // Defensive: reject summaries that look truncated.
    if summary_looks_truncated(&summary) {
        return Err(CompactError::Fatal(
            "Local compaction produced a truncated summary; discarding".into(),
        ));
    }

    // Enrich the summary with file-path annotations.
    let (read_files, modified_files) = extract_file_ops(to_compact);
    let file_annotations = format_file_ops(&read_files, &modified_files);
    let enriched = format!("{summary}{file_annotations}");

    let input_tokens = response.usage.input_tokens;
    let output_tokens = response.usage.output_tokens;

    let msg = Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(
            rig_core::completion::message::Text {
                text: enriched,
                additional_params: None,
            },
        )],
    };

    Ok((msg, input_tokens, output_tokens))
}

/// Run a single-turn summarization completion on `to_compact` with a
/// single retry on transient failure.
///
/// `system_prompt` is the conversation's active system prompt — it is
/// prepended to the summarization instructions in the system preamble
/// so the model has context about its role and constraints.
///
/// Returns a plain assistant text message with token counts, or `None`
/// if all attempts failed. Does not produce a signed `CompactionBlock` —
/// the returned message is an ordinary `Message::Assistant` whose text
/// content is the summary.
pub(crate) async fn compact_local_with_retry(
    model: &GremlinsModel,
    to_compact: &[Message],
    instructions: Option<&str>,
    system_prompt: Option<&str>,
) -> Option<(Message, Option<u64>, Option<u64>)> {
    for attempt in 0..MAX_COMPACTION_ATTEMPTS {
        match compact_local_raw(model, to_compact, instructions, system_prompt).await {
            Ok(v) => return Some(v),
            Err(CompactError::Transient(msg)) if attempt < MAX_COMPACTION_ATTEMPTS - 1 => {
                log::warn!(
                    "Local compaction attempt {} failed (transient): {msg}; retrying in {:?}",
                    attempt + 1,
                    COMPACTION_RETRY_DELAY,
                );
                tokio::time::sleep(COMPACTION_RETRY_DELAY).await;
            }
            Err(e) => {
                log::warn!(
                    "Local compaction failed after {} attempt{}: {e}; entering cooldown",
                    attempt + 1,
                    if attempt > 0 { "s" } else { "" },
                );
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::{AssistantContent, Text, UserContent};

    /// Build a simple user message with one text block.
    fn user_msg<S: Into<String>>(s: S) -> Message {
        Message::User {
            content: vec![UserContent::Text(Text {
                text: s.into(),
                additional_params: None,
            })],
        }
    }

    /// Build a simple assistant message with one text block.
    fn asst_msg<S: Into<String>>(s: S) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::Text(Text {
                text: s.into(),
                additional_params: None,
            })],
        }
    }

    /// Build a history with `pairs` user→assistant exchanges.
    fn paired_history(pairs: usize) -> Vec<Message> {
        let mut h = Vec::with_capacity(pairs * 2);
        for i in 0..pairs {
            h.push(user_msg(format!("user {i}")));
            h.push(asst_msg(format!("assistant {i}")));
        }
        h
    }
}
