// ── Turn-flow subsystem ───────────────────────────────────────────────────────
//
// handle_done_decision   — inspect tool calls for a Done signal
// handle_empty_turn      — act on a turn with no tool calls
// commit_turn_to_history — append assistant turn to conversation
// fold_turn_usage        — fold turn usage into running totals
// emit_turn_artifacts    — emit TurnComplete, raw transcript, metrics
//
// Free functions that accept &mut LoopState to keep the turn-resolution
// pipeline self-contained while still accessing LoopState fields through
// pub(super) visibility.

use std::time::Instant;

use rig_core::completion::message::{Origin, Reasoning, ToolCall};
use rig_core::completion::Usage;

use super::done_assess::{assess, missing_outputs, Assessment, DoneAssessment};
use super::done_helpers::done_user_message;
use crate::clients::cancel::CancelToken;
use crate::clients::conversation::Conversation;
use crate::clients::error::ClientError;
use crate::clients::format::{format_tool_call, trunc};
use crate::clients::interactive::DebugOutcome;
use crate::clients::protocol::{add_opt, CompletedRun, TurnStats, UsageStats};
use crate::clients::tools::{self, AgentLoopRuntime};
use gremlins_magic_bus::{emit_run_event_async, EventKind};

use super::agent_loop::{cancelled_error, DoneDecision, EmptyTurnAction, LoopState};

fn usage_stats(
    turns: usize,
    prompt: Option<u64>,
    completion: Option<u64>,
    cached: Option<u64>,
    cache_creation: Option<u64>,
    reasoning: Option<u64>,
) -> UsageStats {
    UsageStats {
        prompt_tokens: prompt,
        completion_tokens: completion,
        cached_input_tokens: cached,
        cache_creation_input_tokens: cache_creation,
        reasoning_tokens: reasoning,
        turns,
    }
}

fn completed_run(
    text: Option<String>,
    summary: Option<String>,
    events: &Option<Vec<serde_json::Value>>,
    usage: UsageStats,
    conversation: Option<Box<dyn Conversation>>,
) -> CompletedRun {
    CompletedRun {
        exit_code: 0,
        text_result: text,
        summary,
        events: events.clone(),
        cost_usd: None,
        token_usage: Some(usage),
        conversation,
    }
}

