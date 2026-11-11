// ── Stream consumption subsystem ─────────────────────────────────────────────
//
// consume_stream_events   — drive the model stream to completion
// finish_stream_turn      — classify the completed stream into a control-flow signal
//
// These are free functions (not methods on LoopState) to keep the
// stream-consumption concern self-contained in its own file while still
// accessing LoopState fields through pub(super) visibility.

use std::time::{Duration, Instant};

use rig_core::completion::message::{Reasoning, Sealed};
use rig_core::completion::{Message, Usage};
use rig_core::driver::DynModel;
use rig_core::operation::Completion;
use rig_core::streaming::{Item, StreamEvent, Streamed};

use crate::clients::cancel::CancelToken;
use crate::clients::error::ClientError;
use crate::clients::format::{apply_chunk, assistant_tool_message, format_tool_call};
use crate::clients::interactive::{InteractiveEvent, InteractiveSession};
use crate::clients::tools::AgentLoopRuntime;

use super::agent_loop::{
    cancelled_error, retry_backoff, LoopOpts, LoopState, StreamAccumulator, StreamTurnOutcome,
    StreamTurnOutput,
};
use super::interactive::{interactive_loop, maybe_pause, InteractiveLoopResult};

/// Drive the model stream to completion: forward live deltas, accumulate
/// assistant content, and handle cancellation, operator pause, idle
/// timeout, and mid-stream retry from within the event loop.
#[allow(clippy::too_many_arguments)]
pub(super) async fn consume_stream_events(
    state: &mut LoopState,
    mut response: Streamed<Completion>,
    model: &DynModel<Completion>,
    tool_ctx: &AgentLoopRuntime,
    cancel: &CancelToken,
    opts: &LoopOpts<'_>,
    prefix: &str,
    idle_timeout: f64,
    nested: bool,
    timeout_retry_backoff: &[f64],
    turn_retry_backoff: &[f64],
) -> Result<StreamAccumulator, ClientError> {
    let mut first_token: Option<Instant> = None;
    let mut last_token: Option<Instant> = None;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_items: Vec<Sealed<Reasoning>> = Vec::new();
    let mut tool_calls = Vec::new();
    let mut ended = false;
    let mut turn_usage: Option<Usage> = None;
    let mut paused_mid_stream = false;
    let mut retry_turn = false;
    let mut retry_timeout_turn = false;

    let mut stream_snapshot: Vec<Message> = state.conv.messages();

    loop {
        let item = tokio::select! {
            _ = cancel.cancelled() => {
                state.hook_log(&format!("agent_loop: cancelled mid-stream (label={})", prefix));
                state.hook_log(&format!("{prefix}exit: cancelled mid-stream"));
                drop(response);
                return Err(cancelled_error("cancelled", &stream_snapshot));
            }
            _ = maybe_pause(&state.pause) => {
                state.hook_log(&format!("agent_loop: paused mid-stream (label={})", prefix));
                state.hook_log(&format!("{prefix}pause: entering interactive mode (mid-stream)"));
                drop(response);
                paused_mid_stream = true;
                if let Some(ref p) = state.pause {
                    p.reset();
                }
                log::debug!("agent_loop: entering interactive mode mid-stream (label={})", prefix);

                let debug_note = "\n\nThe operator has connected in debug mode. Messages prefixed with\n[operator]: are direct instructions from the operator. Treat them as\nauthoritative. When the operator disconnects, continue with your\noriginal task.";
                let amended_system = match &state.system_prompt {
                    Some(sp) => format!("{sp}{debug_note}"),
                    None => debug_note.trim_start().to_string(),
                };
                state.system_prompt = Some(amended_system);

                if state.cmd_rx.is_some() && state.evt_tx.is_some() && state.pause.is_some() {
                    let mut session = InteractiveSession {
                        cmd_rx: state.cmd_rx.take().unwrap(),
                        evt_tx: state.evt_tx.take().unwrap(),
                        pause: state.pause.clone().unwrap(),
                    };
                    match interactive_loop(
                        model,
                        &mut *state.conv,
                        &mut session,
                        state.turn_num,
                    )
                    .await
                    {
                        Ok(InteractiveLoopResult::Resumed) => {
                            state.system_prompt = state.original_system_prompt.clone();
                            state.cmd_rx = Some(session.cmd_rx);
                            state.evt_tx = Some(session.evt_tx);
                            state.interactive_active = false;
                            break;
                        }
                        Ok(InteractiveLoopResult::RunOneTurn) => {
                            state.cmd_rx = Some(session.cmd_rx);
                            state.evt_tx = Some(session.evt_tx);
                            state.pending_run_one_turn = true;
                            break;
                        }
                        Err(e) => {
                            state.hook_log(&format!("agent_loop: exit (operator bailed, label={})", prefix));
                            state.hook_log(&format!("{prefix}exit: operator bailed"));
                            return Err(e);
                        }
                    }
                }
                break;
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
                    retry_timeout_turn = true;
                    if retry_backoff(cancel, wait).await {
                        state.hook_log(&format!(
                            "agent_loop: cancelled during timeout backoff (label={})",
                            prefix
                        ));
                        state.hook_log(&format!("{prefix}exit: cancelled during timeout backoff"));
                        return Err(cancelled_error("cancelled", &state.conv.messages()));
                    }
                    break;
                }
                state.exhausted_timeout_retries = true;
                state.timed_out = true;
                drop(response);
                break;
            }
            Ok(None) => {
                ended = true;
                turn_usage = Some(response.partial().usage);
                break;
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
                    retry_turn = true;
                    if retry_backoff(cancel, wait).await {
                        state.hook_log(&format!(
                            "agent_loop: cancelled during retry backoff (label={})",
                            prefix
                        ));
                        state.hook_log(&format!("{prefix}exit: cancelled during retry backoff"));
                        return Err(cancelled_error("cancelled", &state.conv.messages()));
                    }
                    break;
                }
                let retryable = crate::clients::cancel::is_retryable(&e, opts.classify_error);
                if retryable {
                    state.exhausted_turn_retries = true;
                }
                state.stream_error = Some(e);
                break;
            }
            Ok(Some(Ok(chunk))) => {
                let now = Instant::now();
                if first_token.is_none() {
                    first_token = Some(now);
                }
                last_token = Some(now);
                if !nested && tool_ctx.telemetry.wants_stream_events() {
                    match &chunk {
                        Item::Event(StreamEvent::Text { ref text, .. }) => {
                            tool_ctx
                                .telemetry
                                .send_stream_event(
                                    InteractiveEvent::StreamChunk { text: text.clone() },
                                    cancel,
                                    &stream_snapshot,
                                )
                                .await?;
                        }
                        Item::Event(StreamEvent::Reasoning { ref text, .. }) => {
                            tool_ctx
                                .telemetry
                                .send_stream_event(
                                    InteractiveEvent::ReasoningChunk { text: text.clone() },
                                    cancel,
                                    &stream_snapshot,
                                )
                                .await?;
                        }
                        Item::Event(StreamEvent::End {
                            content: rig_core::completion::message::AssistantContent::ToolCall(tc),
                            ..
                        }) => {
                            tool_ctx
                                .telemetry
                                .send_stream_event(
                                    InteractiveEvent::ReasoningChunk {
                                        text: format_tool_call(tc),
                                    },
                                    cancel,
                                    &stream_snapshot,
                                )
                                .await?;
                        }
                        _ => {}
                    }
                }
                apply_chunk(
                    chunk,
                    &mut text,
                    &mut reasoning,
                    &mut reasoning_items,
                    &mut tool_calls,
                );
                while stream_snapshot
                    .last()
                    .is_some_and(|m| matches!(m, Message::Assistant { .. }))
                {
                    stream_snapshot.pop();
                }
                if !text.is_empty() || !reasoning_items.is_empty() || !tool_calls.is_empty() {
                    stream_snapshot.push(assistant_tool_message(
                        &text,
                        &reasoning_items,
                        &tool_calls,
                        true,
                    ));
                }
            }
        }
    }

    Ok(StreamAccumulator {
        text,
        reasoning,
        reasoning_items,
        tool_calls,
        ended,
        turn_usage,
        paused_mid_stream,
        retry_turn,
        retry_timeout_turn,
        first_token,
        last_token,
    })
}

