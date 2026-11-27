#[cfg_attr(not(test), allow(dead_code))]
pub mod app;
pub mod buffer;
mod chat_context;
mod chat_prefetch;
#[allow(dead_code)]
pub mod client;
pub mod commands;
pub mod completion;
pub mod editor;
mod home_events;
use home_events::{apply_home_event, apply_home_identity};
pub mod input_buffer;
pub mod markdown;
pub mod motif;
#[cfg_attr(not(test), allow(dead_code))]
mod protocol;
pub mod screen;
pub mod theme;
mod watch_replay;
pub mod widgets;

#[cfg(test)]
mod entry_tests;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    /// Serializes tests across modules that mutate the process-global
    /// `GREMLINS_*` environment variables and call
    /// `gremlins::config::clear_global()`. `chat_context` and
    /// `chat_prefetch` each build throwaway sandboxes but share one process
    /// environment; a per-module lock would let their guards race (for
    /// example, one test removes `GREMLINS_SANDBOX_ROOT` while another reads
    /// it through `state_root()`).
    pub(crate) static CONFIG_ENV_LOCK: Mutex<()> = Mutex::new(());
}

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind};
#[cfg(test)]
use gremlins::executor::socket;
#[cfg(test)]
use tokio::io::BufReader;
use tokio::sync::mpsc;

use crate::display;

use app::{App, ConversationStatus, Screen, Submission, Target};
use commands::{dispatch, CommandResult};
use editor::{open_editor, ReaderChannels, SuspendHandle};
use input_buffer::InputBuffer;
use screen::{new_reasoning_band, LiveArea};

/// A daemon `chat_model` lookup result tagged with the session generation it
/// was issued for. Stale generations are ignored by the event loop.
type ChatModelQuery = (u64, Result<Option<String>, String>);

struct AttachmentFrame {
    generation: u64,
    target: Target,
    execution: Option<protocol::Execution>,
    value: serde_json::Value,
}

#[derive(Default)]
struct Attachment {
    generation: u64,
    control_sequence: u64,
    task: Option<AbortOnDrop>,
    target: Option<Target>,
    execution: Option<protocol::Execution>,
    observed_sequences: HashMap<Target, SequenceRanges>,
    client: Option<Arc<client::DaemonClient>>,
    receipts: Option<mpsc::UnboundedSender<AttachmentFrame>>,
}

/// Sorted, disjoint inclusive ranges; retained across selection changes.
#[derive(Default)]
struct SequenceRanges(Vec<(u64, u64)>);

impl SequenceRanges {
    fn insert(&mut self, sequence: u64) -> bool {
        let index = self.0.partition_point(|&(_, end)| end < sequence);
        if self
            .0
            .get(index)
            .is_some_and(|&(start, _)| start <= sequence)
        {
            return false;
        }
        let joins_left = index > 0 && self.0[index - 1].1.checked_add(1) == Some(sequence);
        let joins_right = self
            .0
            .get(index)
            .is_some_and(|&(start, _)| sequence.checked_add(1) == Some(start));
        match (joins_left, joins_right) {
            (true, true) => {
                let (_, end) = self.0.remove(index);
                self.0[index - 1].1 = end;
            }
            (true, false) => self.0[index - 1].1 = sequence,
            (false, true) => self.0[index].0 = sequence,
            (false, false) => self.0.insert(index, (sequence, sequence)),
        }
        true
    }
}

impl Attachment {
    fn invalidate(&mut self, app: &mut App) {
        app.abandon_pipeline_replay();
        if let Some(client) = &self.client {
            client.release_inputs();
        }
        self.task = None;
        self.generation = self.generation.wrapping_add(1);
        if let Some(target) = self.target.take() {
            if target.is_home() && app.conversation_status(&target) == ConversationStatus::Paused {
                app.set_conversation_status(target.clone(), ConversationStatus::Running);
            }
            if !target.is_home() {
                app.conversations.set_execution(target, None);
            }
        }
        self.execution = None;
    }

    fn accepts(&self, frame: &AttachmentFrame) -> bool {
        self.generation == frame.generation
            && (self.target.as_ref() == Some(&frame.target)
                || matches!((&self.target, &frame.target),
                    (Some(Target::StageExecution { run_id: pinned, .. }),
                     Target::StageExecution { run_id, stage })
                    if pinned == run_id && stage.is_empty() && self.execution.is_some()))
            && (frame.value["_passive_watch"] == true
                || self.execution.is_none()
                || self.execution == frame.execution)
    }
}

/// Install the daemon's reserved home identity before any watch rows arrive.
async fn install_home_session(app: &mut App, client: &client::DaemonClient) {
    app.home_run_id = Some(client.home_run_id().await);
}

fn route_submission<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    client: &Arc<client::DaemonClient>,
    _attachment: &Attachment,
    delivery_tx: &mpsc::UnboundedSender<(u64, Result<(), String>)>,
) -> bool {
    if app.input.is_empty() {
        return true;
    }
    if app.session_recreating {
        live.push_transcript_line("session reset is in progress — wait before sending".to_string());
        return true;
    }
    match app.conversations.submit(&app.selected_target, &app.input) {
        Submission::HomePrompt => false,
        Submission::Blocked(message) => {
            live.push_transcript_line(message.to_string());
            true
        }
        Submission::Correction {
            id,
            execution,
            text,
        } => {
            app.take_input();
            app.clear_completion();
            live.reset_input_scroll();
            let c = Arc::clone(client);
            let tx = delivery_tx.clone();
            let epoch = c.input_epoch();
            tokio::spawn(async move {
                let outcome = c
                    .selected_input_at(
                        epoch,
                        &execution,
                        serde_json::json!({"cmd":"message", "req_id":id.to_string(), "text":text}),
                    )
                    .await;
                let _ = tx.send((id, outcome));
            });
            true
        }
    }
}

fn selected_control<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    attachment: &mut Attachment,
) -> bool {
    // Unselected home leaves /quit to dispatch (local TUI exit).
    if app.selected_target.is_home() && attachment.target != Some(Target::Home) {
        return false;
    }
    let input = app.input.text().trim();
    let (command, reason) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    let reason = reason.trim();
    if command == "/quit"
        && reason.is_empty()
        && (attachment.target.as_ref() != Some(&app.selected_target)
            || attachment.execution.is_none()
            || app.conversation_status(&app.selected_target) == ConversationStatus::ReadOnly)
    {
        return false;
    }
    let op = match command {
        "/exit" if reason.is_empty() => "detach",
        "/pause" if reason.is_empty() => "pause",
        "/step" | "/continue" if reason.is_empty() => "step",
        "/resume-execution" if reason.is_empty() => "resume",
        // Bare /quit resumes the debugged execution; /quit <reason> bails it.
        // Outside a selected attachment, dispatch handles /quit as TUI exit.
        "/quit" if reason.is_empty() => "resume",
        "/quit" | "/bail" => "bail",
        _ => return false,
    };
    let reason = reason.to_owned();
    app.clear_input();
    if op == "detach" {
        attachment.invalidate(app);
        live.select_conversation(app, Target::Home);
    } else if op == "bail" && reason.is_empty() {
        live.push_transcript_line("usage: /bail <reason>".to_string());
    } else if app.conversation_status(&app.selected_target) == ConversationStatus::ReadOnly {
        live.push_transcript_line("selected conversation is read-only".to_string());
    } else {
        attachment.control_sequence = attachment.control_sequence.wrapping_add(1);
        let mut request = serde_json::json!({"cmd":op, "req_id":format!("selected-control-{}-{}", attachment.generation, attachment.control_sequence)});
        if op == "bail" {
            request["reason"] = serde_json::json!(reason);
        }
        if let (Some(client), Some(tx), Some(execution)) = (
            attachment.client.clone(),
            attachment.receipts.clone(),
            attachment.execution.clone(),
        ) {
            let target = app.selected_target.clone();
            let generation = attachment.generation;
            let epoch = client.input_epoch();
            tokio::spawn(async move {
                let result = client.selected_input_at(epoch, &execution, request).await;
                let _ = tx.send(AttachmentFrame {
                    generation,
                    target,
                    execution: Some(execution),
                    value: serde_json::json!({"_control_receipt":true, "command":op,
                        "error":result.err()}),
                });
            });
        } else {
            live.push_transcript_line(
                "selected conversation has no live agent execution".to_string(),
            );
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn watch_enter<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    client: &Arc<client::DaemonClient>,
    attachment: &mut Attachment,
    tx: &mpsc::UnboundedSender<AttachmentFrame>,
    last_stage: &mut Option<Target>,
    follow: &mut Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(target) = app.watch_target() {
        close_watch_follow(app, live, follow);
        select_passive_watch(app, live, client, target, attachment, tx, last_stage);
    }
}

#[allow(clippy::too_many_arguments)]
fn attach_stage_entry<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    client: &Arc<client::DaemonClient>,
    run_id: String,
    stage: Option<String>,
    attachment: &mut Attachment,
    tx: &mpsc::UnboundedSender<AttachmentFrame>,
    last_stage: &mut Option<Target>,
) {
    let stage_parts: Vec<_> = stage.as_deref().unwrap_or("").split_whitespace().collect();
    let exact = if let [name, stage_run_id] = stage_parts.as_slice() {
        Some(Target::Invocation {
            run_id: run_id.clone(),
            stage: (*name).into(),
            stage_run_id: (*stage_run_id).into(),
        })
    } else if let [name] = stage_parts.as_slice() {
        let mut matches = app
            .live_executions
            .values()
            .filter(|e| e.run_id == run_id && (e.stage == *name || e.stage_run_id == *name));
        let first = matches.next().map(|e| e.target());
        if matches.next().is_none() {
            first
        } else {
            None
        }
    } else {
        None
    };
    let target = if let Some(target) = exact {
        target
    } else if stage.is_none() && app.home_run_id.as_deref() == Some(run_id.as_str()) {
        Target::Home
    } else {
        Target::StageExecution {
            run_id,
            stage: stage.unwrap_or_default(),
        }
    };
    let view_only = matches!(target, Target::StageExecution { .. });
    let unavailable = matches!(&target, Target::Invocation { .. })
        && !app.live_executions.values().any(|e| e.target() == target);
    select_passive_watch(app, live, client, target, attachment, tx, last_stage);
    if unavailable {
        live.push_transcript_line(
            "requested invocation is unavailable — retained output is view-only".into(),
        );
    }
    if view_only {
        live.push_transcript_line("view-only selection — use /attach <run> <stage> <stage_run_id> to select an exact agent execution".into());
    }
}

#[allow(clippy::too_many_arguments)]
fn debug_entry<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    client: &Arc<client::DaemonClient>,
    id: String,
    attachment: &mut Attachment,
    tx: &mpsc::UnboundedSender<AttachmentFrame>,
    last_stage: &mut Option<Target>,
    follow: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let parts: Vec<_> = id.split_whitespace().collect();
    let mut matches = app
        .live_executions
        .values()
        .filter(|e| match parts.as_slice() {
            [stage_run_id] => e.stage_run_id == *stage_run_id,
            [run_id, stage_run_id] => e.run_id == *run_id && e.stage_run_id == *stage_run_id,
            _ => false,
        });
    let target = matches.next().map(|e| e.target());
    if target.is_none() || matches.next().is_some() {
        live.push_transcript_line(
            "select a live invocation: /debug <run> <stage_run_id>; find its stage_run_id in /watch (Enter selects the concrete row)".into(),
        );
        return;
    }
    close_watch_follow(app, live, follow);
    let target = target.unwrap();
    select_passive_watch(app, live, client, target, attachment, tx, last_stage);
}