/// Inspect the assistant's tool calls for a Done signal and act on it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_done_decision(
    state: &mut LoopState,
    tool_ctx: &AgentLoopRuntime,
    cancel: &CancelToken,
    nested: bool,
    text: &str,
    reasoning_items: &[Reasoning],
    tool_calls: &[ToolCall],
    done: Option<&DoneAssessment>,
    captured: &mut Option<Vec<serde_json::Value>>,
    loop_start: Instant,
    prefix: &str,
) -> Result<DoneDecision, ClientError> {
    let done_call = tool_calls
        .iter()
        .find(|tc| tools::is_completion(tc.function.name.as_str()));

    match done_call {
        Some(_) if tool_calls.len() != 1 => {
            let mut results = Vec::with_capacity(tool_calls.len());
            for tc in tool_calls {
                let body = if tools::is_completion(tc.function.name.as_str()) {
                    "Error: Done must be called alone — do not combine Done with \
                     other tool calls in the same message. Re-issue your tool \
                     calls without Done, then call Done by itself when finished."
                } else {
                    "Skipped: this call was rejected because Done was combined \
                     with other tool calls in the same message. Re-issue without \
                     Done."
                };
                results.push(rig_core::completion::Message::tool_result(
                    tc.id.clone(),
                    tc.function.name.clone(),
                    body,
                ));
            }
            state.conv.done_rejected(
                text,
                reasoning_items,
                tool_calls,
                results,
                rig_core::completion::Message::user(
                    "Done was rejected because it was combined with other tool calls. \
                     Try again — either use tools without Done, or call Done exactly once by itself.",
                ),
            );
            return Ok(DoneDecision::RetryNeeded);
        }
        Some(done_tc) => {
            let done_summary = done_tc
                .function
                .arguments
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            state.hook_log(&format!("{prefix}tool: Done {}", trunc(&done_summary, 200)));
            if let (false, Some(done)) = (nested, done) {
                match assess(done, tool_ctx.cwd.as_deref(), &done_summary, cancel).await? {
                    Assessment::Accept => {}
                    Assessment::Retry(reminder) if state.reminder_budget > 0 => {
                        state.reminder_budget -= 1;
                        let rejected_ledger = format!(
                            "[turn {}]: Done {} (rejected)\n\n{}",
                            state.turn_num, done_summary, reminder
                        );
                        state.conv.done_rejected(
                            text,
                            reasoning_items,
                            std::slice::from_ref(done_tc),
                            vec![],
                            done_user_message(
                                &rejected_ledger,
                                done_tc.id.clone(),
                                done_tc.function.name.clone(),
                                &reminder,
                            ),
                        );
                        return Ok(DoneDecision::RetryNeeded);
                    }
                    Assessment::Retry(_) => {}
                }
            }

            let result_text = if text.trim().is_empty() && !done_summary.is_empty() {
                done_summary.clone()
            } else {
                text.to_string()
            };
            let usage = usage_stats(
                state.turn_num,
                state.total_prompt_tokens,
                state.total_completion_tokens,
                state.total_cached_tokens,
                state.total_cache_creation_tokens,
                state.total_reasoning_tokens,
            );
            if !nested {
                state.emit_final(prefix, state.turns, "");
                state.emit_summary(
                    prefix,
                    state.turn_num,
                    loop_start,
                    state.total_prompt_tokens,
                    state.total_completion_tokens,
                    state.total_cached_tokens,
                    state.total_cache_creation_tokens,
                    state.total_reasoning_tokens,
                );
            }
            if !nested {
                log::debug!("agent_loop: broadcasting Done (turn={})", state.turn_num);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(cancelled_error());
                    }
                    _ = emit_run_event_async(&state.message_prefix, EventKind::Done {
                        text: result_text.clone(),
                        summary: done_summary.clone(),
                        usage: Some(usage.clone()),
                    }) => {}
                }
                if state.interactive.is_attached() {
                    state.interactive.finish_attachment(DebugOutcome::Done {
                        text: result_text,
                        usage: usage.clone(),
                    });
                }
            }
            let artifact =
                state
                    .conv
                    .finalize(state.turn_num, text, reasoning_items, Some(done_tc));
            return Ok(DoneDecision::Accepted(Box::new(completed_run(
                Some(artifact.final_text),
                Some(done_summary),
                captured,
                usage,
                Some(state.conv.clone_box()),
            ))));
        }
        None => {}
    }
    Ok(DoneDecision::NotDone)
}

