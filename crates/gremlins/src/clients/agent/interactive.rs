use crate::clients::cancel::CancelToken;
use crate::clients::conversation::Conversation;
use crate::clients::error::ClientError;
use crate::clients::execution_input::{InputCommand, InputControl, INPUT_CAPACITY};
use crate::clients::interactive::InteractiveSession;
use gremlins_magic_bus::{emit_corr_event, CorrId, EventKind};

/// Process execution input only at safe conversation boundaries. A step leaves
/// the persistent pause set, granting just the next turn. Sender lifetime has
/// no bearing on this state.
pub(super) async fn inbox_boundary(
    cancel: &CancelToken,
    conv: &mut dyn Conversation,
    session: &mut InteractiveSession,
    message_prefix: &str,
    corr_id: &CorrId,
    mut step: bool,
) -> Result<bool, ClientError> {
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
                if session.execution_ended() {
                    return Err(ClientError::Cancelled);
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ClientError::Cancelled),
                    _ = session.stream_interrupted() => {},
                }
                // Bound each pass even when senders continuously refill.
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
