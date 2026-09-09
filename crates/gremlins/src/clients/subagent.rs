use std::path::PathBuf;
use std::sync::Arc;

use rig_core::completion::CompletionModel;

use super::stream;
use super::tools::{self, ToolContext};

const MAX_DEPTH: u32 = 3;

/// Build a subagent runner closure. Called once per backend before the agent loop.
/// The returned closure captures the model, tool filter, cancel token,
/// context prefix, and the original `ToolContext` — everything needed to run
/// a nested agent loop.
///
/// Recursive subagents are supported and bounded by `MAX_DEPTH`. Depth is a
/// true per-call-chain recursion bound, not a concurrency cap: each invocation
/// injects a child runner at `depth + 1` into the sub-context, so N sibling
/// subagents launched from one parent all share the same depth and never
/// exhaust the bound between them. Only genuine nesting increments depth.
pub(crate) fn make_runner<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: M,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    idle_timeout: f64,
    max_turns: usize,
) -> tools::SubagentFn {
    make_runner_at_depth(
        model,
        tool_filter,
        cancel,
        ctx,
        prefix,
        idle_timeout,
        max_turns,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn make_runner_at_depth<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: M,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    idle_timeout: f64,
    max_turns: usize,
    depth: u32,
) -> tools::SubagentFn {
    Arc::new(move |task: String, cwd: Option<PathBuf>| {
        let model = model.clone();
        let tool_filter = tool_filter.clone();
        let cancel = cancel.clone();
        let mut sub_ctx = ctx.clone();
        let prefix = prefix.clone();

        if let Some(cwd) = cwd {
            sub_ctx.cwd = Some(cwd);
        }

        Box::pin(async move {
            if depth >= MAX_DEPTH {
                return format!("Error: subagent max depth ({MAX_DEPTH}) exceeded");
            }

            let sub_prefix = format!("{}[sub] ", prefix);
            eprintln!(
                "{} {}subagent: begin (max_turns={})",
                stream::ts_internal(),
                sub_prefix,
                max_turns
            );

            // Inject a child runner one level deeper so a nested subagent can
            // recurse again, bounded by MAX_DEPTH along this call chain.
            sub_ctx.subagent_fn = Some(make_runner_at_depth(
                model.clone(),
                tool_filter.clone(),
                cancel.clone(),
                sub_ctx.clone(),
                prefix.clone(),
                idle_timeout,
                max_turns,
                depth + 1,
            ));

            let result = crate::clients::agent_loop::run_agent_loop_nested(
                &model,
                &task,
                &sub_ctx,
                &cancel,
                tool_filter.as_deref(),
                &sub_prefix,
                idle_timeout,
                max_turns,
            )
            .await;

            eprintln!("{} {}subagent: end", stream::ts_internal(), sub_prefix,);

            match result {
                Ok(completed) => completed.text_result.unwrap_or_default(),
                Err(e) => format!("Subagent error: {e}"),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn make_runner_invokes_and_sets_subagent_fn() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-sub-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let ctx = ToolContext {
            cwd: Some(dir.clone()),
            extra_env: None,
            allowed_roots: vec![dir.clone()],
            audit_log: None,
            allowed_tools: None,
            subagent_fn: None,
            audit_lock: None,
        };

        // Model that returns a single text response in one turn.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("nested reply"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_runner(model, None, cancel, ctx, String::new(), 5.0, 10);

        // First invocation: depth 0 < 3, should succeed.
        let output = runner("first call".into(), None).await;
        assert!(
            !output.contains("max depth"),
            "depth 0 should not hit guard, got: {output}"
        );
    }

    /// A model whose stream never resolves — used to keep subagent calls
    /// in-flight so concurrent siblings overlap in time.
    #[derive(Clone)]
    struct PendingModel;

    impl rig_core::completion::CompletionModel for PendingModel {
        type Response = rig_core::test_utils::MockResponse;
        type StreamingResponse = rig_core::test_utils::MockResponse;
        type Client = ();

        fn make(_: &Self::Client, _: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _: rig_core::completion::CompletionRequest,
        ) -> Result<
            rig_core::completion::CompletionResponse<Self::Response>,
            rig_core::completion::CompletionError,
        > {
            Err(rig_core::completion::CompletionError::ProviderError(
                "unused".into(),
            ))
        }

        async fn stream(
            &self,
            _: rig_core::completion::CompletionRequest,
        ) -> Result<
            rig_core::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
            rig_core::completion::CompletionError,
        > {
            let s: rig_core::streaming::StreamingResult<Self::StreamingResponse> =
                Box::pin(futures::stream::pending());
            Ok(rig_core::streaming::StreamingCompletionResponse::stream(s))
        }
    }

    fn depth_test_ctx() -> ToolContext {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-sub-depth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        ToolContext {
            cwd: Some(dir.clone()),
            extra_env: None,
            allowed_roots: vec![dir],
            audit_log: None,
            allowed_tools: None,
            subagent_fn: None,
            audit_lock: None,
        }
    }

    /// Concurrent siblings share one depth level: N calls from the same parent
    /// must not exhaust the recursion bound between them.
    #[tokio::test]
    async fn make_runner_concurrent_siblings_do_not_exhaust_depth() {
        let ctx = depth_test_ctx();
        // Hangs forever so all siblings overlap in time.
        let model = PendingModel;
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_runner(model, None, cancel, ctx, String::new(), 0.2, 10);

        // Ten concurrent siblings at depth 0 — none should be rejected as
        // "max depth" even though they overlap in time.
        let handles: Vec<_> = (0..10)
            .map(|i| tokio::spawn(runner.clone()(format!("call {i}"), None)))
            .collect();
        for h in handles {
            let out = h.await.unwrap();
            assert!(
                !out.contains("max depth"),
                "sibling at depth 0 must not hit the recursion guard, got: {out}"
            );
        }
    }

    /// The recursion bound is enforced per call chain: a runner already at
    /// MAX_DEPTH rejects, standing in for a chain nested that many levels deep.
    #[tokio::test]
    async fn make_runner_rejects_at_max_depth() {
        let ctx = depth_test_ctx();
        let model = PendingModel;
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner =
            make_runner_at_depth(model, None, cancel, ctx, String::new(), 0.2, 10, MAX_DEPTH);

        let blocked = runner("too deep".into(), None).await;
        assert!(
            blocked.contains("max depth (3) exceeded"),
            "call at MAX_DEPTH should be rejected, got: {blocked}"
        );
    }
}
