use std::sync::OnceLock;

use crate::clients::tools::handlers::default_registry;
use crate::clients::tools::registry::ToolRegistry;

static GLOBAL_TOOL_REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();

pub(super) fn global_registry() -> &'static ToolRegistry {
    GLOBAL_TOOL_REGISTRY.get_or_init(default_registry)
}

/// Per-message concurrency budget for a fan-out tool, or `None` when uncapped.
pub(crate) fn max_per_message(name: &str) -> Option<usize> {
    global_registry()
        .handlers
        .get(name)
        .and_then(|h| h.max_per_message())
}

/// Display-truncation length for a tool's live ToolResult preview.
pub(crate) fn display_truncation(name: &str) -> usize {
    global_registry()
        .handlers
        .get(name)
        .map(|h| h.display_truncation())
        .unwrap_or(200)
}

/// Whether `name` is the loop's completion signal.
pub(crate) fn is_completion(name: &str) -> bool {
    global_registry()
        .handlers
        .get(name)
        .map(|h| h.completion())
        .unwrap_or(false)
}

/// Primary title detail for a tool call, or `None` for an unknown tool.
pub(crate) fn title_detail(name: &str, args: &serde_json::Value) -> Option<String> {
    global_registry()
        .handlers
        .get(name)
        .and_then(|h| h.title_detail(args))
}

/// Whether `name` shows its secondary args in the compact TUI title.
pub(crate) fn title_uses_secondaries(name: &str) -> bool {
    global_registry()
        .handlers
        .get(name)
        .map(|h| h.title_uses_secondaries())
        .unwrap_or(false)
}

/// Secondary `key=value` args for a tool call.
pub(crate) fn secondary_args(
    name: &str,
    args: &serde_json::Value,
    string_cap: Option<usize>,
) -> Vec<String> {
    global_registry()
        .handlers
        .get(name)
        .map(|h| h.secondary_args(args, string_cap))
        .unwrap_or_default()
}
