use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::clients::conversation::Conversation;
use crate::clients::path_context::PathContext;
use crate::task_kind::TaskKind;
use gremlins_magic_bus::{emit_run_event_async, EventKind};

type ToolFuture = Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send>>;

/// Output of a tool invocation: the prose text the model sees.
pub(crate) struct ToolOutcome {
    /// The prose output (text result) from the tool.
    pub output: String,
    /// Structured process exit code, when the tool ran a process.
    ///
    /// Additive only: tools that do not run a process leave this `None`, and
    /// Bash keeps the `[exit N]\n` text prefix for backward compatibility.
    pub exit_code: Option<i32>,
}

impl ToolOutcome {
    /// Build an outcome with prose output and no exit code.
    pub(crate) fn text(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            exit_code: None,
        }
    }

    /// Attach a structured exit code to this outcome.
    pub(crate) fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }
}

/// Callback that `invoke` calls for Task tool invocations.
///
/// The third parameter carries the parent's conversation.
/// Fork receives a clone of the parent conversation; Scout/Implement
/// receive a fresh conversation seeded from the parent's contexts.
pub(crate) type TaskFn = Arc<
    dyn Fn(
            TaskKind,
            String,
            Box<dyn Conversation>,
            String, // parent tool-call id (with id_prefix applied)
            String, // formatted tool call for title row
        ) -> ToolFuture
        + Send
        + Sync,
>;

/// How tool-path sandboxing is applied for a stage's agent loop.
///
/// `Roots` is the default: every path is canonicalized and must fall under one
/// of the listed roots (empty roots deny everything). `AllowAll` disables
/// containment entirely — every check short-circuits to "allowed" without
/// canonicalizing or comparing paths. It is a hidden, programmatic-only mode
/// (not exposed in the YAML schema) used by the chat gremlin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SandboxMode {
    Roots(Vec<PathBuf>),
    AllowAll,
}

impl SandboxMode {
    /// The containment roots, or `None` when containment is disabled.
    pub(crate) fn roots(&self) -> Option<&[PathBuf]> {
        match self {
            SandboxMode::Roots(roots) => Some(roots),
            SandboxMode::AllowAll => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentLoopRuntime {
    pub cwd: Option<PathBuf>,
    pub extra_env: Option<HashMap<String, String>>,
    pub(crate) base_env: Option<HashMap<String, String>>,
    pub(crate) sandbox: SandboxMode,
    pub(crate) audit_log: Option<PathBuf>,
    pub(crate) allowed_tools: Option<Vec<String>>,
    pub(crate) task_fn: Option<TaskFn>,
    pub(crate) audit_lock: Option<Arc<std::sync::Mutex<()>>>,
    /// Prefix prepended to tool call IDs emitted on the shared event stream.
    ///
    /// Nested Task conversations run on the parent's stream but reuse raw
    /// provider call IDs (`call_0`, …). Two independent tasks would then share
    /// one TUI live slot and the second result would be dropped, so each nested
    /// loop sets its own task-chain prefix to keep IDs unique. Empty at the top
    /// level, so top-level IDs are unchanged.
    pub(crate) id_prefix: String,
    /// Run-scoped id (gremlin_id) used for direct bus emission of log lines
    /// and stream events. Empty in tests / naked runs.
    pub(crate) message_prefix: String,
    /// Discovered skills for this session. Session-bound so concurrent chat
    /// sessions resolve their own project's skills instead of a shared slot.
    /// When `None`, skills-gated tools (Mischief, WebSearch) are rejected at
    /// dispatch; LoadSkill is gated inside its own handler. When `Some`, they
    /// bypass the tool filter.
    /// `Some(empty_vec)` is a valid "skills configured but none discovered"
    /// state — `LoadSkill` itself checks for individual names.
    pub(crate) discovered_skills: Option<Arc<Vec<crate::skills::LoadedSkill>>>,
    /// Pre-resolved file-path context for short-circuiting discovery tools.
    /// Populated by the chat supervisor from the operator prompt and project
    /// root; empty for headless pipelines.
    pub(crate) path_context: PathContext,
    /// The outer stage's raw-stream path, carried independently of
    /// `audit_log`.  Each nested task loop derives its own transcript path
    /// from this stem + the task id chain.  `None` when there is no root
    /// stream (naked runs / tests) — in that case no per-task files are
    /// created.
    pub(crate) root_raw_stream_path: Option<PathBuf>,
}

impl AgentLoopRuntime {
    /// Emit one live tool-output chunk directly to the bus, tagged with the
    /// run-scoped `message_prefix`. The chunk id carries `id_prefix` so nested
    /// task loops keep their IDs unique on the shared stream.
    pub(crate) async fn emit_output(&self, id: &str, name: &str, call: &str, text: &str) {
        emit_run_event_async(
            &self.message_prefix,
            EventKind::ToolOutputChunk {
                id: format!("{}{}", self.id_prefix, id),
                name: name.to_string(),
                call: call.to_string(),
                text: text.to_string(),
            },
        )
        .await;
    }
}
