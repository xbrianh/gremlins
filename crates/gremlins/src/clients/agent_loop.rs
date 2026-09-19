use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::StreamExt;
use rig_core::completion::message::{AssistantContent, ToolCall};
use rig_core::completion::{
    CompletionError, CompletionModel, GetTokenUsage, Message, ToolDefinition, Usage,
};
use rig_core::streaming::StreamedAssistantContent;
use rig_core::OneOrMany;
use tokio::sync::Notify;

use super::backend::{ClientError, RunParams};
use super::protocol::{CompletedRun, UsageStats};
use super::stream;
use super::tools::{self, ToolContext};

pub(crate) type ErrorClassifier = fn(CompletionError) -> ClientError;

pub(crate) fn default_classify(err: CompletionError) -> ClientError {
    if let Some(status) = err.provider_response_status() {
        let code = status.as_u16();
        // Only retry 5xx and 429.
        if (500..600).contains(&code) || code == 429 {
            return ClientError::ApiServerError {
                message: err.to_string(),
            };
        }
        return ClientError::Runtime {
            message: err.to_string(),
        };
    }
    // No HTTP status — mid-stream SSE error (e.g. X.AI response.failed).
    // Retry it; log at WARNING so we can spot provider patterns later.
    log::warn!("retrying provider error (no HTTP status): {}", err);
    ClientError::ApiServerError {
        message: err.to_string(),
    }
}

pub(crate) const DEFAULT_TEMPERATURE: f64 = 0.4;

pub(crate) struct CancelToken {
    flag: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub(crate) fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    pub(crate) async fn cancelled(&self) {
        let notified = self.notify.notified();
        if self.flag.load(Ordering::Relaxed) {
            return;
        }
        notified.await;
    }
}

#[derive(Clone)]
pub(crate) struct RunContext {
    pub(crate) params: RunParams,
    pub(crate) prefix: String,
    pub(crate) idle_timeout: f64,
    pub(crate) expected_artifact_paths: Vec<PathBuf>,
    pub(crate) reminder_budget: usize,
    pub(crate) completion_nudge_budget: usize,
}

pub(crate) struct LoopOpts<'a> {
    pub(crate) extra: Option<serde_json::Value>,
    pub(crate) tool_filter: Option<&'a [String]>,
    pub(crate) classify_error: Option<ErrorClassifier>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: &M,
    prompt: &str,
    ctx: &RunContext,
    cancel: Arc<CancelToken>,
    opts: LoopOpts<'_>,
    task_model_selector: Option<super::task::TaskModelSelector<M>>,
) -> Result<CompletedRun, ClientError> {
    let cwd = ctx.params.cwd.clone();
    let extra_env = ctx.params.extra_env.clone();
    let prefix = ctx.prefix.clone();
    let raw_path = ctx.params.raw_path.clone();
    let capture_events = ctx.params.capture_events;
    let idle_timeout = ctx.idle_timeout;
    let model_name = ctx
        .params
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "model".into());

    let cwd_display = cwd
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "?".into());
    let reasoning_effort = opts
        .extra
        .as_ref()
        .and_then(|v| v.get("reasoning"))
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str());
    stream::emit_init(&prefix, &model_name, &cwd_display, reasoning_effort);
    stream::flush();

    if cwd.is_none() {
        eprintln!("{prefix}warning: no cwd set for worktree enforcement");
    }

    let mut raw = raw_path
        .as_ref()
        .map(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
        })
        .transpose()
        .map_err(|e| ClientError::Runtime {
            message: format!("failed to open raw_path: {e}"),
        })?;
    let mut captured: Option<Vec<serde_json::Value>> = if capture_events {
        Some(Vec::new())
    } else {
        None
    };

    let worktree = tools::worktree_root(cwd.as_deref());
    let audit_log = raw_path.as_ref().map(|p| tools::audit_log_path(p));
    let max_turns = crate::config::max_agent_turns();
    let mut allowed_roots = vec![worktree];
    if let Some(ref artifact_dir) = ctx.params.artifact_dir {
        allowed_roots.push(artifact_dir.clone());
    }
    if let Some(scratch) = tools::scratch_root() {
        allowed_roots.push(scratch);
    }
    let mut tool_ctx = ToolContext {
        cwd: cwd.clone(),
        extra_env,
        allowed_roots,
        audit_log,
        allowed_tools: opts.tool_filter.map(|s| s.to_vec()),
        task_fn: None,
        audit_lock: Some(Arc::new(std::sync::Mutex::new(()))),
    };
    let tool_defs = tools::tool_definitions(opts.tool_filter);

    // Wire up the Task runner before entering the turn loop.
    let runner = super::task::make_task_runner(
        model.clone(),
        task_model_selector,
        opts.tool_filter.map(|f| f.to_vec()),
        cancel.clone(),
        tool_ctx.clone(),
        prefix.clone(),
        idle_timeout,
        max_turns,
        ctx.completion_nudge_budget,
    );
    tool_ctx.task_fn = Some(runner);

    run_agent_loop_core(
        model,
        prompt,
        ctx.params.system_prompt.clone(),
        &tool_ctx,
        &tool_defs,
        &cancel,
        &opts,
        &prefix,
        max_turns,
        idle_timeout,
        &mut raw,
        &mut captured,
        false,
        &ctx.expected_artifact_paths,
        ctx.reminder_budget,
        ctx.completion_nudge_budget,
    )
    .await
}

/// Nested agent loop — same logic as the parent loop but without raw transcript
/// writes, captured-event pushes, or final/summary stream emissions.
/// Stream events (think, text, tool, result, turn metrics) are emitted.
/// Used by the Task tool.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop_nested<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: &M,
    prompt: &str,
    system_prompt: Option<String>,
    tool_ctx: &ToolContext,
    cancel: &CancelToken,
    tool_filter: Option<&[String]>,
    prefix: &str,
    idle_timeout: f64,
    max_turns: usize,
    completion_nudge_budget: usize,
) -> Result<CompletedRun, ClientError> {
    eprintln!(
        "{} {}task: begin (max_turns={})",
        stream::ts_internal(),
        prefix,
        max_turns
    );
    let opts = LoopOpts {
        extra: None,
        tool_filter,
        classify_error: None,
    };
    let tool_defs = tools::tool_definitions(tool_filter);
    let mut raw: Option<std::fs::File> = None;
    let mut captured: Option<Vec<serde_json::Value>> = None;
    let result = run_agent_loop_core(
        model,
        prompt,
        system_prompt,
        tool_ctx,
        &tool_defs,
        cancel,
        &opts,
        prefix,
        max_turns,
        idle_timeout,
        &mut raw,
        &mut captured,
        true,
        &[],
        0,
        completion_nudge_budget,
    )
    .await;
    eprintln!("{} {}task: end", stream::ts_internal(), prefix);
    result
}

