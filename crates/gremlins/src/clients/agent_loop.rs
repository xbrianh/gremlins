use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use rig_core::completion::message::{
    AssistantContent, CallId, Reasoning, Sealed, ToolCall, ToolName, UserContent,
};
use rig_core::completion::{CompletionRequest, Message, ToolDefinition, Usage};
use rig_core::error::ProviderError;
use rig_core::streaming::{Item, StreamEvent};
use tokio::sync::{mpsc, Notify};

use crate::executor::conversation;

use super::backend::{ClientError, RunParams};
use super::interactive::{
    InteractiveCommand, InteractiveEvent, InteractiveSession, PauseToken, StreamEventSender,
};
use super::display::{apply_chunk, assistant_text_event, assistant_tool_message, enrich_log_line, format_tool_call, format_tool_call_parts, ledger_key_arg, ledger_line, ledger_message, tool_use_event, tool_result_event, write_raw};
use super::log_util::trunc;
use super::model::{GremlinsModel, GremlinsModelError};
use super::protocol::{add_opt, CompletedRun, TurnStats, UsageStats};
use super::tools::{self, AgentLoopRuntime, AgentLoopTelemetry};

fn send_log(tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>, prefix: &str, msg: &str) {
    if let Some(tx) = tx {
        let _ = tx.send(format!("{prefix}{msg}"));
    }
}

fn cancelled_error(message: impl Into<String>, history: &[Message]) -> ClientError {
    ClientError::Cancelled {
        message: message.into(),
        messages: Some(history.to_vec()),
    }
}

/// Send one stream event with backpressure, while still observing
/// cancellation. Returns `true` if the event was sent and `false` if
/// cancellation won the race before the send completed.
///
/// The mpsc stream sender awaits channel capacity; racing that await
/// against the cancel token means `Esc`/`stop` unwinds promptly even when
/// the chat channel is full. Callers must propagate a `false` result so
/// cancellation stops the relevant loop instead of being treated as a
/// completed send.
async fn send_stream_event(
    se: &StreamEventSender,
    event: InteractiveEvent,
    cancel: &CancelToken,
) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = se.send(event) => true,
    }
}

/// [`send_stream_event`] as a `Result`, so call sites can propagate
/// cancellation with `?` instead of treating a lost race as a completed
/// send.
async fn send_stream_event_checked(
    se: &StreamEventSender,
    event: InteractiveEvent,
    cancel: &CancelToken,
    history: &[Message],
) -> Result<(), ClientError> {
    if send_stream_event(se, event, cancel).await {
        Ok(())
    } else {
        Err(cancelled_error("cancelled", history))
    }
}

impl AgentLoopTelemetry {
    /// Send an event on the stream if there is an active receiver.
    ///
    /// No-ops (returns `Ok(())`) when there is no channel or the broadcast
    /// subscriber has disconnected. Races against `cancel` so the caller can
    /// propagate cancellation with `?`.
    async fn send_stream_event(
        &self,
        event: InteractiveEvent,
        cancel: &CancelToken,
        history: &[Message],
    ) -> Result<(), ClientError> {
        if let Some(ref se) = self.stream_events {
            if se.wants_events() {
                return send_stream_event_checked(se, event, cancel, history).await;
            }
        }
        Ok(())
    }

    /// True when there is an active subscriber that will receive events.
    ///
    /// Callers use this to avoid building event payloads when no one is
    /// listening — the `send_stream_event` method already no-ops, but
    /// skipping the clone / format / truncate work saves allocation and
    /// CPU on non-interactive runs.
    fn wants_stream_events(&self) -> bool {
        self.stream_events
            .as_ref()
            .is_some_and(|se| se.wants_events())
    }
}

/// Discard any tool-output lines currently queued in the live-output
/// receiver. Used when leaving a tool batch without the normal post-batch
/// drain so stale chunks cannot be replayed by a later batch.
fn discard_tool_output(rx: &mut Option<mpsc::Receiver<tools::ToolOutputLine>>) {
    if let Some(rx) = rx.as_mut() {
        while rx.try_recv().is_ok() {}
    }
}

fn task_kind_is_fork(args: &serde_json::Value) -> bool {
    args.get("kind")
        .and_then(|v| v.as_str())
        .and_then(super::task_kind::TaskKind::parse)
        == Some(super::task_kind::TaskKind::Fork)
}

pub(crate) type ErrorClassifier = fn(ProviderError) -> ClientError;

pub(crate) fn default_classify(err: ProviderError) -> ClientError {
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
    log::info!("retrying provider error (no HTTP status): {}", err);
    ClientError::ApiServerError {
        message: err.to_string(),
    }
}

/// Classify a provider error and report whether it is retryable at the turn
/// boundary (429 / 5xx / no-HTTP-status mid-stream drops). Reuses the same
/// `default_classify` (or the caller-supplied classifier) used for the
/// terminal error path, so the retry decision stays consistent with what the
/// backend would otherwise surface.
fn is_retryable(err: &ProviderError, opts: &LoopOpts<'_>) -> bool {
    let classify = opts.classify_error.unwrap_or(default_classify);
    matches!(classify(err.clone()), ClientError::ApiServerError { .. })
}

/// Sleep for the turn-level backoff, aborting early when the cancel token is
/// set. Returns `true` if cancellation was observed (the caller must abandon
/// the retry and unwind), `false` if the full backoff elapsed.
async fn retry_backoff(cancel: &CancelToken, wait: f64) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs_f64(wait)) => false,
        _ = cancel.cancelled() => true,
    }
}

#[derive(Debug)]
pub struct CancelToken {
    flag: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        // Cancellation is broadcast: an active chat turn has both
        // `run_chat_turn` and the tool-phase select arm waiting on this
        // token. `notify_waiters()` wakes every waiter so the outer
        // `finish()` path is never left asleep while the inner waiter
        // unwinds the turn.
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    pub async fn cancelled(&self) {
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
    pub(crate) stream_events: Option<StreamEventSender>,
    /// Pre-populated message history carried into the loop's internal history.
    pub(crate) prepopulated_messages: Option<Vec<Message>>,
    /// Chat-only soft compaction settings (mirrors `RunParams`).
    pub(crate) compaction_opts: Option<conversation::CompactionOpts>,
}

impl RunContext {
    pub(crate) fn from_params(params: RunParams) -> Self {
        let idle_timeout = params
            .idle_timeout
            .unwrap_or_else(crate::config::stream_idle_timeout);
        let prefix = if params.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", params.label)
        };
        let compaction_opts = params.compaction_opts;
        Self {
            idle_timeout,
            prefix,
            expected_artifact_paths: params.expected_artifact_paths.clone(),
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
            stream_events: params.stream_events.clone(),
            prepopulated_messages: params.prepopulated_messages.clone(),
            compaction_opts,
            params,
        }
    }
}

pub(crate) struct LoopOpts<'a> {
    pub(crate) extra: Option<serde_json::Value>,
    pub(crate) tool_filter: Option<&'a [String]>,
    pub(crate) classify_error: Option<ErrorClassifier>,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_tokens: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop(
    model: GremlinsModel,
    prompt: &str,
    ctx: RunContext,
    cancel: Arc<CancelToken>,
    opts: LoopOpts<'_>,
    interactive: Option<InteractiveSession>,
) -> Result<CompletedRun, ClientError> {
    let cwd = ctx.params.cwd.clone();
    let extra_env = ctx.params.extra_env.clone();
    let raw_path = ctx.params.raw_path.clone();
    let capture_events = ctx.params.capture_events;
    let idle_timeout = ctx.idle_timeout;
    let model_name = ctx
        .params
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "model".into());

    // Insert model name after the stage bracket so every agent-loop line
    // identifies which model produced it: [stage][model] …
    let prefix = format!("{}[{model_name}] ", ctx.prefix.trim_end());

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
    let log_line = format!(
        "using client cwd={} reasoning_effort={}",
        cwd_display,
        trunc(reasoning_effort.unwrap_or("default"), 50)
    );
    send_log(&ctx.params.log_tx, &prefix, &log_line);

    if cwd.is_none() {
        send_log(
            &ctx.params.log_tx,
            &prefix,
            "warning: no cwd set for worktree enforcement",
        );
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
    // When the run requests allow-all, skip building the roots list entirely;
    // otherwise assemble [worktree, artifact_dir, scratch] as before.
    let sandbox = if ctx.params.allow_all {
        tools::SandboxMode::AllowAll
    } else {
        let mut allowed_roots = vec![worktree];
        if let Some(ref artifact_dir) = ctx.params.artifact_dir {
            allowed_roots.push(artifact_dir.clone());
        }
        if let Some(scratch) = tools::scratch_root() {
            allowed_roots.push(scratch);
        }
        tools::SandboxMode::Roots(allowed_roots)
    };
    // Only build the live-output channel when a subscriber will actually
    // consume it. Non-interactive pipeline runs have no `stream_events`
    // receiver, so building the channel would still cost per-line `String`
    // allocation plus a forwarder that drains-and-discards every chunk.
    // The bounded mpsc gives the read loop backpressure: when the forwarder
    // or chat consumer is slow, `emit_output` awaits capacity instead of
    // dropping live chunks.
    let (tool_output_tx, tool_output_rx) = if ctx
        .stream_events
        .as_ref()
        .is_some_and(|se| se.wants_events())
    {
        let (tx, rx) = mpsc::channel::<tools::ToolOutputLine>(1024);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let telemetry = AgentLoopTelemetry {
        stream_events: ctx.stream_events.clone(),
        tool_output: tool_output_tx,
    };
    let mut tool_ctx = AgentLoopRuntime {
        cwd: cwd.clone(),
        extra_env,
        base_env: ctx.params.base_env.clone(),
        sandbox,
        audit_log,
        allowed_tools: opts.tool_filter.map(|s| s.to_vec()),
        task_fn: None,
        audit_lock: Some(Arc::new(std::sync::Mutex::new(()))),
        parent_history: None,
        id_prefix: String::new(),
        telemetry,
    };
    let tool_defs = tools::tool_definitions(opts.tool_filter);

    // Wire up the Task runner before entering the turn loop.
    let runner = super::task::make_task_runner(
        model.clone(),
        ctx.params.task_models.clone(),
        opts.tool_filter.map(|f| f.to_vec()),
        cancel.clone(),
        tool_ctx.clone(),
        ctx.prefix.clone(),
        model_name.clone(),
        idle_timeout,
        max_turns,
        ctx.completion_nudge_budget,
        ctx.params.log_tx.clone(),
        opts.temperature,
        opts.max_tokens,
        opts.extra.clone(),
    );
    tool_ctx.task_fn = Some(runner);

    run_agent_loop_core(
        &model,
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
        &ctx.params.log_tx,
        interactive,
        tool_output_rx,
        ctx.prepopulated_messages.clone(),
        ctx.compaction_opts,
    )
    .await
}

/// Nested agent loop — same logic as the parent loop but without raw transcript
/// writes, captured-event pushes, or final/summary stream emissions.
/// Stream events (think, text, tool, result, turn metrics) are emitted.
/// Used by the Task tool.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop_nested(
    model: GremlinsModel,
    prompt: &str,
    system_prompt: Option<String>,
    tool_ctx: &AgentLoopRuntime,
    cancel: &CancelToken,
    tool_filter: Option<&[String]>,
    prefix: &str,
    id_chain: &str,
    idle_timeout: f64,
    max_turns: usize,
    completion_nudge_budget: usize,
    log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    extra: Option<serde_json::Value>,
    prepopulated_messages: Option<Vec<Message>>,
) -> Result<CompletedRun, ClientError> {
    if let Some(ref tx) = log_tx {
        let _ = tx.send(format!("{prefix}task: begin (max_turns={max_turns})"));
    }
    let opts = LoopOpts {
        extra,
        tool_filter,
        classify_error: None,
        temperature,
        max_tokens,
    };
    let tool_defs = tools::tool_definitions(tool_filter);
    let mut raw: Option<std::fs::File> = None;
    let mut captured: Option<Vec<serde_json::Value>> = None;
    let mut nested_tool_ctx = tool_ctx.clone();
    // Nested loops keep the parent's stream (so their `ToolResult` events reach
    // the parent TUI) and inherit the parent's tool-output sender rather than
    // owning a second forwarder. Only the top-level loop runs the live-output
    // forwarder, so a child never competes with the parent for a bounded stream
    // slot — the two-forwarder design could stall a small-capacity stream.
    nested_tool_ctx.telemetry = AgentLoopTelemetry {
        stream_events: tool_ctx.telemetry.stream_events.clone(),
        tool_output: tool_ctx.telemetry.tool_output.clone(),
    };
    // Prefix this task's tool call IDs with its chain so two tasks that reuse a
    // raw provider ID (e.g. `call_0`) don't collide on the parent TUI's per-ID
    // live slots. Model history keeps the original IDs.
    nested_tool_ctx.id_prefix = if id_chain.is_empty() {
        String::new()
    } else {
        format!("{id_chain}.")
    };
    let result = run_agent_loop_core(
        &model,
        prompt,
        system_prompt,
        &nested_tool_ctx,
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
        &log_tx,
        None,
        None,
        prepopulated_messages,
        None,
    )
    .await;
    if let Some(ref tx) = log_tx {
        let _ = tx.send(format!("{prefix}task: end"));
    }
    result
}

/// Future that resolves when a [`PauseToken`] fires, or never if there is no token.
async fn maybe_pause(pause: &Option<Arc<PauseToken>>) {
    if let Some(ref p) = pause {
        log::debug!("maybe_pause: waiting on pause token");
        p.paused().await;
        log::debug!("maybe_pause: pause token resolved");
    } else {
        // No interactive session — pause is impossible. This future never
        // resolves, which is intentional: without a session there is no
        // channel for the agent to receive a pause signal.
        log::trace!("maybe_pause: no interactive session, pending forever");
        std::future::pending::<()>().await;
    }
}

// ── interactive loop ─────────────────────────────────────────────────────

/// Interactive state machine. Called when the operator triggers pause
/// (via [`PauseToken`]) or when re-entering after a `RunTurn`/`Inject`.
///
/// On entry, broadcasts `Ready { turn }`. Then blocks reading
/// `InteractiveCommand`s from the session's `cmd_rx`.
/// `Inject` and `RunTurn` return so the outer turn loop executes exactly
/// one turn and then re-enters this function.
#[allow(clippy::too_many_arguments)]
async fn interactive_loop(
    _model: &GremlinsModel,
    history: &mut Vec<Message>,
    next_prompt: &mut Message,
    session: &mut InteractiveSession,
    turn: usize,
) -> Result<InteractiveLoopResult, ClientError> {
    log::debug!("interactive_loop: broadcasting Ready (turn={turn})");
    let _ = session.evt_tx.send(InteractiveEvent::Ready { turn });

    log::debug!("interactive_loop: waiting for command (turn={turn})");
    let cmd = match session.cmd_rx.recv().await {
        Some(cmd) => {
            log::debug!("interactive_loop: received command {cmd:?} (turn={turn})");
            cmd
        }
        None => {
            log::debug!("interactive_loop: cmd_rx closed, ending session (turn={turn})");
            let _ = session.evt_tx.send(InteractiveEvent::Ended {
                reason: "disconnect".to_string(),
            });
            return Ok(InteractiveLoopResult::Resumed);
        }
    };

    match cmd {
        InteractiveCommand::Inject(text) => {
            log::debug!(
                "interactive_loop: Inject command (len={}, turn={turn})",
                text.len()
            );
            let msg = format!("[operator]: {text}");
            history.push(Message::user(msg));
            *next_prompt = Message::user(text);
            Ok(InteractiveLoopResult::RunOneTurn)
        }
        InteractiveCommand::RunTurn => {
            log::debug!("interactive_loop: RunTurn command (turn={turn})");
            Ok(InteractiveLoopResult::RunOneTurn)
        }
        InteractiveCommand::Bail(reason) => {
            log::debug!("interactive_loop: Bail command (reason={reason:?}, turn={turn})");
            let _ = session.evt_tx.send(InteractiveEvent::Ended {
                reason: "bailed".to_string(),
            });
            Err(ClientError::Bail {
                reason: format!("operator bailed: {reason}"),
            })
        }
        InteractiveCommand::Quit => {
            log::debug!("interactive_loop: Quit command (turn={turn})");
            let _ = session.evt_tx.send(InteractiveEvent::Ended {
                reason: "resumed".to_string(),
            });
            Ok(InteractiveLoopResult::Resumed)
        }
    }
}

