//! Native, in-process assessment of an agent's proposed `Done`.
//!
//! A [`DoneCheck`] is a reusable async Rust function attached to an agent
//! stage by [`AgentBuilder::done_check`](crate::builders::AgentBuilder::done_check).
//! It lives only in the typed stage tree and is never serialized.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::clients::CancelToken;

/// A required output the stage must have written before `Done` is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneOutput {
    /// The output key as declared by the stage (without a `?` suffix).
    pub key: String,
    /// The resolved artifact URI.
    pub uri: String,
    /// The local path the agent writes this output to.
    pub path: PathBuf,
}

/// Everything a [`DoneCheck`] may observe about one proposed `Done`.
#[derive(Debug, Clone)]
pub struct DoneContext {
    /// The summary the agent passed to `Done`.
    pub summary: String,
    /// The stage's working directory.
    pub cwd: PathBuf,
    /// The stage's required outputs (optional outputs are omitted).
    pub outputs: Vec<DoneOutput>,
    /// Fires when the run is cancelled; cooperative checks should return
    /// promptly once it does.
    pub cancel: Arc<CancelToken>,
}

/// The verdict of a [`DoneCheck`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneVerdict {
    /// The condition is met; `Done` completes.
    Accept,
    /// The condition is unmet; the diagnostic is returned to the model.
    Retry(String),
    /// The check itself failed; the run fails.
    Error(String),
}

type CheckFn = dyn Fn(DoneContext) -> BoxFuture<'static, DoneVerdict> + Send + Sync;

/// A cloneable handle to a native async done-check.
#[derive(Clone)]
pub struct DoneCheck(Arc<CheckFn>);

impl DoneCheck {
    /// Wrap a reusable async function.
    pub fn new<F, Fut>(check: F) -> Self
    where
        F: Fn(DoneContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DoneVerdict> + Send + 'static,
    {
        DoneCheck(Arc::new(move |ctx| Box::pin(check(ctx))))
    }

    pub(crate) fn call(&self, ctx: DoneContext) -> BoxFuture<'static, DoneVerdict> {
        (self.0)(ctx)
    }
}

impl fmt::Debug for DoneCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DoneCheck(..)")
    }
}

impl PartialEq for DoneCheck {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
