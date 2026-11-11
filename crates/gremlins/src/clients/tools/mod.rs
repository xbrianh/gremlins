pub(crate) mod defaults;
pub(crate) mod definitions;
pub(crate) mod dispatch;
pub(crate) mod handlers;
pub(crate) mod registry;
pub(crate) mod runtime;
pub(crate) mod sandbox;
pub(crate) mod schema;
pub(crate) mod tool_specs;
pub(crate) mod util;

// Re-exports for crate-internal consumers that use `tools::`-prefixed paths.
pub(crate) use defaults::{
    display_truncation, is_completion, max_per_message, secondary_args, title_detail,
    title_uses_secondaries,
};
pub(crate) use definitions::tool_definitions;
pub(crate) use dispatch::invoke_with_conversation;
pub(crate) use runtime::{
    AgentLoopRuntime, AgentLoopTelemetry, SandboxMode, TaskFn, ToolOutcome, ToolOutputLine,
};
pub(crate) use sandbox::audit_log_path;
pub(crate) use tool_specs::MAX_CONCURRENT_TOOLS;
pub(crate) use util::preview_str;

#[cfg(test)]
mod tests;