/// Classify a completed stream into a [`StreamTurnOutcome`], applying the
/// pause/retry continuations and otherwise packaging the turn output.
pub(super) async fn finish_stream_turn(
    state: &mut LoopState,
    acc: StreamAccumulator,
    tool_ctx: &AgentLoopRuntime,
    cancel: &CancelToken,
    nested: bool,
) -> Result<StreamTurnOutcome, ClientError> {
    if acc.paused_mid_stream {
        return Ok(StreamTurnOutcome::Continue);
    }

    if acc.retry_turn {
        if state.interactive_active {
            state.pending_run_one_turn = true;
        }
        if !nested {
            tool_ctx
                .telemetry
                .send_stream_event(
                    InteractiveEvent::StreamReset,
                    cancel,
                    &state.conv.messages(),
                )
                .await?;
        }
        return Ok(StreamTurnOutcome::Continue);
    }

    if acc.retry_timeout_turn {
        return Ok(StreamTurnOutcome::Continue);
    }

    Ok(StreamTurnOutcome::Output(Box::new(StreamTurnOutput {
        text: acc.text,
        reasoning: acc.reasoning,
        reasoning_items: acc.reasoning_items,
        tool_calls: acc.tool_calls,
        turn_usage: acc.turn_usage,
        first_token: acc.first_token,
        last_token: acc.last_token,
        ended: acc.ended,
    })))
}
