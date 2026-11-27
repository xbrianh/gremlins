use std::{io, time::Instant};

use super::{
    app::App, finish_session_chat_request, new_reasoning_band, protocol, screen::LiveArea,
    stats_title, SessionChatInterrupt, REASONING_COMPLETED_TITLE, REASONING_PLACEHOLDER_TITLE,
};

pub(super) fn apply_session_chat_identity<B: ratatui::backend::Backend>(
    app: &mut App,
    live_area: &mut LiveArea<B>,
    raw: &serde_json::Value,
    session_generation: u64,
) {
    if raw.get("type").and_then(|v| v.as_str()) != Some("session_chat_run_identity")
        || raw.get("generation").and_then(|v| v.as_u64()) != Some(session_generation)
    {
        return;
    }
    // The raw identity acknowledgment and terminal display use separate channels.
    // Prefer the current owner; only use the latest submission after it retires,
    // when display completion wins the race with the acknowledgment.
    let Some(active) = app
        .conversations
        .session_chat_turn()
        .or_else(|| app.conversations.latest_session_chat_turn())
        .filter(|turn| !turn.is_empty())
    else {
        return;
    };
    if raw.get("turn").and_then(|v| v.as_str()) != Some(active) {
        return;
    }
    if let Some(id) = raw
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
    {
        let old_target = app.session_chat_target();
        let new_target = super::app::Target::Run {
            run_id: id.to_owned(),
        };
        live_area.sync_app_state(app);
        live_area.migrate_target(&old_target, new_target.clone());
        app.migrate_chat_run_id(id.to_owned());
        if app.selected_target == new_target {
            live_area.copy_app_state_to(app);
        }
    }
}

