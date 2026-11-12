use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use rig_core::completion::message::{CallId, Reasoning, Sealed, ToolCall, ToolName};
use rig_core::completion::{CompletionRequest, Message, ToolDefinition, Usage};
use rig_core::driver::DynModel;
use rig_core::error::ProviderError;
use rig_core::operation::Completion;
use rig_core::streaming::Streamed;
use tokio::sync::{mpsc, Semaphore};

use crate::clients::backend::RunParams;
use crate::clients::interactive::{
    InteractiveCommand, InteractiveEvent, InteractiveSession, PauseToken, StreamEventSender,
};

use super::interactive::{interactive_loop, maybe_pause, InteractiveLoopResult};
use super::plug::LoopHooks;
use crate::clients::cancel::{default_classify, is_retryable, CancelToken, ErrorClassifier};
use crate::clients::conversation::{Conversation, TurnScriptError};
use crate::clients::error::ClientError;
use crate::clients::format::{enrich_log_line, format_tool_call_parts, trunc};
use crate::clients::protocol::{CompletedRun, TurnStats, UsageStats};
use crate::clients::tools::tool_specs::ToolMode;
use crate::clients::tools::{self, AgentLoopRuntime, AgentLoopTelemetry, ToolOutcome};

// ── Architecture ────────────────────────────────────────────────────────────
// agent_loop.rs orchestrates a single agent turn-sequence:
//
//   LoopState::run_agent_loop_core           // main while-loop
//     ├─ LoopState::stream_turn              // one model completion turn
//     │   ├─ prepare_stream_request           // build messages
//     │   ├─ launch_model_stream              // error-handling + retry
//     │   ├─ consume_stream_events            // the async event loop
//     │   └─ finish_stream_turn               // post-stream classification
//     └─ LoopState::execute_tools            // batch tool execution
//         ├─ collect_tool_jobs                // validate + build ToolJob list
//         ├─ dispatch_tool_jobs               // concurrent execute with cancel
//         └─ emit_tool_results                // history + prompt roll-forward
//
// Supporting types: ToolJob, StreamAccumulator, StreamTurnOutcome,
// LoopOpts, InteractiveSession, TurnFlow, etc.
// ────────────────────────────────────────────────────────────────────────────
pub(super) fn cancelled_error(message: impl Into<String>, history: &[Message]) -> ClientError {
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
    pub(super) async fn send_stream_event(
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
    pub(super) fn wants_stream_events(&self) -> bool {
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

/// Sleep for the turn-level backoff, aborting early when the cancel token is
/// set. Returns `true` if cancellation was observed (the caller must abandon
/// the retry and unwind), `false` if the full backoff elapsed.
pub(super) async fn retry_backoff(cancel: &CancelToken, wait: f64) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs_f64(wait)) => false,
        _ = cancel.cancelled() => true,
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
    pub(crate) prepopulated_conversation: Box<dyn Conversation>,
    /// Inversion-of-control hooks carrying cross-package dependencies
    /// (prompt-debug, token-usage).
    pub(crate) hooks: LoopHooks,
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
        Self {
            idle_timeout,
            prefix,
            expected_artifact_paths: params.expected_artifact_paths.clone(),
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
            stream_events: params.stream_events.clone(),
            prepopulated_conversation: params.prepopulated_conversation.clone(),
            hooks: params.hooks.clone().unwrap_or_else(LoopHooks::new),
            params,
        }
    }
}

pub(crate) struct LoopOpts<'a> {
    pub(crate) extra: Option<serde_json::Value>,
    pub(crate) tool_filter: Option<&'a [String]>,
    pub(crate) tool_mode: crate::clients::tools::tool_specs::ToolMode,
    pub(crate) classify_error: Option<ErrorClassifier>,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) preserve_reasoning_in_history: bool,
    /// Fallback system preamble for naked runs that have no conversation
    /// system prompt (e.g. local compaction). Ignored when the rendered turn
    /// already carries a system prompt.
    pub(crate) preamble: Option<String>,
    /// When true, the agent loop will not loop on reasoning-only or empty
    /// turns — it immediately exhausts. Used by single-turn naked
    /// completions (local compaction, etc.) where the conversation has no
    /// record_turn support and budgets are zero.
    pub(crate) single_turn: bool,
}

/// A single tool invocation queued for concurrent execution within a turn.
pub(super) struct ToolJob {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) args: String,
    pub(super) call: String,
    pub(super) over_cap: Option<String>,
}

/// Outcome of one concurrent tool batch dispatched by
/// [`LoopState::dispatch_tool_jobs`].
pub(super) enum ToolDispatchOutcome {
    /// Every tool finished; outcomes are aligned with the job list order.
    Completed(Vec<ToolOutcome>),
    /// The operator entered debug mid-batch: the batch was abandoned, the
    /// turn's history was unwound, and the caller should re-execute the turn.
    ReexecuteTurn,
}

/// Mutable state threaded through one agent-loop run.
pub(super) struct LoopState {
    pub(super) system_prompt: Option<String>,
    pub(super) original_system_prompt: Option<String>,
    pub(super) live_output_rx: Option<mpsc::Receiver<tools::ToolOutputLine>>,
    pub(super) conv: Box<dyn Conversation>,
    pub(super) turns: usize,
    pub(super) turn_num: usize,
    pub(super) turn_retries: usize,
    pub(super) timeout_retries: usize,
    pub(super) timed_out: bool,
    pub(super) exhausted_turn_retries: bool,
    pub(super) exhausted_timeout_retries: bool,
    pub(super) stream_error: Option<ProviderError>,
    pub(super) total_prompt_tokens: Option<u64>,
    pub(super) total_completion_tokens: Option<u64>,
    pub(super) total_cached_tokens: Option<u64>,
    pub(super) total_cache_creation_tokens: Option<u64>,
    pub(super) total_reasoning_tokens: Option<u64>,
    pub(super) interactive_active: bool,
    pub(super) pending_run_one_turn: bool,
    pub(super) cmd_rx: Option<mpsc::Receiver<InteractiveCommand>>,
    pub(super) evt_tx: Option<tokio::sync::broadcast::Sender<InteractiveEvent>>,
    pub(super) pause: Option<Arc<PauseToken>>,
    pub(super) remaining_turns: usize,
    pub(super) reminder_budget: usize,
    pub(super) completion_nudge_budget: usize,
    pub(super) single_turn: bool,
    pub(super) hooks: LoopHooks,
}

/// Accumulated state from one model completion stream.
pub(super) struct StreamAccumulator {
    pub(super) text: String,
    pub(super) reasoning: String,
    pub(super) reasoning_items: Vec<Sealed<Reasoning>>,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) ended: bool,
    pub(super) turn_usage: Option<Usage>,
    pub(super) paused_mid_stream: bool,
    pub(super) retry_turn: bool,
    pub(super) retry_timeout_turn: bool,
    pub(super) first_token: Option<Instant>,
    pub(super) last_token: Option<Instant>,
}

/// Result of streaming one turn's request: the accumulated assistant content
/// plus the timestamps needed for metric emission by the caller.
pub(super) struct StreamTurnOutput {
    pub(super) text: String,
    pub(super) reasoning: String,
    pub(super) reasoning_items: Vec<Sealed<Reasoning>>,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) turn_usage: Option<Usage>,
    pub(super) first_token: Option<Instant>,
    pub(super) last_token: Option<Instant>,
    pub(super) ended: bool,
}