/// Handle a turn that produced no tool calls: continue (interactive / or
/// nudged / reminded) or return the exhausted run.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_empty_turn(
    state: &mut LoopState,
    nested: bool,
    text: &str,
    reasoning: &str,
    reasoning_items: &[Reasoning],
    done: Option<&DoneAssessment>,
    cwd: Option<&std::path::Path>,
    input_tokens: Option<u64>,
    loop_start: Instant,
    raw: &mut Option<std::fs::File>,
    captured: &mut Option<Vec<serde_json::Value>>,
    prefix: &str,
    cancel: &CancelToken,
) -> Result<EmptyTurnAction, ClientError> {
    if state.interactive.is_attached() {
        return Ok(EmptyTurnAction::ContinueLoop);
    }

    if text.is_empty() && !reasoning.is_empty() && !state.single_turn {
        log::debug!("reasoning-only turn: turn={} — continuing", state.turn_num);
        state
            .conv
            .record_turn("", reasoning_items, &[], input_tokens, None);
        state.conv.set_next_prompt("Continue.");
        return Ok(EmptyTurnAction::ContinueLoop);
    }

    log::warn!(
        "empty turn: turn={} text_empty={} reasoning_empty={} budget={}",
        state.turn_num,
        text.is_empty(),
        reasoning.is_empty(),
        state.completion_nudge_budget,
    );

    if !nested && state.reminder_budget > 0 && !text.is_empty() {
        let reminder = match done {
            Some(done) => {
                let signal = CancelToken::new();
                super::done_assess::cancellable(
                    cancel,
                    &signal,
                    || super::done_assess::grace_for(done, false),
                    missing_outputs(done, cwd, "", &signal),
                )
                .await?
            }
            None => None,
        };
        if let Some(reminder) = reminder {
            state.reminder_budget -= 1;
            state.hook_log(&format!(
                "reminder: missing artifact(s) — nudging agent (remaining_budget={})",
                state.reminder_budget,
            ));
            state
                .conv
                .record_turn(text, reasoning_items, &[], input_tokens, None);
            write_raw(
                raw,
                &serde_json::json!({"type": "reminder", "message": reminder}),
            );
            if let Some(evts) = captured.as_mut() {
                evts.push(serde_json::json!({"type": "reminder", "message": reminder}));
            }
            state.conv.set_next_prompt(&reminder);
            return Ok(EmptyTurnAction::ContinueLoop);
        }
    }

    if !text.is_empty() && state.completion_nudge_budget > 0 {
        state.completion_nudge_budget -= 1;
        state.hook_log(&format!(
            "empty turn — nudging agent (remaining_budget={})",
            state.completion_nudge_budget,
        ));
        let nudge_msg = "You produced text but no tool calls. If the request is unfinished, \
             continue the work now. Only call the Done tool when the request is fulfilled.";
        write_raw(
            raw,
            &serde_json::json!({"type": "reminder", "message": nudge_msg}),
        );
        if let Some(evts) = captured.as_mut() {
            evts.push(serde_json::json!({"type": "reminder", "message": nudge_msg}));
        }
        state
            .conv
            .record_turn(text, reasoning_items, &[], input_tokens, None);
        state.conv.set_next_prompt(nudge_msg);
        return Ok(EmptyTurnAction::ContinueLoop);
    }

    log::warn!(
        "empty-turn exhausted: turn={} text_empty={} reasoning_empty={} budget={}",
        state.turn_num,
        text.is_empty(),
        reasoning.is_empty(),
        state.completion_nudge_budget,
    );
    let usage = usage_stats(
        state.turn_num,
        state.total_prompt_tokens,
        state.total_completion_tokens,
        state.total_cached_tokens,
        state.total_cache_creation_tokens,
        state.total_reasoning_tokens,
    );
    if !nested {
        state.emit_final(prefix, state.turns, " (exhausted)");
        state.emit_summary(
            prefix,
            state.turn_num,
            loop_start,
            state.total_prompt_tokens,
            state.total_completion_tokens,
            state.total_cached_tokens,
            state.total_cache_creation_tokens,
            state.total_reasoning_tokens,
        );
    }
    let artifact = state
        .conv
        .finalize(state.turn_num, text, reasoning_items, None);
    Ok(EmptyTurnAction::ReturnRun(Box::new(completed_run(
        Some(artifact.final_text),
        None,
        captured,
        usage,
        Some(state.conv.clone_box()),
    ))))
}

/// Append the current assistant turn to the conversation history.
pub(super) fn commit_turn_to_history(
    state: &mut LoopState,
    text: &str,
    reasoning_items: &[Reasoning],
    tool_calls: &[ToolCall],
    input_tokens: Option<u64>,
    origin: Option<Origin>,
) {
    state
        .conv
        .record_turn(text, reasoning_items, tool_calls, input_tokens, origin);
}

