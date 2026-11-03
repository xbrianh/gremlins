use async_trait::async_trait;

use crate::clients::conversation::Conversation;
use crate::clients::tools::{AgentLoopRuntime, ToolHandler, ToolOutcome};

/// The Mischief tool — the chat-only DSL reference. Its body is bundled text,
/// so it has no sandbox, cap, or audit concerns beyond the shared gate.
pub(crate) struct MischiefTool;

#[async_trait]
impl ToolHandler for MischiefTool {
    async fn invoke(
        &self,
        _id: &str,
        _ctx: &AgentLoopRuntime,
        _conv: &dyn Conversation,
        _args_json: &str,
    ) -> ToolOutcome {
        ToolOutcome::text(include_str!("../../clients/mischief_dsl.md"))
    }
}