/// Control-flow signal returned by [`LoopState::stream_turn`].
pub(super) enum StreamTurnOutcome {
    /// A turn's stream completed — caller processes the accumulated output.
    Output(Box<StreamTurnOutput>),
    /// Caller should re-execute the turn (retry / pause / idle timeout).
    Continue,
    /// Caller should stop the loop (terminal stream error).
    Break,
}

/// Decision from the interactive turn-boundary gate: either re-enter the
/// interactive loop, or proceed to execute a normal turn.
pub(super) enum InteractiveBoundary {
    ContinueLoop,
    RunTurn,
}

/// Result of inspecting the assistant's tool calls for a Done signal.
pub(super) enum DoneDecision {
    /// No Done call present — proceed to execute the remaining tools.
    NotDone,
    /// Done was rejected (mixed or missing artifacts) — re-run the turn.
    RetryNeeded,
    /// Done was accepted — the run is complete.
    Accepted(Box<CompletedRun>),
}

/// Result of handling a turn that produced no tool calls.
pub(super) enum EmptyTurnAction {
    ContinueLoop,
    ReturnRun(Box<CompletedRun>),
}

impl LoopState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        system_prompt: Option<String>,
        prompt: &str,
        prepopulated_conversation: Box<dyn Conversation>,
        reminder_budget: usize,
        completion_nudge_budget: usize,
        single_turn: bool,
        interactive: Option<InteractiveSession>,
        tool_output_rx: Option<mpsc::Receiver<tools::ToolOutputLine>>,
        max_turns: usize,
        prefix: &str,
        preserve_reasoning_in_history: bool,
        hooks: &LoopHooks,
    ) -> Self {
        // Destructure the interactive session into its components so we can
        // thread them independently through the loop.
        let (cmd_rx, evt_tx, pause) = interactive
            .map(|s| {
                log::debug!("agent_loop: interactive session set up (label={prefix})");
                (Some(s.cmd_rx), Some(s.evt_tx), Some(s.pause))
            })
            .unwrap_or((None, None, None));

        let original_system_prompt = system_prompt.clone();

        let mut conv = prepopulated_conversation;
        // Always apply the stage prompt so chat continuations (built by
        // run.rs with an empty "" initial prompt) receive the stage
        // instructions in their first user message.
        conv.set_next_prompt(prompt);
        // Chat continuations are built with preserve_reasoning=false
        // hardcoded. Apply the run's actual setting here.
        conv.set_preserve_reasoning(preserve_reasoning_in_history);

        Self {
            system_prompt,
            original_system_prompt,
            live_output_rx: tool_output_rx,
            conv,
            turns: 0,
            turn_num: 0,
            turn_retries: 0,
            timeout_retries: 0,
            timed_out: false,
            exhausted_turn_retries: false,
            exhausted_timeout_retries: false,
            stream_error: None,
            total_prompt_tokens: None,
            total_completion_tokens: None,
            total_cached_tokens: None,
            total_cache_creation_tokens: None,
            total_reasoning_tokens: None,
            interactive_active: false,
            pending_run_one_turn: false,
            cmd_rx,
            evt_tx,
            pause,
            remaining_turns: max_turns,
            reminder_budget,
            completion_nudge_budget,
            single_turn,
            hooks: hooks.clone(),
        }
    }

    /// Emit a log line through the per-gremlin log channel wired by the
    /// executor. The bus routes each `LogLine` event through `log::info!()`
    /// itself, so this must not also call `log::info!` directly.
    pub(super) fn hook_log(&self, msg: &str) {
        self.hooks.log_line(msg);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_turn(
        &mut self,
        model: &DynModel<Completion>,
        tool_ctx: &AgentLoopRuntime,
        tool_defs: &[ToolDefinition],
        cancel: &CancelToken,
        opts: &LoopOpts<'_>,
        prefix: &str,
        idle_timeout: f64,
        nested: bool,
        turn_retry_backoff: &[f64],
        timeout_retry_backoff: &[f64],
    ) -> Result<StreamTurnOutcome, ClientError> {
        // Phase 1: assemble the request (messages, gen params).
        // Race against cancellation so an unresponsive compaction endpoint
        // cannot block shutdown.
        let builder = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(cancelled_error("cancelled during request preparation", &self.conv.messages()));
            }
            b = self.prepare_stream_request(tool_defs, opts, nested) => b?,
        };

        // Dump the full prompt to the state directory when
        // GREMLINS_LOG_LEVEL=DEBUG and a debug store is configured.
        self.hooks.dump_prompt_debug(&builder);

        // Phase 2: launch the model stream, handling pre-stream errors and
        // retry inline (each path returns early).
        let response = match model.stream(builder) {
            Ok(s) => s,
            Err(e) => {
                // Retryable (429/5xx) error before streaming began — back off
                // and re-execute this same turn with history intact.
                if self.turn_retries < turn_retry_backoff.len()
                    && is_retryable(&e, opts.classify_error)
                {
                    let wait = turn_retry_backoff[self.turn_retries];
                    self.turn_retries += 1;
                    self.hook_log(&format!(
                        "agent_loop: retryable stream error on turn {} (attempt {}), backing off {wait}s: {e}", self.turn_num, self.turn_retries
                    ));
                    self.hook_log(&format!(
                        "{prefix}retry: transient error, backing off {wait}s"
                    ));
                    if retry_backoff(cancel, wait).await {
                        self.hook_log(&format!("{prefix}exit: cancelled during retry backoff"));
                        return Err(cancelled_error("cancelled", &self.conv.messages()));
                    }
                    // Preserve the pending one-turn state only when
                    // already interactive — otherwise an unconditional
                    // set would force RunOneTurn after normal resumption.
                    if self.interactive_active {
                        self.pending_run_one_turn = true;
                    }
                    return Ok(StreamTurnOutcome::Continue);
                }
                let retryable = is_retryable(&e, opts.classify_error);
                if retryable {
                    self.exhausted_turn_retries = true;
                }
                self.stream_error = Some(e);
                return Ok(StreamTurnOutcome::Break);
            }
        };

        // Phase 3: drive the async event loop, accumulating assistant content.
        let acc = self
            .consume_stream_events(
                response,
                model,
                tool_ctx,
                cancel,
                opts,
                prefix,
                idle_timeout,
                nested,
                timeout_retry_backoff,
                turn_retry_backoff,
            )
            .await?;

        // Phase 4: classify the completed stream into a control-flow outcome.
        self.finish_stream_turn(acc, tool_ctx, cancel, nested).await
    }

    /// Build the [`CompletionRequest`] for one model turn: assemble the request
    /// messages,
    /// and apply the caller's generation parameters.
    pub(super) async fn prepare_stream_request(
        &mut self,
        tool_defs: &[ToolDefinition],
        opts: &LoopOpts<'_>,
        nested: bool,
    ) -> Result<CompletionRequest, ClientError> {
        let effective_system = self.system_prompt.clone().or_else(|| opts.preamble.clone());
        let script = self
            .conv
            .render_turn(nested, effective_system)
            .await
            .map_err(|e| match e {
                TurnScriptError::TooLarge { limit, actual } => {
                    ClientError::PromptTooLarge { limit, actual }
                }
            })?;

        let mut builder = CompletionRequest::new(script.next_user_prompt)
            .messages(script.messages)
            .tools(tool_defs.to_vec());
        if let Some(ref sys) = script.system_prompt {
            builder = builder.preamble(sys.clone());
        } else if let Some(preamble) = opts.preamble.clone() {
            builder = builder.preamble(preamble);
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
        Ok(builder)
    }

    /// Drive the model stream to completion: forward live deltas, accumulate
    /// assistant content, and handle cancellation, operator pause, idle
    /// timeout, and mid-stream retry from within the event loop.
    #[allow(clippy::too_many_arguments)]
    async fn consume_stream_events(
        &mut self,
        response: Streamed<Completion>,
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
        super::stream_consume::consume_stream_events(
            self,
            response,
            model,
            tool_ctx,
            cancel,
            opts,
            prefix,
            idle_timeout,
            nested,
            timeout_retry_backoff,
            turn_retry_backoff,
        )
        .await
    }

    /// Classify a completed stream into a [`StreamTurnOutcome`], applying the
    /// pause/retry continuations and otherwise packaging the turn output.
    pub(super) async fn finish_stream_turn(
        &mut self,
        acc: StreamAccumulator,
        tool_ctx: &AgentLoopRuntime,
        cancel: &CancelToken,
        nested: bool,
    ) -> Result<StreamTurnOutcome, ClientError> {
        super::stream_consume::finish_stream_turn(self, acc, tool_ctx, cancel, nested).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_tools(
        &mut self,
        tool_calls: &[ToolCall],
        tool_ctx: &AgentLoopRuntime,
        model: &DynModel<Completion>,
        prefix: &str,
        cancel: &CancelToken,
        raw: &mut Option<std::fs::File>,
        captured: &mut Option<Vec<serde_json::Value>>,
        nested: bool,
    ) -> Result<(), ClientError> {
        // Phase 1: collect + validate tool calls into owned jobs.
        let jobs = self.collect_tool_jobs(tool_calls, prefix, nested, raw, captured);

        // Phase 2: concurrent execution (cancel/pause aware).
        let outcomes = match self
            .dispatch_tool_jobs(&jobs, tool_ctx, model, prefix, cancel)
            .await?
        {
            ToolDispatchOutcome::Completed(outcomes) => outcomes,
            // Operator entered debug mid-batch; the turn is re-executed.
            ToolDispatchOutcome::ReexecuteTurn => return Ok(()),
        };

        // Phase 3: emit results, extend history, roll the prompt forward.
        self.emit_tool_results(
            jobs, outcomes, tool_ctx, prefix, nested, raw, captured, cancel,
        )
        .await?;

        Ok(())
    }

    /// Validate the turn's tool calls and build the owned [`ToolJob`] list.
    /// Emits per-call tool-start events (unless nested) and enforces the
    /// per-message Task cap by marking over-cap jobs for rejection.
    pub(super) fn collect_tool_jobs(
        &mut self,
        tool_calls: &[ToolCall],
        prefix: &str,
        nested: bool,
        raw: &mut Option<std::fs::File>,
        captured: &mut Option<Vec<serde_json::Value>>,
    ) -> Vec<ToolJob> {
        let mut jobs: Vec<ToolJob> = Vec::new();
        let mut task_count: usize = 0;
        for tc in tool_calls {
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
            self.hook_log(&format!("{prefix}{tool_msg}"));
            if !nested {
                let id_str = tc.id.to_string();
                let tool_evt =
                    tool_use_event(&id_str, tc.function.name.as_str(), &tc.function.arguments);
                write_raw(raw, &tool_evt);
                if let Some(evts) = captured.as_mut() {
                    evts.push(tool_evt);
                }
            }
            jobs.push(ToolJob {
                id: tc.id.to_string(),
                name: tc.function.name.to_string(),
                args: args_json,
                call: format_tool_call_parts(&tc.function.name, &tc.function.arguments),
                over_cap: tools::max_per_message(tc.function.name.as_str()).and_then(|cap| {
                    task_count += 1;
                    (task_count > cap).then(|| {
                        format!(
                            "Error: at most {} {} calls per message",
                            cap, tc.function.name
                        )
                    })
                }),
            });
        }
        jobs
    }

    /// Execute a batch of tool jobs concurrently, capped by a semaphore so
    /// excess calls queue rather than being rejected (preserving result
    /// order). A `tokio::select!` races the batch against cancel and the
    /// operator pause token so a long batch stays interruptible.
    ///
    /// The fan-out cap is *not* re-checked here: it is a hard rejection
    /// applied in [`Self::collect_tool_jobs`] via each tool's
    /// `max_per_message()` policy, because a capped tool spawns a child loop.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dispatch_tool_jobs(
        &mut self,
        jobs: &[ToolJob],
        tool_ctx: &AgentLoopRuntime,
        model: &DynModel<Completion>,
        prefix: &str,
        cancel: &CancelToken,
    ) -> Result<ToolDispatchOutcome, ClientError> {
        let ctx = tool_ctx.clone();
        // Forward live tool output only for the duration of this batch. The
        // receiver is borrowed by the select below, so later batches never
        // hear stale chunks and Esc/clear cannot resurrect old output.
        // Capacity is reserved on the chat stream *before* a tool line is
        // dequeued, so the select dropping this branch at batch completion
        // can release an unused permit but can never drop an in-flight line.
        let forward_live_output = async {
            if let (Some(se), Some(rx)) = (
                &tool_ctx.telemetry.stream_events,
                self.live_output_rx.as_mut(),
            ) {
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
        let sem = Arc::new(Semaphore::new(tools::MAX_CONCURRENT_TOOLS));
        // Borrow self.conv as &dyn Conversation so concurrent async blocks
        // can seed child task loops without needing a mutable borrow.
        let conv: &dyn Conversation = &*self.conv;
        let exec_fut = join_all(jobs.iter().map(|j| {
            let ctx = &ctx;
            let sem = sem.clone();
            async move {
                if let Some(msg) = &j.over_cap {
                    ToolOutcome::text(msg.clone())
                } else {
                    let _permit = sem.acquire_owned().await;
                    tools::invoke_with_conversation(&j.id, &j.name, ctx, &j.args, conv).await
                }
            }
        }));

        let outcomes = tokio::select! {
            outcomes = exec_fut => outcomes,
            _ = forward_live_output => unreachable!("live-output forwarder parks forever"),
            _ = cancel.cancelled() => {
                // Esc / cancel_chat during a tool batch: drop the join_all
                // future (cancelling pending tool futures) and unwind the
                // turn immediately. This is what lets run_chat_turn::finish
                // re-insert the chat gremlin without waiting for every tool
                // to complete.
                self.hook_log(&format!("agent_loop: cancelled mid-tool (label={})", prefix));
                self.hook_log(&format!("{prefix}exit: cancelled mid-tool"));
                discard_tool_output(&mut self.live_output_rx);
                return Err(cancelled_error("cancelled", &self.conv.messages()));
            }
            _ = maybe_pause(&self.pause) => {
                // Operator triggered debug mid-tool — drop the join_all future
                // (cancels pending tool futures). Completed tool side effects
                // (files written, bash commands run) are not unwound.
                self.hook_log(&format!("agent_loop: paused mid-tool (label={})", prefix));
                self.hook_log(&format!("{prefix}pause: entering interactive mode (mid-tool)"));
                discard_tool_output(&mut self.live_output_rx);
                if let Some(ref p) = self.pause {
                    p.reset();
                }
                log::debug!("agent_loop: entering interactive mode mid-tool (label={})", prefix);

                // Pop the assistant turn so it can be re-executed
                // cleanly without duplicate history entries.
                self.conv.pop_last_assistant_turn();

                // Amend system prompt with operator note.
                let debug_note = "\n\nThe operator has connected in debug mode. Messages prefixed with\n[operator]: are direct instructions from the operator. Treat them as\nauthoritative. When the operator disconnects, continue with your\noriginal task.";
                let amended_system = match &self.system_prompt {
                    Some(sp) => format!("{sp}{debug_note}"),
                    None => debug_note.trim_start().to_string(),
                };
                self.system_prompt = Some(amended_system);

                // Enter interactive mode.
                if self.cmd_rx.is_some() && self.evt_tx.is_some() && self.pause.is_some() {
                    let mut session = InteractiveSession {
                        cmd_rx: self.cmd_rx.take().unwrap(),
                        evt_tx: self.evt_tx.take().unwrap(),
                        pause: self.pause.clone().unwrap(),
                    };
                    match interactive_loop(
                        model,
                        &mut *self.conv,
                        &mut session,
                        self.turn_num,
                    )
                    .await
                    {
                        Ok(InteractiveLoopResult::Resumed) => {
                            self.system_prompt = self.original_system_prompt.clone();
                            self.cmd_rx = Some(session.cmd_rx);
                            self.evt_tx = Some(session.evt_tx);
                            self.interactive_active = false;
                            return Ok(ToolDispatchOutcome::ReexecuteTurn);
                        }
                        Ok(InteractiveLoopResult::RunOneTurn) => {
                            self.cmd_rx = Some(session.cmd_rx);
                            self.evt_tx = Some(session.evt_tx);
                            self.pending_run_one_turn = true;
                            return Ok(ToolDispatchOutcome::ReexecuteTurn);
                        }
                        Err(e) => {
                            self.hook_log(&format!("agent_loop: exit (operator bailed, label={})", prefix));
                            self.hook_log(&format!("{prefix}exit: operator bailed"));
                            return Err(e);
                        }
                    }
                }
                return Ok(ToolDispatchOutcome::ReexecuteTurn);
            }
        };

        Ok(ToolDispatchOutcome::Completed(outcomes))
    }

    /// Emit tool results in request order: drain trailing live output,
    /// publish ToolResult events, extend history, and roll the prompt forward.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn emit_tool_results(
        &mut self,
        jobs: Vec<ToolJob>,
        outcomes: Vec<ToolOutcome>,
        tool_ctx: &AgentLoopRuntime,
        prefix: &str,
        nested: bool,
        raw: &mut Option<std::fs::File>,
        captured: &mut Option<Vec<serde_json::Value>>,
        cancel: &CancelToken,
    ) -> Result<(), ClientError> {
        // Tools send their final chunks just before returning. The select
        // above stops at the first completed future, so drain anything still
        // sitting in the receiver before emitting ToolResult events.
        if let Some(rx) = self.live_output_rx.as_mut() {
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
                        &self.conv.messages(),
                    )
                    .await?;
            }
        }

        // Jobs and results are 1:1 and in request order; `job_count` stands in
        // for `tool_calls.len()` when bumping the turn counter below.
        let job_count = jobs.len();
        let mut result_msgs = Vec::new();
        for (job, outcome) in jobs.into_iter().zip(outcomes) {
            let output = outcome.output;
            let exit_code = outcome.exit_code;
            let result_msg = format!("result: {}", trunc(&output, 200));
            self.hook_log(&format!("{prefix}{result_msg}"));
            if !nested {
                let result_evt = tool_result_event(&job.id, &output, exit_code);
                write_raw(raw, &result_evt);
                if let Some(evts) = captured.as_mut() {
                    evts.push(result_evt);
                }
            }
            // Emit tool result via stream_events when there is a listener.
            if tool_ctx.telemetry.wants_stream_events() {
                // Per-tool preview budget (Read uses a larger preview).
                let display = trunc(&output, tools::display_truncation(&job.name));
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
                        &self.conv.messages(),
                    )
                    .await?;
            }

            result_msgs.push(Message::tool_result(
                CallId::from_wire(job.id.clone()),
                ToolName::new(job.name.clone()).expect("tool name must not be empty"),
                output,
            ));
        }
        self.turns += job_count;
        // Every tool result lands in history so the results stay adjacent to
        // the assistant tool_calls message.
        self.conv.record_tool_results(result_msgs);
        self.remaining_turns -= 1;
        Ok(())
    }

    /// Interactive turn boundary. Two modes: no active session (check the
    /// pause token) or an active session re-entered after RunOneTurn.
    async fn await_interactive_turn(
        &mut self,
        model: &DynModel<Completion>,
        prefix: &str,
    ) -> Result<InteractiveBoundary, ClientError> {
        super::interactive::await_interactive_turn(self, model, prefix).await
    }

    /// Fold this turn's provider usage into the running totals and return the
    /// cumulative [`UsageStats`]. Absent fields stay `None`; a reported zero
    /// is preserved as `Some(0)`.
    fn fold_turn_usage(&mut self, turn_usage: Option<&Usage>) -> UsageStats {
        super::turn_flow::fold_turn_usage(self, turn_usage)
    }

    /// Emit TurnComplete plus the raw/captured transcript entries and turn
    /// metrics for a completed turn.
    #[allow(clippy::too_many_arguments)]
    async fn emit_turn_artifacts(
        &mut self,
        tool_ctx: &AgentLoopRuntime,
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
    ) -> Result<(), ClientError> {
        super::turn_flow::emit_turn_artifacts(
            self,
            tool_ctx,
            cancel,
            nested,
            text,
            reasoning,
            tool_calls,
            turn_stats,
            totals,
            first_token,
            last_token,
            turn_start,
            turn_usage,
            raw,
            captured,
            prefix,
        )
        .await
    }

    /// Inspect the assistant's tool calls for a Done signal and act on it.
    #[allow(clippy::too_many_arguments)]
    async fn handle_done_decision(
        &mut self,
        tool_ctx: &AgentLoopRuntime,
        cancel: &CancelToken,
        nested: bool,
        text: &str,
        reasoning_items: &[Sealed<Reasoning>],
        tool_calls: &[ToolCall],
        expected_artifact_paths: &[PathBuf],
        captured: &mut Option<Vec<serde_json::Value>>,
        loop_start: Instant,
        prefix: &str,
    ) -> Result<DoneDecision, ClientError> {
        super::turn_flow::handle_done_decision(
            self,
            tool_ctx,
            cancel,
            nested,
            text,
            reasoning_items,
            tool_calls,
            expected_artifact_paths,
            captured,
            loop_start,
            prefix,
        )
        .await
    }

    /// Handle a turn that produced no tool calls: continue (interactive / or
    /// nudged / reminded) or return the exhausted run.
    #[allow(clippy::too_many_arguments)]
    fn handle_empty_turn(
        &mut self,
        nested: bool,
        text: &str,
        reasoning: &str,
        reasoning_items: &[Sealed<Reasoning>],
        expected_artifact_paths: &[PathBuf],
        loop_start: Instant,
        raw: &mut Option<std::fs::File>,
        captured: &mut Option<Vec<serde_json::Value>>,
        prefix: &str,
    ) -> EmptyTurnAction {
        super::turn_flow::handle_empty_turn(
            self,
            nested,
            text,
            reasoning,
            reasoning_items,
            expected_artifact_paths,
            loop_start,
            raw,
            captured,
            prefix,
        )
    }

    /// Append the current assistant turn to the conversation history.
    fn commit_turn_to_history(
        &mut self,
        text: &str,
        reasoning_items: &[Sealed<Reasoning>],
        tool_calls: &[ToolCall],
        input_tokens: Option<u64>,
    ) {
        super::turn_flow::commit_turn_to_history(
            self,
            text,
            reasoning_items,
            tool_calls,
            input_tokens,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop(
    model: DynModel<Completion>,
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
    log::info!("{prefix}{log_line}");

    if cwd.is_none() {
        log::info!("{prefix}warning: no cwd set for worktree enforcement");
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

    let worktree = crate::core::paths::worktree_root(cwd.as_deref());
    let audit_log = raw_path.as_ref().map(|p| tools::audit_log_path(p));
    let max_turns = crate::config::max_agent_turns();
    // When the run requests allow-all, skip building the roots list entirely;
    // otherwise assemble [worktree, artifact_dir, extra_sandbox_roots…].
    let sandbox = if ctx.params.allow_all {
        tools::SandboxMode::AllowAll
    } else {
        let mut allowed_roots = vec![worktree];
        if let Some(ref artifact_dir) = ctx.params.artifact_dir {
            allowed_roots.push(crate::core::paths::canonicalize_or_ancestor(artifact_dir));
        }
        for root in &ctx.params.extra_sandbox_roots {
            allowed_roots.push(crate::core::paths::canonicalize_or_ancestor(root));
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
        log_line_fn: Some(ctx.hooks.log_line_fn.clone()),
        id_prefix: String::new(),
        telemetry,
        active_skills: ctx.params.active_skills.clone(),
        skills_available: ctx.params.skills_available,
        discovered_skills: ctx.params.discovered_skills.clone(),
    };
    let tool_defs = tools::tool_definitions(
        opts.tool_filter,
        ctx.params.skills_available,
        opts.tool_mode,
    );

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
        opts.temperature,
        opts.max_tokens,
        opts.extra.clone(),
        opts.preserve_reasoning_in_history,
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
        &ctx.hooks,
        interactive,
        tool_output_rx,
        ctx.prepopulated_conversation.clone(),
    )
    .await
}

/// Nested agent loop — same logic as the parent loop but without raw transcript
/// writes, captured-event pushes, or final/summary stream emissions.
/// Stream events (think, text, tool, result, turn metrics) are emitted.
/// Used by the Task tool.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop_nested(
    model: DynModel<Completion>,
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
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    extra: Option<serde_json::Value>,
    preserve_reasoning_in_history: bool,
    prepopulated_conversation: Box<dyn Conversation>,
) -> Result<CompletedRun, ClientError> {
    let opts = LoopOpts {
        extra,
        tool_filter,
        tool_mode: crate::clients::tools::tool_specs::ToolMode::Full,
        classify_error: None,
        temperature,
        max_tokens,
        preserve_reasoning_in_history,
        preamble: None,
        single_turn: false,
    };
    // Nested loops never expose LoadSkill even when the parent has skills.
    let is_chat = false;
    let tool_defs = tools::tool_definitions(tool_filter, is_chat, ToolMode::Full);
    let mut raw: Option<std::fs::File> = None;
    let mut captured: Option<Vec<serde_json::Value>> = None;
    let mut nested_tool_ctx = tool_ctx.clone();
    nested_tool_ctx.skills_available = false;
    nested_tool_ctx.active_skills = None;
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
    // Nested-loop log lines go through the process logger; callers observe
    // task begin/end events via the process-wide logger.
    let hooks = LoopHooks {
        log_line_fn: tool_ctx
            .log_line_fn
            .clone()
            .unwrap_or_else(|| Arc::new(|_| {})),
        ..LoopHooks::new()
    };
    hooks.log_line(&format!("{prefix}task: begin (max_turns={max_turns})"));
    log::info!("{prefix}task: begin (max_turns={max_turns})");
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
        &hooks,
        None,
        None,
        prepopulated_conversation,
    )
    .await;
    log::info!("{prefix}task: end");
    hooks.log_line(&format!("{prefix}task: end"));
    result
}

// ── helpers for completion-decision bookkeeping ───────────────────────────

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

// ── agent loop ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop_core(
    model: &DynModel<Completion>,
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
    reminder_budget: usize,
    completion_nudge_budget: usize,
    hooks: &LoopHooks,
    interactive: Option<InteractiveSession>,
    tool_output_rx: Option<mpsc::Receiver<tools::ToolOutputLine>>,
    prepopulated_conversation: Box<dyn Conversation>,
) -> Result<CompletedRun, ClientError> {
    let mut state = LoopState::new(
        system_prompt,
        prompt,
        prepopulated_conversation,
        reminder_budget,
        completion_nudge_budget,
        opts.single_turn,
        interactive,
        tool_output_rx,
        max_turns,
        prefix,
        opts.preserve_reasoning_in_history,
        hooks,
    );
    let loop_start = Instant::now();
    let turn_retry_backoff = crate::config::turn_retry_backoff();
    let timeout_retry_backoff = crate::config::timeout_retry_backoff();
    while state.remaining_turns > 0 {
        if cancel.is_cancelled() {
            log::info!("agent_loop: cancelled before turn (label={})", prefix);
            state.hook_log(&format!("{prefix}exit: cancelled before turn"));
            return Err(cancelled_error("cancelled", &state.conv.messages()));
        }

        match state.await_interactive_turn(model, prefix).await? {
            InteractiveBoundary::ContinueLoop => continue,
            InteractiveBoundary::RunTurn => {}
        }

        // Snapshot before request construction so TTFT includes connection /
        // queue latency, not just server-side time-to-first-token.
        let turn_start = Instant::now();

        let outcome = state
            .stream_turn(
                model,
                tool_ctx,
                tool_defs,
                cancel,
                opts,
                prefix,
                idle_timeout,
                nested,
                &turn_retry_backoff,
                &timeout_retry_backoff,
            )
            .await?;
        let out = match outcome {
            StreamTurnOutcome::Continue => continue,
            StreamTurnOutcome::Break => break,
            StreamTurnOutcome::Output(out) => *out,
        };
        let StreamTurnOutput {
            text,
            reasoning,
            reasoning_items,
            tool_calls,
            turn_usage,
            first_token,
            last_token,
            ended,
        } = out;

        if state.timed_out || state.stream_error.is_some() {
            log::debug!(
                "stream ended: timed_out={} stream_error={:?} turn={}",
                state.timed_out,
                state.stream_error,
                state.turn_num,
            );
            break;
        }
        if !ended && tool_calls.is_empty() && text.is_empty() {
            log::debug!("stream ended: not-ended empty-turn turn={}", state.turn_num);
            break;
        }

        log::debug!(
            "turn complete: turn={} text_len={} reasoning_len={} tool_calls={} ended={}",
            state.turn_num,
            text.len(),
            reasoning.len(),
            tool_calls.len(),
            ended,
        );

        let turn_stats = turn_stats(state.turn_num, turn_usage.as_ref());
        let totals = state.fold_turn_usage(turn_usage.as_ref());

        // ── Emit cumulative token usage via hooks ─────
        let (last_prompt, last_cached) = turn_stats
            .as_ref()
            .map(|ts| (ts.prompt_tokens, ts.cached_input_tokens))
            .unwrap_or((None, None));
        hooks.emit_token_usage(totals.clone(), last_prompt, last_cached);

        emit_turn_stats(prefix, state.turn_num, turn_stats.as_ref(), &totals, hooks);

        state
            .emit_turn_artifacts(
                tool_ctx,
                cancel,
                nested,
                &text,
                &reasoning,
                &tool_calls,
                turn_stats,
                &totals,
                first_token,
                last_token,
                turn_start,
                turn_usage.as_ref(),
                raw,
                captured,
                prefix,
            )
            .await?;

        state.turn_num += 1;
        state.turn_retries = 0;
        state.timeout_retries = 0;

        match state
            .handle_done_decision(
                tool_ctx,
                cancel,
                nested,
                &text,
                &reasoning_items,
                &tool_calls,
                expected_artifact_paths,
                captured,
                loop_start,
                prefix,
            )
            .await?
        {
            DoneDecision::Accepted(run) => return Ok(*run),
            DoneDecision::RetryNeeded => continue,
            DoneDecision::NotDone => {}
        }

        // Done handled — filter it out before execution.
        let tool_calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|tc| !tools::is_completion(tc.function.name.as_str()))
            .collect();

        if tool_calls.is_empty() {
            match state.handle_empty_turn(
                nested,
                &text,
                &reasoning,
                &reasoning_items,
                expected_artifact_paths,
                loop_start,
                raw,
                captured,
                prefix,
            ) {
                EmptyTurnAction::ContinueLoop => continue,
                EmptyTurnAction::ReturnRun(run) => return Ok(*run),
            }
        }

        state.commit_turn_to_history(
            &text,
            &reasoning_items,
            &tool_calls,
            turn_usage.as_ref().and_then(|u| u.input_tokens),
        );

        state
            .execute_tools(
                &tool_calls,
                tool_ctx,
                model,
                prefix,
                cancel,
                raw,
                captured,
                nested,
            )
            .await?;
    }

    if !nested {
        let suffix = if state.timed_out {
            " (timeout)"
        } else if state.stream_error.is_some() {
            " (stream-error)"
        } else {
            ""
        };
        emit_final(prefix, state.turns, suffix, hooks);
        emit_summary(
            prefix,
            state.turn_num,
            loop_start,
            state.total_prompt_tokens,
            state.total_completion_tokens,
            state.total_cached_tokens,
            state.total_cache_creation_tokens,
            state.total_reasoning_tokens,
            hooks,
        );
    }

    if state.timed_out {
        log::info!("agent_loop: exit (stream idle timeout, label={})", prefix);
        log::info!("{prefix}exit: stream idle timeout");
        if state.exhausted_timeout_retries {
            log::error!(
                "agent_loop: exhausted turn-level timeout retries, giving up (label={})",
                prefix
            );
            log::info!("{prefix}exit: exhausted turn-level timeout retries");
            return Err(ClientError::RetryExhausted {
                message: "exhausted turn-level timeout retries".into(),
            });
        }
        return Err(ClientError::Timeout {
            message: "stream idle timeout".into(),
        });
    }
    if let Some(err) = state.stream_error {
        log::info!("agent_loop: exit (stream error, label={})", prefix);
        log::info!("{prefix}exit: stream error");
        if state.exhausted_turn_retries {
            log::error!(
                "agent_loop: exhausted turn-level retries ({} attempts), giving up (label={}): {err}",
                turn_retry_backoff.len() + 1,
                prefix
            );
            log::info!("{prefix}exit: exhausted turn-level retries");
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
    state.hook_log(&format!("{prefix}exit: exceeded max turns ({max_turns})"));
    Err(ClientError::Runtime {
        message: format!("exceeded max turns ({max_turns})"),
    })
}

#[cfg(test)]
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

pub(crate) fn tool_result_event(
    id: &str,
    content: &str,
    exit_code: Option<i32>,
) -> serde_json::Value {
    let mut msg = serde_json::json!({
        "type": "user",
        "message": {
            "content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": content
            }]
        }
    });
    if let Some(code) = exit_code {
        msg["message"]["content"][0]["exit_code"] = serde_json::json!(code);
    }
    msg
}

