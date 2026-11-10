//! Cut-point helpers, compaction orchestration state, and the on-demand
//! `CompactionReport` return type.
//!
//! Cut-point functions operate purely on [`Message`] slices — they have no
//! dependency on [`Backend`] or any Anthropic-specific type.
//!
//! Local compaction is a pure message transform here: it truncates tool
//! results, extracts file operations, builds a preamble, and delegates the
//! actual completion to [`crate::clients::run_naked`]. No I/O driver types
//! cross into this module.

use rig_core::completion::message::{AssistantContent, UserContent};
use rig_core::completion::Message;
use serde::Serialize;

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
pub(crate) const LOCAL_COMPACTION_MAX_TOKENS: u64 = 2048;

/// The user-turn trigger that asks the model to produce the summary. The
/// conversation history is supplied separately, so this is deliberately a
/// short, fixed instruction rather than the full summarization prompt (which
/// rides in the preamble).
const LOCAL_COMPACTION_PROMPT: &str = "Summarize the conversation above.";

/// Maximum characters for a single tool-result text block in the
/// summarization request. Beyond this, the content is truncated with a
/// marker.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

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

// ── Local compaction ─────────────────────────────────────────────────

/// Build the local-compaction request and hand it to
/// [`crate::clients::run_naked`]. Returns the summary text with token
/// counts, or `None` on failure.
///
/// `system_prompt` is the conversation's active system prompt — it is
/// prepended to the summarization instructions in the preamble so the
/// model has context about its role and constraints.
///
/// Token counts are `None` for now: `run_naked` returns only the assistant
/// text. A follow-up can return a richer struct if callers want the
/// compaction call's own usage.
pub(crate) async fn run_local_compaction(
    spec: &str,
    to_compact: &[Message],
    instructions: Option<&str>,
    system_prompt: Option<&str>,
) -> Option<(Message, Option<u64>, Option<u64>)> {
    let summarization_instructions = instructions.unwrap_or(DEFAULT_LOCAL_COMPACTION_PROMPT);

    let preamble = match system_prompt {
        Some(sys) if !sys.is_empty() => {
            format!("{sys}\n\n{summarization_instructions}")
        }
        _ => summarization_instructions.to_string(),
    };

    let truncated = truncate_for_summarization(to_compact);

    // Defense-in-depth: the coarse prompt-size ceiling is re-checked here so
    // a pathological `to_compact` that slips past the truncation floor cannot
    // reach the provider. `render_turn` enforces the same bound inside the
    // agent loop, but by then the request is already being assembled.
    let prompt_len = LOCAL_COMPACTION_PROMPT.len() as u64;
    let actual = preamble.len() as u64
        + prompt_len
        + truncated
            .iter()
            .map(crate::clients::conversation::message_text_byte_len)
            .sum::<u64>();
    let limit = crate::config::max_prompt_bytes();
    if actual > limit {
        log::warn!(
            "Local compaction request {actual} bytes exceeds configured limit of {limit} bytes"
        );
        return None;
    }

    let summary_text = match crate::clients::run_naked(
        spec,
        truncated,
        LOCAL_COMPACTION_PROMPT,
        preamble,
        Some(LOCAL_COMPACTION_MAX_TOKENS),
    )
    .await
    {
        Ok(text) => text,
        Err(e) => {
            log::warn!("Local compaction failed: {e}");
            return None;
        }
    };

    if summary_text.is_empty() {
        return None;
    }

    // The agent loop's done-decision guarantees a completed stream, but a
    // completion can still end at the token cap mid-word. Reject those rather
    // than injecting a truncated summary into the durable history.
    if summary_looks_truncated(&summary_text) {
        log::warn!("Local compaction produced a truncated summary; discarding");
        return None;
    }

    let (read_files, modified_files) = extract_file_ops(to_compact);
    let file_annotations = format_file_ops(&read_files, &modified_files);
    let enriched = format!("{summary_text}{file_annotations}");

    let msg = Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(
            rig_core::completion::message::Text {
                text: enriched,
                additional_params: None,
            },
        )],
    };

    Some((msg, None, None))
}

// ── Defensive failure checks ─────────────────────────────────────────

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

    #[test]
    fn cut_empty_returns_zero() {
        assert_eq!(compaction_cut_index(&[], 10), 0);
    }

    #[test]
    fn cut_keep_recent_zero_returns_full_length() {
        // keep_recent=0 means keep nothing — compact everything
        let h = paired_history(5);
        assert_eq!(compaction_cut_index(&h, 0), h.len());
    }

    #[test]
    fn cut_fewer_than_keep_recent_returns_zero() {
        let h = paired_history(3);
        assert_eq!(compaction_cut_index(&h, 10), 0);
    }

    #[test]
    fn cut_more_than_keep_recent() {
        let h = paired_history(20);
        let cut = compaction_cut_index(&h, 10);
        assert!(cut > 0, "cut should be > 0 for 20 pairs with keep 10");
        assert!(cut < h.len(), "cut should be inside the history");
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
