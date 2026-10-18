//! Shared run-state scaffolding for native provider backends.
//!
//! [`NativeRunState`] holds the per-backend state common to every provider
//! that drives the agent loop through a [`GremlinsModel`]. Each backend plugs
//! its own client construction into [`NativeRunState::single_attempt`] by
//! passing the future that builds the per-attempt model.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{ClientError, RunParams};
use crate::clients::cancels::CancelsMap;
use crate::clients::interactive::InteractiveSession;
use crate::clients::model::GremlinsModel;
use crate::clients::params::ResolvedParams;
use crate::clients::protocol::CompletedRun;

/// Per-backend state shared by every native provider backend.
///
/// `C` is the backend-specific client state (for example an enum of a
/// statically-built client versus the ingredients to build one dynamically
/// per attempt).
pub(crate) struct NativeRunState<C> {
    pub(crate) client_state: C,
    pub(crate) model: String,
    pub(crate) tool_filter: Option<Vec<String>>,
    pub(crate) resolved: ResolvedParams,
    pub(crate) last_ctx: Mutex<Option<RunContext>>,
    pub(crate) cancels: CancelsMap,
    pub(crate) next_id: AtomicU64,
    pub(crate) log_label: String,
}

impl<C> NativeRunState<C> {
    /// The backend's `additional_params` object, or `None` when empty.
    pub(crate) fn extra_params(&self) -> Option<serde_json::Value> {
        self.resolved.additional_value()
    }

    /// The model to use for a run: the caller's override when non-empty,
    /// otherwise the backend's configured default model.
    pub(crate) fn effective_model(&self, override_model: Option<&str>) -> String {
        match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        }
    }

    /// Build a [`RunContext`] from [`RunParams`] and stash it in `last_ctx`
    /// so a later [`last_run_params`](Self::last_run_params) can resume it.
    pub(crate) fn prepare_context(&self, params: RunParams) -> RunContext {
        let ctx = RunContext::from_params(params);
        *self.last_ctx.lock().unwrap() = Some(ctx.clone());
        ctx
    }

    /// The [`RunParams`] from the most recent [`prepare_context`](Self::prepare_context)
    /// call, or an error when no run has happened yet.
    pub(crate) fn last_run_params(&self) -> Result<RunParams, ClientError> {
        let guard = self.last_ctx.lock().unwrap();
        let ctx = guard.as_ref().ok_or_else(|| ClientError::Runtime {
            message: "resume() called before run()".into(),
        })?;
        Ok(ctx.params.clone())
    }

    /// Cancel every in-flight token for `gremlin_id`.
    pub(crate) fn reap(&self, gremlin_id: &str) {
        self.cancels
            .reap_all(gremlin_id, &self.log_label, &self.model);
    }

    /// Execute a single agent-loop attempt, registering and cleaning up a
    /// cancel token for the given `gremlin_id`.
    ///
    /// `make_model` is awaited to obtain the [`GremlinsModel`] for this
    /// attempt; the caller is responsible for building it, including resolving
    /// the effective model name. Once the model is available the agent loop
    /// runs once with the backend's error classifier and request options.
    pub(crate) async fn single_attempt(
        &self,
        prompt: &str,
        ctx: RunContext,
        cancel: Arc<CancelToken>,
        interactive: Option<InteractiveSession>,
        make_model: impl std::future::Future<Output = Result<GremlinsModel, ClientError>>,
    ) -> Result<CompletedRun, ClientError> {
        let gremlin_id = ctx.params.gremlin_id.clone().unwrap_or_default();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.cancels.insert(&gremlin_id, id, cancel.clone())?;

        let result = async {
            let model = make_model.await?;
            run_agent_loop(
                model,
                prompt,
                ctx,
                cancel,
                LoopOpts {
                    extra: self.extra_params(),
                    tool_filter: self.tool_filter.as_deref(),
                    classify_error: Some(default_classify as ErrorClassifier),
                    temperature: self.resolved.temperature,
                    max_tokens: self.resolved.max_tokens,
                },
                interactive,
            )
            .await
        }
        .await;

        self.cancels.remove(&gremlin_id, id);
        result
    }
}