fn write_raw(raw: &mut Option<std::fs::File>, evt: &serde_json::Value) {
    if let Some(f) = raw {
        if let Ok(line) = serde_json::to_string(evt) {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

/// Emit a structured per-turn token/cache log line for every completed turn.
///
/// This is always logged regardless of the telemetry gate; timing and output
/// byte-ratio extras live in the [`emit_turn_metrics`] path in `turn_flow.rs`.
fn emit_turn_stats(
    prefix: &str,
    turn: usize,
    stats: Option<&TurnStats>,
    totals: &UsageStats,
    _hooks: &LoopHooks,
) {
    let payload = serde_json::json!({
        "type": "turn_stats",
        "turn": turn,
        "stats": stats,
        "totals": totals,
    });
    let msg = format!("{prefix}{payload}");
    log::info!("{msg}");
}

/// Emit stage-end telemetry summary (always emitted, not gated by GREMLINS_TELEMETRY).
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_summary(
    prefix: &str,
    turns: usize,
    loop_start: Instant,
    total_prompt: Option<u64>,
    total_completion: Option<u64>,
    total_cached: Option<u64>,
    total_cache_creation: Option<u64>,
    total_reasoning: Option<u64>,
    hooks: &LoopHooks,
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
    let full = format!("{prefix}{msg}");
    log::info!("{full}");
    hooks.log_line(&full);
}

pub(super) fn emit_final(prefix: &str, turns: usize, suffix: &str, hooks: &LoopHooks) {
    let msg = format!("final: turns={turns} cost=not-reported{suffix}");
    let full = format!("{prefix}{msg}");
    log::info!("{full}");
    hooks.log_line(&full);
}

// ── lifted test helpers (used by conversation/standard.rs tests) ────────

#[cfg(test)]
pub(crate) struct LogSink {
    pub(crate) tx: std::sync::mpsc::Sender<String>,
}

#[cfg(test)]
impl LogSink {
    pub(crate) fn new() -> (Self, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx }, rx)
    }
}

/// Process-global logger installed exactly once for tests. Records are
/// forwarded to whichever sink the currently active [`LogGuard`] installed;
/// when no guard is active the records are dropped.
#[cfg(test)]
struct GlobalLogSink;

#[cfg(test)]
static ACTIVE_SINK: std::sync::Mutex<Option<std::sync::mpsc::Sender<String>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
impl log::Log for GlobalLogSink {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }
    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Ok(guard) = ACTIVE_SINK.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(record.args().to_string());
            }
        }
    }
    fn flush(&self) {}
}

