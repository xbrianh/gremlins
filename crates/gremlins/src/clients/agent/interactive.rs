use crate::clients::cancel::CancelToken;
use crate::clients::conversation::Conversation;
use crate::clients::error::ClientError;
use crate::clients::execution_input::{InputCommand, InputControl, INPUT_CAPACITY};
use crate::clients::interactive::{DebugOutcome, InteractiveCommand, InteractiveSession};
use gremlins_magic_bus::{emit_corr_event, emit_corr_event_async, CorrId, EventKind};

/// Process execution input only at safe conversation boundaries. A step leaves
/// the persistent pause set, granting just the next turn. Sender lifetime has
/// no bearing on this state.
#[allow(clippy::too_many_arguments)]
pub(super) async fn inbox_boundary(
    cancel: &CancelToken,
    conv: &mut dyn Conversation,
    session: &mut InteractiveSession,
    turn: usize,
    message_prefix: &str,
    corr_id: &CorrId,
    mut step: bool,
) -> Result<bool, ClientError> {
    let mut announced = false;
    let mut processed = 0;
    loop {
        // Check lifecycle before even polling a queued command.
        if session.execution_ended() {
            return Err(ClientError::Cancelled);
        }
        let command = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ClientError::Cancelled),
            command = async {
                if processed < INPUT_CAPACITY {
                    session.try_recv_input().ok()
                } else {
                    None
                }
            } => command,
        };
        let command = match command {
            Some(command) => command,
            None if !session.inbox_paused() || step => return Ok(step),
            None if session.take_inbox_step() => return Ok(true),
            None => {
                if !announced {
                    // Display publication must not await backpressured readers.
                    emit_corr_event(corr_id, EventKind::Ready { turn });
                    announced = true;
                }
                if session.execution_ended() {
                    return Err(ClientError::Cancelled);
                }
                if session.is_attached() || session.accept_pause() {
                    match interactive_loop(cancel, conv, session, turn, message_prefix, corr_id)
                        .await?
                    {
                        InteractiveLoopResult::RunOneTurn
                        | InteractiveLoopResult::MessageDelivered => {
                            session.deferred_debug_turns += 1;
                        }
                        InteractiveLoopResult::Resumed | InteractiveLoopResult::InputPending => {}
                    }
                } else {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(ClientError::Cancelled),
                        _ = session.stream_interrupted() => {},
                    }
                }
                // A bounded pass lets debugger commands compete with refills.
                processed = 0;
                tokio::task::yield_now().await;
                continue;
            }
        };
        processed += 1;
        match command {
            InputCommand::Correction(correction) => {
                let mut display = None;
                correction.incorporate(|_, text| {
                    conv.inject_operator(text);
                    display = Some(text.to_string());
                });
                // The receipt has settled synchronously before publication.
                // Synchronous bus emission uses try_send, dropping full display
                // mailboxes rather than awaiting subscriber backpressure.
                if let Some(text) = display {
                    emit_corr_event(
                        corr_id,
                        EventKind::OperatorMessage {
                            id: message_prefix.to_string(),
                            text,
                        },
                    );
                }
            }
            InputCommand::Control(control) => {
                let mut bail = false;
                control.incorporate(|_, control| match control {
                    InputControl::Pause => {
                        session.set_inbox_paused(true);
                        step = false;
                        session.clear_inbox_steps();
                    }
                    InputControl::Step => {
                        session.set_inbox_paused(true);
                        session.grant_inbox_step();
                    }
                    InputControl::Resume => {
                        session.set_inbox_paused(false);
                        session.clear_inbox_steps();
                        step = false;
                    }
                    InputControl::Bail => bail = true,
                });
                if bail {
                    return Err(ClientError::Bail {
                        reason: "operator bailed".into(),
                    });
                }
            }
        }
    }
}

// ── interactive loop ─────────────────────────────────────────────────────

pub(super) enum InteractiveLoopResult {
    InputPending,
    /// Quit — interactive session ended, resume normal operation.
    Resumed,
    /// Run one turn, then re-enter interactive_loop (Inject or RunTurn).
    RunOneTurn,
    /// A correction was injected. Grant the next model turn even if the
    /// debugger lease is retained; one-shot message leases are released.
    MessageDelivered,
}

