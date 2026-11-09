//! Compaction dispatch.
//!
//! [`Compaction`] holds the backend reference, model, instructions, strategy,
//! and (for local compaction) a pre-built model instance. Runtime orchestration
//! state — threshold, cooldown, applied flag, force flag — lives in
//! [`crate::conversation::compaction::CompactionState`].
//!
//! The strategy is picked at construction time:
//!
//! - **Server**: Native server-side compaction (Anthropic on-demand API).
//! - **Local**: Model-driven summarization on a pre-built model instance.
//!
//! There is no fallback chain. Server fails → error. Local fails → error.

use std::fmt;
use std::sync::Arc;

use rig_core::completion::message::{AssistantContent, UserContent};
use rig_core::completion::{CompletionRequest, CompletionResponse, Message};

use crate::clients::backend::{Backend, ClientError};
use crate::clients::model::GremlinsModel;
use crate::clients::protocol::{CompactionBlock, CompactionResult};

/// The compaction strategy, picked at construction time.
#[derive(Clone)]
pub(crate) enum CompactionStrategy {
    /// Native server-side compaction (Anthropic on-demand API).
    Server,
    /// Local model-driven summarization.
    Local,
}

/// Compaction dispatch. Built once in `run.rs` and passed to
/// `StandardConversation::new`.
#[derive(Clone)]
pub(crate) struct Compaction {
    backend: Arc<dyn Backend>,
    model: String,
    instructions: Option<String>,
    strategy: CompactionStrategy,
    /// Model instance for local compaction, built once at construction.
    local_model: Option<GremlinsModel>,
}

impl Compaction {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        model: String,
        instructions: Option<String>,
        strategy: CompactionStrategy,
        local_model: Option<GremlinsModel>,
    ) -> Self {
        Self {
            backend,
            model,
            instructions,
            strategy,
            local_model,
        }
    }

    /// Run compaction on `to_compact`. Dispatches to the active strategy
    /// with no fallback chaining.
    ///
    /// - **Server**: calls `backend.compact()` (Anthropic on-demand API).
    ///   Returns `Ok(None)` when the API returns a null content block.
    /// - **Local**: runs a single-turn summarization completion on the
    ///   pre-built model with a single retry on transient failure. Returns
    ///   `Ok(None)` when the model can't be built or the summarization
    ///   call fails.
    ///
    /// `Err(...)` from either path propagates up to `render_turn`, which
    /// logs it and sets cooldown. There is no fallback.
    pub(crate) async fn compact(
        &self,
        to_compact: &[Message],
        kept: &[Message],
        system_prompt: Option<&str>,
    ) -> Result<Option<(Option<CompactionBlock>, Vec<Message>, CompactionResult)>, ClientError>
    {
        match self.strategy {
            CompactionStrategy::Server => {
                let result = self
                    .backend
                    .compact(
                        to_compact,
                        system_prompt,
                        self.instructions.as_deref(),
                        &self.model,
                    )
                    .await?;

                let server_block = match result {
                    Some(r) => r,
                    None => return Ok(None),
                };

                let signed: CompactionBlock = match server_block.block.clone() {
                    Some(b) => b,
                    // Server returned a compaction result with no block —
                    // unexpected but not fatal; return Ok(None) so the
                    // caller sets cooldown rather than panicking.
                    None => {
                        log::warn!("Server compaction result had no block; dropping");
                        return Ok(None);
                    }
                };
                let mut new_history = Vec::with_capacity(1 + kept.len());
                new_history.push(compaction_block_as_message(&signed));
                new_history.extend_from_slice(kept);
                Ok(Some((Some(signed), new_history, server_block)))
            }
            CompactionStrategy::Local => {
                let model = match &self.local_model {
                    Some(m) => m,
                    None => {
                        log::warn!("Compaction: local model not available");
                        return Ok(None);
                    }
                };

                let (summary_msg, input_tokens, output_tokens) = match compact_local_with_retry(
                    model,
                    to_compact,
                    self.instructions.as_deref(),
                    system_prompt,
                )
                .await
                {
                    Some(v) => v,
                    None => return Ok(None),
                };

                let mut new_history = Vec::with_capacity(1 + kept.len());
                new_history.push(summary_msg);
                new_history.extend_from_slice(kept);
                let result = CompactionResult {
                    block: None,
                    input_tokens,
                    output_tokens,
                };
                Ok(Some((None, new_history, result)))
            }
        }
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
enum CompactError {
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
async fn compact_local_raw(
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
async fn compact_local_with_retry(
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

/// Wrap a [`CompactionBlock`] as an assistant [`Message`] suitable for
/// inclusion in the message history.
///
/// The block is stored as an assistant message whose `additional_params`
/// carry the full compaction block under the `anthropic_content` key, so rig's
/// Anthropic wire code replaces the text placeholder with the raw `type:
/// "compaction"` content block during serialization. The `text` field holds
/// the unmodified signed content for signature preservation.
///
/// The `anthropic_content` key is the canonical rig-core mechanism
/// (`ANTHROPIC_RAW_CONTENT_KEY`) for round-tripping content blocks that have
/// no direct `AssistantContent` variant (server_tool_use, web_search_tool_result,
/// code_execution_tool_result, and compaction blocks).
pub(crate) fn compaction_block_as_message(block: &CompactionBlock) -> Message {
    use rig_core::completion::message::{AdditionalParams, AssistantContent, Text};

    // Build the full Anthropic wire content block. This is placed under
    // `anthropic_content` in additional_params so the Anthropic request
    // encoder substitutes it verbatim for the Text content block.
    let mut raw_block = serde_json::Map::new();
    raw_block.insert(
        "type".into(),
        serde_json::Value::String("compaction".into()),
    );
    raw_block.insert(
        "content".into(),
        serde_json::Value::String(block.content.clone()),
    );
    raw_block.insert(
        "signature".into(),
        serde_json::Value::String(block.signature.clone()),
    );
    if let Some(ref ec) = block.encrypted_content {
        raw_block.insert(
            "encrypted_content".into(),
            serde_json::Value::String(ec.clone()),
        );
    }
    if let Some(ref tc) = block.tool_changes {
        raw_block.insert(
            "tool_changes".into(),
            serde_json::to_value(tc).unwrap_or_default(),
        );
    }
    // cache_control enables prompt caching for the compaction block.
    let mut cache = serde_json::Map::new();
    cache.insert("type".into(), serde_json::Value::String("ephemeral".into()));
    raw_block.insert("cache_control".into(), serde_json::Value::Object(cache));

    let mut params = serde_json::Map::new();
    params.insert(
        "anthropic_content".into(),
        serde_json::Value::Object(raw_block),
    );
    let text = Text {
        // Store the unmodified signed content. The text field is a
        // placeholder — rig replaces it with the `anthropic_content`
        // block during serialization.
        text: block.content.clone(),
        additional_params: AdditionalParams::new(params),
    };
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(text)],
    }
}