/// Serializes log-capturing tests and scopes the active sink to the guard's
/// lifetime. The process-global logger itself is installed only once, so a
/// second test still captures its own records.
#[cfg(test)]
pub(crate) struct LogGuard {
    prior_level: log::LevelFilter,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
static LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn log_guard(sink: LogSink) -> LogGuard {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    let lock = LOG_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    INSTALL.call_once(|| {
        log::set_boxed_logger(Box::new(GlobalLogSink)).ok();
    });
    let prior_level = log::max_level();
    log::set_max_level(log::LevelFilter::Trace);
    *ACTIVE_SINK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(sink.tx);
    LogGuard {
        prior_level,
        _lock: lock,
    }
}

#[cfg(test)]
impl Drop for LogGuard {
    fn drop(&mut self) {
        *ACTIVE_SINK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        log::set_max_level(self.prior_level);
    }
}

#[cfg(test)]
pub(crate) fn loop_opts(filter: Option<&[String]>) -> LoopOpts<'_> {
    LoopOpts {
        extra: None,
        tool_filter: filter,
        tool_mode: ToolMode::Full,
        classify_error: None,
        temperature: None,
        max_tokens: None,
        preserve_reasoning_in_history: false,
        preamble: None,
        single_turn: false,
    }
}

#[cfg(test)]
pub(crate) fn loop_opts_preserve_reasoning(filter: Option<&[String]>) -> LoopOpts<'_> {
    LoopOpts {
        extra: None,
        tool_filter: filter,
        tool_mode: ToolMode::Full,
        classify_error: None,
        temperature: None,
        max_tokens: None,
        preserve_reasoning_in_history: true,
        preamble: None,
        single_turn: false,
    }
}