/// Only lifecycle-confirmed agent invocations grant an input address.
fn pin_exact_execution(app: &mut App, target: &Target, attachment: &mut Attachment) {
    let execution = app
        .live_executions
        .values()
        .find(|e| e.target() == *target && e.stage_type == "agent")
        .map(|e| protocol::Execution {
            run_id: e.run_id.clone(),
            stage_run_id: e.stage_run_id.clone(),
            nonce: 0,
        });
    attachment.execution = execution.clone();
    app.conversations
        .set_execution(target.clone(), execution.clone());
    app.set_conversation_status(
        target.clone(),
        if execution.is_none() {
            ConversationStatus::ReadOnly
        } else if app.conversation_status(target) == ConversationStatus::Paused {
            ConversationStatus::Paused
        } else {
            ConversationStatus::Running
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn select_passive_watch<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    client: &Arc<client::DaemonClient>,
    target: Target,
    attachment: &mut Attachment,
    tx: &mpsc::UnboundedSender<AttachmentFrame>,
    last_stage: &mut Option<Target>,
) {
    attachment.invalidate(app);
    attachment.client = Some(Arc::clone(client));
    attachment.receipts = Some(tx.clone());
    if !target.is_home() {
        pin_exact_execution(app, &target, attachment);
    }
    app.screen = Screen::Chat;
    live.exit_screen_view();
    live.select_conversation(app, target.clone());
    let (run_id, stage, stage_run_id) = match &target {
        Target::Home => return,
        Target::StageExecution { run_id, stage } => (
            run_id.clone(),
            (!stage.is_empty()).then(|| stage.clone()),
            None,
        ),
        Target::Invocation {
            run_id,
            stage,
            stage_run_id,
        } => (
            run_id.clone(),
            Some(stage.clone()),
            Some(stage_run_id.clone()),
        ),
    };
    *last_stage = Some(target.clone());
    if attachment.execution.is_none() {
        app.set_conversation_status(target.clone(), ConversationStatus::ReadOnly);
        app.conversations.set_execution(target.clone(), None);
    }
    attachment.target = Some(target.clone());
    let generation = attachment.generation;
    let client = Arc::clone(client);
    let tx = tx.clone();
    attachment.task = Some(AbortOnDrop(tokio::spawn(async move {
        let send = |mut value: serde_json::Value| {
            value["_passive_watch"] = serde_json::json!(true);
            tx.send(AttachmentFrame {
                generation,
                target: target.clone(),
                execution: None,
                value,
            })
            .map_err(|_| ())
        };
        match client
            .follow_watch(&run_id, stage.as_deref(), stage_run_id.as_deref())
            .await
        {
            Ok((mut frames, reader)) => {
                let _reader = AbortOnDrop(reader);
                let _ = send(serde_json::json!({
                    "type":"watched", "replay":true, "id":run_id,
                    "stage":stage, "stage_run_id":stage_run_id
                }));
                while let Some(value) = frames.recv().await {
                    if send(value).is_err() {
                        return;
                    }
                }
                let _ = send(
                    serde_json::json!({"type":"transport_error", "reason":"watch disconnected"}),
                );
            }
            Err(reason) => {
                let _ = send(serde_json::json!({"type":"transport_error", "reason":reason}));
            }
        }
    })));
}

/// Raw event identity does not require a start frame, stage type, or nonce.
fn watch_frame_target(value: &serde_json::Value) -> Option<Target> {
    let field = |key: &str| {
        value[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(Target::Invocation {
        run_id: field("run_id").or_else(|| field("id"))?,
        stage: field("correlation_stage").or_else(|| field("stage"))?,
        stage_run_id: field("stage_run_id")?,
    })
}

fn watch_target_matches(selector: &Target, event: &Target) -> bool {
    match (selector, event) {
        (Target::Invocation { .. }, _) => selector == event,
        (
            Target::StageExecution { run_id, stage },
            Target::Invocation {
                run_id: id,
                stage: name,
                ..
            },
        ) => run_id == id && (stage.is_empty() || stage == name),
        _ => false,
    }
}

fn apply_passive_watch_frame<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    attachment: &mut Attachment,
    frame: &AttachmentFrame,
) {
    let value = &frame.value;
    let kind = value["type"].as_str().unwrap_or("");
    // Reject sibling identities before updating lifecycle or rendering.
    let identity = watch_frame_target(value);
    if identity
        .as_ref()
        .is_some_and(|event| !watch_target_matches(&frame.target, event))
    {
        return;
    }
    app.observe_pipeline_frame(value);
    if matches!(frame.target, Target::Invocation { .. })
        && attachment.execution.is_none()
        && app.conversation_status(&frame.target) != ConversationStatus::Ended
        && kind == "stage_boundary"
        && value["active"] == true
    {
        pin_exact_execution(app, &frame.target, attachment);
    }
    if matches!(kind, "transport_error" | "error") {
        app.abandon_pipeline_replay();
    }
    if matches!(kind, "history_gap" | "lagged" | "transport_error" | "error")
        || (kind == "replay_checkpoint" && value["complete"] != true)
    {
        live.with_target_transcript(&frame.target, |live, _| {
            live.push_transcript_line(format!(
                "watch {kind}: {}",
                value["message"]
                    .as_str()
                    .or(value["reason"].as_str())
                    .unwrap_or("history incomplete")
            ));
        });
        return;
    }
    let Some(identity) = identity else {
        return;
    };
    let mut targets = vec![identity.clone()];
    for selector in [&frame.target, &app.selected_target] {
        if watch_target_matches(selector, &identity) && !targets.contains(selector) {
            targets.push(selector.clone());
        }
    }
    for target in targets {
        if let Some(sequence) = value["sequence"].as_u64() {
            if !attachment
                .observed_sequences
                .entry(target.clone())
                .or_default()
                .insert(sequence)
            {
                continue;
            }
        }
        let terminal = matches!(kind, "done" | "ended")
            || (kind == "stage_boundary" && value["active"] == false);
        if terminal {
            app.conversations.set_execution(target.clone(), None);
            app.set_conversation_status(target.clone(), ConversationStatus::Ended);
            if attachment.target.as_ref() == Some(&target) {
                attachment.execution = None;
            }
        } else if (attachment.target.as_ref() != Some(&target) || attachment.execution.is_none())
            && app.conversation_status(&target) != ConversationStatus::Ended
        {
            app.set_conversation_status(target.clone(), ConversationStatus::ReadOnly);
        }
        render_execution_frame(live, &target, value);
    }
}

fn apply_attachment_frame<B: ratatui::backend::Backend>(
    app: &mut App,
    live: &mut LiveArea<B>,
    attachment: &mut Attachment,
    frame: AttachmentFrame,
) {
    if frame.value["_control_receipt"] == true {
        let command = frame.value["command"].as_str().unwrap_or("control");
        live.with_target_transcript(&frame.target, |live, _| {
            let message = match frame.value["error"].as_str() {
                Some(reason) => format!("{command} failed: {reason}"),
                None => format!("{command} acknowledged"),
            };
            live.push_transcript_line(message);
        });
        return;
    }
    if !attachment.accepts(&frame) {
        return;
    }
    if frame.value["_passive_watch"] == true {
        apply_passive_watch_frame(app, live, attachment, &frame);
    }
}

fn render_execution_frame<B: ratatui::backend::Backend>(
    live: &mut LiveArea<B>,
    target: &Target,
    value: &serde_json::Value,
) {
    let kind = value["type"].as_str().unwrap_or("");
    let invocation = if value["_passive_watch"] == true {
        watch_frame_target(value).unwrap_or_else(|| target.clone())
    } else {
        target.clone()
    };
    live.with_invocation_transcript(target, &invocation, |live, reasoning| {
        let text = value["text"].as_str().unwrap_or("");
        match kind {
            "exec_output" => {
                live.push_exec_output_record(text, value["is_stderr"] == true);
            }
            "stage_boundary" => live.push_transcript_line(format!(
                "stage {}: {}",
                value["stage"].as_str().unwrap_or(""),
                if value["active"] == true {
                    "started"
                } else {
                    "ended"
                }
            )),
            "stream_chunk" => live.push_response(text),
            "reasoning_chunk" => reasoning.push_text(text),
            "tool_output_chunk" => live.push_tool_output_chunk(
                value["id"].as_str().unwrap_or(""),
                value["call"].as_str().unwrap_or(""),
                text,
            ),
            "task_progress" => live.push_task_progress(
                value["parent_id"].as_str().unwrap_or(""),
                value["kind"].as_str().unwrap_or(""),
                value["call"].as_str().unwrap_or(""),
                value["reasoning"].as_str().unwrap_or(""),
                text,
                value.get("stats").filter(|v| !v.is_null()),
                value.get("totals").filter(|v| !v.is_null()),
            ),
            "tool_result" => live.commit_tool_result(
                value["id"].as_str().unwrap_or(""),
                value["call"].as_str().unwrap_or(""),
            ),
            "stream_reset" => {
                live.clear_current_turn();
                *reasoning = new_reasoning_band();
            }
            "turn_complete" => {
                let calls: Vec<String> = value["tool_calls"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                live.reconcile_tool_calls(&calls);
                live.reconcile_response(text);
                reasoning.set_title(&stats_title(
                    value["turn"].as_u64().unwrap_or(0) as usize,
                    value.get("stats"),
                    value.get("totals"),
                ));
            }
            "done" | "ended" => {
                if !text.is_empty() {
                    live.reconcile_response(text);
                }
                let summary = value["summary"].as_str().unwrap_or("");
                if !summary.is_empty() && summary != text {
                    if !live.response_is_empty() {
                        live.push_response("\n\n");
                    }
                    live.push_response(summary);
                }
                if let Some(title) = reasoning.title().map(|title| {
                    title.replacen(REASONING_PLACEHOLDER_TITLE, REASONING_COMPLETED_TITLE, 1)
                }) {
                    reasoning.set_title(&title);
                }
                let stream = std::mem::replace(reasoning, new_reasoning_band());
                let had_turn = live.has_current_turn();
                live.finish_turn();
                if had_turn {
                    live.commit_reasoning_title(stream);
                }
                live.clear_tool_output();
                if let Some(reason) = value["reason"].as_str() {
                    live.push_transcript_line(format!("{kind}: {reason}"));
                }
            }
            "operator_message" => {
                live.submit_prompt(text.to_string());
            }
            "system_warning" | "error" | "lagged" => live.push_transcript_line(format!(
                "{}: {}",
                kind,
                value["message"]
                    .as_str()
                    .or(value["reason"].as_str())
                    .unwrap_or(text)
            )),
            _ => {}
        }
    });
}

/// Drop-guard that aborts a JoinHandle on drop, ensuring a dedicated
/// log-follow socket is closed even when the outer forwarding task is
/// cancelled via `abort()`.
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Abort the dedicated log-follow task for the watch stream pane.
fn abort_watch_follow(handle: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle.take() {
        handle.abort();
    }
}

/// Close the watch stream pane: release the follow socket and clear the
/// rendered log tail.
fn close_watch_follow<B: ratatui::backend::Backend>(
    app: &mut App,
    live_area: &mut LiveArea<B>,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
) {
    if app.watch_following.is_some() {
        app.abandon_inline_pipeline_replay();
    }
    abort_watch_follow(watch_follow_handle);
    app.watch_following = None;
    app.watch_follow_error = None;
    live_area.clear_watch_stream();
}

/// Snapshot of a single run's lifecycle + token/cost state returned by the
/// `ls`-based resync triggered after broadcast lag.
struct ResyncRun {
    id: String,
    status: String,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    turns: Option<usize>,
    cost_usd: Option<f64>,
    last_prompt_tokens: Option<u64>,
    last_cached_input_tokens: Option<u64>,
}

/// Spawn a one-shot `ls` request that refreshes lifecycle status
/// *and* token/cost fields on existing runs. When a run becomes
/// terminal during broadcast lag its last `TokenUsage` may be
/// permanently skipped; the persisted cost and usage carried here
/// ensure the /watch columns are never left stale.
fn fetch_run_statuses(
    client: &Arc<client::DaemonClient>,
    status_tx: &tokio::sync::mpsc::UnboundedSender<(Vec<ResyncRun>, u64)>,
    generation: u64,
) {
    let c = Arc::clone(client);
    let tx = status_tx.clone();
    tokio::spawn(async move {
        if let Ok(resp) = c
            .send_control_request(serde_json::json!({"op": "ls"}))
            .await
        {
            let mut runs = Vec::new();
            if let Some(gremlins) = resp.get("gremlins").and_then(|v| v.as_array()) {
                for entry in gremlins {
                    if let (Some(id), Some(status)) = (
                        entry.get("id").and_then(|v| v.as_str()),
                        entry.get("status").and_then(|v| v.as_str()),
                    ) {
                        runs.push(ResyncRun {
                            id: id.to_string(),
                            status: status.to_string(),
                            prompt_tokens: entry.get("prompt_tokens").and_then(|v| v.as_u64()),
                            completion_tokens: entry
                                .get("completion_tokens")
                                .and_then(|v| v.as_u64()),
                            reasoning_tokens: entry
                                .get("reasoning_tokens")
                                .and_then(|v| v.as_u64()),
                            cached_input_tokens: entry
                                .get("cached_input_tokens")
                                .and_then(|v| v.as_u64()),
                            cache_creation_input_tokens: entry
                                .get("cache_creation_input_tokens")
                                .and_then(|v| v.as_u64()),
                            turns: entry
                                .get("turns")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as usize),
                            cost_usd: entry.get("cost_usd").and_then(|v| v.as_f64()),
                            last_prompt_tokens: entry
                                .get("last_prompt_tokens")
                                .and_then(|v| v.as_u64()),
                            last_cached_input_tokens: entry
                                .get("last_cached_input_tokens")
                                .and_then(|v| v.as_u64()),
                        });
                    }
                }
            }
            let _ = tx.send((runs, generation));
        }
    });
}

/// Open (or switch) the watch stream pane for `id`.
///
/// At most one watch follow socket exists at a time: any prior follow is
/// aborted before the new one is spawned. Lines are scoped through the
/// existing `view_log` channel so queued lines from a superseded follow are
/// never painted into the new pane.
fn start_watch_follow<B: ratatui::backend::Backend>(
    app: &mut App,
    live_area: &mut LiveArea<B>,
    client: &Arc<client::DaemonClient>,
    id: String,
    log_tx: &mpsc::UnboundedSender<serde_json::Value>,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
    watch_follow_generation: &mut u64,
) {
    let execution = app.live_executions.get(&id).cloned();
    let Some(id) = app.watch_run_id(&id).map(str::to_owned) else {
        return;
    };
    abort_watch_follow(watch_follow_handle);
    app.abandon_inline_pipeline_replay();
    live_area.clear_watch_stream();
    app.watch_following = Some(id.clone());
    app.watch_follow_error = None;
    if let Some(view) = live_area.runs_view_mut() {
        view.watch_pending = true;
        view.watch_execution = execution.clone();
    }

    // Each follow is tagged with a unique generation so messages queued by
    // an aborted/superseded follow are never accepted after the same id is
    // reopened.
    *watch_follow_generation = watch_follow_generation.wrapping_add(1);
    let generation = *watch_follow_generation;

    let log_tx = log_tx.clone();
    let client = Arc::clone(client);
    let follow_id = id.clone();

    // Snapshot the current stage and sub_stage so we know which blob
    // to replay. For terminal runs (done/failed/stopped), always
    // select the operational log so final diagnostics remain visible.
    let run = app.active_runs.get(&follow_id);
    let is_terminal =
        run.is_none_or(|r| matches!(r.status.as_str(), "done" | "failed" | "stopped"));
    // Use sub_stage (leaf) for blob lookup when available; falls back
    // to stage for top-level stages and terminal runs.
    let replay_stage = if is_terminal {
        String::new()
    } else if let Some(r) = run {
        if r.sub_stage.is_empty() {
            r.stage.clone()
        } else {
            r.sub_stage.clone()
        }
    } else {
        String::new()
    };

    let follow_task = tokio::spawn(async move {
        watch_replay::replay_selection(
            client,
            follow_id,
            execution,
            replay_stage,
            generation,
            log_tx,
        )
        .await;
    });
    *watch_follow_handle = Some(follow_task);
}

fn apply_watch_frame<B: ratatui::backend::Backend>(
    app: &mut App,
    live_area: &mut LiveArea<B>,
    raw: &serde_json::Value,
    generation: u64,
    follow: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let Some(id) = raw.get("id").and_then(|v| v.as_str()) else {
        return;
    };
    if app.watch_following.as_deref() != Some(id)
        || raw.get("generation").and_then(|v| v.as_u64()) != Some(generation)
    {
        return;
    }
    match raw.get("type").and_then(|v| v.as_str()) {
        Some("view_log") => {
            if let Some(native) = raw.get("native") {
                let Some(view) = live_area.runs_view_mut() else {
                    return;
                };
                if native["type"] == "watched" {
                    view.watch_pending = false;
                } else if view.watch_pending {
                    return;
                }
                if let Some(execution) = &view.watch_execution {
                    if let Some(target) = watch_frame_target(native) {
                        if target != execution.target() {
                            return;
                        }
                    } else if !matches!(
                        native["type"].as_str(),
                        Some("watched" | "history_gap" | "replay_checkpoint" | "lagged")
                    ) {
                        return;
                    }
                }
                app.observe_inline_pipeline_frame(native);
            }
            if let Some(line) = raw.get("line").and_then(|v| v.as_str()) {
                live_area.push_watch_line(line.to_string());
            }
        }
        Some("view_error") => {
            app.abandon_inline_pipeline_replay();
            if let Some(view) = live_area.runs_view_mut() {
                view.watch_pending = false;
            }
            // Terminal runs can reach normal EOF after replaying history.
            let is_terminal = app
                .active_runs
                .get(id)
                .is_some_and(|r| r.status != "running");
            if !is_terminal || raw["force"] == true {
                if let Some(message) = raw.get("message").and_then(|v| v.as_str()) {
                    live_area.push_watch_line(format!("error: {message}"));
                    app.watch_follow_error = Some(message.to_string());
                    abort_watch_follow(follow);
                }
            }
        }
        _ => {}
    }
}

/// What a `Ctrl+W` press should do given the current app state.
///
/// Extracted from the event loop so the key routing can be unit-tested
/// without constructing a terminal, live area, or daemon client.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WatchKeyAction {
    /// The chord is inert (history search or a screen-owned prompt).
    NoOp,
    /// From chat: enter the runs table and refresh its statuses.
    EnterRuns,
    /// From runs with an open pane: close the inline watch pane.
    ClosePane,
    /// From runs with a selection and no pane: open the pane for `id`.
    OpenPane(String),
    /// From runs with nothing to follow: return to chat.
    ExitToChat,
}

/// Classify a `Ctrl+W` press from the current app state.
fn watch_key_action(app: &App) -> WatchKeyAction {
    if app.history_search.is_some() {
        return WatchKeyAction::NoOp;
    }
    match &app.screen {
        Screen::Chat => WatchKeyAction::EnterRuns,
        Screen::Runs => {
            if app.watch_following.is_some() {
                WatchKeyAction::ClosePane
            } else if let Some(selected) = app.watch_selection.clone() {
                WatchKeyAction::OpenPane(selected)
            } else {
                WatchKeyAction::ExitToChat
            }
        }
        #[allow(unreachable_patterns)]
        _ => WatchKeyAction::NoOp,
    }
}

/// Ctrl+C is treated as a reset key (exactly like Esc). A second press
/// within this window quits the TUI; a later press becomes a fresh first
/// press. `/quit` and Ctrl+D on empty input remain the explicit exits.
const PENDING_QUIT_WINDOW: Duration = Duration::from_secs(1);

fn selected_home_turn_active(app: &App) -> bool {
    app.selected_target.is_home() && app.conversations.home_turn().is_some()
}

/// Classify a Ctrl+C press at `now` against the pending-quit state.
///
/// Returns `true` when a prior first press is still within
/// `PENDING_QUIT_WINDOW`, in which case this press should quit the TUI.
/// Otherwise the press becomes the new first press — (re-)arming the
/// confirmation window — and `false` is returned.
fn ctrl_c_should_quit(pending_quit: &mut Option<Instant>, now: Instant) -> bool {
    let quit = pending_quit.is_some_and(|first| now.duration_since(first) <= PENDING_QUIT_WINDOW);
    if !quit {
        *pending_quit = Some(now);
    }
    quit
}

/// What a press should do while a chat turn is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptAction {
    SendSoft,
    SendHard,
    NoOp,
}

/// Count an ESC/Ctrl+C press for the selected home turn owner.
///
/// The first press requests a soft interrupt, the second press within
/// [`PENDING_QUIT_WINDOW`] requests a hard cancel, and any further presses in
/// the same window are no-ops. After the window expires the count resets.
fn interrupt_press(count: &mut u8, window: &mut Option<Instant>, now: Instant) -> InterruptAction {
    let within_window =
        window.is_some_and(|first| now.duration_since(first) <= PENDING_QUIT_WINDOW);
    if within_window {
        *count = count.saturating_add(1);
    } else {
        *count = 1;
        *window = Some(now);
    }
    match *count {
        1 => InterruptAction::SendSoft,
        2 => InterruptAction::SendHard,
        _ => InterruptAction::NoOp,
    }
}

/// Escalation belongs to the home conversation and only lasts one gesture.
#[derive(Default)]
struct HomeInterrupt {
    count: u8,
    window: Option<Instant>,
    ended_turn: Option<String>,
}

impl HomeInterrupt {
    fn in_window(&self, now: Instant) -> bool {
        self.window
            .is_some_and(|first| now.duration_since(first) <= PENDING_QUIT_WINDOW)
    }

    fn disarm_if_stale(&mut self, target: &Target, now: Instant) {
        if !target.is_home() || !self.in_window(now) {
            *self = Self::default();
        }
    }

    fn press(&mut self, now: Instant) -> InterruptAction {
        interrupt_press(&mut self.count, &mut self.window, now)
    }
}

/// Exit any non-chat screen and tear down its session-owned aux sockets.
///
/// Returns `true` when a full-screen view was active and is now dropped.
fn exit_current_screen(
    app: &mut App,
    live_area: &mut LiveArea,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
) -> bool {
    if !app.exit_screen() {
        return false;
    }

    live_area.exit_screen_view();

    close_watch_follow(app, live_area, watch_follow_handle);

    true
}

/// Close any non-chat layer (watch pane first, then full-screen view).
///
/// Returns `true` when a layer was consumed. This runs before active-turn and
/// idle routing for both ESC and Ctrl+C.
fn dismiss_non_chat_layer(
    app: &mut App,
    live_area: &mut LiveArea,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
) -> bool {
    if app.screen == Screen::Runs && app.watch_following.is_some() {
        close_watch_follow(app, live_area, watch_follow_handle);
        return true;
    }
    exit_current_screen(app, live_area, watch_follow_handle)
}

/// Fire a `cancel_chat` request at the daemon without blocking the event loop.
fn send_cancel_chat(client: &Arc<client::DaemonClient>, active_turn: Option<&str>, mode: &str) {
    let c = Arc::clone(client);
    let cancel_turn = active_turn.map(String::from);
    let mode = mode.to_string();
    tokio::spawn(async move {
        let mut req = serde_json::json!({"op": "cancel_chat", "mode": mode});
        if let Some(t) = cancel_turn {
            req["turn"] = serde_json::Value::String(t);
        }
        let _ = c.send_session_request(req).await;
    });
}

/// The live flag mirrors the selected target; home also has a parked flag
/// that must be retired even if its terminal event arrives off screen.
fn finish_home_request<B: ratatui::backend::Backend>(app: &mut App, live_area: &mut LiveArea<B>) {
    app.set_conversation_status(Target::Home, ConversationStatus::Idle);
    live_area.set_target_active_request(&Target::Home, false);
    if app.selected_target.is_home() {
        app.active_request = false;
    }
    app.conversations.retire_home_turn();
}

/// Retire the interrupted turn without dismissing its addressed draft.
fn clear_active_turn_ui<B: ratatui::backend::Backend>(app: &mut App, live_area: &mut LiveArea<B>) {
    live_area.clear_current_turn();
    finish_home_request(app, live_area);
}

/// Request a soft interrupt for the active turn without tearing down the UI.
fn soft_interrupt_active_turn(client: &Arc<client::DaemonClient>, app: &App) {
    send_cancel_chat(client, app.conversations.home_turn(), "soft");
}

/// Hard-cancel the active turn and invalidate its pending events.
fn hard_cancel_active_turn(
    app: &mut App,
    live_area: &mut LiveArea,
    turn_gen: &mut u64,
    client: &Arc<client::DaemonClient>,
) {
    *turn_gen += 1;
    send_cancel_chat(client, app.conversations.home_turn(), "hard");
    clear_active_turn_ui(app, live_area);
}

/// Run the shared Esc / first-Ctrl+C reset path.
/// Ordering is deliberate: an open watch stream pane closes first, then any
/// full-screen view returns to chat, then the main input bar is cleared and
/// any in-flight chat turn is cancelled.
///
/// Returns `true` when a screen was exited or a watch pane was closed, in
/// which case the caller should `continue` the event loop immediately; the
/// clear/cancel path returns `false` and falls through, exactly as the
/// original Esc arm did.
fn handle_esc_reset(
    app: &mut App,
    live_area: &mut LiveArea,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
) -> bool {
    // Note: Esc only clears the ephemeral tool slot when it actually cancels
    // an in-flight turn (via `clear_current_turn` below). Clearing it up
    // front was wrong: Esc on the Runs screen (closing a pane or returning
    // to chat) while a turn streams in the background must not wipe that
    // turn's live tool streams.
    if dismiss_non_chat_layer(app, live_area, watch_follow_handle) {
        return true;
    }

    // Idle Esc is now handled by the event-loop-level routing (it is inert
    // when no turn is active). This path is only reached for the idle
    // Ctrl+C first-press fall-through; clear the prompt to match the
    // historical behaviour.
    app.clear_input();
    app.input_cursor = 0;
    app.clear_completion();
    live_area.reset_input_scroll();

    false
}

/// Overall deadline for the whole repository-context snapshot. Each git
/// subprocess is individually capped, but several run in sequence; this
/// bounds the total so a hung `git` cannot stall request construction for
/// tens of seconds.
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the `chat` request payload sent to the daemon. Carries the operator's
/// project root so the chat gremlin anchors against the actual project rather
/// than the daemon's own working directory.
///
/// `context` is the repository-context snapshot gathered off the UI thread by
/// the caller (see [`send_chat_input`]). It is carried as a separate request
/// field so the daemon can append it to the chat system prompt as low-trust
/// metadata rather than folding it into the operator's message.
fn build_chat_request(
    text: &str,
    turn: &str,
    project_root: &str,
    context: &str,
    env: &HashMap<String, String>,
    skills: &[String],
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "op": "chat",
        "text": text,
        "turn": turn,
        "project_root": project_root,
        "env": env,
    });
    if !context.is_empty() {
        request["context"] = serde_json::Value::String(context.to_string());
    }
    // Always send the skills array — even when empty — so the daemon knows
    // the full active set and `/clear` resets it.
    request["skills"] = serde_json::Value::Array(
        skills
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );
    request
}