enum InteractiveLoopResult {
    /// Quit — interactive session ended, resume normal operation.
    Resumed,
    /// Run one turn, then re-enter interactive_loop.
    RunOneTurn,
}

// ── helpers for completion-decision bookkeeping ───────────────────────────

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

fn turn_stats(turn: usize, usage: Option<&Usage>) -> Option<TurnStats> {
    let u = usage?;
    // `response.partial().usage` is a bare `Usage` struct, so the stream-end
    // handler wraps it in `Some` even when the provider reported no token
    // fields at all. In that case consumers must see `None` (unavailable
    // usage), not a measured zero-filled record. Explicitly reported zeros
    // are preserved because only the all-absent case is filtered out.
    if u.input_tokens.is_none()
        && u.output_tokens.is_none()
        && u.cached_input_tokens.is_none()
        && u.cache_creation_input_tokens.is_none()
        && u.reasoning_tokens.is_none()
    {
        return None;
    }
    Some(TurnStats {
        turn,
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        cached_input_tokens: u.cached_input_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        reasoning_tokens: u.reasoning_tokens,
    })
}

fn completed_run(
    text: Option<String>,
    summary: Option<String>,
    events: &Option<Vec<serde_json::Value>>,
    usage: UsageStats,
    messages: Option<Vec<Message>>,
) -> CompletedRun {
    CompletedRun {
        exit_code: 0,
        text_result: text,
        summary,
        events: events.clone(),
        cost_usd: None,
        token_usage: Some(usage),
        messages,
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

fn user_message_text(message: &Message) -> String {
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

/// Build the message list for the next `CompletionRequest`.
///
/// `history` is the verbatim source of truth and is never mutated here. When
/// compaction is enabled, a cached `CompactedWorking` keeps the request prefix
/// byte-stable across rounds; a new pass is triggered only when the current
/// materialized view exceeds the soft threshold.
fn build_request_messages(
    history: &[Message],
    compacted: &mut Option<conversation::CompactedWorking>,
    current_turn_start: usize,
    opts: Option<conversation::CompactionOpts>,
    overhead: usize,
) -> Vec<Message> {
    let Some(opts) = opts else {
        return history.to_vec();
    };

    if let Some(cached) = compacted.as_ref() {
        if cached.cut > history.len() {
            // History was popped (for example a mid-tool pause re-run); the
            // old cut no longer points into the verbatim transcript.
            *compacted = None;
        }
    }

    let working = match compacted.as_ref() {
        Some(cached) => cached.materialize(history),
        None => history.to_vec(),
    };

    if conversation::serialized_len(&working) + overhead <= opts.threshold_bytes as usize {
        return working;
    }

    let granularity = if history.len() > current_turn_start {
        conversation::CutGranularity::Round { current_turn_start }
    } else {
        conversation::CutGranularity::Turn
    };

    if let Some(fresh) =
        conversation::compact_working(history, opts.target_bytes, overhead, granularity)
    {
        let request = fresh.materialize(history);
        *compacted = Some(fresh);
        request
    } else {
        // The newest round alone exceeds the target (or the transcript has no
        // valid cut). Return the input unchanged and let the hard
        // `max-prompt-bytes` ceiling fail the request; cutting mid-round would
        // orphan an assistant tool-use from its results.
        working
    }
}

// ── agent loop ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_agent_loop_core(
    model: &GremlinsModel,
    prompt: &str,
    system_prompt: Option<String>,
    tool_ctx: &AgentLoopRuntime,
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
    log_tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
    interactive: Option<InteractiveSession>,
    tool_output_rx: Option<mpsc::Receiver<tools::ToolOutputLine>>,
    prepopulated_messages: Option<Vec<Message>>,
    compaction_opts: Option<conversation::CompactionOpts>,
) -> Result<CompletedRun, ClientError> {
    // Destructure the interactive session into its components so we can
    // thread them independently through the loop.
    let (mut cmd_rx, mut evt_tx, pause) = interactive
        .map(|s| {
            log::debug!("agent_loop: interactive session set up (label={prefix})");
            (Some(s.cmd_rx), Some(s.evt_tx), Some(s.pause))
        })
        .unwrap_or((None, None, None));

    let original_system_prompt = system_prompt.clone();
    let mut system_prompt = system_prompt;
    let mut live_output_rx = tool_output_rx;
    let mut history: Vec<Message> = prepopulated_messages.unwrap_or_default();
    let mut next_prompt = Message::user(prompt.to_string());
    // `history` is the verbatim source of truth for the whole loop. The
    // compacted request view is rebuilt around it but never replaces it.
    let current_turn_start = history.len();
    let mut compacted: Option<conversation::CompactedWorking> = None;
    let mut turns: usize = 0;
    let mut turn_num: usize = 0;
    let mut turn_retries: usize = 0;
    let mut timeout_retries: usize = 0;
    let mut final_text = String::new();
    let mut timed_out = false;
    let mut exhausted_turn_retries: bool = false;
    let mut exhausted_timeout_retries: bool = false;
    let mut stream_error: Option<ProviderError> = None;
    let loop_start = Instant::now();

    // Accumulated token totals (summed across turns)
    let mut total_prompt_tokens: Option<u64> = None;
    let mut total_completion_tokens: Option<u64> = None;
    let mut total_cached_tokens: Option<u64> = None;
    let mut total_cache_creation_tokens: Option<u64> = None;
    let mut total_reasoning_tokens: Option<u64> = None;

    struct Job {
        id: String,
        name: String,
        args: String,
        call: String,
        key: String,
        over_cap: bool,
    }

    let mut interactive_active: bool = false;
    let mut pending_run_one_turn: bool = false;

    let turn_retry_backoff = crate::config::turn_retry_backoff();
    let timeout_retry_backoff = crate::config::timeout_retry_backoff();

    let mut remaining_turns = max_turns;
    while remaining_turns > 0 {
        if cancel.is_cancelled() {
            log::info!("agent_loop: cancelled before turn (label={})", prefix);
            send_log(log_tx, prefix, "exit: cancelled before turn");
            return Err(cancelled_error("cancelled", &history));
        }

        // ── interactive turn boundary ──────────────────────────────────
        //
        // Two modes:
        // 1. No active interactive session — check pause token.
        // 2. Active interactive session (re-entering after RunOneTurn) —
        //    enter interactive_loop immediately.
        if cmd_rx.is_some() && evt_tx.is_some() && pause.is_some() {
            if pending_run_one_turn {
                // Came back from a mid-stream or mid-tool pause with
                // RunOneTurn. Execute one turn, then re-enter interactive.
                pending_run_one_turn = false;
                interactive_active = true;
                // Fall through to execute one turn.
            } else if interactive_active {
                // Re-enter interactive_loop after a RunOneTurn/Inject.
                let mut session = InteractiveSession {
                    cmd_rx: cmd_rx.take().unwrap(),
                    evt_tx: evt_tx.take().unwrap(),
                    pause: pause.clone().unwrap(),
                };
                match interactive_loop(
                    model,
                    &mut history,
                    &mut next_prompt,
                    &mut session,
                    turn_num,
                )
                .await
                {
                    Ok(InteractiveLoopResult::Resumed) => {
                        interactive_active = false;
                        system_prompt = original_system_prompt.clone();
                        cmd_rx = Some(session.cmd_rx);
                        evt_tx = Some(session.evt_tx);
                        continue;
                    }
                    Ok(InteractiveLoopResult::RunOneTurn) => {
                        cmd_rx = Some(session.cmd_rx);
                        evt_tx = Some(session.evt_tx);
                        // Fall through to execute one turn.
                    }
                    Err(e) => {
                        log::info!("agent_loop: exit (operator bailed, label={})", prefix);
                        send_log(log_tx, prefix, "exit: operator bailed");
                        return Err(e);
                    }
                }
            } else if pause.as_ref().is_some_and(|p| p.is_paused()) {
                // Pause token was triggered — enter interactive mode.
                log::info!(
                    "agent_loop: pause token detected at turn boundary (label={})",
                    prefix
                );
                send_log(
                    log_tx,
                    prefix,
                    "pause: entering interactive mode (turn boundary)",
                );
                if let Some(ref p) = pause {
                    p.reset();
                }

                // Amend system prompt with operator note.
                let debug_note = "\n\nThe operator has connected in debug mode. Messages prefixed with\n[operator]: are direct instructions from the operator. Treat them as\nauthoritative. When the operator disconnects, continue with your\noriginal task.";
                let amended_system = match &system_prompt {
                    Some(sp) => format!("{sp}{debug_note}"),
                    None => debug_note.trim_start().to_string(),
                };
                system_prompt = Some(amended_system);

                let mut session = InteractiveSession {
                    cmd_rx: cmd_rx.take().unwrap(),
                    evt_tx: evt_tx.take().unwrap(),
                    pause: pause.clone().unwrap(),
                };
                match interactive_loop(
                    model,
                    &mut history,
                    &mut next_prompt,
                    &mut session,
                    turn_num,
                )
                .await
                {
                    Ok(InteractiveLoopResult::Resumed) => {
                        system_prompt = original_system_prompt.clone();
                        cmd_rx = Some(session.cmd_rx);
                        evt_tx = Some(session.evt_tx);
                        continue;
                    }
                    Ok(InteractiveLoopResult::RunOneTurn) => {
                        interactive_active = true;
                        cmd_rx = Some(session.cmd_rx);
                        evt_tx = Some(session.evt_tx);
                        // Fall through to execute one turn.
                    }
                    Err(e) => {
                        log::info!("agent_loop: exit (operator bailed, label={})", prefix);
                        send_log(log_tx, prefix, "exit: operator bailed");
                        return Err(e);
                    }
                }
            }
        }

        // Snapshot before request construction so TTFT includes connection /
        // queue latency, not just server-side time-to-first-token.
        let turn_start = Instant::now();

        let next_prompt_text = user_message_text(&next_prompt);
        let overhead = conversation::completion_request_overhead(
            &next_prompt_text,
            system_prompt.as_deref(),
            tool_defs,
        );
        let request_messages = build_request_messages(
            &history,
            &mut compacted,
            current_turn_start,
            compaction_opts,
            overhead,
        );

        let mut builder = CompletionRequest::new(next_prompt.clone())
            .messages(request_messages)
            .tools(tool_defs.to_vec());
        if let Some(ref sys) = system_prompt {
            builder = builder.preamble(sys.clone());
        }
        if let Some(params) = opts.extra.clone() {
            builder = builder.additional_params(params);
        }
        if let Some(t) = opts.temperature {
            builder = builder.temperature(t);
        }
        if let Some(mt) = opts.max_tokens {
            builder = builder.max_tokens(mt);
        }

        let mut response = match model.stream(builder) {
            Ok(s) => s,
            Err(GremlinsModelError::TooLarge { limit, actual }) => {
                log::info!(
                    "agent_loop: prompt too large on turn {turn_num}: {actual} bytes exceeds configured limit of {limit} bytes"
                );
                send_log(
                    log_tx,
                    prefix,
                    &format!(
                        "error: prompt size {actual} bytes exceeds configured limit of {limit} bytes"
                    ),
                );
                return Err(ClientError::PromptTooLarge { limit, actual });
            }
            Err(GremlinsModelError::Provider(e)) => {
                let e = *e;
                // Retryable (429/5xx) error before streaming began — back off
                // and re-execute this same turn with history intact.
                if turn_retries < turn_retry_backoff.len() && is_retryable(&e, opts) {
                    let wait = turn_retry_backoff[turn_retries];
                    turn_retries += 1;
                    log::info!(
                        "agent_loop: retryable stream error on turn {turn_num} (attempt {turn_retries}), backing off {wait}s: {e}"
                    );
                    send_log(
                        log_tx,
                        prefix,
                        &format!("retry: transient error, backing off {wait}s"),
                    );
                    if retry_backoff(cancel, wait).await {
                        log::info!(
                            "agent_loop: cancelled during retry backoff (label={})",
                            prefix
                        );
                        send_log(log_tx, prefix, "exit: cancelled during retry backoff");
                        return Err(cancelled_error("cancelled", &history));
                    }
                    // Preserve the pending one-turn state only when
                    // already interactive — otherwise an unconditional
                    // set would force RunOneTurn after normal resumption.
                    if interactive_active {
                        pending_run_one_turn = true;
                    }
                    continue;
                }
                let retryable = is_retryable(&e, opts);
                if retryable {
                    exhausted_turn_retries = true;
                }
                stream_error = Some(e);
                break;
            }
        };

        let mut first_token: Option<Instant> = None;
        let mut last_token: Option<Instant> = None;

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut reasoning_items: Vec<Sealed<Reasoning>> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut ended = false;
        let mut turn_usage: Option<Usage> = None;
        let mut paused_mid_stream = false;
        let mut retry_turn = false;
        let mut retry_timeout_turn = false;

        // Build a cancellation snapshot that includes the pending
        // `next_prompt` and (as streaming progresses) accumulated
        // assistant content.  Plain `history` omits both because they
        // are only committed after a successful turn.
        let mut stream_snapshot: Vec<Message> = {
            let mut s = history.clone();
            s.push(next_prompt.clone());
            s
        };

        loop {
            let item = tokio::select! {
                _ = cancel.cancelled() => {
                    log::info!("agent_loop: cancelled mid-stream (label={})", prefix);
                    send_log(log_tx, prefix, "exit: cancelled mid-stream");
                    drop(response);
                    return Err(cancelled_error("cancelled", &stream_snapshot));
                }
                _ = maybe_pause(&pause) => {
                    // Operator triggered debug — cancel the stream and enter
                    // interactive mode. The turn will be re-executed after
                    // the debug session ends.
                    log::info!("agent_loop: paused mid-stream (label={})", prefix);
                    send_log(log_tx, prefix, "pause: entering interactive mode (mid-stream)");
                    drop(response);
                    paused_mid_stream = true;
                    if let Some(ref p) = pause {
                        p.reset();
                    }
                    log::debug!("agent_loop: entering interactive mode mid-stream (label={})", prefix);

                    // Amend system prompt with operator note (same as turn-boundary path).
                    let debug_note = "\n\nThe operator has connected in debug mode. Messages prefixed with\n[operator]: are direct instructions from the operator. Treat them as\nauthoritative. When the operator disconnects, continue with your\noriginal task.";
                    let amended_system = match &system_prompt {
                        Some(sp) => format!("{sp}{debug_note}"),
                        None => debug_note.trim_start().to_string(),
                    };
                    system_prompt = Some(amended_system);

                    // Enter interactive mode inline.
                    if cmd_rx.is_some() && evt_tx.is_some() && pause.is_some() {
                        let mut session = InteractiveSession {
                            cmd_rx: cmd_rx.take().unwrap(),
                            evt_tx: evt_tx.take().unwrap(),
                            pause: pause.clone().unwrap(),
                        };
                        match interactive_loop(
                            model,
                            &mut history,
                            &mut next_prompt,
                            &mut session,
                            turn_num,
                        )
                        .await
                        {
                            Ok(InteractiveLoopResult::Resumed) => {
                                system_prompt = original_system_prompt.clone();
                                cmd_rx = Some(session.cmd_rx);
                                evt_tx = Some(session.evt_tx);
                                interactive_active = false;
                                // Re-build the request from the original next_prompt
                                // (still available — the builder clones it).
                                break; // exit the stream loop, re-enter the turn loop
                            }
                            Ok(InteractiveLoopResult::RunOneTurn) => {
                                cmd_rx = Some(session.cmd_rx);
                                evt_tx = Some(session.evt_tx);
                                pending_run_one_turn = true;
                                break; // exit the stream loop, execute one turn
                            }
                            Err(e) => {
                                log::info!("agent_loop: exit (operator bailed, label={})", prefix);
                                send_log(log_tx, prefix, "exit: operator bailed");
                                return Err(e);
                            }
                        }
                    }
                    // No interactive session — shouldn't happen, but continue.
                    break;
                }
                timed = tokio::time::timeout(
                    Duration::from_secs_f64(idle_timeout),
                    futures::StreamExt::next(&mut response),
                ) => timed,
            };
            match item {
                Err(_) => {
                    if timeout_retries < timeout_retry_backoff.len() {
                        let wait = timeout_retry_backoff[timeout_retries];
                        timeout_retries += 1;
                        log::info!(
                            "agent_loop: idle timeout on turn {turn_num} (attempt {timeout_retries}), backing off {wait}s"
                        );
                        send_log(
                            log_tx,
                            prefix,
                            &format!("retry: idle timeout, backing off {wait}s"),
                        );
                        drop(response);
                        retry_timeout_turn = true;
                        if retry_backoff(cancel, wait).await {
                            log::info!(
                                "agent_loop: cancelled during timeout backoff (label={})",
                                prefix
                            );
                            send_log(log_tx, prefix, "exit: cancelled during timeout backoff");
                            return Err(cancelled_error("cancelled", &history));
                        }
                        break;
                    }
                    exhausted_timeout_retries = true;
                    timed_out = true;
                    drop(response);
                    break;
                }
                Ok(None) => {
                    ended = true;
                    turn_usage = Some(response.partial().usage);
                    break;
                }
                Ok(Some(Err(e))) => {
                    // Retryable (429/5xx) error delivered mid-stream — back off
                    // and re-execute this same turn with history intact.
                    if turn_retries < turn_retry_backoff.len() && is_retryable(&e, opts) {
                        let wait = turn_retry_backoff[turn_retries];
                        turn_retries += 1;
                        log::info!(
                            "agent_loop: retryable mid-stream error on turn {turn_num} (attempt {turn_retries}), backing off {wait}s: {e}"
                        );
                        send_log(
                            log_tx,
                            prefix,
                            &format!("retry: transient error, backing off {wait}s"),
                        );
                        drop(response);
                        retry_turn = true;
                        if retry_backoff(cancel, wait).await {
                            log::info!(
                                "agent_loop: cancelled during retry backoff (label={})",
                                prefix
                            );
                            send_log(log_tx, prefix, "exit: cancelled during retry backoff");
                            return Err(cancelled_error("cancelled", &history));
                        }
                        break;
                    }
                    let retryable = is_retryable(&e, opts);
                    if retryable {
                        exhausted_turn_retries = true;
                    }
                    stream_error = Some(e);
                    break;
                }
                Ok(Some(Ok(chunk))) => {
                    let now = Instant::now();
                    if first_token.is_none() {
                        first_token = Some(now);
                    }
                    last_token = Some(now);
                    // Emit stream deltas via tool_ctx.telemetry.stream_events channel.
                    // Nested loops must not emit text/reasoning deltas on the
                    // parent's stream — their output arrives as tool results.
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
                                content: AssistantContent::ToolCall(tc),
                                ..
                            }) => {
                                // Emit tool calls as reasoning so they
                                // appear in the live thought stream.
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
                    // Keep the cancellation snapshot current with whatever
                    // the model has produced so far so the supervisor can
                    // reconstruct the full partial turn.
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
                        ));
                    }
                }
            }
        }

        if paused_mid_stream {
            // Stream was interrupted by pause — skip post-stream processing.
            // The turn loop will re-enter at the top and either continue
            // (if Resumed) or execute one turn (if RunOneTurn).
            continue;
        }

        if retry_turn {
            // Stream was interrupted by a retryable error — re-execute this
            // same turn. History and next_prompt are untouched (the builder
            // cloned both), so the retry picks up exactly where it left off.
            // Preserve the pending one-turn state only when already
            // interactive — otherwise an unconditional set would force
            // RunOneTurn after normal resumption.
            if interactive_active {
                pending_run_one_turn = true;
            }

            // Send StreamReset so consumers discard partial text /
            // reasoning from the failed attempt. Nested loops skip this —
            // only the parent stream should be reset.
            if !nested {
                tool_ctx
                    .telemetry
                    .send_stream_event(InteractiveEvent::StreamReset, cancel, &history)
                    .await?;
            }
            continue;
        }

        if retry_timeout_turn {
            // Stream was interrupted by an idle timeout — re-execute this
            // same turn with history intact.
            continue;
        }

        if timed_out || stream_error.is_some() {
            log::debug!(
                "stream ended: timed_out={timed_out} stream_error={stream_error:?} turn={turn_num}",
            );
            break;
        }
        if !ended && tool_calls.is_empty() && text.is_empty() {
            log::debug!("stream ended: not-ended empty-turn turn={turn_num}",);
            break;
        }

        log::debug!(
            "turn complete: turn={turn_num} text_len={} reasoning_len={} tool_calls={} ended={ended}",
            text.len(),
            reasoning.len(),
            tool_calls.len(),
        );

        // Fold this turn's provider usage into the running totals before
        // building TurnComplete so the per-prompt totals include the
        // just-completed turn. Keep `turn_stats` None when the provider
        // reported no usage (or the turn completed without a usage block).
        // Absent fields stay `None` and are skipped during accumulation; a
        // reported zero is preserved as `Some(0)`.
        let turn_stats = turn_stats(turn_num, turn_usage.as_ref());
        if let Some(ref u) = turn_usage {
            // Sum two present values, but keep the single `Some` value when
            // either side is absent. Both sides `None` keeps the total absent.
            total_prompt_tokens = add_opt(total_prompt_tokens, u.input_tokens);
            total_completion_tokens = add_opt(total_completion_tokens, u.output_tokens);
            total_cached_tokens = add_opt(total_cached_tokens, u.cached_input_tokens);
            total_cache_creation_tokens =
                add_opt(total_cache_creation_tokens, u.cache_creation_input_tokens);
            total_reasoning_tokens = add_opt(total_reasoning_tokens, u.reasoning_tokens);
        }
        let totals = usage_stats(
            turn_num + 1,
            total_prompt_tokens,
            total_completion_tokens,
            total_cached_tokens,
            total_cache_creation_tokens,
            total_reasoning_tokens,
        );

        emit_turn_stats(prefix, turn_num, turn_stats.as_ref(), &totals, log_tx);

        // Emit TurnComplete via tool_ctx.telemetry.stream_events, before text/tool_calls
        // are consumed by the rest of the turn processing. Nested loops skip
        // this — their child turn numbers restart at 0 and can collide with
        // the parent's.
        if !nested && tool_ctx.telemetry.wants_stream_events() {
            log::debug!("agent_loop: broadcasting TurnComplete (turn={turn_num})");
            tool_ctx
                .telemetry
                .send_stream_event(
                    InteractiveEvent::TurnComplete {
                        turn: turn_num,
                        text: text.clone(),
                        tool_calls: tool_calls.iter().map(format_tool_call).collect(),
                        stats: turn_stats.clone(),
                        totals: Some(totals),
                    },
                    cancel,
                    &history,
                )
                .await?;
        }

        if !reasoning.is_empty() {
            let msg = format!("think: {}", trunc(&reasoning, 200));
            send_log(log_tx, prefix, &msg);
        }
        if !text.is_empty() {
            let msg = format!("text: {}", trunc(&text, 200));
            send_log(log_tx, prefix, &msg);
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

        emit_turn_metrics(
            prefix,
            turn_num,
            first_token,
            last_token,
            turn_start,
            &reasoning,
            &text,
            &tool_calls,
            turn_usage.as_ref(),
            log_tx,
        );

        turn_num += 1;
        turn_retries = 0;
        timeout_retries = 0;

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
                history.push(assistant_tool_message(&text, &reasoning_items, &tool_calls));
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
                    history.push(Message::tool_result(
                        tc.id.clone(),
                        tc.function.name.clone(),
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
                // Extract the summary up front so it can be logged and recorded
                // in the action ledger whether the call is accepted or rejected
                // below (Done never reaches the normal tool-execution path).
                let done_summary = done_tc
                    .function
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                send_log(
                    log_tx,
                    prefix,
                    &format!("tool: Done {}", trunc(&done_summary, 200)),
                );
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
                            history.push(assistant_tool_message(
                                &text,
                                &reasoning_items,
                                std::slice::from_ref(done_tc),
                            ));
                            history.push(Message::tool_result(
                                done_tc.id.clone(),
                                done_tc.function.name.clone(),
                                reminder.clone(),
                            ));
                            next_prompt = Message::user(format!(
                                "Actions taken this turn:\n- Done {} (rejected)\n\n{}",
                                done_summary, reminder
                            ));
                            continue;
                        }
                        // Budget exhausted — accept anyway. Python
                        // verify_produced catches missing files.
                        history.push(next_prompt.clone());
                        history.push(assistant_tool_message(
                            &text,
                            &reasoning_items,
                            std::slice::from_ref(done_tc),
                        ));
                    }
                }

                // Accept completion.
                let result_text = if text.trim().is_empty() && !done_summary.is_empty() {
                    done_summary.clone()
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
                    emit_final(prefix, turns, "", log_tx);
                    emit_summary(
                        prefix,
                        turn_num,
                        loop_start,
                        total_prompt_tokens,
                        total_completion_tokens,
                        total_cached_tokens,
                        total_cache_creation_tokens,
                        total_reasoning_tokens,
                        log_tx,
                    );
                }
                // Emit Done via stream_events. Nested loops skip this —
                // only the parent's Done signals completion.
                if !nested && tool_ctx.telemetry.wants_stream_events() {
                    log::debug!("agent_loop: broadcasting Done (turn={turn_num})");
                    tool_ctx
                        .telemetry
                        .send_stream_event(
                            InteractiveEvent::Done {
                                text: result_text.clone(),
                                summary: done_summary.clone(),
                                usage: Some(usage.clone()),
                            },
                            cancel,
                            &history,
                        )
                        .await?;
                }
                // Capture the full message history (prior turns + this final
                // user prompt + assistant message) so the chat gremlin can
                // persist complete model context between stages.
                //
                // Push a tool_result for Done so the next turn's history
                // starts from a valid round — OpenAI and Anthropic require
                // every assistant tool-use message be paired with a matching
                // tool-result message.
                let mut messages = history.clone();
                messages.push(next_prompt.clone());
                messages.push(assistant_tool_message(
                    &text,
                    &reasoning_items,
                    std::slice::from_ref(done_tc),
                ));
                // Prepend the action ledger so Done is visible in persisted history.
                let done_ledger = format!("Actions taken this turn:\n- Done {}\n", done_summary);
                messages.push(Message::user(done_ledger));
                messages.push(Message::tool_result(
                    done_tc.id.clone(),
                    done_tc.function.name.clone(),
                    "Done accepted.",
                ));
                return Ok(completed_run(
                    Some(result_text),
                    Some(done_summary),
                    captured,
                    usage,
                    Some(messages),
                ));
            }
            _ => {}
        }

        // Done handled — filter it out before execution.
        let tool_calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|tc| tc.function.name != "Done")
            .collect();

        if tool_calls.is_empty() {
            // In interactive mode, empty turns are expected — the operator is
            // driving the conversation. Don't nudge or remind.
            if interactive_active {
                continue;
            }

            // Reasoning-only turn — the model is thinking. Just loop.
            // Record an assistant message carrying the structured reasoning
            // items so the next request preserves them (and the provider's
            // ordering invariant) rather than emitting two consecutive user
            // messages with the reasoning dropped.
            if text.is_empty() && !reasoning.is_empty() {
                log::debug!("reasoning-only turn: turn={turn_num} — continuing",);
                history.push(next_prompt);
                history.push(assistant_tool_message("", &reasoning_items, &[]));
                next_prompt = Message::user("Continue.");
                continue;
            }

            log::warn!(
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
                        "reminder: {} missing artifact(s) — nudging agent (remaining_budget={})",
                        missing.len(),
                        reminder_budget,
                    );
                    history.push(next_prompt);
                    history.push(assistant_tool_message(&text, &reasoning_items, &[]));
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
                history.push(assistant_tool_message(&text, &reasoning_items, &[]));
                next_prompt = Message::user(nudge_msg.to_string());
                continue;
            }
            // Exhausted — give up.
            log::warn!(
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
                emit_final(prefix, turns, " (exhausted)", log_tx);
                emit_summary(
                    prefix,
                    turn_num,
                    loop_start,
                    total_prompt_tokens,
                    total_completion_tokens,
                    total_cached_tokens,
                    total_cache_creation_tokens,
                    total_reasoning_tokens,
                    log_tx,
                );
            }
            // Capture the full message history (prior turns + this final
            // user prompt + assistant text) so the chat gremlin persists
            // complete model context even on the exhausted path.
            let mut messages = history.clone();
            messages.push(next_prompt.clone());
            messages.push(assistant_tool_message(&text, &reasoning_items, &[]));
            return Ok(completed_run(
                Some(final_text),
                None,
                captured,
                usage,
                Some(messages),
            ));
        }

        let fork_batch = tool_calls
            .iter()
            .any(|tc| tc.function.name == "Task" && task_kind_is_fork(&tc.function.arguments));
        // A Fork child inherits the conversation that led up to this turn,
        // including the user message being answered. The current assistant
        // tool-call message is intentionally *not* part of the snapshot:
        // including it without its still-unknown tool results would leave an
        // unmatched assistant tool-use in the child's transcript.
        let fork_parent_history = fork_batch.then(|| {
            let mut snapshot = history.clone();
            snapshot.push(next_prompt.clone());
            snapshot
        });

        history.push(next_prompt.clone());
        history.push(assistant_tool_message(&text, &reasoning_items, &tool_calls));

        // Phase 1: emit tool-start events, collect owned data for concurrent execution
        let mut jobs: Vec<Job> = Vec::new();
        let mut task_count: usize = 0;
        for tc in &tool_calls {
            let args_json =
                serde_json::to_string(&tc.function.arguments).unwrap_or_else(|_| "{}".into());
            let tool_msg = format!(
                "tool: {} {}",
                tc.function.name,
                trunc(
                    &enrich_log_line(&tc.function.name, &tc.function.arguments),
                    200
                )
            );
            send_log(log_tx, prefix, &tool_msg);
            if !nested {
                let id_str = tc.id.to_string();
                let tool_evt =
                    tool_use_event(&id_str, tc.function.name.as_str(), &tc.function.arguments);
                write_raw(raw, &tool_evt);
                if let Some(evts) = captured.as_mut() {
                    evts.push(tool_evt);
                }
            }
            jobs.push(Job {
                id: tc.id.to_string(),
                name: tc.function.name.to_string(),
                args: args_json,
                call: format_tool_call_parts(&tc.function.name, &tc.function.arguments),
                key: ledger_key_arg(&tc.function.arguments),
                over_cap: tc.function.name == "Task" && {
                    task_count += 1;
                    task_count > tools::TASK_MAX_PER_TURN
                },
            });
        }

        // Phase 2: concurrent execution. Over-cap Task calls short-circuit so a
        // single response cannot spawn an unbounded number of child loops.
        // Wrapped in a tokio::select! with maybe_pause so the operator can
        // interrupt a long-running tool batch.
        let mut ctx = tool_ctx.clone();
        ctx.parent_history = fork_parent_history;
        // Forward live tool output only for the duration of this batch. The
        // receiver is borrowed by the select below, so later batches never
        // hear stale chunks and Esc/clear cannot resurrect old output.
        // Capacity is reserved on the chat stream *before* a tool line is
        // dequeued, so the select dropping this branch at batch completion
        // can release an unused permit but can never drop an in-flight line.
        let forward_live_output = async {
            if let (Some(se), Some(rx)) =
                (&tool_ctx.telemetry.stream_events, live_output_rx.as_mut())
            {
                if se.wants_events() {
                    loop {
                        let permit = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => break,
                            permit = se.reserve() => permit,
                        };
                        let Some(line) = rx.recv().await else {
                            break;
                        };
                        permit.send(InteractiveEvent::ToolOutputChunk {
                            id: line.id,
                            name: line.name,
                            call: line.call,
                            text: line.text,
                        });
                    }
                } else {
                    // Drain the channel so the sender doesn't block when
                    // the broadcast subscriber has disconnected.
                    while rx.recv().await.is_some() {}
                }
            }
            // Park forever so this branch never resolves the select; the
            // sender intentionally outlives an individual tool batch.
            std::future::pending::<()>().await;
        };
        let exec_fut = join_all(jobs.iter().map(|j| {
            let ctx = &ctx;
            async move {
                if j.over_cap {
                    format!(
                        "Error: at most {} Task calls per message",
                        tools::TASK_MAX_PER_TURN
                    )
                } else {
                    tools::invoke(&j.id, &j.name, ctx, &j.args).await
                }
            }
        }));

        let results = tokio::select! {
            results = exec_fut => results,
            _ = forward_live_output => unreachable!("live-output forwarder parks forever"),
            _ = cancel.cancelled() => {
                // Esc / cancel_chat during a tool batch: drop the join_all
                // future (cancelling pending tool futures) and unwind the
                // turn immediately. This is what lets run_chat_turn::finish
                // re-insert the chat gremlin without waiting for every tool
                // to complete.
                log::info!("agent_loop: cancelled mid-tool (label={})", prefix);
                send_log(log_tx, prefix, "exit: cancelled mid-tool");
                discard_tool_output(&mut live_output_rx);
                return Err(cancelled_error("cancelled", &history));
            }
            _ = maybe_pause(&pause) => {
                // Operator triggered debug mid-tool — drop the join_all future
                // (cancels pending tool futures). Completed tool side effects
                // (files written, bash commands run) are not unwound.
                log::info!("agent_loop: paused mid-tool (label={})", prefix);
                send_log(log_tx, prefix, "pause: entering interactive mode (mid-tool)");
                discard_tool_output(&mut live_output_rx);
                if let Some(ref p) = pause {
                    p.reset();
                }
                log::debug!("agent_loop: entering interactive mode mid-tool (label={})", prefix);

                // Pop the two messages pushed at lines 1005-1006
                // (next_prompt + assistant_tool_message) so the turn can be
                // re-executed cleanly without duplicate history entries.
                history.pop(); // assistant_tool_message
                history.pop(); // next_prompt

                // Amend system prompt with operator note.
                let debug_note = "\n\nThe operator has connected in debug mode. Messages prefixed with\n[operator]: are direct instructions from the operator. Treat them as\nauthoritative. When the operator disconnects, continue with your\noriginal task.";
                let amended_system = match &system_prompt {
                    Some(sp) => format!("{sp}{debug_note}"),
                    None => debug_note.trim_start().to_string(),
                };
                system_prompt = Some(amended_system);

                // Enter interactive mode.
                if cmd_rx.is_some() && evt_tx.is_some() && pause.is_some() {
                    let mut session = InteractiveSession {
                        cmd_rx: cmd_rx.take().unwrap(),
                        evt_tx: evt_tx.take().unwrap(),
                        pause: pause.clone().unwrap(),
                    };
                    match interactive_loop(
                        model,
                        &mut history,
                        &mut next_prompt,
                        &mut session,
                        turn_num,
                    )
                    .await
                    {
                        Ok(InteractiveLoopResult::Resumed) => {
                            system_prompt = original_system_prompt.clone();
                            cmd_rx = Some(session.cmd_rx);
                            evt_tx = Some(session.evt_tx);
                            interactive_active = false;
                            continue; // re-execute the turn from scratch
                        }
                        Ok(InteractiveLoopResult::RunOneTurn) => {
                            cmd_rx = Some(session.cmd_rx);
                            evt_tx = Some(session.evt_tx);
                            pending_run_one_turn = true;
                            continue; // re-execute the turn from scratch
                        }
                        Err(e) => {
                            log::info!("agent_loop: exit (operator bailed, label={})", prefix);
                            send_log(log_tx, prefix, "exit: operator bailed");
                            return Err(e);
                        }
                    }
                }
                continue;
            }
        };

        // Tools send their final chunks just before returning. The select
        // above stops at the first completed future, so drain anything still
        // sitting in the receiver before emitting ToolResult events.
        if let Some(rx) = live_output_rx.as_mut() {
            while let Ok(line) = rx.try_recv() {
                tool_ctx
                    .telemetry
                    .send_stream_event(
                        InteractiveEvent::ToolOutputChunk {
                            id: line.id,
                            name: line.name,
                            call: line.call,
                            text: line.text,
                        },
                        cancel,
                        &history,
                    )
                    .await?;
            }
        }

        // Phase 3: emit results in order
        let mut result_msgs = Vec::new();
        let mut ledger = Vec::new();
        for (job, output) in jobs.into_iter().zip(results) {
            let result_msg = format!("result: {}", trunc(&output, 200));
            send_log(log_tx, prefix, &result_msg);
            if !nested {
                let result_evt = tool_result_event(&job.id, &output);
                write_raw(raw, &result_evt);
                if let Some(evts) = captured.as_mut() {
                    evts.push(result_evt);
                }
            }
            // Emit tool result via stream_events when there is a listener.
            if tool_ctx.telemetry.wants_stream_events() {
                // For Read, truncate to a reasonable preview.
                let display = if job.name == "Read" {
                    trunc(&output, 500)
                } else {
                    trunc(&output, 200)
                };
                tool_ctx
                    .telemetry
                    .send_stream_event(
                        InteractiveEvent::ToolResult {
                            id: format!("{}{}", tool_ctx.id_prefix, job.id),
                            name: job.name.clone(),
                            call: job.call.clone(),
                            output: display,
                        },
                        cancel,
                        &history,
                    )
                    .await?;
            }
            ledger.push(ledger_line(&job.name, &job.key, &output));
            result_msgs.push(Message::tool_result(
                CallId::from_wire(job.id.clone()),
                ToolName::new(job.name.clone()).expect("tool name must not be empty"),
                output,
            ));
        }
        turns += tool_calls.len();
        // Every tool result lands in history so the results stay adjacent to the
        // assistant tool_calls message; the ledger becomes the next turn's prompt,
        // trailing the complete result block instead of splitting it.
        history.extend(result_msgs);
        next_prompt = Message::user(ledger_message(&ledger));
        remaining_turns -= 1;
    }

    if !nested {
        let suffix = if timed_out {
            " (timeout)"
        } else if stream_error.is_some() {
            " (stream-error)"
        } else {
            ""
        };
        emit_final(prefix, turns, suffix, log_tx);
        emit_summary(
            prefix,
            turn_num,
            loop_start,
            total_prompt_tokens,
            total_completion_tokens,
            total_cached_tokens,
            total_cache_creation_tokens,
            total_reasoning_tokens,
            log_tx,
        );
    }

    if timed_out {
        log::info!("agent_loop: exit (stream idle timeout, label={})", prefix);
        send_log(log_tx, prefix, "exit: stream idle timeout");
        if exhausted_timeout_retries {
            log::error!(
                "agent_loop: exhausted turn-level timeout retries, giving up (label={})",
                prefix
            );
            send_log(log_tx, prefix, "exit: exhausted turn-level timeout retries");
            return Err(ClientError::RetryExhausted {
                message: "exhausted turn-level timeout retries".into(),
            });
        }
        return Err(ClientError::Timeout {
            message: "stream idle timeout".into(),
        });
    }
    if let Some(err) = stream_error {
        log::info!("agent_loop: exit (stream error, label={})", prefix);
        send_log(log_tx, prefix, "exit: stream error");
        if exhausted_turn_retries {
            log::error!(
                "agent_loop: exhausted turn-level retries ({} attempts), giving up (label={}): {err}",
                turn_retry_backoff.len() + 1,
                prefix
            );
            send_log(log_tx, prefix, "exit: exhausted turn-level retries");
            return Err(ClientError::RetryExhausted {
                message: format!(
                    "exhausted turn-level retries ({} attempts): {err}",
                    turn_retry_backoff.len() + 1,
                ),
            });
        }
        let classify = opts.classify_error.unwrap_or(default_classify);
        return Err(classify(err));
    }
    log::info!("agent_loop: exit (exceeded max turns, label={})", prefix);
    send_log(
        log_tx,
        prefix,
        &format!("exit: exceeded max turns ({max_turns})"),
    );
    Err(ClientError::Runtime {
        message: format!("exceeded max turns ({max_turns})"),
    })
}

