use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::clients::conversation::Conversation;
use crate::clients::interactive::StreamEventSender;
use crate::task_kind::TaskKind;

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
pub(crate) type TaskFn =
    Arc<dyn Fn(TaskKind, String, Box<dyn Conversation>) -> ToolFuture + Send + Sync>;

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

/// One live tool-output chunk streamed from a running tool into the TUI's
/// embedded tool slot. Display-only; never reaches the model or the
/// permanent transcript.
#[derive(Clone, Debug)]
pub(crate) struct ToolOutputLine {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) call: String,
    pub(crate) text: String,
}

/// Bundled send-side telemetry channels for the agent loop: stream events
/// (model deltas, tool results, turn-complete events) and live tool-output
/// line forwarding (`Bash`/`WebSearch`).
///
/// Cloned cheaply — every nested loop inherits the parent's channels.
#[derive(Clone)]
pub(crate) struct AgentLoopTelemetry {
    pub(crate) stream_events: Option<StreamEventSender>,
    pub(crate) tool_output: Option<tokio::sync::mpsc::Sender<ToolOutputLine>>,
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
    /// Bundled send-side telemetry channels. Cloned into every nested loop.
    pub(crate) telemetry: AgentLoopTelemetry,
    /// Shared mutable reference to session active skill names.
    /// The LoadSkill tool pushes names here; the supervisor reads them
    /// at the next turn boundary.
    pub(crate) active_skills: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// Whether skills are available (chat-only). Doubles as the canonical
    /// chat predicate for tool gates (Mischief bypass, LoadSkill inclusion).
    pub(crate) skills_available: bool,
    /// Discovered skills for this session. Session-bound so concurrent chat
    /// sessions resolve their own project's skills instead of a shared slot.
    pub(crate) discovered_skills: Option<Arc<Vec<crate::skills::LoadedSkill>>>,
    /// Optional log channel for nested agent loops. When set,
    /// `run_agent_loop_nested` logs "task: begin" and turn-level events
    /// through this sender instead of using a no-op logger.
    pub(crate) log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}
impl AgentLoopRuntime {
    /// Send one completed live-output line for the tool call identified by
    /// `id` to the optional display channel. Tool results returned to the
    /// model are unaffected.
    ///
    /// Awaits channel capacity so a slow live-output forwarder throttles the
    /// producing read loop rather than overflowing the buffer and silently
    /// dropping chunks from the live tool stream.
    pub(crate) async fn emit_output(&self, id: &str, name: &str, call: &str, text: &str) {
        if let Some(tx) = &self.telemetry.tool_output {
            let _ = tx
                .send(ToolOutputLine {
                    id: format!("{}{id}", self.id_prefix),
                    name: name.to_string(),
                    call: call.to_string(),
                    text: text.to_string(),
                })
                .await;
        }
    }
}