// ── helpers for completion-decision bookkeeping ───────────────────────────

fn usage_stats(
    turns: usize,
    prompt: u64,
    completion: u64,
    cached: u64,
    cache_creation: u64,
    reasoning: u64,
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
    events: &Option<Vec<serde_json::Value>>,
    usage: UsageStats,
) -> CompletedRun {
    CompletedRun {
        exit_code: 0,
        text_result: text,
        events: events.clone(),
        cost_usd: None,
        token_usage: Some(usage),
    }
}

fn missing_artifacts(expected: &[PathBuf]) -> Vec<&PathBuf> {
    expected
        .iter()
        .filter(|p| !p.exists() || p.metadata().map(|m| m.len()).unwrap_or(0) == 0)
        .collect()
}

fn artifact_reminder(missing: &[&PathBuf]) -> String {
    let paths: Vec<String> = missing
        .iter()
        .map(|p| format!("  - {}", p.display()))
        .collect();
    format!(
        "The following expected output file(s) were not written:\n{}\n\
         Please write each file using the Write tool now. Do not explain \
         \u{2014} just write the files.",
        paths.join("\n")
    )
}

// ── agent loop ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_agent_loop_core<M: CompletionModel>(
    model: &M,
    prompt: &str,
    system_prompt: Option<String>,
    tool_ctx: &ToolContext,
    tool_defs: &[ToolDefinition],
    cancel: &CancelToken,
    opts: &LoopOpts<'_>,
    prefix: &str,
    max_turns: usize,
    idle_timeout: f64,
    raw: &mut Option<std::fs::File>,
    captured: &mut Option<Vec<serde_json::Value>>,
    nested: bool,
    expected_artifact_paths: &[PathBuf],
    mut reminder_budget: usize,
    mut completion_nudge_budget: usize,
) -> Result<CompletedRun, ClientError> {
    let mut history: Vec<Message> = Vec::new();
    let mut next_prompt = Message::user(prompt.to_string());
    let mut turns: usize = 0;
    let mut turn_num: usize = 0;
    let mut final_text = String::new();
    let mut timed_out = false;
    let mut stream_error: Option<CompletionError> = None;
    let loop_start = Instant::now();

    // Accumulated token totals (summed across turns)
    let mut total_prompt_tokens: u64 = 0;
    let mut total_completion_tokens: u64 = 0;
    let mut total_cached_tokens: u64 = 0;
    let mut total_cache_creation_tokens: u64 = 0;
    let mut total_reasoning_tokens: u64 = 0;

    struct Job {
        id: String,
        call_id: Option<String>,
        name: String,
        args: String,
        key: String,
        over_cap: bool,
    }

    for _ in 0..max_turns {
        if cancel.is_cancelled() {
            log::debug!("agent_loop: cancelled before turn (label={})", prefix);
            return Err(ClientError::Runtime {
                message: "cancelled".into(),
            });
        }

        // Snapshot before request construction so TTFT includes connection /
        // queue latency, not just server-side time-to-first-token.
        let turn_start = Instant::now();

        let mut builder = model
            .completion_request(next_prompt.clone())
            .messages(history.clone())
            .tools(tool_defs.to_vec())
            .temperature(DEFAULT_TEMPERATURE);
        if let Some(ref sys) = system_prompt {
            builder = builder.preamble(sys.clone());
        }
        if let Some(params) = opts.extra.clone() {
            builder = builder.additional_params(params);
        }

        let mut response = match builder.stream().await {
            Ok(s) => s,
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };

        let mut first_token: Option<Instant> = None;
        let mut last_token: Option<Instant> = None;

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut ended = false;
        let mut turn_usage: Option<Usage> = None;

        loop {
            let item = tokio::select! {
                _ = cancel.cancelled() => {
                    log::debug!("agent_loop: cancelled mid-stream (label={})", prefix);
                    response.cancel();
                    return Err(ClientError::Runtime {
                        message: "cancelled".into(),
                    });
                }
                timed = tokio::time::timeout(
                    Duration::from_secs_f64(idle_timeout),
                    response.next(),
                ) => timed,
            };
            match item {
                Err(_) => {
                    timed_out = true;
                    response.cancel();
                    break;
                }
                Ok(None) => {
                    ended = true;
                    break;
                }
                Ok(Some(Err(e))) => {
                    stream_error = Some(e);
                    break;
                }
                Ok(Some(Ok(chunk))) => {
                    let now = Instant::now();
                    if first_token.is_none() {
                        first_token = Some(now);
                    }
                    last_token = Some(now);
                    apply_chunk(
                        chunk,
                        &mut text,
                        &mut reasoning,
                        &mut tool_calls,
                        &mut turn_usage,
                    );
                }
            }
        }

        if timed_out || stream_error.is_some() {
            log::debug!(
                target: "_gremlins_core.clients.agent_loop",
                "stream ended: timed_out={timed_out} stream_error={stream_error:?} turn={turn_num}",
            );
            break;
        }
        if !ended && tool_calls.is_empty() && text.is_empty() {
            log::debug!(
                target: "_gremlins_core.clients.agent_loop",
                "stream ended: not-ended empty-turn turn={turn_num}",
            );
            break;
        }

        log::debug!(
            target: "_gremlins_core.clients.agent_loop",
            "turn complete: turn={turn_num} text_len={} reasoning_len={} tool_calls={} ended={ended}",
            text.len(),
            reasoning.len(),
            tool_calls.len(),
        );

        if !reasoning.is_empty() {
            stream::emit_think(prefix, &reasoning);
        }
        if !text.is_empty() {
            stream::emit_text(prefix, &text);
        }
        if !nested {
            if !text.is_empty() {
                write_raw(raw, &assistant_text_event(&text));
                if let Some(evts) = captured.as_mut() {
                    evts.push(assistant_text_event(&text));
                }
                final_text = text.clone();
            }
        } else {
            final_text = text.clone();
        }

        stream::emit_turn_metrics(
            prefix,
            turn_num,
            first_token,
            last_token,
            turn_start,
            &reasoning,
            &text,
            &tool_calls,
            turn_usage.as_ref(),
        );

        if let Some(ref u) = turn_usage {
            total_prompt_tokens += u.input_tokens;
            total_completion_tokens += u.output_tokens;
            total_cached_tokens += u.cached_input_tokens;
            total_cache_creation_tokens += u.cache_creation_input_tokens;
            total_reasoning_tokens += u.reasoning_tokens;
        }

        turn_num += 1;

        // ---- completion decision ----
        // Solo Done is the only valid completion signal. Mixed Done + other
        // tool calls is incoherent and rejected wholesale so the model retries
        // cleanly.
        let has_non_done = tool_calls.iter().any(|tc| tc.function.name != "Done");
        let done_call = tool_calls.iter().find(|tc| tc.function.name == "Done");

        match (done_call, has_non_done) {
            (Some(_done_tc), true) => {
                // Mixed — produce a tool_result for every call so providers
                // see a valid round of history.
                history.push(next_prompt);
                history.push(assistant_tool_message(&text, &tool_calls));
                for tc in &tool_calls {
                    let body = if tc.function.name == "Done" {
                        "Error: Done must be called alone — do not combine Done with \
                         other tool calls in the same message. Re-issue your tool \
                         calls without Done, then call Done by itself when finished."
                    } else {
                        "Skipped: this call was rejected because Done was combined \
                         with other tool calls in the same message. Re-issue without \
                         Done."
                    };
                    history.push(Message::tool_result_with_call_id(
                        tc.id.clone(),
                        tc.call_id.clone(),
                        body,
                    ));
                }
                next_prompt = Message::user(
                    "Done was rejected because it was combined with other tool calls. \
                     Try again — this time either use tools, or call Done. Never both.",
                );
                continue;
            }
            (Some(done_tc), false) => {
                // Solo Done — validate expected artifacts before accepting.
                if !nested && !expected_artifact_paths.is_empty() {
                    let missing = missing_artifacts(expected_artifact_paths);
                    if !missing.is_empty() {
                        if reminder_budget > 0 {
                            reminder_budget -= 1;
                            let reminder = format!(
                                "Done rejected: {}\nWrite each file using the Write \
                                 tool, then call Done.",
                                artifact_reminder(&missing),
                            );
                            history.push(next_prompt);
                            history
                                .push(assistant_tool_message(&text, std::slice::from_ref(done_tc)));
                            history.push(Message::tool_result_with_call_id(
                                done_tc.id.clone(),
                                done_tc.call_id.clone(),
                                reminder.clone(),
                            ));
                            next_prompt = Message::user(reminder);
                            continue;
                        }
                        // Budget exhausted — accept anyway. Python
                        // verify_produced catches missing files.
                        history.push(next_prompt);
                        history.push(assistant_tool_message(&text, std::slice::from_ref(done_tc)));
                    }
                }

                // Accept completion.
                let summary = done_tc
                    .function
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let result_text = if text.trim().is_empty() && !summary.is_empty() {
                    summary
                } else {
                    text.clone()
                };
                let usage = usage_stats(
                    turn_num,
                    total_prompt_tokens,
                    total_completion_tokens,
                    total_cached_tokens,
                    total_cache_creation_tokens,
                    total_reasoning_tokens,
                );
                if !nested {
                    stream::flush();
                    emit_final(prefix, turns, "");
                    stream::emit_summary(
                        prefix,
                        turn_num,
                        loop_start,
                        total_prompt_tokens,
                        total_completion_tokens,
                        total_cached_tokens,
                        total_cache_creation_tokens,
                        total_reasoning_tokens,
                    );
                }
                return Ok(completed_run(Some(result_text), captured, usage));
            }
            _ => {}
        }

        // Done handled — filter it out before execution.
        let tool_calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|tc| tc.function.name != "Done")
            .collect();

        if tool_calls.is_empty() {
            // Reasoning-only turn — the model is thinking. Just loop.
            if text.is_empty() && !reasoning.is_empty() {
                log::debug!(
                    target: "_gremlins_core.clients.agent_loop",
                    "reasoning-only turn: turn={turn_num} — continuing",
                );
                history.push(next_prompt);
                next_prompt = Message::user("Continue.");
                continue;
            }

            log::warn!(
                target: "_gremlins_core.clients.agent_loop",
                "empty turn: turn={turn_num} text_empty={} reasoning_empty={} budget={completion_nudge_budget}",
                text.is_empty(),
                reasoning.is_empty(),
            );

            // Missing-artifact reminder (only when the model produced text).
            if !nested && reminder_budget > 0 && !text.is_empty() {
                let missing = missing_artifacts(expected_artifact_paths);
                if !missing.is_empty() {
                    reminder_budget -= 1;
                    log::info!(
                        target: "_gremlins_core.clients.agent_loop",
                        "reminder: {} missing artifact(s) — nudging agent (remaining_budget={})",
                        missing.len(),
                        reminder_budget,
                    );
                    history.push(next_prompt);
                    history.push(assistant_tool_message(&text, &[]));
                    let reminder = artifact_reminder(&missing);
                    write_raw(
                        raw,
                        &serde_json::json!({"type": "reminder", "message": reminder}),
                    );
                    if let Some(evts) = captured.as_mut() {
                        evts.push(serde_json::json!({"type": "reminder", "message": reminder}));
                    }
                    next_prompt = Message::user(reminder);
                    continue;
                }
            }

            // Completion nudge — prompt the model to call Done.
            if !text.is_empty() && completion_nudge_budget > 0 {
                completion_nudge_budget -= 1;
                log::info!(
                    target: "_gremlins_core.clients.agent_loop",
                    "empty turn — nudging agent (remaining_budget={})",
                    completion_nudge_budget,
                );
                // Record the nudge in raw and captured streams so the
                // transcript accurately reflects the interaction.
                let nudge_msg = "You produced text but no tool calls. If your work is complete, \
                     call the Done tool. If you still need to make changes, use the \
                     appropriate tool now.";
                write_raw(
                    raw,
                    &serde_json::json!({"type": "reminder", "message": nudge_msg}),
                );
                if let Some(evts) = captured.as_mut() {
                    evts.push(serde_json::json!({"type": "reminder", "message": nudge_msg}));
                }
                history.push(next_prompt);
                history.push(assistant_tool_message(&text, &[]));
                next_prompt = Message::user(nudge_msg.to_string());
                continue;
            }
            // Exhausted — give up.
            log::warn!(
                target: "_gremlins_core.clients.agent_loop",
                "empty-turn exhausted: turn={turn_num} text_empty={} reasoning_empty={} budget={completion_nudge_budget}",
                text.is_empty(),
                reasoning.is_empty(),
            );
            let usage = usage_stats(
                turn_num,
                total_prompt_tokens,
                total_completion_tokens,
                total_cached_tokens,
                total_cache_creation_tokens,
                total_reasoning_tokens,
            );
            if !nested {
                stream::flush();
                emit_final(prefix, turns, " (exhausted)");
                stream::emit_summary(
                    prefix,
                    turn_num,
                    loop_start,
                    total_prompt_tokens,
                    total_completion_tokens,
                    total_cached_tokens,
                    total_cache_creation_tokens,
                    total_reasoning_tokens,
                );
            }
            return Ok(completed_run(Some(final_text), captured, usage));
        }

        history.push(next_prompt);
        history.push(assistant_tool_message(&text, &tool_calls));

        // Phase 1: emit tool-start events, collect owned data for concurrent execution
        let mut jobs: Vec<Job> = Vec::new();
        let mut task_count: usize = 0;
        for tc in &tool_calls {
            let args_json =
                serde_json::to_string(&tc.function.arguments).unwrap_or_else(|_| "{}".into());
            stream::emit_tool(prefix, &tc.function.name, &key_arg(&tc.function.arguments));
            if !nested {
                let tool_evt = tool_use_event(&tc.id, &tc.function.name, &tc.function.arguments);
                write_raw(raw, &tool_evt);
                if let Some(evts) = captured.as_mut() {
                    evts.push(tool_evt);
                }
            }
            jobs.push(Job {
                id: tc.id.clone(),
                call_id: tc.call_id.clone(),
                name: tc.function.name.clone(),
                args: args_json,
                key: ledger_key_arg(&tc.function.arguments),
                over_cap: tc.function.name == "Task" && {
                    task_count += 1;
                    task_count > tools::TASK_MAX_PER_TURN
                },
            });
        }

        // Phase 2: concurrent execution. Over-cap Task calls short-circuit so a
        // single response cannot spawn an unbounded number of child loops.
        let ctx = tool_ctx.clone();
        let results = join_all(jobs.iter().map(|j| {
            let ctx = &ctx;
            async move {
                if j.over_cap {
                    format!(
                        "Error: at most {} Task calls per message",
                        tools::TASK_MAX_PER_TURN
                    )
                } else {
                    tools::invoke(&j.name, ctx, &j.args).await
                }
            }
        }))
        .await;

        // Phase 3: emit results in order
        let mut result_msgs = Vec::new();
        let mut ledger = Vec::new();
        for (job, output) in jobs.into_iter().zip(results) {
            stream::emit_result(prefix, &output, false);
            if !nested {
                let result_evt = tool_result_event(&job.id, &output);
                write_raw(raw, &result_evt);
                if let Some(evts) = captured.as_mut() {
                    evts.push(result_evt);
                }
            }
            ledger.push(ledger_line(&job.name, &job.key, &output));
            result_msgs.push(Message::tool_result_with_call_id(
                job.id,
                job.call_id,
                output,
            ));
        }
        turns += tool_calls.len();
        stream::flush();
        // Every tool result lands in history so the results stay adjacent to the
        // assistant tool_calls message; the ledger becomes the next turn's prompt,
        // trailing the complete result block instead of splitting it.
        history.extend(result_msgs);
        next_prompt = Message::user(ledger_message(&ledger));
    }

    if !nested {
        let suffix = if timed_out {
            " (timeout)"
        } else if stream_error.is_some() {
            " (stream-error)"
        } else {
            ""
        };
        stream::flush();
        emit_final(prefix, turns, suffix);
        stream::emit_summary(
            prefix,
            turn_num,
            loop_start,
            total_prompt_tokens,
            total_completion_tokens,
            total_cached_tokens,
            total_cache_creation_tokens,
            total_reasoning_tokens,
        );
    }

    if timed_out {
        return Err(ClientError::Timeout {
            message: "stream idle timeout".into(),
        });
    }
    if let Some(err) = stream_error {
        let classify = opts.classify_error.unwrap_or(default_classify);
        return Err(classify(err));
    }
    Err(ClientError::Runtime {
        message: format!("exceeded max turns ({max_turns})"),
    })
}