/// Emit a structured per-turn token/cache log line for every completed turn.
///
/// This is always logged regardless of the telemetry gate; timing and output
/// byte-ratio extras live behind the separate [`emit_turn_metrics`] path.
fn emit_turn_stats(
    prefix: &str,
    turn: usize,
    stats: Option<&TurnStats>,
    totals: &UsageStats,
    log_tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    let payload = serde_json::json!({
        "type": "turn_stats",
        "turn": turn,
        "stats": stats,
        "totals": totals,
    });
    send_log(log_tx, prefix, &payload.to_string());
}

/// Emit per-turn telemetry: timing, token counts, cache hit ratio, reasoning ratio.
#[allow(clippy::too_many_arguments)]
fn emit_turn_metrics(
    prefix: &str,
    turn: usize,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    turn_start: Instant,
    reasoning: &str,
    text: &str,
    tool_calls: &[ToolCall],
    usage: Option<&Usage>,
    log_tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    if !crate::config::telemetry_enabled() {
        return;
    }

    let ttft = first_token
        .map(|ft| ft.duration_since(turn_start))
        .map(|d| format!("{:.1}s", d.as_secs_f64()))
        .unwrap_or_else(|| "-".into());

    let gen_time = match (first_token, last_token) {
        (Some(ft), Some(lt)) => {
            let d = lt.duration_since(ft);
            format!("{:.1}s", d.as_secs_f64())
        }
        _ => "-".into(),
    };

    let prompt = usage.and_then(|u| u.input_tokens).unwrap_or(0);
    let completion = usage.and_then(|u| u.output_tokens).unwrap_or(0);
    let cached = usage.and_then(|u| u.cached_input_tokens).unwrap_or(0);
    let reasoning_tok = usage.and_then(|u| u.reasoning_tokens).unwrap_or(0);

    let cache_pct = if prompt > 0 {
        format!("{:.0}%", (cached as f64 / (prompt as f64).max(1.0)) * 100.0)
    } else {
        "-".into()
    };

    // Byte lengths (not char counts) — a cheap proxy for output volume.
    let reasoning_bytes = reasoning.len();
    let text_bytes = text.len();
    let total_bytes = reasoning_bytes + text_bytes;
    let reasoning_byte_ratio = if total_bytes > 0 {
        format!(
            "{:.0}%",
            (reasoning_bytes as f64 / total_bytes as f64) * 100.0
        )
    } else {
        "-".into()
    };

    let msg = format!(
        "metrics: turn={} ttft={} gen={} tools={} prompt={} completion={} cached={}({}) reasoning_tok={} reasoning_byte_ratio={}",
        turn,
        ttft,
        gen_time,
        tool_calls.len(),
        prompt,
        completion,
        cached,
        cache_pct,
        reasoning_tok,
        reasoning_byte_ratio,
    );
    send_log(log_tx, prefix, &msg);
}

