use async_trait::async_trait;

use crate::clients::conversation::Conversation;
use crate::clients::tools::registry::ToolHandler;
use crate::clients::tools::runtime::{AgentLoopRuntime, ToolOutcome};

/// The Mischief tool — the chat-only DSL reference. Its body is bundled text,
/// so it has no sandbox, cap, or audit concerns beyond the shared gate.
pub(crate) struct MischiefTool;

#[async_trait]
impl ToolHandler for MischiefTool {
    fn chat_only(&self) -> bool {
        true
    }

    async fn invoke(
        &self,
        _id: &str,
        _ctx: &AgentLoopRuntime,
        _conv: &dyn Conversation,
        _args_json: &str,
    ) -> ToolOutcome {
        ToolOutcome::text(include_str!("../../mischief_dsl.md"))
    }
}
