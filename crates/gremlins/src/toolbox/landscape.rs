use async_trait::async_trait;

use crate::clients::tools::{AgentLoopRuntime, ToolHandler};

pub(crate) struct LandscapeTool;

#[async_trait]
impl ToolHandler for LandscapeTool {
    async fn invoke(&self, _id: &str, _ctx: &AgentLoopRuntime, _args_json: &str) -> String {
        "Landscape is unavailable (no landscape context active)".into()
    }
}
