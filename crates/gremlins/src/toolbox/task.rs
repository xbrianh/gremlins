use async_trait::async_trait;

use crate::clients::conversation::Conversation;
use crate::clients::tools::{
    parse_args, preview_str, str_detail, AgentLoopRuntime, ToolHandler, ToolOutcome,
};
use crate::task_kind::TaskKind;

/// The Task tool. Unlike the filesystem/process tools, Task fans out to a
/// child agent loop and returns that child's conversation for the parent loop
/// to join. It lives behind the registry like every other tool — the agent
/// loop never names "Task" to decide how to dispatch or join.
pub(crate) struct TaskTool;

#[async_trait]
impl ToolHandler for TaskTool {
    fn title_detail(&self, args: &serde_json::Value) -> Option<String> {
        str_detail(args, "kind")
    }

    fn max_per_message(&self) -> Option<usize> {
        Some(crate::clients::tool_specs::TASK_MAX_PER_TURN)
    }

    async fn invoke(
        &self,
        _id: &str,
        ctx: &AgentLoopRuntime,
        conv: &dyn Conversation,
        args_json: &str,
    ) -> ToolOutcome {
        let args = match parse_args(args_json) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::text(e),
        };

        let Some(f) = &ctx.task_fn else {
            return ToolOutcome::text("Error: Task not available for this backend");
        };

        let kind_str = args.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        if prompt.is_empty() {
            return ToolOutcome::text("Error: Task prompt is required");
        }
        let Some(kind) = TaskKind::parse(kind_str) else {
            return ToolOutcome::text(format!(
                "Error: unknown Task kind {kind_str:?}. Expected one of: {}",
                TaskKind::ALL
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };

        let conversation = if kind == TaskKind::Fork {
            conv.clone_box()
        } else {
            conv.empty_child_with_contexts(prompt)
        };
        let child_outcome = f(kind, prompt.to_string(), conversation).await;
        // Label the result so concurrent Task outputs can be told apart.
        let final_output = format!("# {kind}\n\n{}", child_outcome.output);
        log::info!(
            "task dispatch complete: kind={kind} prompt_len={p_len} prompt_preview={p_preview:?} output_len={o_len} output_preview={o_preview:?}",
            p_len = prompt.len(),
            p_preview = preview_str(prompt, 120),
            o_len = final_output.len(),
            o_preview = preview_str(&final_output, 300),
        );
        ToolOutcome {
            output: final_output,
            child: child_outcome.child,
        }
    }
}