/// Submit plain-text chat input to the daemon.
///
/// This is the shared tail of the Enter and `Ctrl+G`-editor paths. Caller
/// is responsible for taking the input, recording history, and stopping any
/// log follow first. Routing has already validated the Home address and session.
#[allow(clippy::too_many_arguments)]
fn send_chat_input<B: ratatui::backend::Backend>(
    live_area: &mut LiveArea<B>,
    app: &mut App,
    client: &Arc<client::DaemonClient>,
    input: String,
    turn_gen: &mut u64,
    session_generation: u64,
    model_tx: mpsc::UnboundedSender<ChatModelQuery>,
) -> io::Result<()> {
    // Start a new turn with the prompt.
    live_area.clear_tool_output();
    live_area.submit_prompt(input.clone());

    // Force an immediate frame so the pinned prompt appears without waiting
    // for the first daemon event.
    live_area.compose(app)?;

    app.active_request = true;
    *turn_gen += 1;
    let turn_id = turn_gen.to_string();
    app.set_conversation_status(Target::Home, ConversationStatus::Running);
    app.conversations.begin_home_turn(turn_id.clone());
    let client_for_chat = Arc::clone(client);
    let chat_tx = client_for_chat.chat_sender();
    // The operator's project root, resolved the same way the info bar does.
    let project_root = gremlins::config::project_root();
    // Snapshot active skills before the spawn so the borrow of `app` is released.
    let active_skills = app.active_skills.clone();
    tokio::spawn(async move {
        // Gather the repository-context snapshot off the runtime's worker
        // threads (git subprocesses are blocking) and under one overall
        // deadline, so a hung `git` cannot stall the chat turn or starve the
        // event loop. On timeout or failure the prompt is sent without a
        // context block rather than being dropped.
        let project_root_str = project_root.to_string_lossy().into_owned();
        // Establish a shared absolute deadline so pre-fetch and the static
        // context snapshot together fit within CONTEXT_TIMEOUT rather than
        // each getting an independent timeout.
        let shared_deadline = Instant::now() + CONTEXT_TIMEOUT;

        // Pre-fetch context from the operator's message (gremlin IDs, file
        // paths, definition names) so the model doesn't burn its first turn
        // on discovery tools. Passes the shared deadline so the matchers
        // return completed sections when time runs out.
        let prefetch_input = input.clone();
        let prefetch_root = project_root_str.clone();
        let enriched_input = match tokio::time::timeout(
            CONTEXT_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                chat_prefetch::prefetch_context(&prefetch_input, &prefetch_root, shared_deadline)
            }),
        )
        .await
        {
            Ok(Ok(pf)) if !pf.is_empty() => format!("{pf}\n\n---\n\n{input}"),
            _ => input,
        };

        // Use the remaining budget from the shared deadline for the static
        // context snapshot.
        let remaining = shared_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        let context = match tokio::time::timeout(
            remaining,
            tokio::task::spawn_blocking(move || {
                chat_context::build_chat_context(&project_root_str)
            }),
        )
        .await
        {
            Ok(Ok(ctx)) => ctx,
            Ok(Err(_)) | Err(_) => String::new(),
        };
        // Forward the TUI process's environment so the chat gremlin's Bash
        // tool runs commands under the operator's environment rather than an
        // empty one. Collected here (on the chat submission path) so it
        // reflects the operator's shell at submit time. The chat gremlin is
        // interactive, so automatic compaction is disabled by pinning the
        // token threshold to u64::MAX (18446744073709551615); set
        // GREMLINS_COMPACTION_THRESHOLD_TOKENS yourself to opt back in.
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.entry("GREMLINS_COMPACTION_THRESHOLD_TOKENS".to_string())
            .or_insert_with(|| u64::MAX.to_string());
        let request = build_chat_request(
            &enriched_input,
            &turn_id,
            &project_root.to_string_lossy(),
            &context,
            &env,
            &active_skills,
        );
        match client_for_chat
            .send_session_request_for_generation(request, session_generation)
            .await
        {
            Ok(resp) => {
                // A chat ack is `{"type":"ok",...}`; an error response is
                // `{"type":"error",...}`. Surface a setup failure as a chat
                // error so `active_request` is cleared.
                if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let msg = resp
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error")
                        .to_string();
                    let _ = chat_tx.send((
                        gremlins_magic_bus::EventKind::Error {
                            message: msg,
                            turn: Some(turn_id.clone()),
                        },
                        Some(turn_id.clone()),
                        None,
                    ));
                } else {
                    // The chat turn is now airborne: the daemon has created
                    // (or refreshed) the session's chat gremlin and patched
                    // its `state.json` model. Refresh the info-bar cache from
                    // the daemon, which owns model state. Spawn rather than
                    // await so this task finishes as soon as the ack is
                    // handled and never borrows the client longer than needed.
                    let client_for_model = Arc::clone(&client_for_chat);
                    let model_tx = model_tx.clone();
                    tokio::spawn(async move {
                        let result = query_chat_model(&client_for_model, session_generation).await;
                        let _ = model_tx.send(result);
                    });
                }
            }
            Err(e) => {
                let _ = chat_tx.send((
                    gremlins_magic_bus::EventKind::Error {
                        message: e,
                        turn: Some(turn_id.clone()),
                    },
                    Some(turn_id.clone()),
                    None,
                ));
            }
        }
    });

    Ok(())
}

/// Query the daemon for the active chat gremlin's model and return the
/// result tagged with the session generation the query was issued for.
///
/// The response has `model` as a string or `null` when the session has no
/// chat gremlin yet. A non-`ok` reply, a missing `model` key, or a
/// non-string/non-null model is treated as an error so a malformed reply can
/// never silently clear the info bar. The TUI never reads `tui.json` or
/// `state.json`; the daemon owns model state.
async fn query_chat_model(
    client: &Arc<client::DaemonClient>,
    session_generation: u64,
) -> ChatModelQuery {
    let result = client
        .send_session_request_for_generation(
            serde_json::json!({ "op": "chat_model" }),
            session_generation,
        )
        .await
        .and_then(|resp| {
            if resp.get("type").and_then(|v| v.as_str()) != Some("ok") {
                return Err(resp
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("chat_model request failed")
                    .to_string());
            }

            match resp.get("model") {
                Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(model)) => Ok(Some(model.clone())),
                _ => Err("chat_model response missing 'model' field".to_string()),
            }
        });

    (session_generation, result)
}

/// Read a canonical token/cache field from a usage object.
fn usage_field(usage: Option<&serde_json::Value>, field: &str) -> Option<u64> {
    usage.and_then(|v| v.get(field)).and_then(|v| v.as_u64())
}

/// Placeholder reasoning title shown as soon as the first reasoning chunk of
/// a turn arrives, and the fixed label prefix of the live stats title. Shared
/// by [`stats_title`] and the reasoning band in `screen.rs` so the
/// placeholder→stats handoff can never drift between two hard-coded strings.
pub(crate) const REASONING_PLACEHOLDER_TITLE: &str = "clanking";

/// Finished reasoning title used when the agent calls `Done`. Used by
/// `LiveArea::relabel_reasoning_title` (in `screen.rs`) so the active→finished
/// handoff can never drift between two hard-coded strings.
pub(crate) const REASONING_COMPLETED_TITLE: &str = "clawnked";

/// Format a token count compactly for the live stats title: raw integers
/// below 1000, one-decimal `k` for thousands, and two-decimal `M` for
/// millions (with the second decimal trimmed when zero, so 1.00M renders as
/// 1.0M). Promotion to millions happens at 999_950, when the thousands
/// display would reach 1000.0k. Missing values render as a dash.
pub(crate) fn compact_tokens(value: Option<u64>) -> String {
    match value {
        None => "-".to_string(),
        Some(v) if v < 1_000 => v.to_string(),
        Some(v) if v < 999_950 => {
            format!("{:.1}k", v as f64 / 1_000.0)
        }
        Some(v) => {
            let s = format!("{:.2}M", v as f64 / 1_000_000.0);
            // Trim the trailing zero in the hundredths place when it is zero
            // (e.g. "1.00M" → "1.0M", "1.20M" → "1.2M") but leave "1.23M" alone.
            if s.ends_with("0M") && s.as_bytes()[s.len() - 4] == b'.' {
                // "X.X0M" → strip the zero → "X.XM"
                let mut trimmed = s.into_bytes();
                trimmed.remove(trimmed.len() - 2); // remove the trailing zero before 'M'
                String::from_utf8(trimmed).unwrap()
            } else {
                s
            }
        }
    }
}

/// Format one usage object as the compact token/cache portion of the live
/// stats title: prompt (`↑`), completion (`↓`), and reasoning (`∴`) token
/// counts separated by ` · `, plus a single `cache <pct>%` group derived
/// from `cached_input_tokens / prompt_tokens`.
///
/// `usage` is the per-turn delta; `total` is the cumulative conversation-wide
/// total. Completion (`↓`) and reasoning (`∴`) values are drawn from `total`
/// when available and fall back to `usage` so the title never regresses. Prompt
/// (`↑`) and cache are always sourced from the per-turn `usage`.
/// Missing values render as dashes. A zero or absent prompt denominator
/// renders `cache -` and never divides by zero.
pub(crate) fn usage_title_part(
    usage: Option<&serde_json::Value>,
    total: Option<&serde_json::Value>,
) -> String {
    let prompt = usage_field(usage, "prompt_tokens");
    let cache = match prompt {
        Some(p) if p > 0 => match usage_field(usage, "cached_input_tokens") {
            Some(cached) => {
                format!("cache {:.0}%", (cached as f64 / p as f64) * 100.0)
            }
            None => "cache -".to_string(),
        },
        Some(_) | None => "cache -".to_string(),
    };

    // Cumulative figures for completion and reasoning; fall back to per-turn.
    let completion =
        usage_field(total, "completion_tokens").or_else(|| usage_field(usage, "completion_tokens"));
    let reasoning =
        usage_field(total, "reasoning_tokens").or_else(|| usage_field(usage, "reasoning_tokens"));

    format!(
        "↑{} · ↓{} · ∴{} · {cache}",
        compact_tokens(prompt),
        compact_tokens(completion),
        compact_tokens(reasoning)
    )
}

/// Build the reasoning-stream stats title shown on each `TurnComplete`:
/// the `clanking` header label followed by the turn number and the compact
/// per-turn and cumulative token/cache summary.
fn stats_title(
    turn: usize,
    stats: Option<&serde_json::Value>,
    total: Option<&serde_json::Value>,
) -> String {
    format!(
        "{REASONING_PLACEHOLDER_TITLE} · turn {turn} · {}",
        usage_title_part(stats, total)
    )
}

/// Fatal TUI error, classified by whether it has already been reported on
/// the operator's terminal.
#[derive(Debug)]
pub enum RunError {
    /// The error was written to stdout after the terminal was restored. The
    /// caller must still exit nonzero, but must not print the message again.
    Reported,
    /// The error occurred while stderr was still the operator's terminal.
    /// The caller should print `gremlins: <message>` to stderr and exit
    /// nonzero.
    Unreported(String),
}

/// Initialise the terminal, run the event loop, and restore on exit.
pub async fn run() -> Result<(), RunError> {
    // Refuse to start the TUI when stdout is not a terminal.
    if !std::io::stdout().is_terminal() {
        eprintln!("gremlins: stdout is not a terminal — use `gremlins ls` to list gremlins");
        return Ok(());
    }

    // Load the process-global config before taking over the terminal or
    // redirecting stderr. A malformed settings.yaml is therefore returned
    // to the CLI entry point while stderr is still the operator's terminal.
    gremlins::config::init_global().map_err(|e| RunError::Unreported(e.to_string()))?;

    // Terminal setup also runs before the stderr redirect so setup failures
    // land on the real stderr. `LiveArea::new` leaves raw mode entered, so
    // on exit the `LiveArea` drop guard restores the cursor and raw mode.
    let mut live_area =
        LiveArea::new().map_err(|e| RunError::Unreported(format!("terminal setup failed: {e}")))?;

    // Snapshot the real stderr before redirecting it, so a post-redirect
    // failure can restore it before reporting the error. Use a close-on-exec
    // duplicate so a daemon spawned by `run_app` does not inherit a live
    // copy of the operator's stderr (which would keep a pipe reader waiting
    // for EOF after the TUI exits).
    let saved_stderr = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 0) };

    // Redirect stderr to tui.log so debug logs persist next to executor.log.
    // Config is loaded first so a custom `paths.state-root` is honored for
    // the log path. Track whether the redirect succeeded: if it did not,
    // stderr is still the operator's terminal and the CLI entry point can
    // report post-loop failures there as usual.
    let mut stderr_redirected = false;
    let log_path = gremlins::config::state_root().join("tui.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::os::fd::IntoRawFd;
        let fd = file.into_raw_fd();
        unsafe { libc::dup2(fd, 2) };
        unsafe { libc::close(fd) };
        stderr_redirected = true;
    }

    if let Err(e) = run_app(&mut live_area).await {
        // Restore the terminal first so the message is printed on a sane
        // terminal rather than into raw mode, then restore the saved stderr
        // (when we were able to snapshot it).
        drop(live_area);
        if saved_stderr >= 0 {
            unsafe { libc::dup2(saved_stderr, 2) };
            unsafe { libc::close(saved_stderr) };
        }

        if stderr_redirected {
            // The operator's stderr may itself have been redirected before
            // launch (for example `gremlins 2>/dev/null`), so restoring fd 2
            // is not enough to make the error visible. Issue #1951 requires
            // these fatal event-loop/daemon errors on stdout after the
            // terminal is restored. The CLI exits nonzero without printing
            // this message again.
            println!("gremlins: {e}");
            return Err(RunError::Reported);
        }

        return Err(RunError::Unreported(e.to_string()));
    }

    if saved_stderr >= 0 {
        unsafe { libc::close(saved_stderr) };
    }
    Ok(())
}

/// Path to the persisted input history file, under `$state_root/tui/`.
fn history_path() -> std::path::PathBuf {
    gremlins::config::state_root()
        .join("tui")
        .join("history.json")
}

/// Maximum byte size of the history file we are willing to read. A larger
/// file is treated as absent (empty history), guarding against a pathological
/// file exhausting memory before the event loop starts.
const HISTORY_MAX_BYTES: u64 = 1 << 20; // 1 MiB