/// Only the live execution announces a withdrawn owner. Cleanup of a stale
/// session must not announce a disconnect against the successor's execution.
pub(super) async fn announce_live_disconnect(
    session: &InteractiveSession,
    _message_prefix: &str,
    corr_id: &CorrId,
) {
    if !session.execution_ended() {
        emit_corr_event_async(
            corr_id,
            EventKind::Ended {
                reason: "disconnect".into(),
            },
        )
        .await;
    }
}

/// Wait at an accepted pause or at the next boundary after a single step.
///
/// On entry, emits `Ready { turn }` through the bus. Then blocks
/// reading commands from this attachment's private receiver. `Inject` and
/// `RunTurn` return so the outer turn loop executes exactly one turn and then
/// re-enters this function.
pub(super) async fn interactive_loop(
    cancel: &CancelToken,
    conv: &mut dyn Conversation,
    session: &mut InteractiveSession,
    turn: usize,
    message_prefix: &str,
    corr_id: &CorrId,
) -> Result<InteractiveLoopResult, ClientError> {
    log::debug!("interactive_loop: broadcasting Ready (turn={turn})");
    let received = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ClientError::Cancelled),
        cmd = async {
            emit_corr_event_async(corr_id, EventKind::Ready { turn }).await;
            session.next_command().await
        } => cmd,
    };
    match received {
        Some(InteractiveCommand::InputPending) => Ok(InteractiveLoopResult::InputPending),
        Some(InteractiveCommand::Inject(text)) => {
            log::debug!(
                "interactive_loop: Inject command (len={}, turn={turn})",
                text.len()
            );
            conv.inject_operator(&text);
            Ok(InteractiveLoopResult::RunOneTurn)
        }
        Some(InteractiveCommand::Message(text, req_id, delivery)) => {
            log::debug!(
                "interactive_loop: Message command (len={}, req_id={req_id:?}, turn={turn})",
                text.len()
            );
            conv.inject_operator(&text);
            // Incorporation, not display publication, settles delivery.
            if let Some(tx) = delivery {
                let _ = tx.send(());
            }
            emit_corr_event_async(
                corr_id,
                EventKind::OperatorMessage {
                    id: message_prefix.to_string(),
                    text: text.clone(),
                },
            )
            .await;
            // Retained debugger leases stay attached for subsequent commands;
            // one-shot message leases are released so execution resumes.
            session.finish_message_delivery();
            Ok(InteractiveLoopResult::MessageDelivered)
        }
        Some(InteractiveCommand::RunTurn) => {
            log::debug!("interactive_loop: RunTurn command (turn={turn})");
            Ok(InteractiveLoopResult::RunOneTurn)
        }
        Some(InteractiveCommand::Bail(reason)) => {
            log::debug!("interactive_loop: Bail command (reason={reason:?}, turn={turn})");
            Err(ClientError::Bail {
                reason: format!("operator bailed: {reason}"),
            })
        }
        Some(InteractiveCommand::Quit) | None => {
            // A replaced execution has already lost its acquisition. Its
            // cleanup must not report a disconnect for the successor's owner.
            if received.is_none() && !session.has_accepted_acquisition() {
                // A live owner disconnect withdraws its lease without ending
                // the execution. Replacements end the execution itself and
                // must remain silent during stale-session cleanup.
                announce_live_disconnect(session, message_prefix, corr_id).await;
                session.discard_revoked_inbox();
                return Ok(InteractiveLoopResult::Resumed);
            }
            let reason = if received.is_some() {
                "resumed"
            } else {
                "disconnect"
            };
            emit_corr_event_async(
                corr_id,
                EventKind::Ended {
                    reason: reason.into(),
                },
            )
            .await;
            session.finish_attachment(DebugOutcome::Ended {
                reason: reason.into(),
            });
            Ok(InteractiveLoopResult::Resumed)
        }
    }
}