/// Fold this turn's provider usage into the running totals and return the
/// cumulative [`UsageStats`].
pub(super) fn fold_turn_usage(state: &mut LoopState, turn_usage: Option<&Usage>) -> UsageStats {
    if let Some(u) = turn_usage {
        state.total_prompt_tokens = add_opt(state.total_prompt_tokens, u.input_tokens);
        state.total_completion_tokens = add_opt(state.total_completion_tokens, u.output_tokens);
        state.total_cached_tokens = add_opt(state.total_cached_tokens, u.cached_input_tokens);
        state.total_cache_creation_tokens = add_opt(
            state.total_cache_creation_tokens,
            u.cache_creation_input_tokens,
        );
        state.total_reasoning_tokens = add_opt(state.total_reasoning_tokens, u.reasoning_tokens);
    }
    usage_stats(
        state.turn_num + 1,
        state.total_prompt_tokens,
        state.total_completion_tokens,
        state.total_cached_tokens,
        state.total_cache_creation_tokens,
        state.total_reasoning_tokens,
    )
}

/// Emit TurnComplete plus the raw/captured transcript entries and turn
/// metrics for a completed turn.
#[allow(clippy::too_many_arguments)]
pub(super) async fn emit_turn_artifacts(
    state: &mut LoopState,
    _tool_ctx: &AgentLoopRuntime,
    cancel: &CancelToken,
    nested: bool,
    text: &str,
    reasoning: &str,
    tool_calls: &[ToolCall],
    turn_stats: Option<TurnStats>,
    totals: &UsageStats,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    turn_start: Instant,
    turn_usage: Option<&Usage>,
    raw: &mut Option<std::fs::File>,
    captured: &mut Option<Vec<serde_json::Value>>,
    prefix: &str,
    wants_events: bool,
    parent_id: &str,
    parent_call: &str,
    kind: &str,
    chain: &str,
) -> Result<(), ClientError> {
    if wants_events {
        if nested {
            // A sub-agent turn summary updates the parent's Task slot with
            // this turn's stats instead of broadcasting a top-level TurnComplete.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(cancelled_error());
                }
                _ = emit_run_event_async(&state.message_prefix, EventKind::TaskProgress {
                    parent_id: parent_id.to_string(),
                    chain: chain.to_string(),
                    kind: kind.to_string(),
                    call: parent_call.to_string(),
                    reasoning: String::new(),
                    text: String::new(),
                    stats: turn_stats.clone(),
                    totals: Some(totals.clone()),
                }) => {}
            }
        } else {
            log::debug!(
                "agent_loop: broadcasting TurnComplete (turn={})",
                state.turn_num
            );
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(cancelled_error());
                }
                _ = emit_run_event_async(&state.message_prefix, EventKind::TurnComplete {
                    turn: state.turn_num,
                    text: text.to_string(),
                    tool_calls: tool_calls.iter().map(format_tool_call).collect(),
                    stats: turn_stats.clone(),
                    totals: Some(totals.clone()),
                }) => {}
            }
        }
    }

    if !reasoning.is_empty() {
        let msg = format!("think: {}", trunc(reasoning, 200));
        state.hook_log(&format!("{prefix}{msg}"));
    }
    if !text.is_empty() {
        let msg = format!("text: {}", trunc(text, 200));
        state.hook_log(&format!("{prefix}{msg}"));
    }
    if !text.is_empty() {
        let evt = assistant_text_event(text);
        write_raw(raw, &evt);
        if !nested {
            if let Some(evts) = captured.as_mut() {
                evts.push(evt);
            }
        }
    }
    state.conv.record_prose(text);

    state.emit_turn_metrics(
        prefix,
        state.turn_num,
        first_token,
        last_token,
        turn_start,
        reasoning,
        text,
        tool_calls,
        turn_usage,
    );
    Ok(())
}

fn assistant_text_event(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": text}]}
    })
}

fn write_raw(raw: &mut Option<std::fs::File>, evt: &serde_json::Value) {
    use std::io::Write;
    if let Some(f) = raw {
        if let Ok(line) = serde_json::to_string(evt) {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}
