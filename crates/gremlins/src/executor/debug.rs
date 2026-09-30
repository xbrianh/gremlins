//! Interactive operator prompt for agent stages.
//!
//! Types shared between the supervisor and agent loop for the `Op::Debug`
//! streaming socket operation.

use serde::{Deserialize, Serialize};

/// Commands the supervisor sends to the agent loop over an mpsc channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DebugCommand {
    /// Pause at the next turn boundary and enter debug mode.
    Pause,
    /// Inject an operator message and run one turn.
    Talk(String),
    /// Run one turn then re-pause.
    Continue,
    /// Terminate the run with a bail reason.
    Bail(String),
    /// Exit debug mode and resume normal operation.
    Quit,
}

/// Events the agent loop broadcasts back to the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DebugEvent {
    /// Agent has entered debug mode and is ready for commands.
    Ready,
    /// A Talk-initiated turn completed; agent is waiting for the next command.
    TurnComplete,
    /// Agent re-paused after a Continue-initiated turn.
    Paused,
    /// Debug session ended.
    Ended {
        /// How the session ended: "resumed" or "bailed".
        reason: String,
    },
}

/// Returned by [`debug_loop`] to the outer turn loop.
pub enum DebugResult {
    /// Quit — debug session ended, resume normal operation.
    Resumed,
    /// Continue or Talk — run this turn then re-pause.
    RunOneTurn,
    /// Bail — terminate the run with this reason.
    Bailed(String),
}
