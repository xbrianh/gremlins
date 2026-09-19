//! The executor runtime layer.
//!
//! [`state`] owns `state.json` I/O and [`state::StateData`]; [`gremlin`] owns
//! the [`gremlin::Gremlin`] handle — id validation, environment resolution,
//! and the `launch`/`open`/`fork` constructors the run loop builds on; [`run`]
//! owns the sequential run loop that walks a pipeline's stages and drives each
//! one through its guard, scope, and bail bookkeeping.

use thiserror::Error;

use crate::clients::backend::ClientError;
use crate::executor::state::StateError;

pub mod bootstrap;
pub mod gremlin;
pub mod parallel;
pub mod run;
pub mod state;

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
    /// The pipeline asked to bail; the reason is the operator-facing message.
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
    /// pipeline, or a state directory that is missing or malformed.
    #[error("{0}")]
    Message(String),

    /// A filesystem or process failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The model client failed.
    #[error(transparent)]
    Client(#[from] ClientError),

    /// `state.json` I/O failed.
    #[error(transparent)]
    State(#[from] StateError),
}