/// Emit stage-end telemetry summary (always emitted, not gated by GREMLINS_TELEMETRY).
#[allow(clippy::too_many_arguments)]
fn emit_summary(
    prefix: &str,
    turns: usize,
    loop_start: Instant,
    total_prompt: Option<u64>,
    total_completion: Option<u64>,
    total_cached: Option<u64>,
    total_cache_creation: Option<u64>,
    total_reasoning: Option<u64>,
    log_tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    let wall = loop_start.elapsed();
    let total_prompt_u = total_prompt.unwrap_or(0);
    let total_completion_u = total_completion.unwrap_or(0);
    let token_total = total_prompt_u + total_completion_u;

    let prompt_avg = if turns > 0 {
        total_prompt_u / turns as u64
    } else {
        0
    };
    let completion_avg = if turns > 0 {
        total_completion_u / turns as u64
    } else {
        0
    };
    let cached_avg = if total_prompt_u > 0 {
        format!(
            "{:.0}%",
            (total_cached.unwrap_or(0) as f64 / total_prompt_u as f64) * 100.0
        )
    } else {
        "-".into()
    };
    // Reasoning tokens are a subset of completion/output tokens, so report the
    // ratio against completion — "how much of the model's output was reasoning".
    let reasoning_pct = if total_completion_u > 0 {
        format!(
            "{:.0}%",
            (total_reasoning.unwrap_or(0) as f64 / total_completion_u as f64) * 100.0
        )
    } else {
        "-".into()
    };

    let msg = format!(
        "summary: turns={} wall={:.1}s token_total={} prompt_avg={} completion_avg={} cached_avg={} cache_creation={} reasoning_pct={}",
        turns,
        wall.as_secs_f64(),
        token_total,
        prompt_avg,
        completion_avg,
        cached_avg,
        total_cache_creation.unwrap_or(0),
        reasoning_pct,
    );
    send_log(log_tx, prefix, &msg);
}

