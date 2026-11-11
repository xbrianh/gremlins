use std::collections::HashMap;
use std::sync::Arc;

use super::runtime::{AgentLoopRuntime, ToolOutcome};
use crate::clients::conversation::Conversation;
use crate::clients::format::trunc;

// === Tool handler registry =============================================================

/// Opaque handler dispatched by `invoke`. Implementations live in `crate::clients::tools::handlers`.
///
/// `conv` is the parent conversation, used by fan-out tools (`Task`) to seed a
/// child loop — fork inherits the parent history, Scout/Implement get a fresh
/// conversation. Handlers return [`ToolOutcome`] carrying only the prose the
/// model sees; the loop never names a tool to decide how to dispatch.
#[async_trait::async_trait]
pub(crate) trait ToolHandler: Send + Sync {
    async fn invoke(
        &self,
        id: &str,
        ctx: &AgentLoopRuntime,
        conv: &dyn Conversation,
        args_json: &str,
    ) -> ToolOutcome;

    /// Per-message concurrency budget for fan-out tools. `None` means uncapped.
    /// The loop consults this to reject excess calls *without* naming the tool.
    fn max_per_message(&self) -> Option<usize> {
        None
    }

    /// Display-truncation length for live ToolResult previews. Read uses a
    /// larger preview (500) than the 200-char default because its body is
    /// already a focal artifact of every turn.
    fn display_truncation(&self) -> usize {
        200
    }

    /// Sandbox the tool's path arguments against the run's containment roots.
    /// Returns an error string on denial; `None` when allowed. `AllowAll` is
    /// expressed as `roots() == None`, which each implementation must honor by
    /// returning `None` (allowed) — mirroring the old `check_tool` early return.
    fn sandbox_check(&self, _ctx: &AgentLoopRuntime, _args: &serde_json::Value) -> Option<String> {
        None
    }

    /// Whether this tool's output is subject to the byte cap. Read/Bash/Grep/
    /// Glob/Landscape/WebSearch are capped; Edit/Write/Done/Task/Mischief/LoadSkill
    /// are not.
    fn output_cap(&self) -> bool {
        false
    }

    /// The truncation-marker suffix used when [`ToolHandler::output_cap`]
    /// truncates. `cap` is the configured byte budget.
    fn output_marker(&self, cap: u64) -> String {
        format!("\n[...truncated at {cap} bytes...]")
    }

    /// Whether this tool is the loop's completion signal. The loop consults
    /// this to detect a completion request (solo-only validation, expected
    /// artifact check) without naming the tool. Exactly one tool returns
    /// `true`; every other tool keeps the default.
    fn completion(&self) -> bool {
        false
    }

    /// Presentation: the primary `key=value` detail shown in a tool's TUI
    /// title and log label. `None` renders the bare tool name.
    fn title_detail(&self, _args: &serde_json::Value) -> Option<String> {
        None
    }

    /// Presentation: whether the tool's secondary args belong in the compact
    /// TUI title. Only Read and Grep show them there; Bash/WebSearch/Glob
    /// secondaries appear only in the wider log line.
    fn title_uses_secondaries(&self) -> bool {
        false
    }

    /// Presentation: secondary `key=value` args, ordered. `string_cap`
    /// truncates long string values (TUI titles); `None` leaves them whole
    /// (log lines, which apply their own whole-line budget).
    fn secondary_args(&self, _args: &serde_json::Value, _string_cap: Option<usize>) -> Vec<String> {
        Vec::new()
    }

    /// Whether this tool is chat-only and should be rejected in headless
    /// mode. The dispatch layer gates on this instead of comparing tool
    /// names. Only Mischief and WebSearch return `true`.
    fn chat_only(&self) -> bool {
        false
    }

    /// Optionally auto-repair common argument malformations before schema
    /// validation. Returns repaired args, or `None` when no repair logic
    /// exists. Only Edit implements this.
    fn repair_args(&self, _args: serde_json::Value) -> Option<serde_json::Value> {
        None
    }

    /// Whether this tool bypasses the tool-filter allowlist. LoadSkill
    /// returns `true` so it is always reachable in chat sessions
    /// regardless of the configured tool filter.
    fn bypass_filter(&self) -> bool {
        false
    }
}

/// Registry of tool name → handler. Created once by `handlers::default_registry`.
#[allow(dead_code)]
pub(crate) struct ToolRegistry {
    pub(super) handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("handlers", &self.handlers.keys())
            .finish()
    }
}

impl ToolRegistry {
    pub(crate) fn new(handlers: Vec<(&'static str, Arc<dyn ToolHandler>)>) -> Self {
        Self {
            handlers: handlers.into_iter().collect(),
        }
    }
}

// --- Presentation helpers shared by handlers and the agent loop ---

/// `key="value"` detail for a single string argument, used by handlers that
/// present one primary path/command/pattern.
pub(crate) fn str_detail(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| format!("{key}={s:?}"))
}

/// Push `key=value` for a non-zero unsigned argument, used by handlers
/// assembling their secondary args.
pub(crate) fn push_u64_arg(out: &mut Vec<String>, args: &serde_json::Value, key: &str) {
    if let Some(v) = args.get(key).and_then(|v| v.as_u64()).filter(|v| *v > 0) {
        out.push(format!("{key}={v}"));
    }
}

/// Push `key="value"` for a non-empty string argument, optionally truncating
/// the value for the compact TUI title.
pub(crate) fn push_str_arg(
    out: &mut Vec<String>,
    args: &serde_json::Value,
    key: &str,
    string_cap: Option<usize>,
) {
    if let Some(v) = args
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        let rendered = match string_cap {
            Some(cap) => format!("{:?}", trunc(v, cap)),
            None => format!("{v:?}"),
        };
        out.push(format!("{key}={rendered}"));
    }
}