pub(crate) fn apply_chunk<R: GetTokenUsage>(
    chunk: StreamedAssistantContent<R>,
    text: &mut String,
    reasoning: &mut String,
    tool_calls: &mut Vec<ToolCall>,
    usage: &mut Option<Usage>,
) {
    match chunk {
        StreamedAssistantContent::Text(t) => text.push_str(&t.text),
        StreamedAssistantContent::ToolCall { tool_call, .. } => tool_calls.push(tool_call),
        StreamedAssistantContent::ToolCallDelta { .. } => {}
        StreamedAssistantContent::Reasoning(r) => reasoning.push_str(&r.display_text()),
        StreamedAssistantContent::ReasoningDelta { reasoning: r, .. } => reasoning.push_str(&r),
        StreamedAssistantContent::Final(res) => {
            *usage = Some(res.token_usage());
        }
        StreamedAssistantContent::Unknown(_) => {}
    }
}

pub(crate) fn assistant_tool_message(text: &str, tool_calls: &[ToolCall]) -> Message {
    let mut contents = Vec::new();
    if !text.is_empty() {
        contents.push(AssistantContent::text(text.to_string()));
    }
    for tc in tool_calls {
        contents.push(AssistantContent::ToolCall(tc.clone()));
    }
    Message::Assistant {
        id: None,
        content: OneOrMany::from_iter_optional(contents)
            .unwrap_or_else(|| OneOrMany::one(AssistantContent::text(""))),
    }
}