pub(super) fn apply_session_chat_event<B: ratatui::backend::Backend>(
    app: &mut App,
    live_area: &mut LiveArea<B>,
    interrupt: &mut SessionChatInterrupt,
    event: gremlins_magic_bus::EventKind,
    turn: Option<String>,
    execution: Option<protocol::Execution>,
) -> io::Result<()> {
    let session_chat = app.session_chat_target();
    // Filter every event, including task progress, before touching the transcript.
    let expected = app.conversations.session_chat_turn();
    if !turn
        .as_deref()
        .is_some_and(|turn| !turn.is_empty() && Some(turn) == expected)
    {
        return Ok(());
    }

    // Execution identity must be installed before any rendering, not just on TurnStarted.
    if let Some(turn) = turn
        .as_deref()
        .filter(|turn| !turn.is_empty() && app.conversations.session_chat_turn() == Some(*turn))
    {
        if execution.is_some() {
            app.conversations
                .session_chat_turn_started(turn, execution.clone());
        }
    }

    if let gremlins_magic_bus::EventKind::TaskProgress {
        parent_id,
        chain,
        kind,
        call,
        reasoning,
        text,
        stats,
        totals,
        ..
    } = &event
    {
        log::debug!(
            "TaskProgress parent_id={parent_id} kind={kind} chain={chain} reasoning_len={} text_len={}",
            reasoning.len(), text.len()
        );
        let stats_json = stats
            .as_ref()
            .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null));
        let totals_json = totals
            .as_ref()
            .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null));
        live_area.with_target_transcript(&session_chat, |transcript, _| {
            transcript.push_task_progress(
                parent_id,
                kind,
                call,
                reasoning,
                text,
                stats_json.as_ref(),
                totals_json.as_ref(),
            );
        });
        return Ok(());
    }

    match event {
        gremlins_magic_bus::EventKind::TurnStarted { stage_run_id } => {
            log::debug!("chat turn started: stage_run_id={stage_run_id}");
        }
        gremlins_magic_bus::EventKind::StreamChunk { text, .. } => {
            log::debug!("StreamChunk len={}", text.len());
            live_area.with_target_transcript(&session_chat, |transcript, _| {
                transcript.push_response(&text);
            });
        }
        gremlins_magic_bus::EventKind::ReasoningChunk { text, .. } => {
            log::debug!("ReasoningChunk len={}", text.len());
            live_area.with_target_transcript(&session_chat, |_, reasoning| {
                if reasoning.title_is_empty() {
                    reasoning.set_title(REASONING_PLACEHOLDER_TITLE);
                }
                reasoning.push_text(&text);
            });
        }
        gremlins_magic_bus::EventKind::ToolOutputChunk {
            id,
            name,
            call,
            text,
            ..
        } => {
            log::debug!(
                "ToolOutputChunk id={id} name={name} call={call} len={}",
                text.len()
            );
            live_area.with_target_transcript(&session_chat, |transcript, _| {
                transcript.push_tool_output_chunk(&id, &call, &text);
            });
        }
        gremlins_magic_bus::EventKind::ToolResult {
            id,
            name: _,
            call,
            output,
            ..
        } => {
            log::debug!("ToolResult id={id} output_len={}", output.len());
            live_area.with_target_transcript(&session_chat, |transcript, _| {
                transcript.reconcile_tool_result(&id, &call, &output);
            });
        }
        gremlins_magic_bus::EventKind::TurnComplete {
            turn,
            text,
            tool_calls,
            stats,
            totals,
        } => {
            log::debug!(
                "TurnComplete text_len={} tool_calls={:?}",
                text.len(),
                tool_calls
            );
            live_area.with_target_transcript(&session_chat, |transcript, reasoning| {
                transcript.clear_tool_output();
                transcript.reconcile_response(&text);
                transcript.reconcile_tool_calls(&tool_calls);
                let stats_json = stats
                    .as_ref()
                    .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null));
                let totals_json = totals
                    .as_ref()
                    .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null));
                reasoning.set_title(&stats_title(
                    turn,
                    stats_json.as_ref(),
                    totals_json.as_ref(),
                ));
            });
        }
        gremlins_magic_bus::EventKind::Done { text, summary, .. } => {
            log::debug!(
                "Done text_len={} summary_len={} has_current_turn={}",
                text.len(),
                summary.len(),
                live_area.target_has_current_turn(&session_chat)
            );
            live_area.with_target_transcript(&session_chat, |transcript, reasoning| {
                if !transcript.has_current_turn() {
                    return;
                }
                transcript.reconcile_response(&text);
                if transcript.response_is_empty() && text.is_empty() {
                    transcript.push_response(&summary);
                }
                let new_title = reasoning
                    .title()
                    .map(|t| t.replacen(REASONING_PLACEHOLDER_TITLE, REASONING_COMPLETED_TITLE, 1));
                if let Some(new_title) = new_title {
                    reasoning.set_title(&new_title);
                }
                let stream = std::mem::replace(reasoning, new_reasoning_band());
                transcript.finish_turn();
                transcript.commit_reasoning_title(stream);
                transcript.clear_tool_output();
            });
            finish_session_chat_request(app, live_area);
            live_area.compose(app)?;
        }
        gremlins_magic_bus::EventKind::Ended { reason, .. } => {
            // Transcript absence is not owner absence: a submitted turn can end
            // before any transcript turn exists. The owner filter above is authoritative.
            let ended_turn = app.conversations.session_chat_turn().map(str::to_owned);
            finish_session_chat_request(app, live_area);
            let msg = format!("chat ended: {reason}");
            live_area.with_target_transcript(&session_chat, |transcript, reasoning| {
                let had_turn = transcript.has_current_turn();
                let stream = std::mem::replace(reasoning, new_reasoning_band());
                transcript.finish_turn();
                transcript.clear_tool_output();
                if had_turn {
                    transcript.commit_reasoning_title(stream);
                }
                transcript.push_transcript_line(msg);
            });
            // Preserve the interrupted turn for double-press hard cancellation.
            if reason == "interrupted" && interrupt.in_window(Instant::now()) {
                interrupt.ended_turn = ended_turn;
            } else {
                interrupt.ended_turn = None;
            }
            live_area.compose(app)?;
        }
        gremlins_magic_bus::EventKind::Error { message: msg, .. } => {
            // Submission can fail before execution creates a transcript turn.
            finish_session_chat_request(app, live_area);
            let full = format!("chat error: {msg}");
            live_area.with_target_transcript(&session_chat, |transcript, reasoning| {
                let stream = std::mem::replace(reasoning, new_reasoning_band());
                let had_turn = transcript.has_current_turn();
                transcript.finish_turn();
                if had_turn {
                    transcript.commit_reasoning_title(stream);
                }
                transcript.clear_tool_output();
                transcript.push_transcript_line(full);
            });
            live_area.compose(app)?;
        }
        gremlins_magic_bus::EventKind::SystemWarning { message: msg, .. } => {
            if !live_area.target_has_current_turn(&session_chat) {
                return Ok(());
            }
            let full = format!("⚠ {msg}");
            live_area.with_target_transcript(&session_chat, |transcript, _| {
                transcript.push_transcript_line(full);
            });
        }
        gremlins_magic_bus::EventKind::TaskProgress { .. } => unreachable!("handled above"),
        _ => {}
    }
    Ok(())
}