#[cfg(test)]
pub(crate) fn test_ctx(
    cwd: Option<std::path::PathBuf>,
    raw_path: Option<std::path::PathBuf>,
) -> RunContext {
    let conv: Box<dyn crate::clients::conversation::Conversation> =
        Box::new(crate::clients::conversation::ConversationStub::new("hi"));
    RunContext {
        params: crate::clients::backend::RunParams {
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
            base_env: None,
            allow_all: false,
            task_models: None,
            cancel_token: None,
            stream_events: None,
            prepopulated_conversation: conv.clone(),
            skills_available: false,
            discovered_skills: None,
            active_skills: None,
            extra_sandbox_roots: Vec::new(),
            hooks: None,
        },
        prefix: "[t] ".into(),
        idle_timeout: 0.05,
        expected_artifact_paths: vec![],
        reminder_budget: 0,
        completion_nudge_budget: 0,
        stream_events: None,
        prepopulated_conversation: conv,
        hooks: crate::clients::agent::plug::LoopHooks::new(),
    }
}

#[cfg(test)]
pub(crate) fn test_tool_ctx() -> crate::clients::tools::AgentLoopRuntime {
    crate::clients::tools::AgentLoopRuntime {
        cwd: None,
        extra_env: None,
        base_env: None,
        sandbox: crate::clients::tools::SandboxMode::Roots(vec![]),
        audit_log: None,
        allowed_tools: None,
        task_fn: None,
        audit_lock: Some(std::sync::Arc::new(std::sync::Mutex::new(()))),
        log_line_fn: None,
        id_prefix: String::new(),
        telemetry: crate::clients::tools::AgentLoopTelemetry {
            stream_events: None,
            tool_output: None,
        },
        active_skills: None,
        skills_available: false,
        discovered_skills: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::conversation::ConversationStub;
    use crate::clients::format::{assistant_tool_message, key_arg};
    use crate::clients::interactive::InteractiveChannels;
    use http::StatusCode;
    use rig_core::completion::message::AssistantContent;
    use rig_core::message::{Issuer, Reasoning, ReasoningContent, ToolFunction, UserContent};
    use rig_core::operation::Completion;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::broadcast::error::RecvError;

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

        let result = tool_result_event("id42", "ok", None);
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

    #[tokio::test]
    async fn oversized_prompt_fails_hard_before_any_request() {
        let _env = crate::test_support::EnvGuard::lock();
        crate::config::set_test_max_prompt_bytes(16);
        let inner = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("should not run"),
        ]]);
        let model = inner.clone().erase();
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
            model.clone(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            true,
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

    #[test]
    fn assistant_tool_message_omits_reasoning_items_when_false() {
        let reasoning = Reasoning {
            id: Some("rs_skip".to_string()),
            content: vec![ReasoningContent::Text {
                text: "should be excluded".to_string(),
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
            "some text",
            std::slice::from_ref(&reasoning),
            std::slice::from_ref(&call),
            false,
        );

        let Message::Assistant { content, .. } = message else {
            panic!("expected an assistant message");
        };
        assert_eq!(content.len(), 2, "text and tool call, no reasoning");
        match &content[0] {
            AssistantContent::Text(t) => assert_eq!(&t.text, "some text"),
            other => panic!("first item must be text, got {other:?}"),
        }
        match &content[1] {
            AssistantContent::ToolCall(_) => {}
            other => panic!("second item must be tool call, got {other:?}"),
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
            model.clone().erase(),
            "read then finish",
            ctx,
            cancel,
            loop_opts_preserve_reasoning(None),
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

    /// End-to-end: with `preserve_reasoning_in_history = false`, a turn that
    /// streams a structured `reasoning` item followed by a `function_call`
    /// referencing it must omit the reasoning item from the replayed history
    /// while keeping the text and tool calls — the reasoning content is only
    /// needed in logs/metrics/TUI, not in re-sent input.
    #[tokio::test]
    async fn loop_omits_reasoning_item_from_replay_when_flag_false() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-omit-reasoning-{}-{}",
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
                rig_core::test_utils::MockStreamEvent::reasoning("thinking")
                    .with_reasoning_id("rs_skip"),
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
            model.clone().erase(),
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

        // The reasoning item must NOT appear in the replayed history.
        assert!(
            !content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))),
            "flag=false must omit reasoning items from replayed history"
        );
        // The tool call must still be present.
        let call = content
            .iter()
            .find(|c| matches!(c, AssistantContent::ToolCall(_)));
        assert!(call.is_some(), "tool call must survive into the replay");
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
            model.clone().erase(),
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
            model.clone(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
        let history: Vec<&Message> = reqs[1].chat_history.iter().collect();
        // Denied calls appear in history as tool results.
        let history_debug = format!("{:?}", history);
        assert!(
            history_debug.contains("Write"),
            "denied call must appear in history: {history_debug}"
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
            model.clone().erase(),
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
        let _env = crate::test_support::EnvGuard::lock();
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
            model.clone().erase(),
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
            model.clone().erase(),
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

    /// A single message can request at most the Task fan-out cap; the excess
    /// short-circuit instead of spawning more child loops.
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

        let cap = tools::max_per_message("Task").expect("Task must be capped");
        let over = cap + 1;
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
            model.clone().erase(),
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
                model: child_handle.clone().erase(),
                temperature: None,
                max_tokens: None,
                extra: None,
                preserve_reasoning: None,
            })
        });

        let selector = super::super::task::TaskModelSelector::new(
            HashMap::from([(crate::task_kind::TaskKind::Scout, "openai/mini".to_string())]),
            factory,
        )
        .expect("a configured task-models map yields a selector");
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.params.task_models = Some(selector);

        run_agent_loop(
            parent.clone().erase(),
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
        run_agent_loop(model.clone().erase(), "go", ctx, cancel, opts, None)
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
        run_agent_loop(model.clone().erase(), "go", ctx, cancel, opts, None)
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
                model: child_handle.clone().erase(),
                temperature: None,
                max_tokens: Some(8192),
                extra: None,
                preserve_reasoning: None,
            })
        });

        let selector = super::super::task::TaskModelSelector::new(
            HashMap::from([(crate::task_kind::TaskKind::Scout, "openai/mini".to_string())]),
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
            parent.clone().erase(),
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
        let _env = crate::test_support::EnvGuard::lock();
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
                model.erase(),
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
                model: child_handle.clone().erase(),
                temperature: None,
                max_tokens: None,
                extra: None,
                preserve_reasoning: None,
            })
        });

        let selector = super::super::task::TaskModelSelector::new(
            HashMap::from([(crate::task_kind::TaskKind::Scout, "openai/mini".to_string())]),
            factory,
        )
        .expect("configured");
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.params.task_models = Some(selector);

        run_agent_loop(
            parent.clone().erase(),
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
        let dyn_model = model.erase();
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
                &LoopHooks::new(),
                Some(session),
                None,
                Box::new(ConversationStub::new("hi")),
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
        let tool_defs = tools::tool_definitions(None, false, ToolMode::Full);
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
        let dyn_model = model.erase();
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
                &LoopHooks::new(),
                Some(session),
                None,
                Box::new(ConversationStub::new("hi")),
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
        let dyn_model = model.erase();
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
                &LoopHooks::new(),
                Some(session),
                None,
                Box::new(ConversationStub::new("hi")),
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
        let dyn_model = model.erase();
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
                &LoopHooks::new(),
                Some(session),
                None,
                Box::new(ConversationStub::new("hi")),
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
                model.erase(),
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
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("all finished"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "implemented the fix"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]])
        .erase();

        let tool_ctx = test_tool_ctx();
        let tool_defs = tools::tool_definitions(None, false, ToolMode::Full);
        let cancel = CancelToken::new();
        let opts = loop_opts(None);
        let (sink, log_rx) = LogSink::new();
        let _guard = log_guard(sink);

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
            &LoopHooks::new(),
            None,
            None,
            Box::new(ConversationStub::new("hi")),
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
        let messages = result
            .conversation
            .as_ref()
            .expect("Done run must return conversation")
            .messages();
        let ledger = messages
            .iter()
            .find_map(|m| match m {
                Message::User { content } => content.iter().find_map(|c| match c {
                    UserContent::Text(t)
                        if t.text.starts_with("[turn ")
                            && t.text.contains("Done implemented the fix") =>
                    {
                        Some(t.text.clone())
                    }
                    _ => None,
                }),
                _ => None,
            })
            .expect("Done ledger entry must be present in returned history");
        assert!(ledger.contains("Done implemented the fix"), "{ledger}");
    }

    /// The Done completion path must never produce two consecutive `Message::User`
    /// values in the persisted history. Anthropic and other providers reject
    /// consecutive user messages because they violate the required
    /// user/assistant alternation, and a `tool_use` must be immediately
    /// followed by a matching `tool_result` block rather than a plain text
    /// user message.
    #[tokio::test]
    async fn loop_done_persisted_history_has_no_consecutive_user_messages() {
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("all finished"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "implemented the fix"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]])
        .erase();

        let tool_ctx = test_tool_ctx();
        let tool_defs = tools::tool_definitions(None, false, ToolMode::Full);
        let cancel = CancelToken::new();
        let opts = loop_opts(None);

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
            &LoopHooks::new(),
            None,
            None,
            // Run against the shipped implementation, not the stub: this test
            // guards the prompt-commit invariant in `StandardConversation`.
            crate::conversation::new_blank_conversation("hi", false, None),
        )
        .await
        .unwrap();

        let messages = result
            .conversation
            .as_ref()
            .expect("Done run must return conversation")
            .messages();

        // Assert no consecutive user messages.
        let mut prev_was_user = false;
        for (i, m) in messages.iter().enumerate() {
            let is_user = matches!(m, Message::User { .. });
            if is_user {
                assert!(
                    !prev_was_user,
                    "consecutive User messages at indices {} and {}: {messages:#?}",
                    i - 1,
                    i
                );
            }
            prev_was_user = is_user;
        }

        // The Done tool_use (assistant) must be immediately followed by a user
        // message containing a matching ToolResult.
        let mut done_tool_use_seen = false;
        for (i, m) in messages.iter().enumerate() {
            if let Message::Assistant { content, .. } = m {
                let has_done = content.iter().any(
                    |c| matches!(c, AssistantContent::ToolCall(tc) if tc.function.name == "Done"),
                );
                if has_done {
                    done_tool_use_seen = true;
                    assert!(
                        i + 1 < messages.len(),
                        "Done tool_use at end of history with no following message"
                    );
                    let next = &messages[i + 1];
                    assert!(
                        matches!(next, Message::User { .. }),
                        "Done tool_use at {i} must be followed by a User message, got: {next:?}"
                    );
                    if let Message::User { content } = next {
                        let has_tool_result = content
                            .iter()
                            .any(|c| matches!(c, UserContent::ToolResult(_)));
                        assert!(
                            has_tool_result,
                            "Done tool_use at {i} must be followed by a user message with a ToolResult, got: {content:?}"
                        );
                    }
                }
            }
        }
        assert!(
            done_tool_use_seen,
            "Done tool_use must appear in persisted history"
        );

        // The Done tool_result and action ledger text must appear in the
        // same user message (ToolResult first, then Text — Anthropic
        // requires tool_result blocks to precede text blocks).
        let merged = messages.iter().find_map(|m| match m {
            Message::User { content } => {
                let has_ledger = content
                    .iter()
                    .any(|c| matches!(c, UserContent::Text(t) if t.text.starts_with("[turn ")));
                let has_tool_result = content
                    .iter()
                    .any(|c| matches!(c, UserContent::ToolResult(_)));
                if has_ledger && has_tool_result {
                    Some(content.clone())
                } else {
                    None
                }
            }
            _ => None,
        });
        assert!(
            merged.is_some(),
            "ledger text and Done tool_result must be merged into one user message"
        );
        let content = merged.unwrap();
        let ledger_pos = content
            .iter()
            .position(|c| matches!(c, UserContent::Text(_)));
        let result_pos = content
            .iter()
            .position(|c| matches!(c, UserContent::ToolResult(_)));
        assert!(
            result_pos < ledger_pos,
            "Done tool_result must precede action ledger in the merged message (Anthropic requires tool_result blocks before text blocks)"
        );
    }

    /// When the artifact-reminder budget is exhausted (zero), Done is
    /// accepted immediately even with missing artifacts. The persisted
    /// history must also avoid consecutive User messages.
    #[tokio::test]
    async fn loop_done_artifact_reminder_exhausted_no_consecutive_user_messages() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-remind-exh-done-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("never-written.md");

        // Single Done turn with reminder budget 0 — accepted immediately
        // via the exhausted-acceptance path.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("still no write"),
            rig_core::test_utils::MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "done without artifact"}),
            ),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.expected_artifact_paths = vec![target.clone()];
        ctx.reminder_budget = 0; // exhausted — Done accepted with missing artifacts
        ctx.completion_nudge_budget = 0;
        // Guard the shipped prompt-commit invariant, not the test stub.
        ctx.prepopulated_conversation =
            crate::conversation::new_blank_conversation("hi", false, None);
        let cancel = CancelToken::new();
        let result = run_agent_loop(
            model.clone().erase(),
            "write",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        let messages = result
            .conversation
            .as_ref()
            .expect("Done run must return conversation")
            .messages();

        // No consecutive User messages.
        let mut prev_was_user = false;
        for (i, m) in messages.iter().enumerate() {
            let is_user = matches!(m, Message::User { .. });
            if is_user {
                assert!(
                    !prev_was_user,
                    "consecutive User messages at indices {} and {} in artifact-exhausted path: {messages:#?}",
                    i - 1,
                    i
                );
            }
            prev_was_user = is_user;
        }

        // The artifact-exhausted path must not duplicate the user prompt /
        // assistant message pair in the persisted history.
        // Count how many times the Done tool_use appears.
        let done_count = messages.iter().filter(|m| {
            matches!(m, Message::Assistant { content, .. }
                if content.iter().any(|c| matches!(c, AssistantContent::ToolCall(tc) if tc.function.name == "Done")))
        }).count();
        assert_eq!(
            done_count, 1,
            "Done tool_use must appear exactly once in artifact-exhausted path, got {done_count}"
        );
    }
}