pub(crate) fn key_arg(args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        for k in [
            "file_path",
            "command",
            "pattern",
            "url",
            "output_file",
            "description",
        ] {
            if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    String::new()
}

/// Compact label for a tool call in the per-turn action ledger.
pub(crate) fn ledger_key_arg(args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        for k in [
            "file_path",
            "command",
            "pattern",
            "path",
            "description",
            "url",
            "output_file",
        ] {
            if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return truncate_ledger_value(v);
                }
            }
        }
    }
    String::new()
}

/// One ledger bullet: tool name, its most informative argument, and whether the
/// call actually ran.
fn ledger_line(name: &str, key: &str, output: &str) -> String {
    let label = if key.is_empty() {
        name.to_string()
    } else {
        format!("{name} {key}")
    };
    let note = if output.starts_with("Error: unknown tool") {
        " (not run: unknown tool)"
    } else if tools::result_status(output) == "error" {
        " (failed)"
    } else {
        ""
    };
    format!("- {label}{note}")
}

fn ledger_message(lines: &[String]) -> String {
    format!("Actions taken this turn:\n{}\n", lines.join("\n"))
}

fn truncate_ledger_value(v: &str) -> String {
    const MAX: usize = 80;
    // Collapse embedded line breaks so one action is always one ledger line.
    let v: String = v
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let chars: Vec<char> = v.chars().collect();
    if chars.len() <= MAX {
        return v;
    }
    let head: String = chars[..40].iter().collect();
    let tail: String = chars[chars.len() - 39..].iter().collect();
    format!("{head}…{tail}")
}

