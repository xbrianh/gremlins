//! Consume one model stream and report its outcome to the agent loop.

use std::time::{Duration, Instant};

use crate::clients::request_usage::UsageStream;
use rig_core::completion::message::{Origin, Reasoning};
use rig_core::streaming::{Item, StreamEvent};

use crate::clients::cancel::CancelToken;
use crate::clients::error::ClientError;
use crate::clients::format::{apply_chunk, format_tool_call};
use gremlins_magic_bus::{emit_corr_event_async, EventKind};

use super::agent_loop::{retry_backoff, LoopOpts, LoopState, StreamTurnOutcome, StreamTurnOutput};

/// Drive the model stream to completion: forward live deltas, accumulate
/// assistant content, and handle cancellation, operator pause, idle
/// timeout, and mid-stream retry from within the event loop.
#[allow(clippy::too_many_arguments)]
pub(super) async fn consume_stream_events(
    state: &mut LoopState,
    mut response: UsageStream,
    cancel: &CancelToken,
    opts: &LoopOpts<'_>,
    prefix: &str,
    idle_timeout: f64,
    nested: bool,
    wants_events: bool,
    parent_id: &str,
    parent_call: &str,
    kind: &str,
    chain: &str,
    timeout_retry_backoff: &[f64],
    turn_retry_backoff: &[f64],
) -> Result<StreamTurnOutcome, ClientError> {
    let mut first_token: Option<Instant> = None;
    let mut last_token: Option<Instant> = None;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_items: Vec<Reasoning> = Vec::new();
    let mut tool_calls = Vec::new();
    let origin: Option<Origin>;

    let usage = loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                state.hook_log(&format!("agent_loop: cancelled mid-stream (label={})", prefix));
                state.hook_log(&format!("{prefix}exit: cancelled mid-stream"));
                drop(response);
                return Err(ClientError::Cancelled);
            }
            interrupted = state.interactive.stream_interrupted() => {
                if !interrupted {
                    drop(response);
                    return Err(ClientError::Cancelled);
                }
                // Inbox input interrupts only model streaming, never a tool
                // batch. Preserve partial prose and sealed reasoning exactly
                // as for the legacy pause boundary.
                return Ok(StreamTurnOutcome::Interrupted {
                    text: response.partial().text(),
                    reasoning_items,
                });
            }
            timed = tokio::time::timeout(
                Duration::from_secs_f64(idle_timeout),
                futures::StreamExt::next(&mut response),
            ) => timed,
        };
        match item {
            Err(_) => {
                if state.timeout_retries < timeout_retry_backoff.len() {
                    let wait = timeout_retry_backoff[state.timeout_retries];
                    state.timeout_retries += 1;
                    state.hook_log(&format!(
                        "agent_loop: idle timeout on turn {} (attempt {}), backing off {wait}s",
                        state.turn_num, state.timeout_retries
                    ));
                    state.hook_log(&format!("{prefix}retry: idle timeout, backing off {wait}s"));
                    drop(response);
                    if retry_backoff(cancel, wait).await {
                        state.hook_log(&format!(
                            "agent_loop: cancelled during timeout backoff (label={})",
                            prefix
                        ));
                        state.hook_log(&format!("{prefix}exit: cancelled during timeout backoff"));
                        return Err(ClientError::Cancelled);
                    }
                    return Ok(StreamTurnOutcome::RetryTimeout);
                }
                state.exhausted_timeout_retries = true;
                state.timed_out = true;
                return Ok(StreamTurnOutcome::Break);
            }
            Ok(None) => {
                origin = Some(response.partial().origin);
                break response.partial().usage;
            }
            Ok(Some(Err(e))) => {
                if state.turn_retries < turn_retry_backoff.len()
                    && crate::clients::cancel::is_retryable(&e, opts.classify_error)
                {
                    let wait = turn_retry_backoff[state.turn_retries];
                    state.turn_retries += 1;
                    state.hook_log(&format!(
                        "agent_loop: retryable mid-stream error on turn {} (attempt {}), backing off {wait}s: {e}", state.turn_num, state.turn_retries
                    ));
                    state.hook_log(&format!(
                        "{prefix}retry: transient error, backing off {wait}s"
                    ));
                    drop(response);
                    if retry_backoff(cancel, wait).await {
                        state.hook_log(&format!(
                            "agent_loop: cancelled during retry backoff (label={})",
                            prefix
                        ));
                        state.hook_log(&format!("{prefix}exit: cancelled during retry backoff"));
                        return Err(ClientError::Cancelled);
                    }
                    if !nested {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(ClientError::Cancelled),
                            _ = emit_corr_event_async(&state.corr_id, EventKind::StreamReset) => {}
                        }
                    }
                    return Ok(StreamTurnOutcome::RetryModelError);
                }
                let retryable = crate::clients::cancel::is_retryable(&e, opts.classify_error);
                if retryable {
                    state.exhausted_turn_retries = true;
                }
                state.stream_error = Some(e);
                return Ok(StreamTurnOutcome::Break);
            }
            Ok(Some(Ok(chunk))) => {
                let now = Instant::now();
                if first_token.is_none() {
                    first_token = Some(now);
                }
                last_token = Some(now);
                if wants_events {
                    if nested {
                        // Emit TaskProgress for text/reasoning deltas so a
                        // sub-agent's live output reaches the parent TUI slot
                        // identified by `parent_id`.
                        match &chunk {
                            Item::Event(StreamEvent::Text { ref text, .. }) => {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        return Err(ClientError::Cancelled);
                                    }
                                    _ = emit_corr_event_async(&state.corr_id, EventKind::TaskProgress {
                                        parent_id: parent_id.to_string(),
                                        chain: chain.to_string(),
                                        kind: kind.to_string(),
                                        call: parent_call.to_string(),
                                        reasoning: String::new(),
                                        text: text.clone(),
                                        stats: None,
                                        totals: None,
                                    }) => {}
                                }
                            }
                            Item::Event(StreamEvent::Reasoning { ref text, .. }) => {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        return Err(ClientError::Cancelled);
                                    }
                                    _ = emit_corr_event_async(&state.corr_id, EventKind::TaskProgress {
                                        parent_id: parent_id.to_string(),
                                        chain: chain.to_string(),
                                        kind: kind.to_string(),
                                        call: parent_call.to_string(),
                                        reasoning: text.clone(),
                                        text: String::new(),
                                        stats: None,
                                        totals: None,
                                    }) => {}
                                }
                            }
                            Item::Event(StreamEvent::End {
                                content:
                                    rig_core::completion::message::AssistantContent::ToolCall(tc),
                                ..
                            }) => {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        return Err(ClientError::Cancelled);
                                    }
                                    _ = emit_corr_event_async(&state.corr_id, EventKind::TaskProgress {
                                        parent_id: parent_id.to_string(),
                                        chain: chain.to_string(),
                                        kind: kind.to_string(),
                                        call: parent_call.to_string(),
                                        reasoning: format_tool_call(tc),
                                        text: String::new(),
                                        stats: None,
                                        totals: None,
                                    }) => {}
                                }
                            }
                            _ => {}
                        }
                    } else {
                        match &chunk {
                            Item::Event(StreamEvent::Text { ref text, .. }) => {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        return Err(ClientError::Cancelled);
                                    }
                                    _ = emit_corr_event_async(&state.corr_id, EventKind::StreamChunk {
                                        text: text.clone(),
                                    }) => {}
                                }
                            }
                            Item::Event(StreamEvent::Reasoning { ref text, .. }) => {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        return Err(ClientError::Cancelled);
                                    }
                                    _ = emit_corr_event_async(&state.corr_id, EventKind::ReasoningChunk {
                                        text: text.clone(),
                                    }) => {}
                                }
                            }
                            Item::Event(StreamEvent::End {
                                content:
                                    rig_core::completion::message::AssistantContent::ToolCall(tc),
                                ..
                            }) => {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        return Err(ClientError::Cancelled);
                                    }
                                    _ = emit_corr_event_async(&state.corr_id, EventKind::ReasoningChunk {
                                        text: format_tool_call(tc),
                                    }) => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
                apply_chunk(
                    chunk,
                    &mut text,
                    &mut reasoning,
                    &mut reasoning_items,
                    &mut tool_calls,
                );
            }
        }
    };

    Ok(StreamTurnOutcome::Output(Box::new(StreamTurnOutput {
        text,
        reasoning,
        reasoning_items,
        tool_calls,
        turn_usage: Some(usage),
        first_token,
        last_token,
        origin,
    })))
}
