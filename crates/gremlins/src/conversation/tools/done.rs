use async_trait::async_trait;

use crate::clients::conversation::Conversation;
use crate::clients::tools::{parse_args, AgentLoopRuntime, ToolHandler, ToolOutcome};

/// The Done tool — the completion signal. Behavior is trivial (echo the
/// summary); the agent loop treats Done as the completion protocol (solo-only
/// validation, artifact check) via its `completion` policy, not by naming this
/// tool during normal dispatch.
pub(crate) struct DoneTool;

#[async_trait]
impl ToolHandler for DoneTool {
    fn completion(&self) -> bool {
        true
    }

    fn title_detail(&self, args: &serde_json::Value) -> Option<String> {
        args.get("summary")
            .and_then(|v| v.as_str())
            .map(|s| format!("summary={:?}", crate::clients::format::trunc(s, 120)))
    }

    async fn invoke(
        &self,
        _id: &str,
        _ctx: &AgentLoopRuntime,
        _conv: &dyn Conversation,
        args_json: &str,
    ) -> ToolOutcome {
        let args = match parse_args(args_json) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::text(e),
        };
        let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
        ToolOutcome::text(format!("Done. {summary}"))
    }
}