pub(crate) fn assistant_text_event(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": text}]}
    })
}

pub(crate) fn tool_use_event(id: &str, name: &str, input: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [{
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }]
        }
    })
}

pub(crate) fn tool_result_event(id: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user",
        "message": {
            "content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": content
            }]
        }
    })
}

pub(crate) fn write_raw(raw: &mut Option<std::fs::File>, evt: &serde_json::Value) {
    if let Some(f) = raw {
        if let Ok(line) = serde_json::to_string(evt) {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

pub(crate) fn emit_final(prefix: &str, turns: usize, suffix: &str) {
    eprintln!(
        "{} {}final: turns={turns} cost=not-reported{suffix}",
        stream::ts_internal(),
        prefix
    );
    stream::flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use rig_core::message::UserContent;
    use std::collections::HashMap;

    #[test]
    fn default_classifier() {
        // 5xx → retryable
        let err = CompletionError::from_http_response(StatusCode::SERVICE_UNAVAILABLE, "boom");
        assert!(matches!(
            default_classify(err),
            ClientError::ApiServerError { .. }
        ));

        // 429 → retryable
        let err = CompletionError::from_http_response(StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert!(matches!(
            default_classify(err),
            ClientError::ApiServerError { .. }
        ));

        // 400 → NOT retryable
        let err = CompletionError::from_http_response(StatusCode::BAD_REQUEST, "bad prompt");
        assert!(matches!(default_classify(err), ClientError::Runtime { .. }));

        // 401 → NOT retryable
        let err = CompletionError::from_http_response(StatusCode::UNAUTHORIZED, "bad key");
        assert!(matches!(default_classify(err), ClientError::Runtime { .. }));

        // No HTTP status → retryable
        let err = CompletionError::ProviderError("something broke".into());
        assert!(matches!(
            default_classify(err),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn event_shapes() {
        let text = assistant_text_event("hello");
        assert_eq!(text["type"], "assistant");
        assert_eq!(text["message"]["content"][0]["text"], "hello");

        let tool = tool_use_event("id42", "Bash", &serde_json::json!({"command": "ls"}));
        assert_eq!(tool["message"]["content"][0]["type"], "tool_use");
        assert_eq!(tool["message"]["content"][0]["name"], "Bash");
        assert_eq!(tool["message"]["content"][0]["id"], "id42");

        let result = tool_result_event("id42", "ok");
        assert_eq!(result["type"], "user");
        assert_eq!(result["message"]["content"][0]["tool_use_id"], "id42");
        assert_eq!(result["message"]["content"][0]["content"], "ok");
    }

    #[test]
    fn key_arg_picks_known_fields() {
        assert_eq!(
            key_arg(&serde_json::json!({"file_path": "/tmp/x.py"})),
            "/tmp/x.py"
        );
        assert_eq!(
            key_arg(&serde_json::json!({"command": "echo hi"})),
            "echo hi"
        );
        assert_eq!(key_arg(&serde_json::json!({})), "");
    }

    #[test]
    fn ledger_key_arg_covers_extra_fields_and_truncates() {
        assert_eq!(ledger_key_arg(&serde_json::json!({"path": "/tmp"})), "/tmp");
        assert_eq!(
            ledger_key_arg(&serde_json::json!({"description": "fix it"})),
            "fix it"
        );
        assert_eq!(ledger_key_arg(&serde_json::json!({"pattern": ""})), "");
        let long = ledger_key_arg(&serde_json::json!({"command": "x".repeat(200)}));
        assert_eq!(long.chars().count(), 80);
        assert!(long.contains('…'));
        assert!(ledger_key_arg(&serde_json::json!({
            "file_path": format!("{}/deep/path.txt", "d".repeat(200))
        }))
        .ends_with("path.txt"));

        // Multiline commands must stay on one ledger line.
        assert_eq!(
            ledger_key_arg(&serde_json::json!({"command": "a\nb\r\nc"})),
            "a b  c"
        );
    }

    #[test]
    fn ledger_line_flags_denied_and_failed_calls() {
        assert_eq!(ledger_line("Read", "/tmp/x", "content"), "- Read /tmp/x");
        assert_eq!(ledger_line("Read", "", "content"), "- Read");
        assert_eq!(
            ledger_line("Write", "/tmp/x", "Error: unknown tool Write"),
            "- Write /tmp/x (not run: unknown tool)"
        );
        assert_eq!(
            ledger_line("Bash", "cargo test", "Error: timed out"),
            "- Bash cargo test (failed)"
        );
        assert_eq!(
            ledger_line("Bash", "false", "[exit 1]\n"),
            "- Bash false (failed)"
        );
    }

    #[tokio::test]
    async fn cancel_token_wakes_waiters() {
        let token = CancelToken::new();
        let t = token.clone();
        let handle = tokio::spawn(async move {
            t.cancelled().await;
        });
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(token.is_cancelled());
    }

    fn loop_opts(filter: Option<&[String]>) -> LoopOpts<'_> {
        LoopOpts {
            extra: None,
            tool_filter: filter,
            classify_error: None,
        }
    }

    fn test_ctx(
        cwd: Option<std::path::PathBuf>,
        raw_path: Option<std::path::PathBuf>,
    ) -> RunContext {
        RunContext {
            params: RunParams {
                prompt: "hi".into(),
                label: "t".into(),
                model: Some("mock".into()),
                raw_path,
                capture_events: true,
                on_timeout_prompt: None,
                max_retries: 0,
                cwd,
                artifact_dir: None,
                idle_timeout: Some(0.05),
                extra_env: None,
                expected_artifact_paths: vec![],
                system_prompt: None,
            },
            prefix: "[t] ".into(),
            idle_timeout: 0.05,
            expected_artifact_paths: vec![],
            reminder_budget: 0,
            completion_nudge_budget: 0,
        }
    }

    #[derive(Clone)]
    struct PendingModel;

    impl CompletionModel for PendingModel {
        type Response = rig_core::test_utils::MockResponse;
        type StreamingResponse = rig_core::test_utils::MockResponse;
        type Client = ();

        fn make(_: &Self::Client, _: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _: rig_core::completion::CompletionRequest,
        ) -> Result<
            rig_core::completion::CompletionResponse<Self::Response>,
            rig_core::completion::CompletionError,
        > {
            Err(rig_core::completion::CompletionError::ProviderError(
                "unused".into(),
            ))
        }

        async fn stream(
            &self,
            _: rig_core::completion::CompletionRequest,
        ) -> Result<
            rig_core::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
            rig_core::completion::CompletionError,
        > {
            let s: rig_core::streaming::StreamingResult<Self::StreamingResponse> =
                Box::pin(futures::stream::pending());
            Ok(rig_core::streaming::StreamingCompletionResponse::stream(s))
        }
    }

    #[tokio::test]
    async fn loop_idle_timeout_is_client_timeout() {
        let ctx = test_ctx(None, None);
        let cancel = CancelToken::new();
        let err = run_agent_loop(&PendingModel, "hi", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Timeout { .. }));
    }

    #[tokio::test]
    async fn loop_tool_then_text() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt");
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Write",
                    serde_json::json!({
                        "file_path": target.to_str().unwrap(),
                        "content": "hello"
                    }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("wrote it"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "write", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.text_result.as_deref(), Some("wrote it"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        let events = result.events.unwrap();
        assert!(events
            .iter()
            .any(|e| e["message"]["content"][0]["type"] == "tool_use"));
        assert!(events
            .iter()
            .any(|e| e["message"]["content"][0]["type"] == "tool_result"));
    }

    #[tokio::test]
    async fn loop_bash_tool_uses_cwd() {
        // Reproduction: a Bash tool call issued by the model must run inside
        // the gremlin worktree (cwd), not the process's own current directory.
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "in-worktree").unwrap();

        // Use a relative Bash command so any cwd mishandling shows up: `pwd`
        // must resolve to `dir`, not the process cwd.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Bash",
                    serde_json::json!({ "command": "pwd; ls" }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("saw it"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "where am i", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        let events = result.events.unwrap();
        let result_evt = events
            .iter()
            .find(|e| e["message"]["content"][0]["type"] == "tool_result")
            .unwrap();
        let content = result_evt["message"]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(
            content.contains(dir.to_str().unwrap()),
            "Bash tool should run in cwd={}, got: {content}",
            dir.display()
        );
        assert!(
            content.contains("marker.txt"),
            "Bash `ls` should see worktree marker, got: {content}"
        );
    }

    #[tokio::test]
    async fn loop_filtered_tool_refused() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-filter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("must-not-exist.txt");
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Write",
                    serde_json::json!({
                        "file_path": target.to_str().unwrap(),
                        "content": "nope"
                    }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("blocked"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let filter = vec!["Read".to_string()];
        let result = run_agent_loop(
            &model,
            "write",
            &ctx,
            cancel,
            loop_opts(Some(&filter)),
            None,
        )
        .await
        .unwrap();
        assert!(!target.exists());
        let events = result.events.unwrap();
        let result_evt = events
            .iter()
            .find(|e| e["message"]["content"][0]["type"] == "tool_result")
            .unwrap();
        let content = result_evt["message"]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(content.contains("unknown tool"));
        let reqs = model.requests();
        let ledger = format!("{:?}", reqs[1].chat_history);
        assert!(
            ledger.contains("Write ") && ledger.contains("(not run: unknown tool)"),
            "denied call must be marked as not run in the ledger: {ledger}"
        );
    }

    #[tokio::test]
    async fn loop_writes_audit_jsonl() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("run.jsonl");
        let target = dir.join("out.txt");
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Write",
                    serde_json::json!({
                        "file_path": target.to_str().unwrap(),
                        "content": "x"
                    }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("ok"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), Some(raw.clone()));
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        run_agent_loop(&model, "write", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        let audit = dir.join("run.audit.jsonl");
        assert!(audit.exists());
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&audit).unwrap()).unwrap();
        assert_eq!(entry["tool"], "Write");
        assert_eq!(entry["status"], "ok");
    }

    #[tokio::test]
    async fn loop_concurrent_multi_tool_in_one_turn() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-conc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, "alpha").unwrap();
        std::fs::write(&f2, "beta").unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({"file_path": f1.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c2",
                    "Read",
                    serde_json::json!({"file_path": f2.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("both read"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "read both", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("both read"));
        let events = result.events.unwrap();
        // Two tool_use events, then two tool_result events, in order
        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| e["message"]["content"][0]["type"] == "tool_use")
            .collect();
        assert_eq!(tool_uses.len(), 2);
        assert_eq!(tool_uses[0]["message"]["content"][0]["id"], "c1");
        assert_eq!(tool_uses[1]["message"]["content"][0]["id"], "c2");
        let results: Vec<_> = events
            .iter()
            .filter(|e| e["message"]["content"][0]["type"] == "tool_result")
            .collect();
        assert_eq!(results.len(), 2);
        assert!(results[0]["message"]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("alpha"));
        assert!(results[1]["message"]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("beta"));
    }

    #[tokio::test]
    async fn loop_concurrent_mixed_tools_with_failure() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-mixed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let read_file = dir.join("readme.txt");
        let write_file = dir.join("out.txt");
        std::fs::write(&read_file, "before").unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "r1",
                    "Read",
                    serde_json::json!({"file_path": read_file.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "w1",
                    "Write",
                    serde_json::json!({
                        "file_path": write_file.to_str().unwrap(),
                        "content": "mixed"
                    }),
                ),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "b1",
                    "Bash",
                    serde_json::json!({"command": "exit 1"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "mix", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("done"));
        let events = result.events.unwrap();
        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| e["message"]["content"][0]["type"] == "tool_use")
            .collect();
        assert_eq!(tool_uses.len(), 3);
        assert_eq!(tool_uses[0]["message"]["content"][0]["name"], "Read");
        assert_eq!(tool_uses[1]["message"]["content"][0]["name"], "Write");
        assert_eq!(tool_uses[2]["message"]["content"][0]["name"], "Bash");
        let results: Vec<_> = events
            .iter()
            .filter(|e| e["message"]["content"][0]["type"] == "tool_result")
            .collect();
        assert_eq!(results.len(), 3);
        // Read result contains "before"
        assert!(results[0]["message"]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("before"));
        // Write result confirms write
        assert_eq!(
            results[1]["message"]["content"][0]["content"]
                .as_str()
                .unwrap(),
            "OK"
        );
        // Bash result contains exit failure info
        let bash_content = results[2]["message"]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(
            bash_content.contains("[exit 1]"),
            "bash failure not as expected: {bash_content}"
        );
        // All three tool_results map to their ids
        assert_eq!(results[0]["message"]["content"][0]["tool_use_id"], "r1");
        assert_eq!(results[1]["message"]["content"][0]["tool_use_id"], "w1");
        assert_eq!(results[2]["message"]["content"][0]["tool_use_id"], "b1");
        // Write actually happened
        assert_eq!(std::fs::read_to_string(&write_file).unwrap(), "mixed");
    }

    #[tokio::test]
    async fn loop_system_prompt_injected_as_preamble() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-sys-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::text("ok"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "done"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.params.system_prompt = Some("you are a harness".into());
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "hi", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("ok"));

        let requests = model.requests();
        assert!(!requests.is_empty());
        for req in requests {
            match req.chat_history.first() {
                Message::System { content } => assert_eq!(content, "you are a harness"),
                other => panic!("system prompt must lead the history, got: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn loop_no_system_preamble_injected() {
        // With no system_prompt set, the harness must not inject a preamble.
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-nosys-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::text("ok"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "done"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "hi", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("ok"));

        for req in model.requests() {
            assert!(req.preamble.is_none(), "harness must not inject preamble");
            for msg in req.chat_history.iter() {
                if let Message::System { content } = msg {
                    panic!("harness injected system message: {content}");
                }
            }
        }
    }

    #[tokio::test]
    async fn loop_ledger_injected_into_history() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, "content").unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({"file_path": f.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        run_agent_loop(&model, "read", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();

        let reqs = model.requests();
        assert_eq!(reqs.len(), 2);
        let history: Vec<&Message> = reqs[1].chat_history.iter().collect();
        let key = ledger_key_arg(&serde_json::json!({"file_path": f.to_str().unwrap()}));
        let expected = Message::user(format!("Actions taken this turn:\n- Read {key}\n"));
        assert!(key.contains("x.txt"), "ledger must name the file: {key}");
        assert_eq!(history.len(), 4, "unexpected history: {history:#?}");
        assert!(matches!(history[1], Message::Assistant { .. }));
        assert_eq!(
            message_kind(history[2]),
            "result",
            "tool result must follow the assistant tool_calls message: {:?}",
            history[2]
        );
        assert_eq!(history[3], &expected, "ledger must trail all tool results");
    }

    fn message_kind(m: &Message) -> &'static str {
        match m {
            Message::User { content } if content.iter().any(|c| matches!(c, UserContent::Text(t) if t.text.starts_with("Actions taken this turn:"))) => "ledger",
            Message::User { content } if content
                .iter()
                .any(|c| matches!(c, UserContent::ToolResult(_))) => "result",
            Message::Assistant { .. } => "assistant",
            _ => "other",
        }
    }

    #[tokio::test]
    async fn loop_ledger_follows_all_tool_results() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-ledger-multi-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, "alpha").unwrap();
        std::fs::write(&f2, "beta").unwrap();
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({"file_path": f1.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c2",
                    "Read",
                    serde_json::json!({"file_path": f2.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        run_agent_loop(
            &model,
            "read both",
            &ctx,
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        let reqs = model.requests();
        let history: Vec<&Message> = reqs[1].chat_history.iter().collect();
        let kinds: Vec<&str> = history.iter().map(|m| message_kind(m)).collect();
        assert_eq!(
            kinds,
            vec!["other", "assistant", "result", "result", "ledger"]
        );
        let ledger = format!("{:?}", history[4]);
        assert!(
            ledger.contains("a.txt") && ledger.contains("b.txt"),
            "ledger must list both actions: {ledger}"
        );
    }

    #[tokio::test]
    async fn reminder_writes_missing_artifact_on_second_turn() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-remind-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("expected.md");

        // Turn 1: text-only, no tool calls → triggers reminder.
        // Turn 2: Write tool call → writes the file.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::text("here is the content"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Write",
                    serde_json::json!({
                        "file_path": target.to_str().unwrap(),
                        "content": "written after reminder"
                    }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.expected_artifact_paths = vec![target.clone()];
        ctx.reminder_budget = 1;
        ctx.completion_nudge_budget = 0;
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "write", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("done"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "written after reminder"
        );
    }

    #[tokio::test]
    async fn reminder_exhausted_still_returns_when_missing() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-remind-exh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("never-written.md");

        // Turn 1: text-only → reminder injected.
        // Turn 2: text-only again → budget exhausted (reminder + nudge both 0),
        // falls through and returns.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::text("first try"),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("still no write"),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.expected_artifact_paths = vec![target.clone()];
        ctx.reminder_budget = 1;
        ctx.completion_nudge_budget = 0;
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "write", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        // Returns normally — file is still missing (Python verify_produced catches it).
        assert_eq!(result.text_result.as_deref(), Some("still no write"));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn completion_nudge_then_done() {
        // Positive completion-nudge budget: text-only turn gets a nudge,
        // model responds with Done on the next turn.
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-nudge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Turn 1: text-only → triggers completion nudge.
        // Turn 2: Done call → completes.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::text("thinking..."),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("all done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "completed task"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.completion_nudge_budget = 1;
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "do it", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        // Done succeeds; text is non-empty so it's the result.
        assert_eq!(result.text_result.as_deref(), Some("all done"));
        // Two requests: initial turn + post-nudge turn.
        assert_eq!(model.requests().len(), 2);
        // Verify the nudge was recorded in captured events.
        let events = result.events.unwrap();
        let nudge_evt = events.iter().find(|e| {
            e["type"] == "reminder"
                && e["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("call the Done tool"))
        });
        assert!(
            nudge_evt.is_some(),
            "completion nudge must appear in captured events"
        );
    }

    #[tokio::test]
    async fn mixed_done_with_other_tools_is_rejected() {
        // Done mixed with other tool calls in the same turn must be
        // rejected with an error; no tools from that turn may execute.
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-mixed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt");

        // Turn 1: Write + Done mixed → rejected, nothing executes.
        // Turn 2: Write solo → writes the file.
        // Turn 3: Done solo → completes.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Write",
                    serde_json::json!({
                        "file_path": target.to_str().unwrap(),
                        "content": "hello"
                    }),
                ),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "mixed"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c2",
                    "Write",
                    serde_json::json!({
                        "file_path": target.to_str().unwrap(),
                        "content": "hello"
                    }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done2",
                    "Done",
                    serde_json::json!({"summary": "done after fix"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "write", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        // File was written on the retry, not in the mixed turn.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        assert_eq!(result.text_result.as_deref(), Some("done"));
        // The Done error was recorded in captured events.
        let events = result.events.unwrap();
        let done_error = events.iter().find(|e| {
            e["message"]["content"][0]["type"] == "tool_result"
                && e["message"]["content"][0]["tool_use_id"] == "done1"
        });
        assert!(
            done_error.is_none(),
            "mixed-turn Done does not produce a captured tool result — the rejection is in history only"
        );
        // The Write also did not produce a captured event from the mixed turn.
        assert!(
            !events.iter().any(|e| {
                e["message"]["content"][0]
                    .get("tool_use_id")
                    .is_some_and(|id| id == "c1")
            }),
            "mixed-turn Write must not appear in captured events"
        );
    }

    #[tokio::test]
    async fn reminder_not_triggered_when_no_expected_paths() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-remind-none-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::text("just text"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "done"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        // No expected paths, no reminder budget — should be a single-turn run.
        ctx.expected_artifact_paths = vec![];
        ctx.reminder_budget = 0;
        let cancel = CancelToken::new();
        let result = run_agent_loop(&model, "hi", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("just text"));
        // Only one request — no reminder loop.
        assert_eq!(model.requests().len(), 1);
    }

    /// A single message can request at most `TASK_MAX_PER_TURN` Task calls;
    /// the excess short-circuit instead of spawning more child loops.
    #[tokio::test]
    async fn task_calls_are_capped_per_message() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-task-cap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let over = tools::TASK_MAX_PER_TURN + 1;
        let mut turn = Vec::new();
        for i in 0..over {
            turn.push(rig_core::test_utils::MockStreamEvent::tool_call(
                format!("c{i}"),
                "Task",
                serde_json::json!({"description": format!("d{i}"), "prompt": format!("p{i}")}),
            ));
        }
        turn.push(rig_core::test_utils::MockStreamEvent::final_response_with_default_usage());
        // Each permitted Task runs a nested loop, so script enough text turns
        // for all of them plus the outer loop's wrap-up turn.
        let mut turns = vec![turn];
        turns.extend((0..over + 1).map(|_| {
            vec![
                rig_core::test_utils::MockStreamEvent::text("ok"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ]
        }));
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns(turns);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        run_agent_loop(&model, "fan out", &ctx, cancel, loop_opts(None), None)
            .await
            .unwrap();

        let reqs = model.requests();
        // The outer loop's second request carries every Task result.
        let results: Vec<String> = reqs
            .last()
            .unwrap()
            .chat_history
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::ToolResult(r) => Some(
                            r.content
                                .iter()
                                .filter_map(|rc| match rc {
                                    rig_core::completion::message::ToolResultContent::Text(t) => {
                                        Some(t.text.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect();
        assert_eq!(results.len(), over, "every call must get a result");
        let rejected = results.iter().filter(|r| r.contains("at most")).count();
        assert_eq!(rejected, 1, "only the call past the cap is rejected");
    }

    /// A `task-clients` entry matching the task's `description` must swap in the
    /// model the factory builds, and nothing else about the Task call changes.
    #[tokio::test]
    async fn task_clients_override_selects_the_task_model() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = tempfile::tempdir().unwrap();

        let task_call = |id: &str| {
            MockStreamEvent::tool_call(
                id,
                "Task",
                serde_json::json!({"description": "Scout", "prompt": "look around"}),
            )
        };
        let done_call = |id: &str, summary: &str| {
            MockStreamEvent::tool_call(id, "Done", serde_json::json!({"summary": summary}))
        };

        // The outer loop delegates once, then finishes.
        let parent = MockCompletionModel::from_stream_turns([
            vec![
                task_call("t1"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                done_call("d1", "outer done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        // The overridden model is the one that actually answers the task.
        let child = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("scout result"),
            done_call("d2", "scout result"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let child_handle = child.clone();
        let factory: super::super::task::TaskModelFactory<MockCompletionModel> =
            Arc::new(move |spec: &str| {
                assert_eq!(spec, "openai:mini", "factory receives the matched spec");
                Some(child_handle.clone())
            });

        let selector = super::super::task::TaskModelSelector::new(
            HashMap::from([("scout".to_string(), "openai:mini".to_string())]),
            HashMap::new(),
            factory,
        )
        .expect("a configured task-clients map yields a selector");
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);

        run_agent_loop(
            &parent,
            "go",
            &ctx,
            CancelToken::new(),
            loop_opts(None),
            Some(selector),
        )
        .await
        .unwrap();

        // The overridden model ran the task and its reply came back verbatim.
        assert!(
            !child.requests().is_empty(),
            "the task-clients model must be the one that ran the Task"
        );
        let reqs = parent.requests();
        let parent_history = &reqs.last().unwrap().chat_history;
        let task_result = parent_history
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::ToolResult(r) => Some(
                            r.content
                                .iter()
                                .filter_map(|rc| match rc {
                                    rig_core::completion::message::ToolResultContent::Text(t) => {
                                        Some(t.text.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .find(|t| t.contains("scout result"))
            .expect("task result must reach the parent history");
        assert!(
            task_result.contains("# Scout"),
            "task result should carry its description header, got: {task_result}"
        );
    }
}
