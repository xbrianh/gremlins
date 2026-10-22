//! The executor runtime layer.
//!
//! [`state`] owns `state.json` I/O and [`state::StateData`]; [`gremlin`] owns
//! the [`gremlin::Gremlin`] handle — id validation, environment resolution,
//! and the `launch`/`open`/`fork` constructors the run loop builds on; [`run`]
//! owns the sequential run loop that walks a definition's stages and drives each
//! one through its guard, scope, and bail bookkeeping.

use serde_json::{Map, Value};
use thiserror::Error;

use rig_core::completion::Message;

use crate::clients::backend::ClientError;
use crate::executor::state::StateError;

/// Send a line through the per-gremlin log channel when one is configured.
pub(crate) fn send_log(
    tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
    msg: impl AsRef<str>,
) {
    if let Some(tx) = tx {
        let _ = tx.send(msg.as_ref().to_string());
    }
}

/// Read a string field from a JSON object, defaulting to `""`.
pub(crate) fn str_field(map: &Map<String, Value>, field: &str) -> String {
    map.get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Read a field or return `Value::Null` when it is absent or not a string.
pub(crate) fn null_field(tree: &Map<String, Value>, key: &str) -> Value {
    tree.get(key).cloned().unwrap_or(Value::Null)
}

/// Why a gremlin run failed.
///
/// The variants are deliberately coarse: the run loop (M2/M3) reports a
/// stage-level failure as [`StageFailed`](RunError::StageFailed), a bootstrap
/// script that exits non-zero as [`BootstrapFailed`](RunError::BootstrapFailed),
/// and a requested bail as [`Bail`](RunError::Bail) — the three outcomes the
/// Python executor distinguished. The remaining variants carry through the
/// lower layers' errors unchanged.
#[derive(Debug, Error)]
pub enum RunError {
    /// The definition asked to bail; the reason is the operator-facing message.
    #[error("{reason}")]
    Bail { reason: String },

    /// A bootstrap command exited non-zero.
    #[error("bootstrap failed (exit {exit_code}): {stderr}")]
    BootstrapFailed { exit_code: i32, stderr: String },

    /// A stage failed, named so the operator knows where.
    #[error("stage {stage}: {message}")]
    StageFailed { stage: String, message: String },

    /// A git operation failed.
    #[error("{message}")]
    Git { message: String },

    /// A failure with no more specific home: a bad gremlin id, an unloadable
    /// definition, or a state directory that is missing or malformed.
    #[error("{0}")]
    Message(String),

    /// A filesystem or process failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A model client failed.
    #[error(transparent)]
    Client(#[from] ClientError),

    /// The operator cancelled the agent loop. Carries the loop's accumulated
    /// message history when the backend captured it before unwinding; chat
    /// soft-interrupt recovery uses this to reconstruct the partial turn.
    #[error("{message}")]
    Cancelled {
        message: String,
        messages: Option<Vec<Message>>,
    },

    /// `state.json` I/O failed.
    #[error(transparent)]
    State(#[from] StateError),
}

pub mod agent_runner;
pub mod bootstrap;
pub mod conversation;
pub mod exec_runner;
pub mod gremlin;
pub mod parallel;
pub mod run;
pub mod socket;
pub mod state;
pub mod supervisor;
pub mod vars;

pub use supervisor::DaemonEvent;