pub(crate) fn emit_final(
    prefix: &str,
    turns: usize,
    suffix: &str,
    log_tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    let msg = format!("final: turns={turns} cost=not-reported{suffix}");
    send_log(log_tx, prefix, &msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::display::key_arg;
    use crate::clients::interactive::InteractiveChannels;
    use http::StatusCode;
    use rig_core::driver::DynModel;
    use rig_core::message::{Issuer, Reasoning, ReasoningContent, ToolFunction, UserContent};
    use rig_core::operation::Completion;
    use std::collections::HashMap;
    use tokio::sync::broadcast::error::RecvError;

    fn gm(model: DynModel<Completion>) -> GremlinsModel {
        GremlinsModel::new(model, crate::config::DEFAULT_MAX_PROMPT_BYTES)
    }

    #[test]
    fn turn_stats_passes_through_partial_usage_without_zero_filling() {
        let usage = Usage {
            input_tokens: Some(10),
            output_tokens: None,
            cached_input_tokens: Some(4),
            cache_creation_input_tokens: None,
            reasoning_tokens: Some(3),
            tool_use_prompt_tokens: None,
            total_tokens: None,
        };

        let stats = turn_stats(1, Some(&usage)).unwrap();

        assert_eq!(stats.turn, 1);
        assert_eq!(stats.prompt_tokens, Some(10));
        assert_eq!(stats.completion_tokens, None);
        assert_eq!(stats.cached_input_tokens, Some(4));
        assert_eq!(stats.cache_creation_input_tokens, None);
        assert_eq!(stats.reasoning_tokens, Some(3));
    }

    #[test]
    fn turn_stats_none_when_usage_block_is_entirely_absent() {
        let usage = Usage {
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
            tool_use_prompt_tokens: None,
            total_tokens: None,
        };

        assert!(turn_stats(0, Some(&usage)).is_none());
        assert!(turn_stats(3, None).is_none());
    }

    #[test]
    fn default_classifier() {
        // 5xx → retryable
        let err = ProviderError::from_http_response(StatusCode::SERVICE_UNAVAILABLE, "boom");
        assert!(matches!(
            default_classify(err),
            ClientError::ApiServerError { .. }
        ));

        // 429 → retryable
        let err = ProviderError::from_http_response(StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert!(matches!(
            default_classify(err),
            ClientError::ApiServerError { .. }
        ));

        // 400 → NOT retryable
        let err = ProviderError::from_http_response(StatusCode::BAD_REQUEST, "bad prompt");
        assert!(matches!(default_classify(err), ClientError::Runtime { .. }));

        // 401 → NOT retryable
        let err = ProviderError::from_http_response(StatusCode::UNAUTHORIZED, "bad key");
        assert!(matches!(default_classify(err), ClientError::Runtime { .. }));

        // No HTTP status → retryable
        let err = ProviderError::Provider("something broke".into());
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
            ledger_key_arg(&serde_json::json!({"kind": "Scout"})),
            "Scout"
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

    #[test]
    fn format_tool_call_parts_read_with_offset_limit() {
        // Read with offset and limit.
        let args = serde_json::json!({"file_path": "src/main.rs", "offset": 100, "limit": 50});
        assert_eq!(
            format_tool_call_parts("Read", &args),
            "Read(file_path=\"src/main.rs\", offset=100, limit=50)"
        );
    }

    #[test]
    fn format_tool_call_parts_read_bare_no_secondaries() {
        // Read with no offset/limit renders same as today.
        let args = serde_json::json!({"file_path": "src/main.rs"});
        assert_eq!(
            format_tool_call_parts("Read", &args),
            "Read(file_path=\"src/main.rs\")"
        );
    }

    #[test]
    fn format_tool_call_parts_read_zero_secondaries_omitted() {
        // Read with offset=0, limit=0 — both omitted.
        let args = serde_json::json!({"file_path": "src/main.rs", "offset": 0, "limit": 0});
        assert_eq!(
            format_tool_call_parts("Read", &args),
            "Read(file_path=\"src/main.rs\")"
        );
    }

    #[test]
    fn format_tool_call_parts_grep_with_all_secondaries() {
        let args = serde_json::json!({
            "pattern": "fn run",
            "path": "src/",
            "glob": "*.rs",
            "max_matches": 50,
            "context_before": 2,
            "context_after": 3
        });
        let title = format_tool_call_parts("Grep", &args);
        assert!(title.starts_with("Grep(pattern=\"fn run\""), "{title}");
        assert!(title.contains("path=\"src/\""), "{title}");
        assert!(title.contains("glob=\"*.rs\""), "{title}");
        assert!(title.contains("max_matches=50"), "{title}");
        assert!(title.contains("context_before=2"), "{title}");
        assert!(title.contains("context_after=3"), "{title}");
    }

    #[test]
    fn format_tool_call_parts_grep_bare_pattern_only() {
        // Grep with only pattern — no secondaries.
        let args = serde_json::json!({"pattern": "fn run"});
        assert_eq!(
            format_tool_call_parts("Grep", &args),
            "Grep(pattern=\"fn run\")"
        );
    }

    #[test]
    fn format_tool_call_parts_grep_zero_secondaries_omitted() {
        // Grep with zero context values — omitted.
        let args = serde_json::json!({
            "pattern": "fn run",
            "context_before": 0,
            "context_after": 0,
            "max_matches": 0
        });
        assert_eq!(
            format_tool_call_parts("Grep", &args),
            "Grep(pattern=\"fn run\")"
        );
    }

    #[test]
    fn format_tool_call_parts_grep_empty_string_secondaries_omitted() {
        // Grep with empty glob/path — omitted.
        let args = serde_json::json!({
            "pattern": "fn run",
            "glob": "",
            "path": ""
        });
        assert_eq!(
            format_tool_call_parts("Grep", &args),
            "Grep(pattern=\"fn run\")"
        );
    }

    #[test]
    fn format_tool_call_parts_long_grep_glob_truncated() {
        let long_glob = format!("*{}.rs", "x".repeat(300));
        let args = serde_json::json!({"pattern": "fn run", "glob": &long_glob});
        let title = format_tool_call_parts("Grep", &args);
        assert!(title.starts_with("Grep(pattern=\"fn run\""), "{title}");
        assert!(title.contains("glob=\""), "{title}");
        // The glob value must be truncated; the full 300+ chars won't appear.
        assert!(
            !title.contains(&long_glob),
            "long glob must be truncated: {title}"
        );
    }

    #[test]
    fn format_tool_call_parts_uses_file_path_key() {
        // Read, Write, Edit must use file_path, not path.
        let args = serde_json::json!({"file_path": "src/main.rs"});
        assert_eq!(
            format_tool_call_parts("Read", &args),
            "Read(file_path=\"src/main.rs\")"
        );
        assert_eq!(
            format_tool_call_parts("Write", &args),
            "Write(file_path=\"src/main.rs\")"
        );
        assert_eq!(
            format_tool_call_parts("Edit", &args),
            "Edit(file_path=\"src/main.rs\")"
        );
    }

    #[test]
    fn format_tool_call_parts_landscape_and_done() {
        assert_eq!(
            format_tool_call_parts("Landscape", &serde_json::json!({"file_path": "Cargo.toml"})),
            "Landscape(file_path=\"Cargo.toml\")"
        );
        // Landscape with depth.
        assert_eq!(
            format_tool_call_parts(
                "Landscape",
                &serde_json::json!({"file_path": "src", "depth": 2})
            ),
            "Landscape(file_path=\"src\", depth=2)"
        );
        assert_eq!(
            format_tool_call_parts("Done", &serde_json::json!({"summary": "Fixed it"})),
            "Done(summary=\"Fixed it\")"
        );
        // Long Done summary is truncated.
        let long = format_tool_call_parts("Done", &serde_json::json!({"summary": "x".repeat(300)}));
        assert!(long.len() < 200, "long summary must be truncated: {long}");
        assert!(long.starts_with("Done(summary=\""));
    }

    #[test]
    fn format_tool_call_parts_no_vestigial_web_fetch() {
        // WebFetch no longer exists; unrecognized tools get bare name.
        assert_eq!(
            format_tool_call_parts("WebFetch", &serde_json::json!({"url": "https://x.com"})),
            "WebFetch"
        );
    }

    #[test]
    fn enrich_log_line_secondary_args() {
        // Read with offset and limit.
        let line = enrich_log_line(
            "Read",
            &serde_json::json!({"file_path": "foo.rs", "offset": 100, "limit": 50}),
        );
        assert!(line.contains("offset=100"), "{line}");
        assert!(line.contains("limit=50"), "{line}");

        // Read with zero offset/limit — omitted.
        let line = enrich_log_line(
            "Read",
            &serde_json::json!({"file_path": "foo.rs", "offset": 0, "limit": 0}),
        );
        assert!(!line.contains("offset"), "{line}");
        assert!(!line.contains("limit"), "{line}");

        // Bash with timeout.
        let line = enrich_log_line(
            "Bash",
            &serde_json::json!({"command": "make test", "timeout": 60}),
        );
        assert!(line.contains("timeout=60"), "{line}");
        assert!(
            line.contains("\"make test\""),
            "primary must be quoted when extras present: {line}"
        );

        // Bash with zero timeout — omitted, no quoting.
        let line = enrich_log_line(
            "Bash",
            &serde_json::json!({"command": "make test", "timeout": 0}),
        );
        assert!(!line.contains("timeout"), "{line}");
        assert!(!line.contains('"'), "no extras → no quoting: {line}");

        // Grep with path, glob, max_matches, context_before, context_after.
        let line = enrich_log_line(
            "Grep",
            &serde_json::json!({
                "pattern": "fn run",
                "path": "src/",
                "glob": "*.rs",
                "max_matches": 50,
                "context_before": 2,
                "context_after": 0,
            }),
        );
        assert!(line.contains("path=\"src/\""), "{line}");
        assert!(line.contains("glob=\"*.rs\""), "{line}");
        assert!(line.contains("max_matches=50"), "{line}");
        assert!(line.contains("context_before=2"), "{line}");
        assert!(
            !line.contains("context_after"),
            "zero context_after omitted: {line}"
        );

        // Glob with path.
        let line = enrich_log_line(
            "Glob",
            &serde_json::json!({"pattern": "**/*.rs", "path": "src/"}),
        );
        assert!(line.contains("path=\"src/\""), "{line}");

        // Glob without path.
        let line = enrich_log_line("Glob", &serde_json::json!({"pattern": "**/*.rs"}));
        assert!(!line.contains("path="), "{line}");
    }

    #[test]
    fn enrich_log_line_truncates_long_primary_before_extras() {
        // A very long Bash command should be truncated so timeout fits.
        let long_cmd = "echo ".repeat(50).trim().to_string();
        let line = enrich_log_line(
            "Bash",
            &serde_json::json!({"command": long_cmd, "timeout": 30}),
        );
        assert!(line.contains("timeout=30"), "timeout lost: {line}");
        assert!(
            line.contains("...\""),
            "long primary should be truncated: {line}"
        );
        assert!(
            line.len() <= 200,
            "enriched line must fit the 200-char cap, got {}: {line}",
            line.len()
        );
    }

    /// Fix 2 regression: a Grep whose secondary args alone exceed the cap must
    /// truncate the suffix and still fit the final 200-char log line, keeping
    /// the leading context values visible instead of losing them to the outer
    /// `trunc(..., 200)`.
    #[test]
    fn enrich_log_line_suffix_exceeds_cap() {
        let line = enrich_log_line(
            "Grep",
            &serde_json::json!({
                "pattern": "fn run",
                "max_matches": 50,
                "glob": format!("*{}.rs", "x".repeat(300)),
                "path": "crates/gremlins/src/clients",
                "context_before": 2,
                "context_after": 2,
            }),
        );
        assert!(
            line.starts_with("\"...\" "),
            "oversized suffix must use the quoted placeholder: {line}"
        );
        assert!(
            line.len() <= 200,
            "enriched line must fit the 200-char cap, got {}: {line}",
            line.len()
        );
        assert!(
            line.contains("max_matches=50"),
            "leading context values should survive truncation: {line}"
        );
        assert!(
            !line.contains("context_after=2"),
            "trailing context past the cap must be truncated: {line}"
        );
    }

    #[test]
    fn enrich_log_line_keeps_long_grep_glob_untruncated() {
        // A glob that exceeds the title cap must still be logged verbatim:
        // logging applies only its own whole-line 200-char budget.
        let glob = "y".repeat(150);
        let line = enrich_log_line(
            "Grep",
            &serde_json::json!({ "pattern": "fn run", "glob": glob }),
        );
        let expected = format!("\"fn run\" glob={glob:?}");
        assert_eq!(line, expected);
        assert!(
            line.len() <= 200,
            "enriched line must fit the 200-char cap, got {}: {line}",
            line.len()
        );
    }

    #[test]
    fn enrich_log_line_keeps_long_glob_path_untruncated() {
        // Same regression for Glob's path secondary value.
        let path = "z".repeat(150);
        let line = enrich_log_line(
            "Glob",
            &serde_json::json!({ "pattern": "**/*.rs", "path": path }),
        );
        let expected = format!("\"**/*.rs\" path={path:?}");
        assert_eq!(line, expected);
        assert!(
            line.len() <= 200,
            "enriched line must fit the 200-char cap, got {}: {line}",
            line.len()
        );
    }

    #[test]
    fn ledger_key_arg_includes_summary() {
        assert_eq!(
            ledger_key_arg(&serde_json::json!({"summary": "Fixed the bug"})),
            "Fixed the bug"
        );
        // summary is lower priority than file_path, command, pattern.
        assert_eq!(
            ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x", "summary": "ignored"})),
            "/tmp/x"
        );
        assert_eq!(
            ledger_key_arg(&serde_json::json!({"command": "ls", "summary": "ignored"})),
            "ls"
        );
        assert_eq!(
            ledger_key_arg(&serde_json::json!({"pattern": "fn", "summary": "ignored"})),
            "fn"
        );
    }

    #[test]
    fn key_arg_includes_summary() {
        assert_eq!(
            key_arg(&serde_json::json!({"summary": "Completed task"})),
            "Completed task"
        );
        // summary is lower priority than file_path, command, pattern.
        assert_eq!(
            key_arg(&serde_json::json!({"file_path": "/tmp/x", "summary": "ignored"})),
            "/tmp/x"
        );
    }

    #[tokio::test]
    async fn cancel_token_wakes_all_waiters() {
        let token = CancelToken::new();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let t = token.clone();
                tokio::spawn(async move {
                    t.cancelled().await;
                })
            })
            .collect();
        token.cancel();
        for handle in handles {
            tokio::time::timeout(Duration::from_secs(1), handle)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn send_stream_event_cancels_while_channel_full() {
        let token = CancelToken::new();
        let (tx, _rx) = mpsc::channel::<InteractiveEvent>(1);
        let sender = StreamEventSender::Mpsc(tx);

        // Fill the only slot.
        assert!(
            send_stream_event(
                &sender,
                InteractiveEvent::StreamChunk { text: "one".into() },
                &token,
            )
            .await,
            "an unblocked send should complete"
        );

        token.cancel();

        // The next send would block forever waiting for capacity; the cancel
        // branch must win the race and report cancellation.
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            send_stream_event(
                &sender,
                InteractiveEvent::StreamChunk { text: "two".into() },
                &token,
            ),
        )
        .await;
        assert!(
            matches!(result, Ok(false)),
            "cancellation must interrupt a blocked stream send and report not sent"
        );
    }

    fn loop_opts(filter: Option<&[String]>) -> LoopOpts<'_> {
        LoopOpts {
            extra: None,
            tool_filter: filter,
            classify_error: None,
            temperature: None,
            max_tokens: None,
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
                gremlin_id: None,
                log_tx: None,
                base_env: None,
                allow_all: false,
                task_models: None,
                cancel_token: None,
                stream_events: None,
                prepopulated_messages: None,
                compaction_opts: None,
            },
            prefix: "[t] ".into(),
            idle_timeout: 0.05,
            expected_artifact_paths: vec![],
            reminder_budget: 0,
            completion_nudge_budget: 0,
            stream_events: None,
            prepopulated_messages: None,
            compaction_opts: None,
        }
    }

    fn padded_history(rounds: usize, assistant_text_len: usize) -> Vec<Message> {
        let mut history = vec![Message::user("operator prompt")];
        for index in 0..rounds {
            history.push(assistant_tool_message(
                &"x".repeat(assistant_text_len),
                &[],
                &[],
            ));
            history.push(Message::user(format!(
                "Actions taken this turn:\n- round {index}\n"
            )));
        }
        history
    }

    fn bash_history(rounds: usize, output_len: usize) -> Vec<Message> {
        let mut history = vec![Message::user("operator prompt")];
        for index in 0..rounds {
            let call = ToolCall::from_dual_wire(
                "",
                format!("bash_{index}"),
                ToolFunction::new(
                    ToolName::new("Bash".to_string()).unwrap(),
                    serde_json::json!({"command": format!("cargo check {index}")}),
                ),
            );
            history.push(assistant_tool_message("", &[], &[call]));
            history.push(Message::tool_result(
                CallId::from_wire(format!("bash_{index}")),
                ToolName::new("Bash".to_string()).unwrap(),
                format!("output for batch {index}\n{}", "x".repeat(output_len)),
            ));
            history.push(Message::user(format!(
                "Actions taken this turn:\n- Bash cargo check {index}\n"
            )));
        }
        history
    }

    #[test]
    fn build_request_messages_compacts_mid_turn_with_sticky_prefix() {
        let mut history = padded_history(6, 3_000);
        let full_len = conversation::serialized_len(&history);
        let opts = conversation::CompactionOpts {
            threshold_bytes: full_len as u64 - 1,
            target_bytes: full_len as u64 / 2,
        };

        let mut compacted = None;
        let first = build_request_messages(&history, &mut compacted, 0, Some(opts), 0);
        assert!(
            compacted.is_some(),
            "an over-threshold multi-round turn must compact"
        );
        assert!(conversation::serialized_len(&first) < full_len);

        let prefix_before = compacted.as_ref().unwrap().prefix.clone();
        let cut_before = compacted.as_ref().unwrap().cut;

        // Append several small rounds. The cached prefix must stay
        // byte-identical and no second compaction pass may run while the view
        // is under threshold.
        for round in 0..5 {
            history.push(assistant_tool_message("small", &[], &[]));
            history.push(Message::user(format!(
                "Actions taken this turn:\n- small {round}\n"
            )));
            let request = build_request_messages(&history, &mut compacted, 0, Some(opts), 0);
            assert!(compacted.is_some());
            assert_eq!(compacted.as_ref().unwrap().prefix, prefix_before);
            assert_eq!(compacted.as_ref().unwrap().cut, cut_before);
            let mut expected = prefix_before.clone();
            expected.extend_from_slice(&history[cut_before..]);
            assert_eq!(request, expected);
        }
    }

    #[test]
    fn build_request_messages_runs_two_passes_and_keeps_earlier_facts() {
        let mut history = bash_history(6, 2_000);
        let full_len = conversation::serialized_len(&history);
        let opts = conversation::CompactionOpts {
            threshold_bytes: full_len as u64 - 1,
            target_bytes: full_len as u64 / 2,
        };

        let mut compacted = None;
        let first = build_request_messages(&history, &mut compacted, 0, Some(opts), 0);
        assert!(compacted.is_some(), "first pass must compact");
        assert!(conversation::serialized_len(&first) < full_len);
        let first_cut = compacted.as_ref().unwrap().cut;
        let first_prefix = compacted.as_ref().unwrap().prefix.clone();
        assert!(user_message_text(&first_prefix[0]).contains("Bash: cargo check 0"));

        // Push the materialized view back over the soft threshold.
        for batch in 6..12 {
            let index = batch;
            let call = ToolCall::from_dual_wire(
                "",
                format!("bash_{index}"),
                ToolFunction::new(
                    ToolName::new("Bash".to_string()).unwrap(),
                    serde_json::json!({"command": format!("cargo check {index}")}),
                ),
            );
            history.push(assistant_tool_message("", &[], &[call]));
            history.push(Message::tool_result(
                CallId::from_wire(format!("bash_{index}")),
                ToolName::new("Bash".to_string()).unwrap(),
                format!("output for batch {index}\n{}", "x".repeat(2_000)),
            ));
            history.push(Message::user(format!(
                "Actions taken this turn:\n- Bash cargo check {index}\n"
            )));
        }

        let second = build_request_messages(&history, &mut compacted, 0, Some(opts), 0);
        let fresh = compacted
            .as_ref()
            .expect("a second compaction pass must be cached");
        assert!(
            fresh.cut != first_cut || fresh.prefix != first_prefix,
            "the second over-threshold pass must choose a new sticky prefix"
        );
        assert!(conversation::serialized_len(&second) < conversation::serialized_len(&history));

        let memory = user_message_text(&fresh.prefix[0]);
        assert!(
            memory.contains("Bash: cargo check 0"),
            "facts from the first pass must survive into the second pass"
        );
        assert!(
            memory.contains("Bash: cargo check 5"),
            "facts from rounds before the second pass must be present"
        );
    }

    #[test]
    fn build_request_messages_uses_verbatim_history_without_opts() {
        let history = padded_history(3, 1_000);
        let mut compacted = None;
        let request = build_request_messages(&history, &mut compacted, 0, None, 0);
        assert_eq!(request, history);
        assert!(compacted.is_none());
    }

    #[test]
    fn build_request_messages_resets_cached_cut_after_history_pop() {
        let mut history = padded_history(6, 3_000);
        let full_len = conversation::serialized_len(&history);
        let opts = conversation::CompactionOpts {
            threshold_bytes: full_len as u64 - 1,
            target_bytes: full_len as u64 / 2,
        };

        let mut compacted = None;
        let _ = build_request_messages(&history, &mut compacted, 0, Some(opts), 0);
        assert!(compacted.is_some());

        // Simulate a mid-tool pause re-run popping messages back below the cut.
        while history.len() > compacted.as_ref().unwrap().cut {
            history.pop();
        }
        history.pop();

        let request = build_request_messages(&history, &mut compacted, 0, Some(opts), 0);
        assert!(
            compacted.is_none() || compacted.as_ref().is_some_and(|c| c.cut <= history.len()),
            "a stale cached cut must not survive a history pop"
        );
        assert_eq!(request, history);
    }

    #[tokio::test]
    async fn oversized_prompt_fails_hard_before_any_request() {
        let inner = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("should not run"),
        ]]);
        let model = GremlinsModel::new(inner.clone().erase(), 16);
        let prompt = "x".repeat(10_000);

        let result = run_agent_loop(
            model,
            &prompt,
            test_ctx(None, None),
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await;

        match result {
            Err(ClientError::PromptTooLarge { limit, actual }) => {
                assert_eq!(limit, 16);
                assert!(actual > 16);
                assert!(actual as usize >= prompt.len());
            }
            other => panic!("expected PromptTooLarge, got {other:?}"),
        }
        assert!(
            inner.requests().is_empty(),
            "oversized agent-loop prompt must not reach the model"
        );
    }

    /// A transport whose frames never resolve — the stream hangs forever.
    #[derive(Clone)]
    struct PendingTransport;

    impl rig_core::driver::Transport<rig_core::test_utils::MockScript> for PendingTransport {
        fn send(
            &self,
            _request: rig_core::completion::CompletionRequest,
            _exchange: rig_core::driver::Exchange,
        ) -> rig_core::driver::Opening<rig_core::test_utils::MockFrame> {
            rig_core::driver::Opening::ready(rig_core::driver::Opened::new(
                futures::stream::pending(),
            ))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn loop_idle_timeout_exhausts_timeout_retries() {
        let ctx = test_ctx(None, None);
        let cancel = CancelToken::new();
        let model = rig_core::driver::Model::new(
            rig_core::test_utils::MockScript::default(),
            PendingTransport,
        )
        .erase();
        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone()),
            "hi",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        ));
        // The stream hangs forever, so each turn hits the idle timeout
        // (0.05 s). After 3 turn-level timeout retries (5 + 15 + 30 s) the
        // fourth attempt exhausts the budget and returns RetryExhausted.
        // Total wall-clock: 4 × 0.05 + 5 + 15 + 30 ≈ 50.3 s.
        tokio::time::advance(Duration::from_secs(52)).await;
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            matches!(err, ClientError::RetryExhausted { .. }),
            "expected ClientError::RetryExhausted after exhausted timeout retries, got {err:?}"
        );
    }

    #[tokio::test]
    async fn loop_mid_tool_cancel_returns_runtime_without_waiting_for_tool() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let pidfile = dir.path().join("pid");

        // A Bash tool call that records its PID and signals when it starts,
        // then hangs until killed by the dropped join_all future.
        // Uses a pure-bash infinite loop (while true; do :; done) so no
        // child process is spawned — avoiding GNU Make jobserver pipe leaks
        // that would keep `make -j` waiting forever.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::tool_call(
                "c1",
                "Bash",
                serde_json::json!({
                    "command": format!(
                        "echo $$ > {}; touch {}; while true; do :; done",
                        pidfile.display(),
                        marker.display()
                    ),
                    "timeout": 3600
                }),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();

        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone().erase()),
            "go",
            ctx,
            cancel.clone(),
            loop_opts(None),
            None,
        ));

        // Wait until the Bash child has started so the loop is in Phase 2.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Bash tool should start");

        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("pidfile should exist")
            .trim()
            .parse()
            .expect("pidfile should hold a valid pid");

        cancel.cancel();

        // Cancellation must unblock the loop promptly instead of waiting for
        // the (3600s) tool future to finish.
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("cancel must unblock the agent loop promptly")
            .unwrap();
        match result {
            Err(ClientError::Cancelled { message, .. }) => assert_eq!(message, "cancelled"),
            other => panic!("expected Cancelled, got: {other:?}"),
        }

        // The dropped join_all future must have killed the Bash child.
        // Poll until the pid is gone.
        let reaped = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !process_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if reaped.is_err() {
            // Best-effort cleanup so a failed assertion does not orphan the
            // child on the test host.
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        assert!(
            !process_alive(pid),
            "Bash child pid {pid} should have been killed by the dropped tool future"
        );
    }

    /// Returns true if a process with the given pid exists on this host.
    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "write",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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

    /// A prior turn's structured `reasoning` item (with an id) must survive
    /// into the reconstructed history so that a `function_call` referencing
    /// it is re-serialized beside its reasoning item — not orphaned.
    #[test]
    fn assistant_tool_message_retains_reasoning_items() {
        let reasoning = Reasoning {
            id: Some("rs_reasoning_item".to_string()),
            content: vec![ReasoningContent::Text {
                text: "thinking hard".to_string(),
                signature: None,
            }],
        }
        .sealed("openai");
        let call = ToolCall::from_dual_wire(
            "fc_item",
            "call_1",
            ToolFunction::new(
                ToolName::new("Read").unwrap(),
                serde_json::json!({ "file_path": "/tmp/x" }),
            ),
        );

        let message = assistant_tool_message(
            "",
            std::slice::from_ref(&reasoning),
            std::slice::from_ref(&call),
        );

        let Message::Assistant { content, .. } = message else {
            panic!("expected an assistant message");
        };
        assert_eq!(content.len(), 2);
        match &content[0] {
            AssistantContent::Reasoning(sealed) => {
                let opened = sealed
                    .open(&Issuer::from("openai"))
                    .expect("issuer preserved");
                assert_eq!(opened.id.as_deref(), Some("rs_reasoning_item"));
            }
            other => panic!("first item must be reasoning, got {other:?}"),
        }
        // The referencing `function_call` must stay beside its reasoning item,
        // with its provider `item_id` naming that reasoning id so the replayed
        // `input` array does not orphan it.
        match &content[1] {
            AssistantContent::ToolCall(tc) => {
                let provider = tc
                    .id
                    .provider()
                    .expect("dual-wire call keeps a provider id");
                assert_eq!(provider.item_id.as_deref(), Some("fc_item"));
            }
            other => panic!("second item must be the tool call, got {other:?}"),
        }
    }

    /// End-to-end: a turn that streams a structured `reasoning` item followed
    /// by a `function_call` referencing it must replay that reasoning item on
    /// the next request, so it is not orphaned in the reconstructed history.
    #[tokio::test]
    async fn loop_replays_reasoning_item_beside_function_call() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-reasoning-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, "content").unwrap();

        // Turn 0: reasoning (rs_1) then a Read tool call referencing it.
        // Turn 1: Done. The second request must carry rs_1 beside the call.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::reasoning("thinking")
                    .with_reasoning_id("rs_1"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({ "file_path": f.to_str().unwrap() }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({ "summary": "done" }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "read then finish",
            ctx,
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, 0);

        let reqs = model.requests();
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];
        let assistant = second
            .chat_history
            .iter()
            .find(|m| matches!(m, Message::Assistant { .. }))
            .expect("second request carries the prior assistant turn");
        let Message::Assistant { content, .. } = assistant else {
            unreachable!()
        };
        let reasoning = content.iter().find_map(|c| match c {
            AssistantContent::Reasoning(sealed) => {
                sealed.open(&Issuer::from("mock")).map(|r| r.id.clone())
            }
            _ => None,
        });
        assert_eq!(
            reasoning.flatten().as_deref(),
            Some("rs_1"),
            "reasoning item id must survive into the replay"
        );
        // The reasoning item must precede the referencing function_call in the
        // replayed content so the call is not orphaned.
        let reasoning_at = content
            .iter()
            .position(|c| matches!(c, AssistantContent::Reasoning(_)));
        let call_at = content
            .iter()
            .position(|c| matches!(c, AssistantContent::ToolCall(_)));
        assert!(
            reasoning_at.is_some() && call_at.is_some(),
            "replayed turn must carry both reasoning and the function call"
        );
        assert!(
            reasoning_at.unwrap() < call_at.unwrap(),
            "reasoning item must precede its referencing function_call"
        );
    }

    /// A reasoning-only turn (reasoning deltas but no text and no tool call)
    /// must still record an assistant message carrying the structured
    /// reasoning item before nudging with "Continue.". Dropping it would both
    /// omit the item from the next request and leave two consecutive user
    /// messages — breaking the replay invariant the provider relies on.
    #[tokio::test]
    async fn loop_replays_reasoning_item_on_reasoning_only_turn() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-reasoning-only-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Turn 0: reasoning only (rs_0), no text, no tool call — the loop
        // records the reasoning item and continues with "Continue.".
        // Turn 1: Done. The second request must carry rs_0 as an assistant
        // message, not as two consecutive user messages.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::reasoning_delta_with_id("rs_0", "thinking"),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({ "summary": "done" }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "think then finish",
            ctx,
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, 0);

        let reqs = model.requests();
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];

        // The reasoning-only turn must be replayed as an assistant message.
        let assistant = second
            .chat_history
            .iter()
            .find(|m| matches!(m, Message::Assistant { .. }))
            .expect("second request carries the prior assistant turn");
        let Message::Assistant { content, .. } = assistant else {
            unreachable!()
        };
        let reasoning = content.iter().find_map(|c| match c {
            AssistantContent::Reasoning(sealed) => {
                sealed.open(&Issuer::from("mock")).map(|r| r.id.clone())
            }
            _ => None,
        });
        assert_eq!(
            reasoning.flatten().as_deref(),
            Some("rs_0"),
            "reasoning-only turn's structured item must survive into the replay"
        );

        // No two consecutive user messages: the assistant turn (with its
        // reasoning item) must sit between the original prompt and "Continue.".
        let mut prev_user = false;
        for m in &second.chat_history {
            match m {
                Message::User { .. } => {
                    assert!(
                        !prev_user,
                        "replayed history must not contain consecutive user messages"
                    );
                    prev_user = true;
                }
                _ => prev_user = false,
            }
        }
    }

    /// A 429 on turn N must back off and retry turn N in place — not restart
    /// the loop from turn 0 — and `turn_num` must count only completed turns.
    #[tokio::test(start_paused = true)]
    async fn loop_429_retries_same_turn_without_losing_history() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-429-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, "content").unwrap();

        // Turn 0: Read tool call (completes).
        // Turn 1 attempt 1: text + tool call, then a 429 mid-stream error
        // (retried — the partial text and tool call must be discarded).
        // Turn 1 attempt 2: Done (completes).
        let side_effect = dir.join("side.txt");
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({ "file_path": f.to_str().unwrap() }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("partial"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "w1",
                    "Write",
                    serde_json::json!({ "file_path": side_effect.to_str().unwrap(), "content": "x" }),
                ),
                rig_core::test_utils::MockStreamEvent::error("http://429: slow down"),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("recovered"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({ "summary": "done" }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();

        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone().erase()),
            "read then finish",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        ));
        // Skip past the default turn-level schedule's first backoff (2s;
        // schedule [2, 5, 10, 60, 120]).
        tokio::time::advance(Duration::from_secs(3)).await;
        let result = handle.await.unwrap().unwrap();

        assert_eq!(result.text_result.as_deref(), Some("recovered"));

        // The aborted turn's partial text and tool call must be discarded:
        // no duplicated text and no tool side effect from the 429 turn.
        assert!(
            !side_effect.exists(),
            "aborted turn's Write must not have executed"
        );

        // Three requests: turn 0, turn 1 (429), turn 1 retry (Done).
        assert_eq!(model.requests().len(), 3);

        // The retried turn must carry the prior turn's history — the Read
        // tool result from turn 0 must still be present in the final request.
        let reqs = model.requests();
        let final_req = reqs.last().unwrap();
        let history = format!("{:?}", final_req.chat_history);
        assert!(
            history.contains("x.txt"),
            "retried turn must retain turn 0 history, got: {history}"
        );

        // turn_num counts completed turns only: turn 0 + turn 1 = 2, not 3.
        let usage = result.token_usage.unwrap();
        assert_eq!(usage.turns, 2, "turn_num must exclude the retried attempt");
    }

    /// A wire that fails `encode` once — a request error with no HTTP status,
    /// which `default_classify` treats as retryable — then delegates to
    /// [`MockScript`]. Exercises the `model.stream(builder)` `Err(e)` branch,
    /// which the mid-stream 429 tests never reach.
    #[derive(Clone)]
    struct FailingOnceWire {
        inner: rig_core::test_utils::MockScript,
        failed: Arc<AtomicBool>,
    }

    impl rig_core::wire::Wire for FailingOnceWire {
        type Op = Completion;
        type Payload = CompletionRequest;
        type Frame = rig_core::test_utils::MockFrame;
        type Decoder<'id> = rig_core::test_utils::MockDecoder<'id>;

        fn describe(&self) -> rig_core::wire::Descriptor<'_> {
            self.inner.describe()
        }

        fn encode(
            &self,
            request: rig_core::wire::Request<Self>,
            mode: rig_core::wire::Mode,
        ) -> Result<Self::Payload, rig_core::error::EncodeError> {
            if !self.failed.swap(true, Ordering::SeqCst) {
                return Err(rig_core::error::EncodeError::request(
                    "transient encode failure",
                ));
            }
            self.inner.encode(request, mode)
        }

        fn decoder<'id>(&self) -> Self::Decoder<'id> {
            self.inner.decoder()
        }
    }

    /// A transport that records each request and answers with a single `Done`
    /// turn, so the retried attempt can be observed to succeed.
    #[derive(Clone)]
    struct RecordingDoneTransport {
        requests: Arc<std::sync::Mutex<Vec<CompletionRequest>>>,
    }

    impl rig_core::driver::Transport<FailingOnceWire> for RecordingDoneTransport {
        fn send(
            &self,
            request: CompletionRequest,
            _exchange: rig_core::driver::Exchange,
        ) -> rig_core::driver::Opening<rig_core::test_utils::MockFrame> {
            self.requests.lock().unwrap().push(request);
            rig_core::driver::Opening::ready(rig_core::driver::Opened::new(futures::stream::iter(
                [
                    Ok(rig_core::test_utils::MockFrame::Event(
                        rig_core::test_utils::MockStreamEvent::tool_call(
                            "done1",
                            "Done",
                            serde_json::json!({"summary": "done"}),
                        ),
                    )),
                    Ok(rig_core::test_utils::MockFrame::Event(
                        rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
                    )),
                ],
            )))
        }
    }

    /// An immediate `model.stream(builder)` failure — a request error with no
    /// HTTP status, which `default_classify` treats as retryable — must back
    /// off and re-execute the same turn, then succeed. This exercises the
    /// `Err(e)` branch of the stream-open match, distinct from mid-stream
    /// errors.
    #[tokio::test(start_paused = true)]
    async fn loop_immediate_stream_error_retries_same_turn() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-immediate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let model = rig_core::driver::Model::new(
            FailingOnceWire {
                inner: rig_core::test_utils::MockScript::default(),
                failed: Arc::new(AtomicBool::new(false)),
            },
            RecordingDoneTransport {
                requests: requests.clone(),
            },
        )
        .erase();

        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();

        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone()),
            "finish",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        ));
        // Skip past the default turn-level schedule's first backoff (2s;
        // schedule [2, 5, 10, 60, 120]).
        tokio::time::advance(Duration::from_secs(3)).await;
        let result = handle.await.unwrap().unwrap();

        assert_eq!(result.text_result.as_deref(), Some("done"));

        // The failed encode never reaches the transport; only the retried
        // attempt does, and it must carry the original prompt.
        let recorded = requests.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "only the retried attempt reaches the transport"
        );
        let history = format!("{:?}", recorded[0].chat_history);
        assert!(
            history.contains("finish"),
            "retried request must carry the original prompt, got: {history}"
        );

        // turn_num counts completed turns only: the failed attempt is excluded.
        let usage = result.token_usage.unwrap();
        assert_eq!(usage.turns, 1, "turn_num must exclude the retried attempt");
    }

    /// A cancel token set during the turn-level backoff must abandon the
    /// retry immediately rather than sleeping out the full backoff window.
    #[tokio::test(start_paused = true)]
    async fn loop_429_backoff_observes_cancellation() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-429-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::error("http://429: slow down"),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();

        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone().erase()),
            "boom",
            ctx.clone(),
            cancel.clone(),
            loop_opts(None),
            None,
        ));

        // Let the loop reach the retry backoff, then cancel mid-sleep.
        tokio::time::advance(Duration::from_millis(500)).await;
        cancel.cancel();
        tokio::time::advance(Duration::from_millis(500)).await;

        let result = handle.await.unwrap();
        match result {
            Err(ClientError::Cancelled { message, .. }) => {
                assert_eq!(message, "cancelled");
            }
            other => panic!("expected cancellation, got: {other:?}"),
        }
    }

    /// When turn-level 429 retries are exhausted, the error must surface as
    /// `RetryExhausted` so callers do not mistake it for a still-retryable
    /// transient error.
    #[tokio::test(start_paused = true)]
    async fn loop_429_exhausted_retries_returns_retry_exhausted() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-429-exhaust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Every attempt returns 429 — the mock repeats the last turn.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::error("http://429: slow down"),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();

        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone().erase()),
            "boom",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        ));
        // Advance past all five turn-level backoffs:
        // 2 + 5 + 10 + 60 + 120 = 197 s.
        tokio::time::advance(Duration::from_secs(200)).await;
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            matches!(err, ClientError::RetryExhausted { .. }),
            "expected ClientError::RetryExhausted after exhausted turn retries, got {err:?}"
        );
    }

    /// A timeout on turn N must back off and retry turn N in place — not
    /// restart the loop from turn 0 — and `turn_num` must count only
    /// completed turns.
    #[tokio::test(start_paused = true)]
    async fn loop_timeout_retries_same_turn_without_losing_history() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, "content").unwrap();

        // Turn 0: Read tool call (completes).
        // Turn 1 attempt 1: hangs (idle timeout fires) — retried.
        // Turn 1 attempt 2: Done (completes).
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({ "file_path": f.to_str().unwrap() }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            // This stream never produces an event — it hangs until the idle
            // timeout fires.
            vec![],
            vec![
                rig_core::test_utils::MockStreamEvent::text("recovered"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({ "summary": "done" }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        // Short idle timeout so we don't need huge advances.
        ctx.idle_timeout = 0.1;
        ctx.params.idle_timeout = Some(0.1);
        let cancel = CancelToken::new();

        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone().erase()),
            "read then finish",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        ));
        // Advance to let turn 0 complete, then the timeout fires (0.1 s),
        // then the turn-level backoff runs (2 s), then turn 1 retry
        // completes.
        tokio::time::advance(Duration::from_secs(5)).await;
        let result = handle.await.unwrap().unwrap();

        assert_eq!(result.text_result.as_deref(), Some("recovered"));

        // Three requests: turn 0, turn 1 (timeout), turn 1 retry (Done).
        assert_eq!(model.requests().len(), 3);

        // turn_num counts completed turns only: turn 0 + turn 1 = 2.
        let usage = result.token_usage.unwrap();
        assert_eq!(
            usage.turns, 2,
            "turn_num must exclude the timed-out attempt"
        );
    }

    /// A cancel token set during the timeout backoff must abandon the retry
    /// immediately.
    #[tokio::test(start_paused = true)]
    async fn loop_timeout_backoff_observes_cancellation() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-timeout-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Empty stream — hangs until timeout.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 0.1;
        ctx.params.idle_timeout = Some(0.1);
        let cancel = CancelToken::new();

        let handle = tokio::spawn(run_agent_loop(
            gm(model.clone().erase()),
            "boom",
            ctx.clone(),
            cancel.clone(),
            loop_opts(None),
            None,
        ));

        // Let the idle timeout fire (0.1 s), then cancel during backoff.
        tokio::time::advance(Duration::from_millis(500)).await;
        cancel.cancel();
        tokio::time::advance(Duration::from_millis(500)).await;

        let result = handle.await.unwrap();
        match result {
            Err(ClientError::Cancelled { message, .. }) => {
                assert_eq!(message, "cancelled");
            }
            other => panic!("expected cancellation, got: {other:?}"),
        }
    }

    /// The `Done` summary must survive alongside streamed text: when the
    /// model streams a final message and then calls `Done(summary)`, the
    /// returned `CompletedRun` carries both the streamed text (as
    /// `text_result`) and the summary (as `summary`), distinctly.
    #[tokio::test]
    async fn loop_done_summary_survives_alongside_streamed_text() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-summary-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::text("here is my full answer"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "answered the question"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "hi",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();
        // Streamed text remains authoritative for the response body.
        assert_eq!(
            result.text_result.as_deref(),
            Some("here is my full answer")
        );
        // The summary is preserved distinctly, not discarded.
        assert_eq!(result.summary.as_deref(), Some("answered the question"));
    }

    /// When the model calls `Done(summary)` without streaming any text, the
    /// summary still becomes the response (existing fallback preserved), and
    /// is also carried as the distinct summary field.
    #[tokio::test]
    async fn loop_done_summary_fallback_when_no_streamed_text() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-summary-fb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "did the thing"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "hi",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("did the thing"));
        assert_eq!(result.summary.as_deref(), Some("did the thing"));
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "where am i",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
            gm(model.clone().erase()),
            "write",
            ctx.clone(),
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
        run_agent_loop(
            gm(model.clone().erase()),
            "write",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "read both",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "mix",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "hi",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("ok"));

        let requests = model.requests();
        assert!(!requests.is_empty());
        for req in requests {
            match req.chat_history.first() {
                Some(Message::System { content }) => assert_eq!(content, "you are a harness"),
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "hi",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("ok"));

        for req in model.requests() {
            assert!(
                req.system_instructions().is_none(),
                "harness must not inject preamble"
            );
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
        run_agent_loop(
            gm(model.clone().erase()),
            "read",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
            gm(model.clone().erase()),
            "read both",
            ctx.clone(),
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "write",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "write",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "do it",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "write",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "hi",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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
                serde_json::json!({"kind": "Scout", "prompt": format!("p{i}")}),
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
        run_agent_loop(
            gm(model.clone().erase()),
            "fan out",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
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

    /// A `task-models` entry matching the task's `kind` must swap in the
    /// model the factory builds, and nothing else about the Task call changes.
    #[tokio::test]
    async fn task_models_override_selects_the_task_model() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = tempfile::tempdir().unwrap();

        let task_call = |id: &str| {
            MockStreamEvent::tool_call(
                id,
                "Task",
                serde_json::json!({"kind": "Scout", "prompt": "look around"}),
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
        let factory: super::super::task::TaskModelFactory = Arc::new(move |spec: &str| {
            assert_eq!(spec, "openai/mini", "factory receives the matched spec");
            Ok(super::super::task::TaskModel {
                model: gm(child_handle.clone().erase()),
                temperature: None,
                max_tokens: None,
                extra: None,
            })
        });

        let selector = super::super::task::TaskModelSelector::new(
            HashMap::from([(
                super::super::task_kind::TaskKind::Scout,
                "openai/mini".to_string(),
            )]),
            factory,
        )
        .expect("a configured task-models map yields a selector");
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.params.task_models = Some(selector);

        run_agent_loop(
            gm(parent.clone().erase()),
            "go",
            ctx.clone(),
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        // The overridden model ran the task and its reply came back verbatim.
        assert!(
            !child.requests().is_empty(),
            "the task-models model must be the one that ran the Task"
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
            "task result should carry its kind header, got: {task_result}"
        );
    }

    // ── max_tokens propagation tests ─────────────────────────────────

    /// A parent agent-loop turn with prior assistant/tool-result history that
    /// emits `Task(kind="Fork")` must hand the child the snapshot of that
    /// history (including the current user prompt), followed by the Fork
    /// prompt, in exact order. This exercises the agent-loop snapshot path and
    /// the `tools::invoke` handoff rather than calling the task runner directly.
    #[tokio::test]
    async fn fork_child_receives_parent_history_then_fork_prompt() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("note.txt");
        std::fs::write(&note, "hello from parent").unwrap();

        let read_call = MockStreamEvent::tool_call(
            "r1",
            "Read",
            serde_json::json!({ "file_path": note.to_str().unwrap() }),
        );
        let fork_call = MockStreamEvent::tool_call(
            "f1",
            "Task",
            serde_json::json!({"kind": "Fork", "prompt": "finish as fork"}),
        );
        let done_call = |id: &str, summary: &str| {
            MockStreamEvent::tool_call(id, "Done", serde_json::json!({ "summary": summary }))
        };

        // One model drives every loop: parent Read turn, parent Fork turn,
        // forked child turn, then parent wrap-up turn.
        let model = MockCompletionModel::from_stream_turns([
            vec![
                read_call,
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                fork_call,
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("fork reply"),
                done_call("d2", "fork reply"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("parent done"),
                done_call("d1", "parent done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);

        run_agent_loop(
            gm(model.clone().erase()),
            "parent go",
            ctx,
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        fn user_text_of(m: &Message) -> Option<&str> {
            match m {
                Message::User { content } => content.iter().find_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                }),
                _ => None,
            }
        }

        let requests = model.requests();
        let fork_req = requests
            .iter()
            .find(|req| {
                matches!(
                    req.chat_history.first(),
                    Some(Message::System { content })
                        if content.contains("This is a forked child agent")
                )
            })
            .expect("the forked child must issue its own request");

        let history = &fork_req.chat_history;
        assert_eq!(history.len(), 6, "unexpected fork history: {history:#?}");

        // Fork system prompt leads the child history.
        assert!(matches!(
            &history[0],
            Message::System { content }
                if content.contains("<tools>") && content.contains("Do not call")
        ));

        // Inherited parent conversation, in exact order.
        assert_eq!(user_text_of(&history[1]), Some("parent go"));
        assert!(matches!(&history[2], Message::Assistant { .. }));
        assert!(matches!(&history[3], Message::User { content }
                if content.iter().any(|c| matches!(c, UserContent::ToolResult(_)))));
        assert!(
            user_text_of(&history[4]).is_some_and(|t| t.starts_with("Actions taken this turn:"))
        );

        // The Fork prompt is the final user message in the child request.
        assert_eq!(user_text_of(&history[5]), Some("finish as fork"));
    }

    /// When `LoopOpts.max_tokens` is `Some(8192)`, every turn's
    /// `CompletionRequest` must carry `max_tokens: Some(8192)`.
    #[tokio::test]
    async fn max_tokens_set_on_every_turn() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-mt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Two-turn run: tool call then Done.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({"file_path": "/dev/null"}),
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
        let mut opts = loop_opts(None);
        opts.max_tokens = Some(8192);
        run_agent_loop(gm(model.clone().erase()), "go", ctx, cancel, opts, None)
            .await
            .unwrap();

        for req in model.requests() {
            assert_eq!(
                req.max_tokens,
                Some(8192),
                "every turn must carry max_tokens=8192"
            );
        }
    }

    /// When `LoopOpts.max_tokens` is `None`, the `CompletionRequest`
    /// must leave `max_tokens` unset.
    #[tokio::test]
    async fn max_tokens_none_leaves_field_unset() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-mtnone-{}-{}",
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
        let opts = loop_opts(None); // max_tokens: None
        run_agent_loop(gm(model.clone().erase()), "go", ctx, cancel, opts, None)
            .await
            .unwrap();

        for req in model.requests() {
            assert_eq!(
                req.max_tokens, None,
                "max_tokens must be None when not set in opts"
            );
        }
    }

    /// When `max_tokens` is set, it propagates through the Task runner
    /// to every nested agent loop turn.
    #[tokio::test]
    async fn max_tokens_propagates_to_nested_task() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = tempfile::tempdir().unwrap();

        let task_call = |id: &str| {
            MockStreamEvent::tool_call(
                id,
                "Task",
                serde_json::json!({"kind": "Scout", "prompt": "look"}),
            )
        };
        let done_call = |id: &str, summary: &str| {
            MockStreamEvent::tool_call(id, "Done", serde_json::json!({"summary": summary}))
        };

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
        let child = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("scout result"),
            done_call("d2", "scout result"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let child_handle = child.clone();
        let factory: super::super::task::TaskModelFactory = Arc::new(move |_spec: &str| {
            Ok(super::super::task::TaskModel {
                model: gm(child_handle.clone().erase()),
                temperature: None,
                max_tokens: Some(8192),
                extra: None,
            })
        });

        let selector = super::super::task::TaskModelSelector::new(
            HashMap::from([(
                super::super::task_kind::TaskKind::Scout,
                "openai/mini".to_string(),
            )]),
            factory,
        )
        .expect("configured");
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.params.task_models = Some(selector);
        let mut opts = loop_opts(None);
        opts.max_tokens = Some(8192);

        run_agent_loop(
            gm(parent.clone().erase()),
            "go",
            ctx,
            CancelToken::new(),
            opts,
            None,
        )
        .await
        .unwrap();

        // Parent turns must carry max_tokens.
        for req in parent.requests() {
            assert_eq!(
                req.max_tokens,
                Some(8192),
                "parent turn must carry max_tokens"
            );
        }
        // Child (nested task) turns must also carry max_tokens.
        for req in child.requests() {
            assert_eq!(
                req.max_tokens,
                Some(8192),
                "nested task turn must carry max_tokens"
            );
        }
    }

    /// Regression for nested `Task` output sharing the parent's event stream.
    ///
    /// Two independent tasks reuse raw provider call IDs (`call_0`, …), so
    /// nested loops must (a) inherit the parent's tool-output sender instead of
    /// owning a second forwarder — otherwise the two forwarders contend for a
    /// bounded stream slot — and (b) prefix their IDs with the task chain so a
    /// child result can't collide on the parent TUI's per-ID live slots.
    #[tokio::test]
    async fn nested_task_shares_stream_and_prefixes_ids() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let mut ctx = test_ctx(None, None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        // Small bounded stream, mirroring the chat path's backpressure.
        let (stream_tx, mut stream_rx) = mpsc::channel::<InteractiveEvent>(8);
        ctx.stream_events = Some(StreamEventSender::Mpsc(stream_tx));

        let collected = Arc::new(std::sync::Mutex::new(Vec::<InteractiveEvent>::new()));
        let sink = collected.clone();
        let drain = tokio::spawn(async move {
            while let Some(ev) = stream_rx.recv().await {
                sink.lock().unwrap().push(ev);
            }
        });

        // parent turn 1 delegates, child runs a Bash command (live output)
        // then finishes, parent turn 2 finishes.
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call(
                    "t1",
                    "Task",
                    serde_json::json!({"kind": "Scout", "prompt": "go"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call(
                    "call_0",
                    "Bash",
                    serde_json::json!({"command": "echo hello-from-child"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call(
                    "call_1",
                    "Done",
                    serde_json::json!({"summary": "child done"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call(
                    "t2",
                    "Done",
                    serde_json::json!({"summary": "parent done"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);

        let run = tokio::time::timeout(
            Duration::from_secs(15),
            run_agent_loop(
                gm(model.erase()),
                "go",
                ctx,
                CancelToken::new(),
                loop_opts(None),
                None,
            ),
        )
        .await
        .expect("nested Task must not stall on a bounded stream");
        run.unwrap();
        // Every sender drops once the loop returns, so the drain task ends.
        let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;

        let events = collected.lock().unwrap().clone();

        // The child's live Bash output must reach the parent stream, carrying
        // the task-chain prefix rather than the raw provider id.
        let child_chunk_id = events
            .iter()
            .find_map(|e| match e {
                InteractiveEvent::ToolOutputChunk { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("child Bash live output must be forwarded to the parent stream");
        assert_ne!(
            child_chunk_id, "call_0",
            "child id must not reuse the raw provider id"
        );
        assert!(
            child_chunk_id.ends_with(".call_0"),
            "child id must carry the task-chain prefix, got {child_chunk_id}"
        );

        // The matching ToolResult must use the same prefixed id so the live
        // slot and its committed block line up.
        let child_result_id = events
            .iter()
            .find_map(|e| match e {
                InteractiveEvent::ToolResult { id, name, .. }
                    if name == "Bash" && id.ends_with("call_0") =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .expect("child Bash ToolResult must be forwarded");
        assert_eq!(
            child_result_id, child_chunk_id,
            "child chunk and result ids must match"
        );

        // The parent's own Task result keeps its raw, unprefixed id.
        assert!(
            events.iter().any(|e| matches!(
                e,
                InteractiveEvent::ToolResult { id, .. } if id == "t1"
            )),
            "parent ToolResult id must stay unprefixed"
        );
    }

    /// Regression: the agent loop must never inject a `temperature` field on
    /// its own. No `LoopOpts` knob exists for it, so every request — parent
    /// loop and nested Task alike — must leave `temperature` unset.
    #[tokio::test]
    async fn temperature_is_never_set() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = tempfile::tempdir().unwrap();

        let task_call = |id: &str| {
            MockStreamEvent::tool_call(
                id,
                "Task",
                serde_json::json!({"kind": "Scout", "prompt": "look"}),
            )
        };
        let done_call = |id: &str, summary: &str| {
            MockStreamEvent::tool_call(id, "Done", serde_json::json!({ "summary": summary }))
        };

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
        let child = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("scout result"),
            done_call("d2", "scout result"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let child_handle = child.clone();
        let factory: super::super::task::TaskModelFactory = Arc::new(move |_spec: &str| {
            Ok(super::super::task::TaskModel {
                model: gm(child_handle.clone().erase()),
                temperature: None,
                max_tokens: None,
                extra: None,
            })
        });

        let selector = super::super::task::TaskModelSelector::new(
            HashMap::from([(
                super::super::task_kind::TaskKind::Scout,
                "openai/mini".to_string(),
            )]),
            factory,
        )
        .expect("configured");
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.params.task_models = Some(selector);

        run_agent_loop(
            gm(parent.clone().erase()),
            "go",
            ctx,
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        for req in parent.requests() {
            assert_eq!(
                req.temperature, None,
                "parent turn must not set temperature"
            );
        }
        for req in child.requests() {
            assert_eq!(
                req.temperature, None,
                "nested task turn must not set temperature"
            );
        }
    }

    // ── PauseToken tests ───────────────────────────────────────────────

    /// Regression: `pause()` between flag load and `notified().await` must
    /// not be lost. With `notify_one` the permit is stored, so the next
    /// `notified().await` completes immediately.
    #[tokio::test]
    async fn pause_token_race_regression() {
        let token = PauseToken::new();

        // Simulate the race: call paused() (which creates a Notified future
        // but does not poll it), then pause(), then poll.
        let paused_fut = token.paused();
        token.pause();
        // The flag is now true AND a permit is stored. The future must
        // resolve immediately when polled.
        tokio::pin!(paused_fut);
        let result = tokio::time::timeout(Duration::from_millis(100), &mut paused_fut).await;
        assert!(
            result.is_ok(),
            "paused() should resolve immediately after pause()"
        );
        assert!(token.is_paused());
    }

    /// `paused()` must resolve immediately when the flag is already set,
    /// even without a stored permit.
    #[tokio::test]
    async fn pause_token_paused_resolves_immediately_when_flag_set() {
        let token = PauseToken::new();
        token.pause();
        let result = tokio::time::timeout(Duration::from_millis(100), token.paused()).await;
        assert!(
            result.is_ok(),
            "paused() should resolve immediately when flag is set"
        );
    }

    /// After `reset()`, `is_paused()` returns false and `paused()` does not
    /// resolve (it stays pending).
    #[tokio::test]
    async fn pause_token_reset_clears_flag() {
        let token = PauseToken::new();
        token.pause();
        assert!(token.is_paused());
        token.reset();
        assert!(!token.is_paused());
        // paused() should not resolve — it stays pending.
        let result = tokio::time::timeout(Duration::from_millis(50), token.paused()).await;
        assert!(result.is_err(), "paused() should stay pending after reset");
    }

    /// Multiple pause/reset cycles work correctly.
    #[tokio::test]
    async fn pause_token_multiple_cycles() {
        let token = PauseToken::new();
        for _ in 0..3 {
            token.pause();
            assert!(token.is_paused());
            let result = tokio::time::timeout(Duration::from_millis(100), token.paused()).await;
            assert!(result.is_ok());
            token.reset();
            assert!(!token.is_paused());
        }
    }

    // ── Interactive session tests ───────────────────────────────────────

    /// Helper: build a minimal AgentLoopRuntime for interactive tests.
    fn test_tool_ctx() -> AgentLoopRuntime {
        AgentLoopRuntime {
            cwd: None,
            extra_env: None,
            base_env: None,
            sandbox: tools::SandboxMode::Roots(vec![]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: Some(Arc::new(std::sync::Mutex::new(()))),
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        }
    }

    /// Pause at turn boundary: the agent loop checks `pause.is_paused()`
    /// before making the API call, enters interactive mode, broadcasts
    /// Ready, and waits for a command.
    #[tokio::test]
    async fn interactive_pause_at_turn_boundary_broadcasts_ready() {
        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        // Pre-set the pause token so the agent enters interactive mode
        // at the turn boundary before the first API call.
        handle.pause.pause();

        let mut evt_rx = handle.evt_tx.subscribe();

        let mut tool_ctx = test_tool_ctx();
        let tool_defs = vec![];
        let cancel = CancelToken::new();
        let opts = loop_opts(None);

        // Spawn the agent loop — it will enter interactive mode and block.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::text("ok"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "done"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let dyn_model = gm(model.erase());
        let evt_tx_clone = handle.evt_tx.clone();
        tool_ctx.telemetry = AgentLoopTelemetry {
            stream_events: Some(StreamEventSender::Broadcast(evt_tx_clone.clone())),
            tool_output: None,
        };
        let agent = tokio::spawn(async move {
            run_agent_loop_core(
                &dyn_model,
                "hi",
                None,
                &tool_ctx,
                &tool_defs,
                &cancel,
                &opts,
                "[t] ",
                10,
                5.0,
                &mut None,
                &mut None,
                false,
                &[],
                0,
                0,
                &None,
                Some(session),
                None,
                None,
                None,
            )
            .await
        });

        // Wait for Ready.
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match evt_rx.recv().await {
                    Ok(InteractiveEvent::Ready { .. }) => return true,
                    Ok(InteractiveEvent::Ended { .. }) => return false,
                    Err(RecvError::Closed) => return false,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert!(ready, "agent should broadcast Ready when paused");

        // Send Quit to resume.
        let _ = handle.cmd_tx.send(InteractiveCommand::Quit).await;

        // Agent should complete normally.
        let result = tokio::time::timeout(Duration::from_secs(2), agent)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("ok"));
    }

    /// Inject a message during interactive mode, verify the agent processes
    /// it, emits TurnComplete, then Quit resumes normal operation.
    #[tokio::test]
    async fn interactive_inject_then_run_turn_then_quit() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-int-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        handle.pause.pause();
        let mut evt_rx = handle.evt_tx.subscribe();

        let mut tool_ctx = test_tool_ctx();
        tool_ctx.sandbox = tools::SandboxMode::Roots(vec![dir.clone()]);
        let tool_defs = tools::tool_definitions(None);
        let cancel = CancelToken::new();
        let opts = loop_opts(None);

        // Turn 1 (Inject): Write tool call so agent continues.
        // Turn 2 (Quit): Done call to finish.
        let target = dir.join("out.txt");
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::text("got it"),
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
                rig_core::test_utils::MockStreamEvent::text("all done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let dyn_model = gm(model.erase());
        let evt_tx_clone = handle.evt_tx.clone();
        tool_ctx.telemetry = AgentLoopTelemetry {
            stream_events: Some(StreamEventSender::Broadcast(evt_tx_clone.clone())),
            tool_output: None,
        };
        let agent = tokio::spawn(async move {
            run_agent_loop_core(
                &dyn_model,
                "hi",
                None,
                &tool_ctx,
                &tool_defs,
                &cancel,
                &opts,
                "[t] ",
                10,
                5.0,
                &mut None,
                &mut None,
                false,
                &[],
                0,
                0,
                &None,
                Some(session),
                None,
                None,
                None,
            )
            .await
        });

        // Wait for Ready.
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match evt_rx.recv().await {
                    Ok(InteractiveEvent::Ready { .. }) => return true,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert!(ready);

        // Inject a message.
        let _ = handle
            .cmd_tx
            .send(InteractiveCommand::Inject("look at this".into()))
            .await;

        // Wait for TurnComplete.
        let tc = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match evt_rx.recv().await {
                    Ok(InteractiveEvent::TurnComplete { .. }) => return true,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert!(tc, "should get TurnComplete after Inject");

        // Quit — agent resumes normal operation and completes.
        let _ = handle.cmd_tx.send(InteractiveCommand::Quit).await;

        let result = tokio::time::timeout(Duration::from_secs(2), agent)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("all done"));
    }

    /// Bail during interactive mode terminates the agent loop.
    #[tokio::test]
    async fn interactive_bail_terminates_agent() {
        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        handle.pause.pause();
        let mut evt_rx = handle.evt_tx.subscribe();

        let mut tool_ctx = test_tool_ctx();
        let tool_defs = vec![];
        let cancel = CancelToken::new();
        let opts = loop_opts(None);

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::text("ok"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "done"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let dyn_model = gm(model.erase());
        let evt_tx_clone = handle.evt_tx.clone();
        tool_ctx.telemetry = AgentLoopTelemetry {
            stream_events: Some(StreamEventSender::Broadcast(evt_tx_clone.clone())),
            tool_output: None,
        };
        let agent = tokio::spawn(async move {
            run_agent_loop_core(
                &dyn_model,
                "hi",
                None,
                &tool_ctx,
                &tool_defs,
                &cancel,
                &opts,
                "[t] ",
                10,
                5.0,
                &mut None,
                &mut None,
                false,
                &[],
                0,
                0,
                &None,
                Some(session),
                None,
                None,
                None,
            )
            .await
        });

        // Wait for Ready.
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match evt_rx.recv().await {
                    Ok(InteractiveEvent::Ready { .. }) => return true,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert!(ready);

        // Bail.
        let _ = handle
            .cmd_tx
            .send(InteractiveCommand::Bail("test bail".into()))
            .await;

        let result = tokio::time::timeout(Duration::from_secs(2), agent)
            .await
            .unwrap()
            .unwrap();
        match result {
            Err(ClientError::Bail { reason }) => {
                assert!(reason.contains("test bail"));
            }
            other => panic!("expected Bail, got {other:?}"),
        }
    }

    /// When the cmd_tx sender is dropped (simulating supervisor disconnect),
    /// the interactive loop returns Resumed gracefully.
    #[tokio::test]
    async fn interactive_cmd_tx_drop_resumes_gracefully() {
        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        handle.pause.pause();
        let mut evt_rx = handle.evt_tx.subscribe();

        let mut tool_ctx = test_tool_ctx();
        let tool_defs = vec![];
        let cancel = CancelToken::new();
        let opts = loop_opts(None);

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([[
            rig_core::test_utils::MockStreamEvent::text("ok"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "done"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let dyn_model = gm(model.erase());
        let evt_tx_clone = handle.evt_tx.clone();
        tool_ctx.telemetry = AgentLoopTelemetry {
            stream_events: Some(StreamEventSender::Broadcast(evt_tx_clone.clone())),
            tool_output: None,
        };
        let agent = tokio::spawn(async move {
            run_agent_loop_core(
                &dyn_model,
                "hi",
                None,
                &tool_ctx,
                &tool_defs,
                &cancel,
                &opts,
                "[t] ",
                10,
                5.0,
                &mut None,
                &mut None,
                false,
                &[],
                0,
                0,
                &None,
                Some(session),
                None,
                None,
                None,
            )
            .await
        });

        // Wait for Ready.
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match evt_rx.recv().await {
                    Ok(InteractiveEvent::Ready { .. }) => return true,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert!(ready);

        // Drop the handle's cmd_tx to simulate disconnect.
        drop(handle);

        // Agent should resume and complete normally.
        let result = tokio::time::timeout(Duration::from_secs(2), agent)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("ok"));
    }

    /// Covers the `run_agent_loop` wrapper path (not just `run_agent_loop_core`).
    /// Regression: the original class of bug — dropping the session while
    /// forwarding/cloning context — would pass the suite because every
    /// `run_agent_loop` test passed `None` and interactive tests called
    /// `run_agent_loop_core` directly.
    #[tokio::test]
    async fn run_agent_loop_with_interactive_session_broadcasts_ready() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-ral-int-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        // Pre-set the pause token so the agent enters interactive mode
        // at the turn boundary before the first API call.
        handle.pause.pause();

        let mut evt_rx = handle.evt_tx.subscribe();

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

        let agent = tokio::spawn(async move {
            run_agent_loop(
                gm(model.erase()),
                "hi",
                ctx,
                cancel,
                loop_opts(None),
                Some(session),
            )
            .await
        });

        // Wait for Ready.
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match evt_rx.recv().await {
                    Ok(InteractiveEvent::Ready { .. }) => return true,
                    Ok(InteractiveEvent::Ended { .. }) => return false,
                    Err(RecvError::Closed) => return false,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap();
        assert!(
            ready,
            "agent should broadcast Ready when paused via run_agent_loop"
        );

        // Send Quit to resume.
        let _ = handle.cmd_tx.send(InteractiveCommand::Quit).await;

        // Agent should complete normally.
        let result = tokio::time::timeout(Duration::from_secs(2), agent)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.text_result.as_deref(), Some("ok"));
    }

    /// Fix 1 regression: a solo `Done` never flows through the normal
    /// tool-execution path, so the completion handler must itself emit the
    /// `tool: Done <summary>` log line and record the Done action-ledger entry
    /// in the returned history. Both are asserted here at the loop level.
    #[tokio::test]
    async fn loop_done_emits_log_and_ledger() {
        let model = gm(
            rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
                rig_core::test_utils::MockStreamEvent::text("all finished"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "implemented the fix"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ]])
            .erase(),
        );

        let tool_ctx = test_tool_ctx();
        let tool_defs = tools::tool_definitions(None);
        let cancel = CancelToken::new();
        let opts = loop_opts(None);
        let (log_tx, mut log_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let log_slot = Some(log_tx);

        let result = run_agent_loop_core(
            &model,
            "do the thing",
            None,
            &tool_ctx,
            &tool_defs,
            &cancel,
            &opts,
            "[t] ",
            10,
            5.0,
            &mut None,
            &mut None,
            false,
            &[],
            0,
            0,
            &log_slot,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // The Done summary must reach the `tool:` log line.
        let mut logs = Vec::new();
        while let Ok(line) = log_rx.try_recv() {
            logs.push(line);
        }
        assert!(
            logs.iter()
                .any(|l| l.contains("tool: Done implemented the fix")),
            "Done must reach the tool log line, got: {logs:#?}"
        );

        // The Done action ledger must be persisted in the returned history.
        let messages = result.messages.expect("Done run must return messages");
        let ledger = messages
            .iter()
            .find_map(|m| match m {
                Message::User { content } => content.iter().find_map(|c| match c {
                    UserContent::Text(t)
                        if t.text.starts_with("Actions taken this turn:")
                            && t.text.contains("- Done implemented the fix") =>
                    {
                        Some(t.text.clone())
                    }
                    _ => None,
                }),
                _ => None,
            })
            .expect("Done ledger entry must be present in returned history");
        assert!(ledger.contains("- Done implemented the fix"), "{ledger}");
    }
}