/// Load the persisted input history (a JSON array of strings, newest-last).
///
/// A missing, unreadable, oversized, or malformed file yields an empty
/// history — never a fatal error and never a transcript line. The result is
/// capped to [`App::HISTORY_CAP`] entries so a valid oversized file cannot
/// bypass the bounded-history policy.
fn load_history() -> Vec<String> {
    let path = history_path();
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(_) => return Vec::new(),
    };
    if data.len() as u64 > HISTORY_MAX_BYTES {
        return Vec::new();
    }
    let mut history: Vec<String> = serde_json::from_slice(&data).unwrap_or_default();
    if history.len() > App::HISTORY_CAP {
        history.drain(..history.len() - App::HISTORY_CAP);
    }
    history
}

/// Persist the input history as a JSON array of strings (newest-last).
///
/// Errors are silently ignored (logged at most); history persistence must
/// never fail the TUI. The write is atomic: data goes to a sibling temp file
/// which is then renamed over the target, so a crash mid-write leaves the
/// previous file intact rather than a truncated one.
fn save_history(history: &[String]) {
    let path = history_path();
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let data = match serde_json::to_string_pretty(history) {
        Ok(data) => data,
        Err(_) => return,
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, data).is_err() {
        log::debug!("tui: failed to write history to {}", tmp.display());
        return;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        log::debug!("tui: failed to rename history into {}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

async fn run_app(live_area: &mut LiveArea) -> io::Result<()> {
    let mut app = App::new();

    // Seed the in-memory history from the persisted file before the event
    // loop starts, so Up/Down recall survives a TUI restart.
    app.input_history = load_history();

    // ── Connect to the daemon socket ──────────────────────────────
    let (client, mut event_rx, mut chat_rx, mut raw_rx) = match client::connect().await {
        Ok(c) => c,
        Err(e) => {
            return Err(io::Error::new(io::ErrorKind::NotConnected, e));
        }
    };
    let client = Arc::new(client);
    install_home_session(&mut app, &client).await;

    // ── Initial state snapshot ────────────────────────────────────
    match client
        .send_control_request(serde_json::json!({"op": "ls"}))
        .await
    {
        Ok(resp) => {
            if let Some(gremlins) = resp.get("gremlins").and_then(|v| v.as_array()) {
                for entry in gremlins {
                    app.upsert_run_from_ls(entry);
                }
            }
        }
        Err(e) => {
            let msg = format!("error fetching initial state: {e}");
            live_area.push_transcript_line(msg);
        }
    }

    // Query the daemon for the active chat model before the first prompt so
    // the info bar can show a restored session's model immediately.
    let session_generation = client.session_generation().await;
    match query_chat_model(&client, session_generation).await {
        (_, Ok(model)) => app.chat_model = model,
        (_, Err(e)) => log::debug!("initial chat_model query failed: {e}"),
    }

    // ── Crossterm event channel (polling task → async) ─────────
    // Uses poll + sleep instead of blocking read so that the event
    // loop is never starved by a background reader holding the stdin lock.
    // The reader can be suspended (parked) while an external editor owns
    // the terminal, so it never races the editor for keystrokes.
    let (suspend_handle, reader) = SuspendHandle::new();
    let (ct_tx, mut ct_rx) = mpsc::unbounded_channel::<Event>();
    {
        let ReaderChannels {
            mut suspend_rx,
            mut resume_rx,
            parked,
            dead,
        } = reader;
        tokio::spawn(async move {
            loop {
                // Park on a suspend request: acknowledge, then wait for the
                // resume signal before polling again.
                if suspend_rx.try_recv().is_ok() {
                    parked.notify_one();
                    if resume_rx.recv().await.is_none() {
                        break;
                    }
                    continue;
                }
                match crossterm::event::poll(Duration::from_millis(20)) {
                    Ok(true) => match crossterm::event::read() {
                        Ok(ev) => {
                            if ct_tx.send(ev).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(_) => break,
                }
            }
            dead.notify_one();
        });
    }

    // ── Channel for socket-op results ─────────────────────────────
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<Vec<String>>();

    // ── Channel for session-recreation completion ─────────────────
    let (reset_tx, mut reset_rx) = mpsc::unbounded_channel::<Result<(), String>>();

    // ── Channel for daemon chat-model lookups ─────────────────────
    let (model_tx, mut model_rx) = mpsc::unbounded_channel::<ChatModelQuery>();

    // ── Channel for lightweight status resync after event_lagged ──
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<(Vec<ResyncRun>, u64)>();
    let mut status_generation: u64 = 0;

    // ── Log follow state ──────────────────────────────────────────
    let mut log_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Dedicated follow for the inline watch stream pane.
    let mut watch_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Monotonic token for the current watch follow; queued lines must match
    // both id and generation before being painted.
    let mut watch_follow_generation: u64 = 0;
    // ── Chat turn state ────────────────────────────────────────
    // (the chat request is sent from a detached task; we no longer need to
    // abort it, so no JoinHandle is kept)
    // Monotonic turn id, bumped on every submit and every Esc so that a
    // stale turn's terminal events (which arrive after a cancel or a new
    // submit) can be recognised and dropped.
    let mut turn_gen: u64 = 0;
    // The turn id of the currently active turn, if any.
    // A navigation shortcut must not replace the very draft being parked.
    let mut last_stage: Option<Target> = None;
    let mut attachment = Attachment::default();
    let (attach_tx, mut attach_rx) = mpsc::unbounded_channel::<AttachmentFrame>();
    let (delivery_tx, mut delivery_rx) = mpsc::unbounded_channel::<(u64, Result<(), String>)>();
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    // ── Startup banner ────────────────────────────────────────────
    for line in motif::banner() {
        live_area.push_styled_line(line);
    }

    // ── Ctrl+C pending-quit state (event-loop-local input routing) ──
    let mut pending_quit: Option<Instant> = None;
    // ── Active-turn interrupt state (soft -> hard -> no-op) ──
    let mut interrupt = HomeInterrupt::default();

    // ── Event loop ────────────────────────────────────────────────
    loop {
        let (cols, _rows) = live_area.size();
        app.width = cols;
        interrupt.disarm_if_stale(&app.selected_target, Instant::now());
        live_area.compose(&app)?;

        tokio::select! {
            // ── Crossterm events ──────────────────────────────
            Some(ev) = ct_rx.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('t')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                                && app.screen.is_chat() && app.history_search.is_none() =>
                        {
                            let next = if app.selected_target.is_home() {
                                last_stage.clone()
                            } else {
                                Some(Target::Home)
                            };
                            if let Some(target) = next {
                                select_passive_watch(&mut app, live_area, &client, target, &mut attachment, &attach_tx, &mut last_stage);
                            }
                        }
                        KeyCode::Char('r')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            // Ctrl+R reverse history search — Chat screen only.
                            if !app.screen.is_chat() {
                                continue;
                            }
                            if app.history_search.is_some() {
                                // Already searching: cycle to next (older) match.
                                app.search_next();
                            } else {
                                // Enter search mode.
                                app.enter_search();
                            }
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::Char('c')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            // Ctrl+C during search cancels the search and
                            // restores the saved input — it must NEVER
                            // propagate to the interrupt / quit path.
                            if app.history_search.is_some() {
                                app.cancel_search();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                                continue;
                            }

                            let now = Instant::now();

                            if selected_home_turn_active(&app) {
                                // Non-chat layering runs before the interrupt
                                // routing: on the Runs screen, Ctrl+C closes
                                // the pane / exits to chat rather than
                                // interrupting the background turn.
                                if dismiss_non_chat_layer(
                                    &mut app,
                                    live_area,
                                    &mut watch_follow_handle,
                                ) {
                                    continue;
                                }
                                match interrupt.press(now) {
                                    InterruptAction::SendSoft => {
                                        soft_interrupt_active_turn(&client, &app);
                                    }
                                    InterruptAction::SendHard => {
                                        hard_cancel_active_turn(
                                            &mut app,
                                            live_area,
                                            &mut turn_gen,
                                            &client,
                                        );
                                    }
                                    InterruptAction::NoOp => {}
                                }
                                continue;
                            }

                            // After a hard cancel retired the home turn,
                            // the interrupt window may still be open.  Treat
                            // any press inside it as a no-op so Ctrl+C never
                            // arms the quit path while the turn is being
                            // torn down.
                            if interrupt.in_window(now) && interrupt.count >= 2
                            {
                                continue;
                            }

                            if ctrl_c_should_quit(&mut pending_quit, now) {
                                break;
                            }
                            // First press (or an expired previous press): reset
                            // exactly like Esc. The one-second quit window is
                            // already armed by `ctrl_c_should_quit`.
                            if handle_esc_reset(
                                &mut app,
                                live_area,
                                &mut watch_follow_handle,
                            ) {
                                continue;
                            }
                        }
                        KeyCode::Char('d')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            if app.history_search.is_some() {
                                continue;
                            }
                            if app.input.is_empty() {
                                break;
                            }
                        }
                        KeyCode::Char('l')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            live_area.invalidate();
                        }
                        KeyCode::Char('o')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            if app.screen != Screen::Runs {
                                continue;
                            }
                            let Some(selected) = app.watch_selection.clone() else {
                                continue;
                            };
                            if app.watch_following.as_deref() == Some(selected.as_str()) {
                                close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                            } else {
                                start_watch_follow(
                                    &mut app,
                                    live_area,
                                    &client,
                                    selected,
                                    &log_tx,
                                    &mut watch_follow_handle,
                                    &mut watch_follow_generation,
                                );
                            }
                        }
                        KeyCode::Char('w')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            // Ctrl+W enters/exits the watch (Runs) screen.
                            match watch_key_action(&app) {
                                WatchKeyAction::NoOp => continue,
                                WatchKeyAction::EnterRuns => {
                                    close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                                    app.show_runs();
                                    live_area.show_runs();
                                    app.ensure_watch_selection();
                                    status_generation = status_generation.wrapping_add(1);
                                    fetch_run_statuses(&client, &status_tx, status_generation);
                                }
                                WatchKeyAction::ClosePane => {
                                    close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                                }
                                WatchKeyAction::OpenPane(selected) => {
                                    start_watch_follow(
                                        &mut app,
                                        live_area,
                                        &client,
                                        selected,
                                        &log_tx,
                                        &mut watch_follow_handle,
                                        &mut watch_follow_generation,
                                    );
                                }
                                WatchKeyAction::ExitToChat => {
                                    app.exit_screen();
                                    live_area.exit_screen_view();
                                }
                            }
                        }
                        KeyCode::Char('g')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            if app.history_search.is_some() {
                                continue;
                            }
                            if !app.screen.is_chat() {
                                continue;
                            }
                            // Home's turn must not gate another conversation's correction.
                            if selected_home_turn_active(&app) {
                                live_area.push_transcript_line(
                                    "a request is already in progress — wait for the response"
                                        .to_string(),
                                );
                                continue;
                            }

                            match open_editor(app.input.text(), &suspend_handle).await {
                                Ok(outcome) => {
                                    // `open_editor` returns the reader-resume
                                    // guard so we can drain the event channel
                                    // while the reader is still parked. This
                                    // guarantees no handoff keystrokes are
                                    // either replayed afterwards or discarded
                                    // accidentally: the channel cannot change
                                    // during the drain.
                                    let (result, _resume) = outcome.into_parts();
                                    while ct_rx.try_recv().is_ok() {}
                                    match result {
                                        Ok(Some(text)) => {
                                            // Write the composed text back into the
                                            // input bar for review; it is only sent
                                            // to the model when the user presses
                                            // Enter. Reset history navigation so the
                                            // fresh text isn't treated as a history
                                            // position.
                                            // Editor round-trip resets path
                                            // substitutions: the editor edits the
                                            // display form, so completed paths
                                            // revert to their as-typed form.
                                            app.replace_input(&text);
                                            app.reset_cursor_to_end();
                                            app.clear_completion();
                                            app.history_cursor = None;
                                            live_area.ensure_input_cursor_visible(
                                                app.input_cursor,
                                                app.input.text(),
                                                app.width,
                                            );
                                        }
                                        Ok(None) => {
                                            // Editor aborted without changes; leave
                                            // the input bar exactly as it was.
                                        }
                                        Err(e) => {
                                            // The editor never returned composed
                                            // text; preserve what the user already
                                            // typed in the input bar.
                                            live_area.push_transcript_line(format!(
                                                "editor error: {e}"
                                            ));
                                        }
                                    }
                                    // `_resume` drops here and wakes the parked
                                    // reader after the drain completes.
                                    drop(_resume);
                                }
                                Err(e) => {
                                    // Failed to suspend the input reader (it
                                    // had already exited). No terminal state
                                    // was touched, so there is nothing to
                                    // drain or resume.
                                    live_area.push_transcript_line(format!(
                                        "editor error: {e}"
                                    ));
                                }
                            }
                        }

                        KeyCode::Esc => {
                            // Search-mode Esc cancels the search without
                            // clearing the input bar — the saved input is
                            // restored verbatim.
                            if app.history_search.is_some() {
                                app.cancel_search();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                                continue;
                            }

                            // Non-chat layering first (watch pane close ->
                            // exit to chat); unchanged regardless of turn
                            // state.
                            if dismiss_non_chat_layer(
                                &mut app,
                                live_area,
                                &mut watch_follow_handle,
                            ) {
                                continue;
                            }

                            // Dismiss the current conversation's input, regardless of where it was typed.
                            if !app.input.is_empty()
                                && (!app.selected_target.is_home() || selected_home_turn_active(&app))
                            {
                                live_area.clear_target_input(&app.selected_target.clone(), &mut app);
                                continue;
                            }
                            // Idle ESC is inert — the input bar is left intact.
                            if selected_home_turn_active(&app) {
                                match interrupt.press(Instant::now()) {
                                    InterruptAction::SendSoft => {
                                        soft_interrupt_active_turn(&client, &app);
                                    }
                                    InterruptAction::SendHard => {
                                        hard_cancel_active_turn(
                                            &mut app,
                                            live_area,
                                            &mut turn_gen,
                                            &client,
                                        );
                                    }
                                    InterruptAction::NoOp => {}
                                }
                                continue;
                            }

                            // Post-interrupt escalation: a turn that was
                            // soft-interrupted can still be hard-cancelled
                            // within the double-press window.  Only
                            // `SendHard` is meaningful here — a soft
                            // cancel on an already-interrupted turn is a
                            // no-op (the supervisor ignores it).
                            let now = Instant::now();
                            if interrupt.ended_turn.is_some() && interrupt.in_window(now)
                                && interrupt.press(now) == InterruptAction::SendHard
                            {
                                send_cancel_chat(
                                    &client,
                                    interrupt.ended_turn.as_deref(),
                                    "hard",
                                );
                                clear_active_turn_ui(&mut app, live_area);
                                interrupt.ended_turn = None;
                            }
                        }
                        KeyCode::Tab => {
                            // Completion is for the main input bar only.
                            if app.history_search.is_some() || !app.screen.is_chat() {
                                continue;
                            }

                            let active = app.completion.as_ref().map(|state| {
                                (state.span.clone(), state.contains_current(app.input.text()))
                            });
                            if let Some((span, advance)) = active {
                                // The initial Tab only inserted the common
                                // prefix, so it must not cycle yet. On
                                // subsequent presses, advance to the next
                                // candidate before committing so the input
                                // always matches the state's current index.
                                if advance {
                                    app.completion_advance();
                                }
                                let (next_absolute, next) = {
                                    let state =
                                        app.completion.as_ref().expect("completion is active");
                                    (
                                        state.current_absolute().to_string(),
                                        state.current().to_string(),
                                    )
                                };
                                app.replace_input_span(span.clone(), next.clone());
                                let new_span = span.start..span.start + next.len();
                                app.update_completion_span(new_span.clone());
                                app.add_input_substitution(new_span, next_absolute);
                            } else {
                                let cursor = app.input_cursor.min(app.input.len());
                                let Some(mut state) = completion::complete(app.input.text(), cursor) else {
                                    continue;
                                };
                                let span = state.span.clone();
                                if state.candidates.len() == 1 {
                                    let candidate = state.candidates[0].clone();
                                    let absolute = state.absolutes[0].clone();
                                    app.replace_input_span(span.clone(), candidate.clone());
                                    let new_span = span.start..span.start + candidate.len();
                                    app.add_input_substitution(new_span, absolute);
                                } else {
                                    let prefix = completion::common_prefix(&state.candidates);
                                    let absolute_prefix =
                                        completion::common_prefix(&state.absolutes);
                                    let new_span = span.start..span.start + prefix.len();
                                    state.index = 0;
                                    state.span = new_span.clone();
                                    app.set_completion(state);
                                    app.replace_input_span(span, prefix);
                                    if !new_span.is_empty() {
                                        app.add_input_substitution(new_span, absolute_prefix);
                                    }
                                }
                            }
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::BackTab => {
                            if app.history_search.is_some() || !app.screen.is_chat() {
                                continue;
                            }
                            let Some(span) =
                                app.completion.as_ref().map(|state| state.span.clone())
                            else {
                                continue;
                            };
                            // Rewind first, then commit the previous candidate
                            // so the input always matches the state's current
                            // index.
                            app.completion_rewind();
                            let (candidate, absolute) = {
                                let state = app.completion.as_ref().expect("completion is active");
                                (state.current().to_string(), state.current_absolute().to_string())
                            };
                            app.replace_input_span(span.clone(), candidate.clone());
                            let new_span = span.start..span.start + candidate.len();
                            app.update_completion_span(new_span.clone());
                            app.add_input_substitution(new_span, absolute);
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::Enter => {
                            // Search-mode Enter places the matched line into
                            // the input bar but does NOT submit it.
                            if app.history_search.is_some() {
                                app.accept_search();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                                continue;
                            }

                            // Runs screen: Enter opens the selected gremlin.
                            if app.screen == Screen::Runs {
                                watch_enter(&mut app, live_area, &client, &mut attachment, &attach_tx, &mut last_stage, &mut watch_follow_handle);
                                continue;
                            }

                            // Any other non-chat screen has no input bar; ignore
                            // Enter until the operator returns to chat.
                            if !app.screen.is_chat() {
                                continue;
                            }

                            if selected_control(&mut app, live_area, &mut attachment) {
                                continue;
                            }
                            let command = dispatch(app.input.text());
                            if command.is_none() && route_submission(&mut app, live_area, &client, &attachment, &delivery_tx) {
                                continue;
                            }

                            let input_buf: InputBuffer = app.take_input();
                            let display_input = input_buf.text().to_string();
                            let input = input_buf.resolve();
                            app.input_cursor = 0;
                            app.clear_completion();
                            live_area.reset_input_scroll();
                            if input.is_empty() {
                                continue;
                            }

                            // Record the submission for Up/Down history. The
                            // display form is stored so recall reproduces what
                            // the operator typed (relative paths, not resolved
                            // absolute paths).
                            if app.push_history(display_input.clone()) {
                                save_history(&app.input_history);
                            }

                            // Stop log following on any new command.
                            if let Some(handle) = log_follow_handle.take() {
                                handle.abort();
                                app.following_log = None;
                            }

                            if let Some(result) = command {
                                match result {
                                    CommandResult::ShowRuns => {
                                        close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                                        app.show_runs();
                                        live_area.show_runs();
                                        // Trigger a fresh ls so the table reflects the daemon's
                                        // current state, including any gremlins rm'd outside the TUI.
                                        status_generation = status_generation.wrapping_add(1);
                                        fetch_run_statuses(&client, &status_tx, status_generation);
                                    }
                                    CommandResult::Debug(id) => {
                                        debug_entry(&mut app, live_area, &client, id, &mut attachment, &attach_tx, &mut last_stage, &mut watch_follow_handle);
                                    }
                                    CommandResult::Lines(lines) => {
                                        let mut block = Vec::with_capacity(lines.len() + 1);
                                        block.push(format!("> {display_input}"));
                                        block.extend(lines.iter().cloned());
                                        live_area.push_transcript_block(block);
                                    }
                                    CommandResult::RestartChat => {
                                        if app.session_recreating {
                                            live_area.push_transcript_line(
                                                "session reset already in progress".to_string(),
                                            );
                                            continue;
                                        }
                                        attachment.invalidate(&mut app);
                                        live_area.select_conversation(&mut app, Target::Home);
                                        // Clear current turn.
                                        live_area.clear_current_turn();
                                        // Clear the transcript/scrollback so the
                                        // UI no longer shows the old conversation
                                        // after the daemon has discarded it. The
                                        // transcript also clears its embedded
                                        // reasoning and tool slots.
                                        live_area.clear_transcript();
                                        for line in motif::banner() {
                                            live_area.push_styled_line(line);
                                        }
                                        // `/clear` closes the current session and
                                        // opens a fresh one. The daemon cancels
                                        // any in-flight turn and drops the old
                                        // chat gremlin when the session closes.
                                        // Drop every socket/stream that belonged
                                        // to the dying session first, before the
                                        // new session is installed.
                                        turn_gen += 1;
                                        interrupt = HomeInterrupt::default();
                                        app.home_run_id = None;
                                        app.set_conversation_status(Target::Home, ConversationStatus::Idle);
                                        app.conversations.retire_home_session();
                                        app.active_request = false;
                                        if let Some(handle) = log_follow_handle.take() {
                                            handle.abort();
                                        }
                                        app.following_log = None;
                                        close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                                        // Block new chat submissions and further
                                        // `/clear` requests until the replacement
                                        // session socket is installed. Recreate
                                        // requests are also serialized inside
                                        // `DaemonClient::recreate_session`.
                                        app.session_recreating = true;
                                        let c = Arc::clone(&client);
                                        let tx = reset_tx.clone();
                                        tokio::spawn(async move {
                                            let result = c.recreate_session().await;
                                            let _ = tx.send(result);
                                        });
                                        app.clear_input();
                                        app.input_cursor = 0;
                                        app.clear_completion();
                                        app.active_skills.clear();
                                        live_area.reset_input_scroll();
                                    }
                                    CommandResult::Quit => {
                                        break;
                                    }
                                    CommandResult::AttachHome => {
                                        attachment.invalidate(&mut app);
                                        live_area.select_conversation(&mut app, Target::Home);
                                    }
                                    CommandResult::AttachStage { run_id, stage } => {
                                        attach_stage_entry(&mut app, live_area, &client, run_id, stage, &mut attachment, &attach_tx, &mut last_stage);
                                    }
                                    CommandResult::ActivateSkill(name) => {
                                        // Discover skills to validate the name.
                                        let project_root = gremlins::config::project_root();
                                        let skills = gremlins::skills::discover(&project_root);
                                        match gremlins::skills::find_skill(&skills, &name, false) {
                                            Some(_) => {
                                                if !app.active_skills.contains(&name) {
                                                    app.active_skills.push(name.clone());
                                                }
                                                live_area.push_transcript_line(format!(
                                                    "> /skill {name}\nskill {name:?} activated"
                                                ));
                                            }
                                            None => {
                                                live_area.push_transcript_line(format!(
                                                    "> /skill {name}\nskill {name:?} not found or not user-invocable"
                                                ));
                                            }
                                        }
                                    }
                                    CommandResult::ListSkills => {
                                        let project_root = gremlins::config::project_root();
                                        let skills = gremlins::skills::discover(&project_root);
                                        let index = gremlins::skills::render_operator_index(&skills);
                                        let mut lines: Vec<String> = vec!["> /skills".to_string()];
                                        lines.extend(index.lines().map(String::from));
                                        live_area.push_transcript_block(lines);
                                    }
                                    CommandResult::SocketOp { op, payload } => {
                                        if op == "log" {
                                            if let Some(id) = payload
                                                .get("id")
                                                .and_then(|v| v.as_str())
                                                .map(String::from)
                                            {
                                                app.following_log = Some(id.clone());
                                                let log_tx = log_tx.clone();
                                                let client = Arc::clone(&client);
                                                let follow_task = tokio::spawn(async move {
                                                    match client.follow_log(&id).await {
                                                        Ok((mut rx, handle)) => {
                                                            let _guard = AbortOnDrop(handle);
                                                            while let Some(line) = rx.recv().await {
                                                                if log_tx.send(line).is_err() {
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            let _ = log_tx.send(
                                                                serde_json::json!({
                                                                    "type": "error",
                                                                    "message": e,
                                                                }),
                                                            );
                                                        }
                                                    }
                                                });
                                                log_follow_handle = Some(follow_task);
                                            }
                                        } else {
                                            // Request-response op.
                                            let c = Arc::clone(&client);
                                            let tx = result_tx.clone();
                                            tokio::spawn(async move {
                                                let result = if op == "compact" {
                                                    c.send_session_request(payload).await
                                                } else {
                                                    c.send_control_request(payload).await
                                                };
                                                let lines = match result {
                                                    Ok(resp) => format_socket_response(&op, &resp),
                                                    Err(e) => vec![format!("error: {e}")],
                                                };
                                                let _ = tx.send(lines);
                                            });
                                        }
                                    }
                                }
                            } else {
                                // Plain text — send to chat agent.
                                // Capture the session generation at submission
                                // time. If `/clear` recreates the session while
                                // repository context is still being gathered,
                                // the request is discarded atomically before it
                                // can execute in the new conversation.
                                let session_generation = client.session_generation().await;
                                interrupt = HomeInterrupt::default();
                                send_chat_input(
                                    live_area,
                                    &mut app,
                                    &client,
                                    input,
                                    &mut turn_gen,
                                    session_generation,
                                    model_tx.clone(),
                                )?;
                            }
                        }
                        KeyCode::Left => {
                            if app.history_search.is_some() || !app.screen.is_chat() {
                                continue;
                            }
                            app.cursor_left();
                            app.dismiss_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::Right => {
                            if app.history_search.is_some() || !app.screen.is_chat() {
                                continue;
                            }
                            app.cursor_right();
                            app.dismiss_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::Home => {
                            if app.history_search.is_some() || !app.screen.is_chat() {
                                continue;
                            }
                            app.cursor_home();
                            app.dismiss_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::End => {
                            if app.history_search.is_some() || !app.screen.is_chat() {
                                continue;
                            }
                            app.cursor_end();
                            app.dismiss_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::Backspace => {
                            if app.history_search.is_some() {
                                app.search_backspace();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                            } else if app.screen.is_chat() {
                                app.delete_before_cursor();
                                app.dismiss_completion();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                            }
                        }
                        KeyCode::Delete => {
                            // Delete is a no-op during search mode.
                            if app.history_search.is_some() {
                                continue;
                            }
                            if !app.screen.is_chat() {
                                continue;
                            }
                            app.delete_at_cursor();
                            app.dismiss_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                app.input.text(),
                                app.width,
                            );
                        }
                        KeyCode::Char('j')
                            if key.modifiers.is_empty()
                                && app.screen == Screen::Runs =>
                        {
                            app.watch_move(1);
                        }
                        KeyCode::Char('k')
                            if key.modifiers.is_empty()
                                && app.screen == Screen::Runs =>
                        {
                            app.watch_move(-1);
                        }
                        KeyCode::Char('s')
                            if key.modifiers.is_empty()
                                && app.screen == Screen::Runs =>
                        {
                            let Some(selected) = app.watch_selection.clone() else {
                                continue;
                            };
                            // Only send stop for running gremlins.
                            let is_running = app.active_runs.get(&selected)
                                .map(|r| r.status == "running")
                                .unwrap_or(false);
                            if !is_running {
                                live_area.push_transcript_line(
                                    "stop: selected gremlin is not running".to_string(),
                                );
                                continue;
                            }
                            let c = Arc::clone(&client);
                            let tx = result_tx.clone();
                            tokio::spawn(async move {
                                let result = c
                                    .send_control_request(serde_json::json!({"op": "stop", "id": selected}))
                                    .await;
                                let lines = match result {
                                    Ok(resp) => format_socket_response("stop", &resp),
                                    Err(e) => vec![format!("error: {e}")],
                                };
                                let _ = tx.send(lines);
                            });
                        }
                        KeyCode::Char('r')
                            if key.modifiers.is_empty()
                                && app.screen == Screen::Runs =>
                        {
                            let Some(selected) = app.watch_selection.clone() else {
                                continue;
                            };
                            // Only send resume for stopped or failed gremlins.
                            let can_resume = app.active_runs.get(&selected)
                                .map(|r| matches!(r.status.as_str(), "stopped" | "failed"))
                                .unwrap_or(false);
                            if !can_resume {
                                live_area.push_transcript_line(
                                    "resume: selected gremlin does not need resuming".to_string(),
                                );
                                continue;
                            }
                            let c = Arc::clone(&client);
                            let tx = result_tx.clone();
                            tokio::spawn(async move {
                                let result = c
                                    .send_control_request(serde_json::json!({"op": "resume", "id": selected}))
                                    .await;
                                let lines = match result {
                                    Ok(resp) => format_socket_response("resume", &resp),
                                    Err(e) => vec![format!("error: {e}")],
                                };
                                let _ = tx.send(lines);
                            });
                        }
                        KeyCode::Char(ch) => {
                            if app.history_search.is_some() {
                                app.search_push_char(ch);
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                            } else if app.screen.is_chat() {
                                app.insert_char(ch);
                                app.dismiss_completion();
                                // Typing a fresh character resets history navigation.
                                app.history_cursor = None;
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                            }
                        }
                        KeyCode::Up => {
                            if app.history_search.is_some() {
                                continue;
                            }
                            if app.screen == Screen::Runs {
                                app.watch_move(-1);
                            } else if app.screen.is_chat() {
                                if let Some(entry) = app.history_up().map(|s| s.to_string()) {
                                    app.replace_input(&entry);
                                    app.reset_cursor_to_end();
                                    app.dismiss_completion();
                                    live_area.ensure_input_cursor_visible(
                                        app.input_cursor,
                                        app.input.text(),
                                        app.width,
                                    );
                                }
                            }
                        }
                        KeyCode::Down => {
                            if app.history_search.is_some() {
                                continue;
                            }
                            if app.screen == Screen::Runs {
                                app.watch_move(1);
                            } else if app.screen.is_chat() && app.history_cursor.is_some() {
                                let entry = app.history_down().unwrap_or("").to_string();
                                app.replace_input(&entry);
                                app.reset_cursor_to_end();
                                app.dismiss_completion();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    app.input.text(),
                                    app.width,
                                );
                            }
                        }
                        _ => {}
                    },
                    Event::Resize(cols, rows) => {
                        live_area.resize(cols, rows)?;
                    }
                    _ => {}
                }
            }

            // ── Daemon events ─────────────────────────────────
            Some(event) = event_rx.recv() => {
                // EventKind drops envelope correlation, but identified boundary
                // payloads still carry enough identity to maintain invocation rows.
                // client::read_loop routes each frame exclusively to event_rx,
                // chat_rx, a response waiter, or raw_rx, never both event/raw.
                if matches!(&event, gremlins_magic_bus::EventKind::StageBoundary { .. }) {
                    if let Ok(value) = serde_json::to_value(&event) {
                        app.observe_live_pipeline_frame(&value);
                    }
                }
                match event {
                    gremlins_magic_bus::EventKind::RunStarted { id, definition, stage } => {
                        log::info!("[run started] {id} ({definition}) stage={stage}");
                        app.on_run_started(id, definition, stage);
                        // Invalidate any in-flight ls snapshot so a stale
                        // response cannot later prune this newly started run
                        // (the snapshot predates the daemon event).
                        status_generation = status_generation.wrapping_add(1);
                    }
                    gremlins_magic_bus::EventKind::RunCompleted { id, exit_code } => {
                        log::info!("[run completed] {id} (exit {exit_code})");
                        if app.watch_following.as_deref() == Some(id.as_str()) {
                            app.watch_follow_error = None;
                        }
                        app.on_run_completed(id);
                    }
                    gremlins_magic_bus::EventKind::RunFailed { id, exit_code, error } => {
                        let err_detail = error.as_deref().unwrap_or("");
                        if err_detail.is_empty() {
                            log::info!("[run failed] {id} (exit {exit_code})");
                        } else {
                            log::info!("[run failed] {id} (exit {exit_code}): {err_detail}");
                        }
                        if app.watch_following.as_deref() == Some(id.as_str()) {
                            app.watch_follow_error = None;
                        }
                        app.on_run_failed(id);
                    }
                    gremlins_magic_bus::EventKind::RunStopped { id } => {
                        log::info!("[run stopped] {id}");
                        if app.watch_following.as_deref() == Some(id.as_str()) {
                            app.watch_follow_error = None;
                        }
                        app.on_run_stopped(id);
                    }
                    gremlins_magic_bus::EventKind::RunRemoved { id } => {
                        log::info!("[run removed] {id}");
                        if app.watch_following.as_deref() == Some(id.as_str()) {
                            close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                        }
                        app.on_run_removed(&id);
                        // Invalidate any in-flight ls snapshot so a stale
                        // response cannot resurrect this run after it was
                        // already removed from active_runs.
                        status_generation = status_generation.wrapping_add(1);
                    }
                    gremlins_magic_bus::EventKind::StageTransition { id, stage, sub_stage } => {
                        log::info!("[{id}] stage → {stage}, sub_stage={:?}", sub_stage);
                        app.on_stage_transition(id, stage, sub_stage);
                    }
                    gremlins_magic_bus::EventKind::LogLine { id, line } => {
                        if app.following_log.as_deref() == Some(&id) {
                            live_area.push_transcript_line(line.clone());
                        }
                    }
                    gremlins_magic_bus::EventKind::Bail { id, reason } => {
                        log::info!("[{id}] bail: {reason}");
                    }
                    // Execution transcripts have a single writer: the passive
                    // watch (or debugger) stream, never current-stage guesses.
                    gremlins_magic_bus::EventKind::TokenUsage {
                        id,
                        usage,
                        cost_usd,
                        last_prompt_tokens,
                        last_cached_input_tokens,
                    } => {
                        app.on_token_usage(
                            id,
                            usage.prompt_tokens,
                            usage.completion_tokens,
                            usage.reasoning_tokens,
                            usage.cached_input_tokens,
                            usage.cache_creation_input_tokens,
                            usage.turns,
                            cost_usd,
                            last_prompt_tokens,
                            last_cached_input_tokens,
                        );
                    }
                    _ => {}
                }
            }

            // ── Raw lines (non-event JSON from the main socket) ─
            Some(raw) = raw_rx.recv() => {
                apply_home_identity(&mut app, &raw, client.session_generation().await);
                app.observe_live_pipeline_frame(&raw);
                if raw.get("type").and_then(|v| v.as_str()) == Some("event_lagged") {
                    // Resync lifecycle status from the daemon so terminal
                    // events (RunCompleted / RunFailed / RunStopped) that
                    // were dropped are not permanently stuck at "running".
                    status_generation = status_generation.wrapping_add(1);
                    fetch_run_statuses(&client, &status_tx, status_generation);
                } else if raw.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let msg = raw
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let full = format!("error: {msg}");
                    live_area.push_transcript_line(full);
                }
            }

            // ── Log follow lines (from dedicated connections) ──
            Some(raw) = log_rx.recv() => {
                match raw.get("type").and_then(|v| v.as_str()) {
                    Some("log_line") => {
                        if let Some(line) = raw.get("line").and_then(|v| v.as_str()) {
                            if app.following_log.is_some() {
                                live_area.push_transcript_line(line.to_string());
                            }
                        }
                    }
                    Some("view_log" | "view_error") => {
                        apply_watch_frame(
                            &mut app,
                            live_area,
                            &raw,
                            watch_follow_generation,
                            &mut watch_follow_handle,
                        );
                    }
                    Some("error") => {
                        let msg = raw
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        let full = format!("error: {msg}");
                        if app.following_log.is_some() {
                            live_area.push_transcript_line(full.clone());
                            app.following_log = None;
                        }
                    }
                    _ => {}
                }
            }

            // ── Session recreation completion ─────────────
            Some(result) = reset_rx.recv() => {
                app.session_recreating = false;
                match result {
                    Ok(()) => {
                        install_home_session(&mut app, &client).await;
                        // The old chat gremlin died with the old session; the
                        // daemon reports `model: null` until the next prompt.
                        app.chat_model = None;
                        let c = Arc::clone(&client);
                        let tx = model_tx.clone();
                        tokio::spawn(async move {
                            let session_generation = c.session_generation().await;
                            let result = query_chat_model(&c, session_generation).await;
                            let _ = tx.send(result);
                        });
                    }
                    Err(e) => {
                        live_area.push_transcript_line(format!("error: {e}"));
                    }
                }
            }

            // ── Daemon chat-model lookup results ──────────
            Some((query_generation, result)) = model_rx.recv() => {
                let current_generation = client.session_generation().await;
                if query_generation != current_generation {
                    log::debug!(
                        "dropping stale chat_model query for generation {query_generation} \
                         (current {current_generation})"
                    );
                    continue;
                }
                match result {
                    Ok(model) => app.chat_model = model,
                    Err(e) => log::debug!("chat_model query failed: {e}"),
                }
            }

            // ── Status resync after event_lagged / ShowRuns ─
            Some((runs, generation)) = status_rx.recv() => {
                // Drop stale responses — a newer fetch already superseded this one,
                // or a lifecycle event (RunStarted / RunRemoved) invalidated the
                // snapshot.
                if generation != status_generation {
                    continue;
                }
                // Prune entries that are no longer in the daemon's ls output
                // (gremlins rm'd from another terminal, etc.). Always compute
                // the ID difference — equal row counts do not guarantee
                // matching IDs. Collect first to avoid borrow conflicts with
                // the per-run loop below.
                let live_ids: std::collections::HashSet<&str> =
                    runs.iter().map(|r| r.id.as_str()).collect();
                let stale: Vec<String> = app
                    .active_runs
                    .keys()
                    .filter(|id| !live_ids.contains(id.as_str()))
                    .cloned()
                    .collect();
                for id in &stale {
                    if app.watch_following.as_deref() == Some(id.as_str()) {
                        close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                    }
                    app.on_run_removed(id);
                }
                for r in runs {
                    app.update_run_status(&r.id, &r.status);
                    // When a run became terminal during broadcast lag its
                    // last TokenUsage may be permanently skipped. The
                    // persisted cost and usage carried by the ls response
                    // repair the /watch columns.
                    if r.status == "done" || r.status == "failed" || r.status == "stopped" {
                        app.on_token_usage(
                            r.id,
                            r.prompt_tokens,
                            r.completion_tokens,
                            r.reasoning_tokens,
                            r.cached_input_tokens,
                            r.cache_creation_input_tokens,
                            r.turns.unwrap_or(0),
                            r.cost_usd,
                            r.last_prompt_tokens,
                            r.last_cached_input_tokens,
                        );
                    }
                }
            }

            Some(frame) = attach_rx.recv() => {
                apply_attachment_frame(&mut app, live_area, &mut attachment, frame);
            }

            Some((id, result)) = delivery_rx.recv() => {
                if let Some(receipt) = app.finish_delivery(id, result) {
                    live_area.show_receipt(receipt);
                }
            }

            // ── Socket-op results ─────────────────────────────
            Some(lines) = result_rx.recv() => {
                live_area.push_transcript_block(lines);
            }

            // ── Chat events ────────────────────────────────
            Some((event, turn, execution)) = chat_rx.recv() => {
                apply_home_event(&mut app, live_area, &mut interrupt, event, turn, execution)?;
            }

        }
    }

    // ── Exit ──────────────────────────────────────────────────────

    // The daemon owns the conversation; nothing to commit on exit.
    app.active_request = false;

    // Abort any lingering log follow on exit.
    if let Some(handle) = log_follow_handle.take() {
        handle.abort();
    }

    // Abort any lingering watch follow on exit.
    if let Some(handle) = watch_follow_handle.take() {
        handle.abort();
    }

    attachment.invalidate(&mut app);
    client.release_inputs();

    // Close the persistent socket connection.
    client.shutdown().await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Response formatting (mirrors the CLI output for each op)
// ---------------------------------------------------------------------------

fn format_socket_response(op: &str, resp: &serde_json::Value) -> Vec<String> {
    if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
        let msg = resp
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return vec![format!("error: {msg}")];
    }

    match op {
        "ls" => format_ls_response(resp),
        "info" => {
            vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
        }
        "stop" => {
            let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let status = resp.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let message = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if !message.is_empty() {
                vec![format!("gremlin {id}: {message}")]
            } else {
                vec![format!("gremlin {id} {status}")]
            }
        }
        "resume" => {
            let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("");
            vec![id.to_string()]
        }
        "artifact" => {
            if let Some(keys) = resp.get("keys").and_then(|v| v.as_array()) {
                if keys.is_empty() {
                    return vec!["(no artifacts)".to_string()];
                }
                keys.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            } else if let Some(content) = resp.get("content").and_then(|v| v.as_str()) {
                // Content may contain embedded newlines; split so each vector
                // element is one logical transcript line (preserving a trailing
                // empty line produced by `split`).
                content.split('\n').map(String::from).collect()
            } else {
                vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
            }
        }
        "model" => {
            match (
                resp.get("model").and_then(|v| v.as_str()),
                resp.get("source").and_then(|v| v.as_str()),
            ) {
                (Some(model), Some(source)) => {
                    vec![format!("chat model: {model} (source: {source})")]
                }
                _ => {
                    vec![resp
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("chat model: not configured")
                        .to_string()]
                }
            }
        }
        "launch" => match resp.get("id").and_then(|v| v.as_str()) {
            Some(id) => vec![id.to_string()],
            None => {
                vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
            }
        },
        _ => {
            vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
        }
    }
}

pub(crate) fn format_token_count(v: Option<u64>) -> String {
    match v {
        Some(n) => {
            let s = n.to_string();
            let mut result = String::new();
            for (i, c) in s.chars().rev().enumerate() {
                if i > 0 && i % 3 == 0 {
                    result.push(',');
                }
                result.push(c);
            }
            result.chars().rev().collect()
        }
        None => "-".to_string(),
    }
}

pub(crate) fn format_cache_pct(prompt: Option<u64>, cached: Option<u64>) -> String {
    match (prompt, cached) {
        (Some(p), Some(c)) if p > 0 => format!("{:.0}%", (c as f64 / p as f64) * 100.0),
        _ => "-".to_string(),
    }
}

pub(crate) fn format_cost_usd(v: Option<f64>) -> String {
    match v {
        Some(c) => format!("${c:.4}"),
        None => "-".to_string(),
    }
}

fn format_ls_response(resp: &serde_json::Value) -> Vec<String> {
    const LAUNCH_COL: usize = 5;
    const COL_GAP: usize = 2;
    /// The transcript widget reserves a one-cell left margin, so /ls rows
    /// have one fewer usable column than the full terminal width.
    const TRANSCRIPT_MARGIN: usize = 1;

    let gremlins = match resp.get("gremlins").and_then(|v| v.as_array()) {
        Some(g) => g,
        None => return vec!["(no gremlins)".to_string()],
    };

    let headers = [
        "ID",
        "STATUS",
        "STAGE",
        "DATE",
        "PROJECT",
        "LAUNCH",
        "TOKENS IN",
        "TOKENS OUT",
        "REASONING",
        "CACHE",
        "COST",
    ];
    let mut rows: Vec<Vec<String>> = Vec::new();

    for entry in gremlins {
        let prompt_tokens = entry.get("prompt_tokens").and_then(|v| v.as_u64());
        let completion_tokens = entry.get("completion_tokens").and_then(|v| v.as_u64());
        let reasoning_tokens = entry.get("reasoning_tokens").and_then(|v| v.as_u64());
        let cached_input_tokens = entry.get("cached_input_tokens").and_then(|v| v.as_u64());
        let cost_usd = entry.get("cost_usd").and_then(|v| v.as_f64());

        rows.push(vec![
            entry
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("stage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("started_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("project")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("launch")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            format_token_count(prompt_tokens),
            format_token_count(completion_tokens),
            format_token_count(reasoning_tokens),
            format_cache_pct(prompt_tokens, cached_input_tokens),
            format_cost_usd(cost_usd),
        ]);
    }

    rows.sort_by(|a, b| a[3].cmp(&b[3]));

    let mut widths = display::compute_display_widths(&headers, &rows);

    // When stdout is a terminal, fit the LAUNCH column within the terminal
    // width so the table never wraps. The transcript widget reserves a
    // one-cell left margin, so subtract it from the raw terminal width.
    let term_width = if std::io::stdout().is_terminal() {
        crossterm::terminal::size()
            .ok()
            .map(|(cols, _)| (cols as usize).saturating_sub(TRANSCRIPT_MARGIN))
    } else {
        None
    };
    display::clamp_launch_column(&mut widths, &mut rows, LAUNCH_COL, COL_GAP, term_width);

    let mut lines: Vec<String> = Vec::new();

    let header_line = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let w = widths.get(i).copied().unwrap_or(0);
            display::pad_to_display_width(h, w)
        })
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(header_line);

    for row in &rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or(0);
                display::pad_to_display_width(cell, w)
            })
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(line);
    }

    lines
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    // Draft addressing, retry eligibility, and receipt retirement are covered in
    // app/conversations.rs. These tests witness attachment-to-UI integration.
    fn correction_fixture() -> (
        App,
        LiveArea<ratatui::backend::TestBackend>,
        Attachment,
        Target,
    ) {
        let mut app = App::new();
        let mut live = LiveArea::with_backend(ratatui::backend::TestBackend::new(100, 40), 100, 40);
        let target = Target::StageExecution {
            run_id: "run".into(),
            stage: "review".into(),
        };
        live.select_conversation(&mut app, target.clone());
        app.conversations.set_execution(
            target.clone(),
            Some(protocol::Execution {
                run_id: "run".into(),
                stage_run_id: "execution".into(),
                nonce: 7,
            }),
        );
        let attachment = Attachment {
            target: Some(target.clone()),
            ..Default::default()
        };
        (app, live, attachment, target)
    }

    fn pending_correction(app: &mut App, target: &Target, text: &str) -> u64 {
        app.replace_input(text);
        let Submission::Correction { id, .. } = app.conversations.submit(target, &app.input) else {
            panic!("expected correction")
        };
        id
    }

    fn correction_frame(
        target: &Target,
        generation: u64,
        value: serde_json::Value,
    ) -> AttachmentFrame {
        AttachmentFrame {
            generation,
            target: target.clone(),
            execution: None,
            value,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn passive_sibling_frame(
        app: &mut App,
        live: &mut LiveArea<ratatui::backend::TestBackend>,
        attachment: &mut Attachment,
        target: &Target,
        invocation: &str,
        sequence: u64,
        kind: &str,
        text: &str,
    ) {
        apply_passive_watch_frame(
            app,
            live,
            attachment,
            &correction_frame(
                target,
                0,
                serde_json::json!({
                    "_passive_watch":true, "type":kind, "text":text,
                    "run_id":"run", "stage":"review", "stage_run_id":invocation,
                    "sequence":sequence
                }),
            ),
        );
    }

    #[test]
    fn aggregate_sibling_completion_and_reset_are_isolated() {
        for stage in ["", "review"] {
            let mut app = App::new();
            let mut live =
                LiveArea::with_backend(ratatui::backend::TestBackend::new(100, 40), 100, 40);
            let target = Target::StageExecution {
                run_id: "run".into(),
                stage: stage.into(),
            };
            live.select_conversation(&mut app, target.clone());
            let mut attachment = Attachment::default();
            passive_sibling_frame(
                &mut app,
                &mut live,
                &mut attachment,
                &target,
                "a",
                1,
                "stream_chunk",
                "alpha",
            );
            passive_sibling_frame(
                &mut app,
                &mut live,
                &mut attachment,
                &target,
                "b",
                2,
                "stream_chunk",
                "beta",
            );
            let rendered = rendered_attachment(&mut live, &app);
            assert_eq!(rendered.matches("alpha").count(), 1, "{rendered}");
            assert_eq!(rendered.matches("beta").count(), 1, "{rendered}");
            passive_sibling_frame(
                &mut app,
                &mut live,
                &mut attachment,
                &target,
                "a",
                3,
                "done",
                "alpha",
            );
            let rendered = rendered_attachment(&mut live, &app);
            assert_eq!(rendered.matches("alpha").count(), 1, "{rendered}");
            assert_eq!(rendered.matches("beta").count(), 1, "{rendered}");
            let b = Target::Invocation {
                run_id: "run".into(),
                stage: "review".into(),
                stage_run_id: "b".into(),
            };
            live.with_invocation_transcript(&target, &b, |transcript, _| {
                assert_eq!(transcript.response_accumulator(), "beta");
            });
            passive_sibling_frame(
                &mut app,
                &mut live,
                &mut attachment,
                &target,
                "a",
                4,
                "stream_reset",
                "",
            );
            live.with_invocation_transcript(&target, &b, |transcript, _| {
                assert_eq!(transcript.response_accumulator(), "beta");
            });
            passive_sibling_frame(
                &mut app,
                &mut live,
                &mut attachment,
                &target,
                "b",
                5,
                "done",
                "beta",
            );
            let rendered = rendered_attachment(&mut live, &app);
            assert_eq!(rendered.matches("beta").count(), 1, "{rendered}");
        }
    }

    #[test]
    fn exec_output_records_restore_newlines_including_empty_records() {
        let mut live = LiveArea::with_backend(ratatui::backend::TestBackend::new(100, 40), 100, 40);
        for (text, stderr) in [
            ("first", false),
            ("", false),
            ("second", false),
            ("failure", true),
            ("", true),
        ] {
            render_execution_frame(
                &mut live,
                &Target::Home,
                &serde_json::json!({
                    "type":"exec_output", "text":text, "is_stderr":stderr
                }),
            );
        }
        assert_eq!(
            live.response_text(),
            "first\n\nsecond\nstderr: failure\nstderr: \n"
        );
        let rendered = rendered_attachment(&mut live, &App::new());
        let rows: Vec<_> = rendered.lines().map(str::trim).collect();
        let first = rows.iter().position(|row| *row == "first").unwrap();
        assert_eq!(
            &rows[first..first + 5],
            &["first", "", "second", "stderr: failure", "stderr:"]
        );
    }

    #[test]
    fn failed_native_delivery_preserves_draft_and_renders_error() {
        let (mut app, mut live, _, target) = correction_fixture();
        let id = pending_correction(&mut app, &target, "keep this correction");
        let receipt = app
            .finish_delivery(id, Err("input disconnected".into()))
            .unwrap();
        live.show_receipt(receipt);
        assert_eq!(app.input.text(), "keep this correction");
        let rendered = rendered_attachment(&mut live, &app);
        assert!(rendered.contains("correction: keep this correction"));
        assert!(rendered.contains("input disconnected"));
        let retry = pending_correction(&mut app, &target, "keep this correction");
        assert_ne!(retry, id);
    }

    #[test]
    fn exact_agent_selection_pins_native_address_but_exec_does_not() {
        let mut app = App::new();
        let mut attachment = Attachment::default();
        let mut execution = app::LiveExecution {
            run_id: "run".into(),
            stage: "work".into(),
            stage_type: "agent".into(),
            stage_run_id: "exact".into(),
        };
        let target = execution.target();
        app.live_executions.insert("key".into(), execution.clone());
        pin_exact_execution(&mut app, &target, &mut attachment);
        assert_eq!(attachment.execution.as_ref().unwrap().stage_run_id, "exact");
        assert_eq!(
            app.conversation_status(&target),
            ConversationStatus::Running
        );
        app.set_conversation_status(target.clone(), ConversationStatus::Paused);
        pin_exact_execution(&mut app, &target, &mut attachment);
        assert_eq!(app.conversation_status(&target), ConversationStatus::Paused);
        assert_eq!(attachment.execution.as_ref().unwrap().stage_run_id, "exact");
        execution.stage_type = "exec".into();
        app.live_executions.insert("key".into(), execution);
        pin_exact_execution(&mut app, &target, &mut attachment);
        assert!(attachment.execution.is_none());
        assert_eq!(
            app.conversation_status(&target),
            ConversationStatus::ReadOnly
        );
    }

    #[test]
    fn control_receipt_survives_selection_generation_change() {
        let (mut app, mut live, mut attachment, target) = correction_fixture();
        attachment.invalidate(&mut app);
        apply_attachment_frame(
            &mut app,
            &mut live,
            &mut attachment,
            AttachmentFrame {
                generation: u64::MAX,
                target: target.clone(),
                execution: None,
                value: serde_json::json!({"_control_receipt":true,"command":"pause","error":"rejected"}),
            },
        );
        assert!(rendered_attachment(&mut live, &app).contains("pause failed: rejected"));
    }

    #[test]
    fn invalidating_pending_correction_preserves_receipt_and_draft() {
        let (mut app, mut live, mut attachment, target) = correction_fixture();
        let id = pending_correction(&mut app, &target, "original correction");
        live.push_response("partial response");
        attachment.invalidate(&mut app);
        // Selection release preserves pending delivery; its native receipt still settles.
        let receipt = app.finish_delivery(id, Ok(())).unwrap();
        live.show_receipt(receipt);
        assert_eq!(app.input.text(), "original correction");
        assert_eq!(live.response_text(), "partial response");
        let rendered = rendered_attachment(&mut live, &app);
        assert!(!rendered.contains("correction:"));
        assert!(app.finish_delivery(id, Ok(())).is_none());
    }

    #[test]
    fn repeated_correction_rejections_render_once() {
        let (mut app, mut live, _, target) = correction_fixture();
        let id = pending_correction(&mut app, &target, "unique correction");
        for _ in 0..2 {
            if let Some(receipt) = app.finish_delivery(id, Err("delivery rejected".into())) {
                live.show_receipt(receipt);
            }
        }
        assert_eq!(app.input.text(), "unique correction");
        app.clear_input();
        let rendered = rendered_attachment(&mut live, &app);
        assert_eq!(
            rendered.matches("delivery rejected").count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("unique correction").count(),
            1,
            "{rendered}"
        );
    }

    #[test]
    fn control_error_does_not_consume_pending_correction() {
        let (mut app, mut live, mut attachment, target) = correction_fixture();
        let id = pending_correction(&mut app, &target, "keep this correction");
        apply_attachment_frame(
            &mut app,
            &mut live,
            &mut attachment,
            correction_frame(
                &target,
                0,
                serde_json::json!({
                    "_control_receipt":true, "command":"continue", "error":"cannot continue"
                }),
            ),
        );
        assert_eq!(app.input.text(), "keep this correction");
        assert!(rendered_attachment(&mut live, &app).contains("continue failed: cannot continue"));
        let receipt = app.finish_delivery(id, Ok(())).unwrap();
        live.show_receipt(receipt);
        assert_eq!(app.input.text(), "keep this correction");
    }

    fn rendered_attachment(
        live: &mut LiveArea<ratatui::backend::TestBackend>,
        app: &App,
    ) -> String {
        live.compose(app).unwrap();
        let buffer = live.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn passive_frame(
        app: &mut App,
        live: &mut LiveArea<ratatui::backend::TestBackend>,
        attachment: &mut Attachment,
        value: serde_json::Value,
    ) {
        let mut value = value;
        value["_passive_watch"] = serde_json::json!(true);
        apply_attachment_frame(
            app,
            live,
            attachment,
            AttachmentFrame {
                generation: attachment.generation,
                target: attachment.target.clone().unwrap(),
                execution: None,
                value,
            },
        );
    }

    #[test]
    fn sequence_ranges_compact_sequential_delivery() {
        let mut seen = SequenceRanges::default();
        for sequence in 0..10_000 {
            assert!(seen.insert(sequence));
        }
        assert_eq!(seen.0, vec![(0, 9_999)]);
        for sequence in [0, 5_000, 9_999] {
            assert!(!seen.insert(sequence));
        }
        assert_eq!(seen.0, vec![(0, 9_999)]);
    }

    #[test]
    fn sequence_ranges_merge_reordered_delivery_without_hiding_gaps() {
        let mut seen = SequenceRanges::default();
        for sequence in [5, 1, 3] {
            assert!(seen.insert(sequence));
        }
        assert_eq!(seen.0, vec![(1, 1), (3, 3), (5, 5)]);
        assert!(seen.insert(2));
        assert_eq!(seen.0, vec![(1, 3), (5, 5)]);
        assert!(seen.insert(4));
        assert!(seen.insert(0));
        assert_eq!(seen.0, vec![(0, 5)]);
        for sequence in 0..=5 {
            assert!(!seen.insert(sequence));
        }
    }

    #[test]
    fn sequence_ranges_handle_max_u64_without_wrapping() {
        let mut seen = SequenceRanges::default();
        for sequence in [u64::MAX, 0, u64::MAX - 2, u64::MAX - 1] {
            assert!(seen.insert(sequence));
        }
        assert_eq!(seen.0, vec![(0, 0), (u64::MAX - 2, u64::MAX)]);
        assert!(!seen.insert(u64::MAX));
        assert!(!seen.insert(u64::MAX - 1));
        assert!(!seen.insert(0));
    }

    #[test]
    fn passive_reopen_suppresses_only_repeated_sequences() {
        let (mut app, mut live, mut attachment, target) = correction_fixture();
        let event = |sequence: Option<u64>| {
            serde_json::json!({
                "type":"exec_output", "run_id":"run", "stage":"review",
                "stage_run_id":"execution", "text":"echo-token ", "sequence":sequence
            })
        };
        passive_frame(&mut app, &mut live, &mut attachment, event(Some(1)));
        attachment.invalidate(&mut app);
        live.select_conversation(&mut app, Target::Home);
        live.select_conversation(&mut app, target.clone());
        attachment.target = Some(target);
        passive_frame(&mut app, &mut live, &mut attachment, event(Some(1)));
        assert_eq!(
            rendered_attachment(&mut live, &app)
                .matches("echo-token")
                .count(),
            1
        );
        passive_frame(&mut app, &mut live, &mut attachment, event(Some(2)));
        assert_eq!(
            rendered_attachment(&mut live, &app)
                .matches("echo-token")
                .count(),
            2
        );
        for _ in 0..2 {
            passive_frame(&mut app, &mut live, &mut attachment, event(None));
        }
        assert_eq!(
            rendered_attachment(&mut live, &app)
                .matches("echo-token")
                .count(),
            4
        );
    }

    #[test]
    fn passive_sequences_are_per_target_and_siblings_are_rejected() {
        let (mut app, mut live, mut attachment, selector) = correction_fixture();
        let event = serde_json::json!({
            "type":"exec_output", "run_id":"run", "stage":"review",
            "stage_run_id":"execution", "text":"routed-token ", "sequence":7
        });
        let identity = watch_frame_target(&event).unwrap();
        passive_frame(&mut app, &mut live, &mut attachment, event.clone());
        attachment.invalidate(&mut app);
        live.select_conversation(&mut app, identity.clone());
        attachment.target = Some(identity.clone());
        passive_frame(&mut app, &mut live, &mut attachment, event.clone());
        assert_eq!(
            rendered_attachment(&mut live, &app)
                .matches("routed-token")
                .count(),
            1
        );
        let mut sibling = event.clone();
        sibling["stage_run_id"] = serde_json::json!("sibling");
        sibling["sequence"] = serde_json::json!(8);
        passive_frame(&mut app, &mut live, &mut attachment, sibling);
        assert_eq!(
            rendered_attachment(&mut live, &app)
                .matches("routed-token")
                .count(),
            1
        );
        let run_selector = Target::StageExecution {
            run_id: "run".into(),
            stage: String::new(),
        };
        attachment.invalidate(&mut app);
        attachment.target = Some(run_selector.clone());
        live.select_conversation(&mut app, run_selector.clone());
        passive_frame(&mut app, &mut live, &mut attachment, event);
        assert_eq!(
            rendered_attachment(&mut live, &app)
                .matches("routed-token")
                .count(),
            1
        );
        for target in [selector, identity, run_selector] {
            assert_eq!(attachment.observed_sequences[&target].0, vec![(7, 7)]);
        }
    }

    #[test]
    fn passive_replay_rebuilds_lifecycle_even_for_seen_sequences() {
        let (mut app, mut live, mut attachment, target) = correction_fixture();
        let boundary = serde_json::json!({
            "type":"stage_boundary", "run_id":"run", "stage":"review",
            "stage_run_id":"execution", "stage_type":"exec", "active":true, "sequence":1
        });
        for _ in 0..2 {
            passive_frame(
                &mut app,
                &mut live,
                &mut attachment,
                serde_json::json!({"type":"watched", "id":"run", "stage":"review", "replay":true}),
            );
            passive_frame(&mut app, &mut live, &mut attachment, boundary.clone());
            passive_frame(
                &mut app,
                &mut live,
                &mut attachment,
                serde_json::json!({"type":"replay_checkpoint", "id":"run", "complete":true}),
            );
            assert_eq!(app.live_executions.len(), 1);
            attachment.invalidate(&mut app);
            attachment.target = Some(target.clone());
        }
        assert_eq!(
            rendered_attachment(&mut live, &app)
                .matches("stage review: started")
                .count(),
            1
        );
    }

    #[test]
    fn attachment_task_progress_renders_live_text_and_reasoning() {
        let mut app = App::new();
        let mut live = LiveArea::with_backend(ratatui::backend::TestBackend::new(100, 40), 100, 40);
        let target = Target::Invocation {
            run_id: "run".into(),
            stage: "review".into(),
            stage_run_id: "execution".into(),
        };
        live.select_conversation(&mut app, target.clone());
        let mut attachment = Attachment {
            target: Some(target.clone()),
            ..Default::default()
        };
        apply_attachment_frame(
            &mut app,
            &mut live,
            &mut attachment,
            AttachmentFrame {
                generation: 0,
                target,
                execution: None,
                value: serde_json::json!({
                    "_passive_watch": true,
                    "run_id": "run", "stage": "review", "stage_run_id": "execution",
                    "type": "task_progress",
                    "parent_id": "task-1",
                    "kind": "Scout",
                    "call": "Task(inspect)",
                    "reasoning": "checking attachment routing\n",
                    "text": "found progress handler gap\n",
                    "stats": null,
                    "totals": null
                }),
            },
        );
        let rendered = rendered_attachment(&mut live, &app);
        for text in [
            "Scout: Task(inspect)",
            "checking attachment routing",
            "found progress handler gap",
        ] {
            assert!(rendered.contains(text), "{text}: {rendered}");
        }
    }

    #[test]
    fn attachment_tools_and_summary_render_once_at_done() {
        for with_operator_message in [false, true] {
            let mut app = App::new();
            let mut live =
                LiveArea::with_backend(ratatui::backend::TestBackend::new(100, 40), 100, 40);
            let target = Target::Invocation {
                run_id: "run".into(),
                stage: "review".into(),
                stage_run_id: "execution".into(),
            };
            live.select_conversation(&mut app, target.clone());
            let mut attachment = Attachment {
                target: Some(target.clone()),
                ..Default::default()
            };
            for value in [
                serde_json::json!({"type":"operator_message", "text":"question"}),
                serde_json::json!({"type":"tool_output_chunk", "id":"call-1", "call":"Bash(echo hello)", "text":"tool tail"}),
                serde_json::json!({"type":"tool_result", "id":"call-1", "call":"Bash(echo hello)", "output":"not the title"}),
                serde_json::json!({"type":"stream_chunk", "text":"answer body"}),
                serde_json::json!({"type":"turn_complete", "text":"answer body", "tool_calls":["Bash(echo hello)"], "turn":1}),
            ] {
                if !with_operator_message && value["type"] == "operator_message" {
                    continue;
                }
                let mut value = value;
                value["run_id"] = serde_json::json!("run");
                value["stage"] = serde_json::json!("review");
                value["stage_run_id"] = serde_json::json!("execution");
                passive_frame(&mut app, &mut live, &mut attachment, value);
            }
            assert_eq!(live.target_has_current_turn(&target), with_operator_message);
            let before_done = rendered_attachment(&mut live, &app);
            assert_eq!(before_done.matches("Bash(echo hello)").count(), 1);
            assert_eq!(before_done.matches("tool tail").count(), 1);
            assert_eq!(live.response_text(), "answer body");
            let done = || AttachmentFrame {
                generation: 0,
                target: target.clone(),
                execution: None,
                value: serde_json::json!({"_passive_watch":true,
                    "run_id":"run", "stage":"review", "stage_run_id":"execution",
                    "sequence":10, "type":"done", "text":"answer body", "summary":"distinct summary"}),
            };
            apply_attachment_frame(&mut app, &mut live, &mut attachment, done());
            apply_attachment_frame(&mut app, &mut live, &mut attachment, done());
            assert!(!live.target_has_current_turn(&target));
            let rendered = rendered_attachment(&mut live, &app);
            for text in [
                "answer body",
                "distinct summary",
                "Bash(echo hello)",
                "tool tail",
            ] {
                assert_eq!(rendered.matches(text).count(), 1, "{text}: {rendered}");
            }
            assert!(!rendered.contains("not the title"));
        }
    }

    #[test]
    fn delivered_receipt_leaves_operator_event_authoritative() {
        let (mut app, mut live, mut attachment, target) = correction_fixture();
        let id = pending_correction(&mut app, &target, "unique correction");
        apply_attachment_frame(
            &mut app,
            &mut live,
            &mut attachment,
            correction_frame(
                &target,
                0,
                serde_json::json!({
                    "_passive_watch":true, "run_id":"run", "stage":"review",
                    "stage_run_id":"execution", "type":"operator_message", "text":"unique correction"
                }),
            ),
        );
        let receipt = app.finish_delivery(id, Ok(())).unwrap();
        live.show_receipt(receipt);
        assert_eq!(app.input.text(), "unique correction");
        app.clear_input();
        let rendered = rendered_attachment(&mut live, &app);
        assert_eq!(
            rendered.matches("unique correction").count(),
            1,
            "{rendered}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_chat_request, compact_tokens, ctrl_c_should_quit, format_cache_pct, format_cost_usd,
        format_ls_response, format_socket_response, format_token_count, interrupt_press,
        stats_title, usage_field, usage_title_part, watch_key_action, App, InterruptAction, Screen,
        WatchKeyAction,
    };
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn stats_title_formats_compact_token_cache_summary() {
        let stats = serde_json::json!({
            "turn": 3,
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "cached_input_tokens": 40,
            "cache_creation_input_tokens": 5,
            "reasoning_tokens": 30
        });

        let title = stats_title(2, Some(&stats), None);

        assert_eq!(
            title, "clanking · turn 2 · ↑100 · ↓50 · ∴30 · cache 40%",
            "{title}"
        );
    }

    #[test]
    fn stats_title_reports_compact_per_turn_prompt_tokens() {
        let stats = serde_json::json!({
            "turn": 2,
            "prompt_tokens": 77,
            "completion_tokens": 9,
            "cached_input_tokens": 0,
            "cache_creation_input_tokens": 3,
            "reasoning_tokens": 4
        });

        let title = stats_title(2, Some(&stats), None);

        // The `↑` figure is the per-turn prompt size (77).
        assert_eq!(
            title, "clanking · turn 2 · ↑77 · ↓9 · ∴4 · cache 0%",
            "{title}"
        );
    }

    #[test]
    fn stats_title_renders_dashes_when_usage_is_absent() {
        let title = stats_title(0, None, None);

        assert_eq!(title, "clanking · turn 0 · ↑- · ↓- · ∴- · cache -");
    }

    #[test]
    fn stats_title_renders_zero_prompt_with_cache_dash() {
        let stats = serde_json::json!({
            "turn": 1,
            "prompt_tokens": 0,
            "completion_tokens": 7,
            "cached_input_tokens": 0,
            "cache_creation_input_tokens": 2,
            "reasoning_tokens": 1
        });

        let title = stats_title(1, Some(&stats), None);

        assert_eq!(title, "clanking · turn 1 · ↑0 · ↓7 · ∴1 · cache -");
    }

    #[test]
    fn usage_title_part_compacts_thousands_and_reports_cache_rate() {
        let usage = serde_json::json!({
            "prompt_tokens": 46884,
            "cached_input_tokens": 46415,
            "completion_tokens": 500,
            "reasoning_tokens": 200
        });

        // No cumulative total: per-turn figures are used throughout.
        assert_eq!(
            usage_title_part(Some(&usage), None),
            "↑46.9k · ↓500 · ∴200 · cache 99%"
        );
    }

    #[test]
    fn usage_title_part_renders_cache_dash_when_cached_absent() {
        let usage = serde_json::json!({
            "prompt_tokens": 46884,
            "cache_creation_input_tokens": 80,
            "completion_tokens": 500,
            "reasoning_tokens": 200
        });

        assert_eq!(
            usage_title_part(Some(&usage), None),
            "↑46.9k · ↓500 · ∴200 · cache -"
        );
    }

    #[test]
    fn usage_title_part_prompt_only_renders_dashes_for_missing_stats() {
        let usage = serde_json::json!({"prompt_tokens": 46884});

        assert_eq!(
            usage_title_part(Some(&usage), None),
            "↑46.9k · ↓- · ∴- · cache -"
        );
    }

    #[test]
    fn usage_title_part_zero_prompt_omits_new_when_cache_creation_absent() {
        let usage = serde_json::json!({
            "prompt_tokens": 0,
            "cached_input_tokens": 0,
            "completion_tokens": 7,
            "reasoning_tokens": 1
        });

        assert_eq!(
            usage_title_part(Some(&usage), None),
            "↑0 · ↓7 · ∴1 · cache -"
        );
    }

    #[test]
    fn usage_title_part_zero_prompt_omits_cache_group_when_cached_absent() {
        let usage = serde_json::json!({
            "prompt_tokens": 0,
            "cache_creation_input_tokens": 2,
            "completion_tokens": 7,
            "reasoning_tokens": 1
        });

        assert_eq!(
            usage_title_part(Some(&usage), None),
            "↑0 · ↓7 · ∴1 · cache -"
        );
    }

    #[test]
    fn usage_title_part_prefers_cumulative_completion_and_reasoning() {
        // Per-turn stats show completion=50, reasoning=30.
        // Cumulative total shows completion=150, reasoning=90.
        // The title should report the larger cumulative figures for ↓/∴.
        let stats = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "cached_input_tokens": 40,
            "reasoning_tokens": 30
        });
        let total = serde_json::json!({
            "completion_tokens": 150,
            "reasoning_tokens": 90
        });

        assert_eq!(
            usage_title_part(Some(&stats), Some(&total)),
            "↑100 · ↓150 · ∴90 · cache 40%"
        );
    }

    #[test]
    fn usage_title_part_falls_back_to_per_turn_when_total_missing_fields() {
        // Total is present but only carries `completion_tokens`;
        // reasoning should fall back to the per-turn value.
        let stats = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "cached_input_tokens": 40,
            "reasoning_tokens": 30
        });
        let total = serde_json::json!({
            "completion_tokens": 150
        });

        assert_eq!(
            usage_title_part(Some(&stats), Some(&total)),
            "↑100 · ↓150 · ∴30 · cache 40%"
        );
    }

    #[test]
    fn usage_title_part_nil_total_uses_per_turn_figures() {
        // No cumulative total at all — should behave identically to the
        // pre-cumulative path (no regression).
        let stats = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "cached_input_tokens": 40,
            "reasoning_tokens": 30
        });

        assert_eq!(
            usage_title_part(Some(&stats), None),
            "↑100 · ↓50 · ∴30 · cache 40%"
        );
    }

    #[test]
    fn stats_title_with_cumulative_total_puts_label_first() {
        let stats = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "cached_input_tokens": 40,
            "reasoning_tokens": 30
        });
        let total = serde_json::json!({
            "completion_tokens": 150,
            "reasoning_tokens": 90
        });

        let title = stats_title(3, Some(&stats), Some(&total));
        assert_eq!(title, "clanking · turn 3 · ↑100 · ↓150 · ∴90 · cache 40%");
    }

    #[test]
    fn compact_tokens_renders_raw_thousands_and_millions() {
        assert_eq!(compact_tokens(Some(512)), "512");
        assert_eq!(compact_tokens(Some(999)), "999");
        assert_eq!(compact_tokens(Some(1_000)), "1.0k");
        assert_eq!(compact_tokens(Some(46_884)), "46.9k");
        assert_eq!(compact_tokens(Some(999_499)), "999.5k");
        assert_eq!(compact_tokens(Some(999_500)), "999.5k");
        assert_eq!(compact_tokens(Some(999_949)), "999.9k");
        assert_eq!(compact_tokens(Some(999_950)), "1.0M");
        assert_eq!(compact_tokens(Some(1_000_000)), "1.0M");
        assert_eq!(compact_tokens(Some(1_200_000)), "1.2M");
        assert_eq!(compact_tokens(Some(1_234_567)), "1.23M");
        assert_eq!(compact_tokens(None), "-");
    }

    #[test]
    fn usage_field_reads_optional_counter_values() {
        let usage = serde_json::json!({"prompt_tokens": 5});
        assert_eq!(usage_field(Some(&usage), "prompt_tokens"), Some(5));
        assert_eq!(usage_field(Some(&usage), "completion_tokens"), None);
        assert_eq!(usage_field(None, "prompt_tokens"), None);
    }

    #[test]
    fn format_artifact_content_splits_embedded_newlines() {
        let resp = serde_json::json!({ "id": "gr-1", "content": "line one\nline two\n" });
        let lines = format_socket_response("artifact", &resp);
        assert_eq!(lines, vec!["line one", "line two", ""]);
    }

    #[test]
    fn format_model_response_shows_resolved_model_and_source() {
        let resp = serde_json::json!({
            "type": "ok",
            "model": "cmd/echo",
            "source": "tui.json",
        });
        let lines = format_socket_response("model", &resp);
        assert_eq!(
            lines,
            vec!["chat model: cmd/echo (source: tui.json)".to_string()]
        );
    }

    #[test]
    fn format_model_response_renders_not_configured_message() {
        let resp = serde_json::json!({
            "type": "ok",
            "model": null,
            "source": null,
            "message": "chat model: not configured",
        });
        let lines = format_socket_response("model", &resp);
        assert_eq!(lines, vec!["chat model: not configured".to_string()]);
    }

    #[test]
    fn chat_request_carries_project_root() {
        let env = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);
        let req = build_chat_request("push this branch", "3", "/home/user/proj", "", &env, &[]);
        assert_eq!(req["op"], "chat");
        assert_eq!(req["text"], "push this branch");
        assert_eq!(req["turn"], "3");
        assert_eq!(req["project_root"], "/home/user/proj");
        assert_eq!(req["env"]["PATH"], "/usr/bin");
        assert!(req.get("id").is_none());
    }

    #[test]
    fn chat_request_does_not_carry_a_gremlin_id() {
        let env = HashMap::new();
        let req = build_chat_request("hi", "2", "/p", "", &env, &[]);
        assert!(req.get("id").is_none());
        assert_eq!(req["project_root"], "/p");
    }

    #[test]
    fn chat_request_keeps_context_separate_from_text() {
        let env = HashMap::new();
        let req = build_chat_request("hello", "1", "/p", "Current branch: main", &env, &[]);
        assert_eq!(req["text"], "hello");
        assert_eq!(req["context"], "Current branch: main");
    }

    #[test]
    fn chat_request_omits_empty_context() {
        let env = HashMap::new();
        let req = build_chat_request("hello", "1", "/p", "", &env, &[]);
        assert!(req.get("context").is_none());
        assert_eq!(req["text"], "hello");
    }

    #[test]
    fn chat_request_always_includes_skills_array() {
        let env = HashMap::new();
        // Skills present.
        let req = build_chat_request("hello", "1", "/p", "", &env, &["pdf".to_string()]);
        assert_eq!(req["skills"].as_array().unwrap().len(), 1);
        assert_eq!(req["skills"][0], "pdf");
        // Skills empty (so /clear resets the daemon).
        let req = build_chat_request("hello", "1", "/p", "", &env, &[]);
        assert_eq!(req["skills"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ctrl_c_first_press_arms_window_and_does_not_quit() {
        let mut pending = None;
        assert!(!ctrl_c_should_quit(&mut pending, Instant::now()));
        assert!(pending.is_some());
    }

    #[test]
    fn ctrl_c_second_press_within_window_quits() {
        let first = Instant::now();
        let mut pending = Some(first);
        assert!(ctrl_c_should_quit(
            &mut pending,
            first + Duration::from_millis(500)
        ));
    }

    #[test]
    fn ctrl_c_second_press_after_window_rearms_and_does_not_quit() {
        let mut pending = Some(Instant::now() - Duration::from_secs(2));
        assert!(!ctrl_c_should_quit(&mut pending, Instant::now()));
        assert!(pending.is_some());
    }

    // ── interrupt_press ──

    #[test]
    fn home_completion_while_viewing_a_stage_retires_only_home() {
        use super::{finish_home_request, LiveArea, Target};
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        let mut area = LiveArea::with_backend(TestBackend::new(80, 24), 80, 24);
        app.active_request = true;
        let stage = Target::StageExecution {
            run_id: "pipeline".into(),
            stage: "review".into(),
        };
        area.select_conversation(&mut app, stage.clone());
        assert!(!app.active_request);
        app.active_request = true;
        finish_home_request(&mut app, &mut area);
        assert!(app.active_request, "the visible stage is unaffected");
        area.select_conversation(&mut app, Target::Home);
        assert!(!app.active_request, "home must not restore a stale request");
        area.select_conversation(&mut app, stage);
        assert!(app.active_request);
    }

    #[test]
    fn switching_away_from_home_disarms_interrupt_escalation() {
        use super::{HomeInterrupt, Target};

        let mut interrupt = HomeInterrupt::default();
        let now = Instant::now();
        assert_eq!(interrupt.press(now), InterruptAction::SendSoft);
        interrupt.ended_turn = Some("old turn".into());
        interrupt.disarm_if_stale(
            &Target::StageExecution {
                run_id: "pipeline".into(),
                stage: "review".into(),
            },
            now,
        );
        assert!(interrupt.ended_turn.is_none());
        assert!(!interrupt.in_window(now));
        assert_eq!(interrupt.press(now), InterruptAction::SendSoft);
    }

    #[test]
    fn editor_allows_writable_correction_while_home_is_running() {
        use super::{app::Execution, selected_home_turn_active, Submission, Target};

        let mut app = App::new();
        app.conversations.begin_home_turn("home-turn".into());
        let target = Target::StageExecution {
            run_id: "pipeline".into(),
            stage: "review".into(),
        };
        let execution = Execution {
            run_id: "pipeline".into(),
            stage_run_id: "review-turn".into(),
            nonce: 1,
        };
        app.conversations
            .set_execution(target.clone(), Some(execution.clone()));
        app.select_composer(target.clone());
        app.replace_input("change course");
        for active in [false, true] {
            app.active_request = active;
            assert!(!selected_home_turn_active(&app));
        }
        assert!(matches!(
            app.conversations.submit(&target, &app.input),
            Submission::Correction { execution: addressed, .. } if addressed == execution
        ));
        assert_eq!(app.conversations.home_turn(), Some("home-turn"));
    }

    #[test]
    fn composer_and_interrupts_use_home_owner_not_display_state() {
        use super::selected_home_turn_active;

        let mut app = App::new();
        app.conversations.begin_home_turn("home-turn".into());
        for active in [false, true] {
            app.active_request = active;
            assert!(selected_home_turn_active(&app));
        }
        app.conversations.retire_home_turn();
        assert!(app.active_request);
        assert!(!selected_home_turn_active(&app));
    }

    #[test]
    fn hard_interrupt_retires_home_turn_without_dismissing_its_correction() {
        use super::{app::Execution, clear_active_turn_ui, LiveArea, Submission, Target};
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        let mut area = LiveArea::with_backend(TestBackend::new(80, 24), 80, 24);
        app.conversations.begin_home_turn("old".into());
        let execution = |turn: &str, nonce| Execution {
            run_id: "session".into(),
            stage_run_id: turn.into(),
            nonce,
        };
        app.active_request = true;
        app.conversations
            .set_execution(Target::Home, Some(execution("old", 1)));
        app.replace_input("change course");
        app.cursor_end();

        clear_active_turn_ui(&mut app, &mut area);
        assert!(app.conversations.home_turn().is_none());
        assert!(!app.active_request);
        assert_eq!(app.input.text(), "change course");
        assert_eq!(app.input_cursor, app.input.len());
        assert!(matches!(
            app.conversations.submit(&Target::Home, &app.input),
            Submission::HomePrompt
        ));

        app.conversations
            .set_execution(Target::Home, Some(execution("next", 2)));
        assert!(matches!(
            app.conversations.submit(&Target::Home, &app.input),
            Submission::Correction { .. }
        ));
        area.clear_target_input(&Target::Home, &mut app);
        assert!(app.input.is_empty());
    }

    #[test]
    fn interrupt_first_press_is_soft() {
        let mut count: u8 = 0;
        let mut window = None;
        assert_eq!(
            interrupt_press(&mut count, &mut window, Instant::now()),
            InterruptAction::SendSoft
        );
        assert_eq!(count, 1);
        assert!(window.is_some());
    }

    #[test]
    fn interrupt_second_press_within_window_is_hard() {
        let first = Instant::now();
        let mut count: u8 = 1;
        let mut window = Some(first);
        assert_eq!(
            interrupt_press(&mut count, &mut window, first + Duration::from_millis(500),),
            InterruptAction::SendHard
        );
        assert_eq!(count, 2);
    }

    #[test]
    fn interrupt_third_press_within_window_is_noop() {
        let first = Instant::now();
        let mut count: u8 = 2;
        let mut window = Some(first);
        assert_eq!(
            interrupt_press(&mut count, &mut window, first + Duration::from_millis(700),),
            InterruptAction::NoOp
        );
        assert_eq!(count, 3);
    }

    #[test]
    fn interrupt_after_window_expires_is_soft_again() {
        let mut count: u8 = 2;
        let mut window = Some(Instant::now() - Duration::from_secs(2));
        assert_eq!(
            interrupt_press(&mut count, &mut window, Instant::now()),
            InterruptAction::SendSoft
        );
        assert_eq!(count, 1);
        assert!(window.is_some());
    }

    // ── format_token_count ──

    #[test]
    fn format_token_count_renders_int_with_thousands_separators() {
        assert_eq!(format_token_count(Some(0)), "0");
        assert_eq!(format_token_count(Some(999)), "999");
        assert_eq!(format_token_count(Some(1_000)), "1,000");
        assert_eq!(format_token_count(Some(46_884)), "46,884");
        assert_eq!(format_token_count(Some(1_000_000)), "1,000,000");
    }

    #[test]
    fn format_token_count_renders_dash_for_none() {
        assert_eq!(format_token_count(None), "-");
    }

    // ── format_cache_pct ──

    #[test]
    fn format_cache_pct_renders_integer_percentage() {
        assert_eq!(format_cache_pct(Some(100), Some(40)), "40%");
        assert_eq!(format_cache_pct(Some(100), Some(100)), "100%");
        assert_eq!(format_cache_pct(Some(100), Some(0)), "0%");
    }

    #[test]
    fn format_cache_pct_dash_when_prompt_missing() {
        assert_eq!(format_cache_pct(None, Some(40)), "-");
    }

    #[test]
    fn format_cache_pct_dash_when_cached_missing() {
        assert_eq!(format_cache_pct(Some(100), None), "-");
    }

    #[test]
    fn format_cache_pct_dash_when_prompt_zero() {
        assert_eq!(format_cache_pct(Some(0), Some(40)), "-");
        assert_eq!(format_cache_pct(Some(0), Some(0)), "-");
    }

    // ── format_cost_usd ──

    #[test]
    fn format_cost_usd_renders_dollar_amount() {
        assert_eq!(format_cost_usd(Some(0.0)), "$0.0000");
        assert_eq!(format_cost_usd(Some(0.1234)), "$0.1234");
        assert_eq!(format_cost_usd(Some(1.5)), "$1.5000");
    }

    #[test]
    fn format_cost_usd_renders_dash_for_none() {
        assert_eq!(format_cost_usd(None), "-");
    }

    // ── format_ls_response ──

    #[test]
    fn format_ls_response_shows_token_cost_columns() {
        let resp = serde_json::json!({
            "gremlins": [{
                "id": "gr-1",
                "status": "running",
                "stage": "build",
                "started_at": "2025-01-01T00:00:00Z",
                "project": "/p",
                "launch": "gremlins run",
                "prompt_tokens": 1_000,
                "completion_tokens": 500,
                "reasoning_tokens": 200,
                "cached_input_tokens": 400,
                "cost_usd": 0.0123
            }]
        });
        let lines = format_ls_response(&resp);
        assert!(lines.len() >= 2, "expected at least header + one row");
        let header = &lines[0];
        assert!(
            header.contains("TOKENS IN"),
            "header missing TOKENS IN: {header}"
        );
        assert!(
            header.contains("TOKENS OUT"),
            "header missing TOKENS OUT: {header}"
        );
        assert!(
            header.contains("REASONING"),
            "header missing REASONING: {header}"
        );
        assert!(header.contains("CACHE"), "header missing CACHE: {header}");
        assert!(header.contains("COST"), "header missing COST: {header}");
        let row = &lines[1];
        assert!(row.contains("1,000"), "row missing prompt tokens: {row}");
        assert!(row.contains("500"), "row missing completion tokens: {row}");
        assert!(row.contains("40%"), "row missing cache pct: {row}");
        assert!(row.contains("$0.0123"), "row missing cost: {row}");
    }

    #[test]
    fn format_ls_response_renders_dashes_for_missing_token_fields() {
        let resp = serde_json::json!({
            "gremlins": [{
                "id": "gr-2",
                "status": "done",
                "stage": "",
                "started_at": "2025-01-01T00:00:00Z",
                "project": "/p",
                "launch": ""
            }]
        });
        let lines = format_ls_response(&resp);
        assert!(lines.len() >= 2);
        let row = &lines[1];
        // All five token/cost columns should render as `-` when absent.
        // We count five dashes appearing in positions after the LAUNCH column.
        let dash_count = row.matches('-').count();
        assert!(
            dash_count >= 5,
            "expected at least 5 dashes for absent token/cost fields, got {dash_count}: {row}"
        );
    }

    #[test]
    fn format_ls_response_handles_zero_prompt_tokens() {
        let resp = serde_json::json!({
            "gremlins": [{
                "id": "gr-3",
                "status": "running",
                "stage": "chat",
                "started_at": "2025-01-01T00:00:00Z",
                "project": "/p",
                "launch": "",
                "prompt_tokens": 0,
                "completion_tokens": 10,
                "cached_input_tokens": 0
            }]
        });
        let lines = format_ls_response(&resp);
        assert!(lines.len() >= 2);
        let row = &lines[1];
        // Cache should be `-` when prompt is zero.
        assert!(
            row.contains("-"),
            "cache should be dash when prompt is 0: {row}"
        );
        // prompt_tokens of 0 should render as "0"
        assert!(row.contains('0'), "prompt 0 should render as 0: {row}");
    }

    // ── watch_key_action ──

    #[test]
    fn watch_key_action_enters_runs_from_chat() {
        let app = App::new();
        assert_eq!(watch_key_action(&app), WatchKeyAction::EnterRuns);
    }

    #[test]
    fn watch_key_action_opens_selected_run_pane() {
        let mut app = App::new();
        app.screen = Screen::Runs;
        app.watch_selection = Some("gr-1".to_string());
        assert_eq!(
            watch_key_action(&app),
            WatchKeyAction::OpenPane("gr-1".to_string())
        );
    }

    #[test]
    fn watch_key_action_closes_open_pane_before_selection() {
        let mut app = App::new();
        app.screen = Screen::Runs;
        app.watch_selection = Some("gr-1".to_string());
        app.watch_following = Some("gr-1".to_string());
        assert_eq!(watch_key_action(&app), WatchKeyAction::ClosePane);
    }

    #[test]
    fn watch_key_action_exits_to_chat_when_runs_table_is_empty() {
        let mut app = App::new();
        app.screen = Screen::Runs;
        assert_eq!(watch_key_action(&app), WatchKeyAction::ExitToChat);
    }

    #[test]
    fn watch_key_action_is_inert_during_history_search() {
        let mut app = App::new();
        app.enter_search();
        assert_eq!(watch_key_action(&app), WatchKeyAction::NoOp);
    }
}
