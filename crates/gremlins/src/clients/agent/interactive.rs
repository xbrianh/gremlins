use crate::clients::cancel::CancelToken;
use crate::clients::conversation::Conversation;
use crate::clients::error::ClientError;
use crate::clients::interactive::{DebugOutcome, InteractiveCommand, InteractiveSession};
use gremlins_magic_bus::{emit_run_event_async, EventKind};

// ── interactive loop ─────────────────────────────────────────────────────

pub(super) enum InteractiveLoopResult {
    /// Quit — interactive session ended, resume normal operation.
    Resumed,
    /// Run one turn, then re-enter interactive_loop (Inject or RunTurn).
    RunOneTurn,
    /// A correction was injected and the pause slot released.
    /// The agent loop continues without re-entering interactive mode;
    /// the `pending_run_one_turn` flag is not set.
    MessageDelivered,
}

/// Only the live execution announces a withdrawn owner. Cleanup of a stale
/// session must not announce a disconnect against the successor's execution.
pub(super) async fn announce_live_disconnect(session: &InteractiveSession, message_prefix: &str) {
    if !session.execution_ended() {
        emit_run_event_async(
            message_prefix,
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
) -> Result<InteractiveLoopResult, ClientError> {
    log::debug!("interactive_loop: broadcasting Ready (turn={turn})");
    let received = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ClientError::Cancelled),
        cmd = async {
            emit_run_event_async(message_prefix, EventKind::Ready { turn }).await;
            session.next_command().await
        } => cmd,
    };
    match received {
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
            emit_run_event_async(
                message_prefix,
                EventKind::OperatorMessage {
                    id: message_prefix.to_string(),
                    text: text.clone(),
                },
            )
            .await;
            // Fire the delivery confirmation *after* conv.inject_operator
            // succeeded so the owner learns about incorporation — not just
            // enqueue or display echo.
            if let Some(tx) = delivery {
                let _ = tx.send(());
            }
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
            emit_run_event_async(
                message_prefix,
                EventKind::Ended {
                    reason: "bailed".to_string(),
                },
            )
            .await;
            session.finish_attachment(DebugOutcome::Ended {
                reason: "bailed".into(),
            });
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
                announce_live_disconnect(session, message_prefix).await;
                session.discard_revoked_inbox();
                return Ok(InteractiveLoopResult::Resumed);
            }
            let reason = if received.is_some() {
                "resumed"
            } else {
                "disconnect"
            };
            emit_run_event_async(
                message_prefix,
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
