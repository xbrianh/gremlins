//! Cut-point helpers and local model-driven compaction.
//!
//! Cut-point functions operate purely on [`Message`] slices — they have no
//! dependency on [`Backend`], [`Compaction`], or any Anthropic-specific type.
//!
//! Local compaction runs a single-turn summarization completion on the same
//! model, producing a plain assistant text message (no signed block, no
//! beta headers, no Anthropic-specific content type).

use rig_core::completion::message::AssistantContent;
use rig_core::completion::message::UserContent;
use rig_core::completion::{CompletionRequest, Message};

use crate::clients::model::GremlinsModel;

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

/// Run a single-turn summarization completion on `to_compact`.
///
/// Returns a plain assistant text message with token counts, or `None`
/// if the call failed. Does not produce a signed `CompactionBlock` —
/// the returned message is an ordinary `Message::Assistant` whose text
/// content is the summary.
pub(crate) async fn compact_local(
    model: &GremlinsModel,
    to_compact: &[Message],
    instructions: Option<&str>,
) -> Option<(Message, Option<u64>, Option<u64>)> {
    let prompt = instructions.unwrap_or(DEFAULT_LOCAL_COMPACTION_PROMPT);

    // Build a CompletionRequest with no tools and capped tokens.
    // `CompletionRequest::messages()` inserts `to_compact` between the
    // system preamble (already set below) and the prompt message (set
    // via `new()` and kept last). The resulting order is: system,
    // history messages, "summarize" instruction — so the model sees
    // the conversation it should summarise before the summarise
    // instruction.
    let request = CompletionRequest::new(Message::user(prompt))
        .messages(to_compact.to_vec())
        .max_tokens(LOCAL_COMPACTION_MAX_TOKENS);

    // Unary call — the summary is at most a few paragraphs.
    let response = match model.call(request).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("Local compaction call failed: {e}");
            return None;
        }
    };

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
        log::warn!("Local compaction produced empty summary");
        return None;
    }

    let input_tokens = response.usage.input_tokens;
    let output_tokens = response.usage.output_tokens;

    let msg = Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(
            rig_core::completion::message::Text {
                text: summary,
                additional_params: None,
            },
        )],
    };

    Some((msg, input_tokens, output_tokens))
}
