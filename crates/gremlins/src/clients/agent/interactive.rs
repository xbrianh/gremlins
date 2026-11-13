use std::sync::Arc;

use rig_core::driver::DynModel;
use rig_core::operation::Completion;

use crate::clients::conversation::Conversation;
use crate::clients::error::ClientError;
use crate::clients::event::EventKind;
use crate::clients::interactive::{InteractiveCommand, InteractiveSession, PauseToken};

use super::agent_loop::{InteractiveBoundary, LoopState};
use super::plug::LoopHooks;

/// Future that resolves when a [`PauseToken`] fires, or never if there is no token.
pub(super) async fn maybe_pause(pause: &Option<Arc<PauseToken>>) {
    if let Some(ref p) = pause {
        log::debug!("maybe_pause: waiting on pause token");
        p.paused().await;
        log::debug!("maybe_pause: pause token resolved");
    } else {
        log::trace!("maybe_pause: no interactive session, pending forever");
        std::future::pending::<()>().await;
    }
}

// ── interactive loop ─────────────────────────────────────────────────────

pub(super) enum InteractiveLoopResult {
    /// Quit — interactive session ended, resume normal operation.
    Resumed,
    /// Run one turn, then re-enter interactive_loop.
    RunOneTurn,
}

/// Interactive state machine. Called when the operator triggers pause
/// (via [`PauseToken`]) or when re-entering after a `RunTurn`/`Inject`.
///
/// On entry, emits `Ready { turn }` through the stream-event hook. Then blocks
/// reading `InteractiveCommand`s from the session's `cmd_rx`. `Inject` and
/// `RunTurn` return so the outer turn loop executes exactly one turn and then
/// re-enters this function.
pub(super) async fn interactive_loop(
    _model: &DynModel<Completion>,
    conv: &mut dyn Conversation,
    session: &mut InteractiveSession,
    turn: usize,
    hooks: &LoopHooks,
) -> Result<InteractiveLoopResult, ClientError> {
    log::debug!("interactive_loop: broadcasting Ready (turn={turn})");
    if hooks.wants_stream_events {
        hooks.emit_stream_event(EventKind::Ready { turn }).await;
    }

    log::debug!("interactive_loop: waiting for command (turn={turn})");
    let cmd = match session.cmd_rx.recv().await {
        Some(cmd) => {
            log::debug!("interactive_loop: received command {cmd:?} (turn={turn})");
            cmd
        }
        None => {
            log::debug!("interactive_loop: cmd_rx closed, ending session (turn={turn})");
            if hooks.wants_stream_events {
                hooks
                    .emit_stream_event(EventKind::Ended {
                        reason: "disconnect".to_string(),
                    })
                    .await;
            }
            return Ok(InteractiveLoopResult::Resumed);
        }
    };

    match cmd {
        InteractiveCommand::Inject(text) => {
            log::debug!(
                "interactive_loop: Inject command (len={}, turn={turn})",
                text.len()
            );
            conv.inject_operator(&text);
            Ok(InteractiveLoopResult::RunOneTurn)
        }
        InteractiveCommand::RunTurn => {
            log::debug!("interactive_loop: RunTurn command (turn={turn})");
            Ok(InteractiveLoopResult::RunOneTurn)
        }
        InteractiveCommand::Bail(reason) => {
            log::debug!("interactive_loop: Bail command (reason={reason:?}, turn={turn})");
            if hooks.wants_stream_events {
                hooks
                    .emit_stream_event(EventKind::Ended {
                        reason: "bailed".to_string(),
                    })
                    .await;
            }
            Err(ClientError::Bail {
                reason: format!("operator bailed: {reason}"),
            })
        }
        InteractiveCommand::Quit => {
            log::debug!("interactive_loop: Quit command (turn={turn})");
            if hooks.wants_stream_events {
                hooks
                    .emit_stream_event(EventKind::Ended {
                        reason: "resumed".to_string(),
                    })
                    .await;
            }
            Ok(InteractiveLoopResult::Resumed)
        }
    }
}

/// Interactive turn boundary. Two modes: no active session (check the
/// pause token) or an active session re-entered after RunOneTurn.
pub(super) async fn await_interactive_turn(
    state: &mut LoopState,
    model: &DynModel<Completion>,
    prefix: &str,
) -> Result<InteractiveBoundary, ClientError> {
    if state.cmd_rx.is_some() && state.pause.is_some() {
        if state.pending_run_one_turn {
            state.pending_run_one_turn = false;
            state.interactive_active = true;
        } else if state.interactive_active {
            let mut session = InteractiveSession {
                cmd_rx: state.cmd_rx.take().unwrap(),
                pause: state.pause.clone().unwrap(),
            };
            match interactive_loop(
                model,
                &mut *state.conv,
                &mut session,
                state.turn_num,
                &state.hooks,
            )
            .await
            {
                Ok(InteractiveLoopResult::Resumed) => {
                    state.interactive_active = false;
                    state.system_prompt = state.original_system_prompt.clone();
                    state.cmd_rx = Some(session.cmd_rx);
                    return Ok(InteractiveBoundary::ContinueLoop);
                }
                Ok(InteractiveLoopResult::RunOneTurn) => {
                    state.cmd_rx = Some(session.cmd_rx);
                }
                Err(e) => {
                    state.hook_log(&format!(
                        "agent_loop: exit (operator bailed, label={})",
                        prefix
                    ));
                    state.hook_log(&format!("{prefix}exit: operator bailed"));
                    return Err(e);
                }
            }
        } else if state.pause.as_ref().is_some_and(|p| p.is_paused()) {
            state.hook_log(&format!(
                "agent_loop: pause token detected at turn boundary (label={})",
                prefix
            ));
            state.hook_log(&format!(
                "{prefix}pause: entering interactive mode (turn boundary)"
            ));
            if let Some(ref p) = state.pause {
                p.reset();
            }

            let debug_note = "\n\nThe operator has connected in debug mode. Messages prefixed with\n[operator]: are direct instructions from the operator. Treat them as\nauthoritative. When the operator disconnects, continue with your\noriginal task.";
            let amended_system = match &state.system_prompt {
                Some(sp) => format!("{sp}{debug_note}"),
                None => debug_note.trim_start().to_string(),
            };
            state.system_prompt = Some(amended_system);

            let mut session = InteractiveSession {
                cmd_rx: state.cmd_rx.take().unwrap(),
                pause: state.pause.clone().unwrap(),
            };
            match interactive_loop(
                model,
                &mut *state.conv,
                &mut session,
                state.turn_num,
                &state.hooks,
            )
            .await
            {
                Ok(InteractiveLoopResult::Resumed) => {
                    state.system_prompt = state.original_system_prompt.clone();
                    state.cmd_rx = Some(session.cmd_rx);
                    return Ok(InteractiveBoundary::ContinueLoop);
                }
                Ok(InteractiveLoopResult::RunOneTurn) => {
                    state.interactive_active = true;
                    state.cmd_rx = Some(session.cmd_rx);
                }
                Err(e) => {
                    state.hook_log(&format!(
                        "agent_loop: exit (operator bailed, label={})",
                        prefix
                    ));
                    state.hook_log(&format!("{prefix}exit: operator bailed"));
                    return Err(e);
                }
            }
        }
    }
    Ok(InteractiveBoundary::RunTurn)
}
