use std::any::Any;

use crate::clients::protocol::FileObservations;

// ── conversation machinery ──────────────────────────────────────────────────

/// The transcript produced for one model turn.
#[derive(Clone, Debug)]
pub(crate) struct TurnScript {
    pub(crate) messages: Vec<rig_core::completion::Message>,
    pub(crate) system_prompt: Option<String>,
    /// The final user prompt, ready to hand to [`CompletionRequest::new`].
    /// This is a full [`Message`] (not a flat `String`) so any
    /// [`ToolResult`] content blocks that ride on a Done-rejection or
    /// operator-injected next-prompt survive into the outgoing request.
    pub(crate) next_user_prompt: rig_core::completion::Message,
}

/// The final snapshot of a completed agent run.
#[derive(Clone, Debug)]
pub(crate) struct RunArtifact {
    #[allow(dead_code)]
    pub(crate) messages: Vec<rig_core::completion::Message>,
    pub(crate) final_text: String,
}

/// Owns agent-loop conversation state: durable [`Message`] history plus the
/// pending user prompt. The loop narrates what happened; the conversation
/// assembles the transcript.
pub(crate) trait Conversation: Any + Send + Sync {
    /// Downcast to a concrete type.
    fn as_any(&self) -> &dyn Any;

    /// Downcast to a concrete type (mutable).
    #[allow(dead_code)]
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Clone this conversation into a new heap allocation.
    fn clone_box(&self) -> Box<dyn Conversation>;

    /// Note assistant prose without committing an assistant turn to history
    /// (used to update the findings ledger and `last_text` before completion
    /// classification decides whether to record or reject the turn).
    fn record_prose(&mut self, text: &str);

    /// Record the assistant turn (text, reasoning, tool calls) into history.
    /// The findings ledger is already fed by a prior `record_prose` call, so
    /// this method should NOT re-feed the ledger — it only commits the message.
    fn record_turn(
        &mut self,
        text: &str,
        reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        tool_calls: &[rig_core::completion::message::ToolCall],
    );

    /// Record tool-result messages into history.
    fn record_tool_results(&mut self, results: Vec<rig_core::completion::Message>);

    /// Drop the most recent assistant turn from history so the turn can be
    /// re-executed cleanly.
    fn pop_last_two(&mut self);

    /// Build the outgoing request for the given turn.
    fn render_turn(
        &mut self,
        stage_prompt: &str,
        turn: usize,
        nested: bool,
        scratchpad: Option<String>,
        system_prompt: Option<String>,
    ) -> TurnScript;

    /// Build the final transcript for a finished run.
    ///
    /// `done` carries the accepted `Done` tool call when the run completed via
    /// `Done`; it is `None` for the empty-turn-exhausted path. The returned
    /// [`RunArtifact`] contains the full durable message transcript plus the
    /// final assistant text result.
    fn finalize(
        &mut self,
        turn: usize,
        text: &str,
        reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        done: Option<&rig_core::completion::message::ToolCall>,
    ) -> RunArtifact;

    /// Produce a child conversation seeded from this conversation's history.
    fn fork(&self, kind: crate::task_kind::TaskKind) -> Box<dyn Conversation>;

    /// Serve a tool result through active conversation contexts, short-circuiting
    /// the normal handler dispatch. Called after arg validation and sandbox
    /// checking. Read-only (`&self`).
    ///
    /// Returns `Some(output)` when a context handles the tool; `None`
    /// falls through to the normal handler.
    fn serve_tool(&self, _name: &str, _args: &serde_json::Value) -> Option<String> {
        None
    }

    /// Create a fresh child conversation seeded with propagating contexts from
    /// this conversation, but with a clean message history. Used for
    /// Scout/Implement tasks that need inherited context state without
    /// the parent's message transcript.
    fn empty_child_with_contexts(&self, _prompt: &str) -> Box<dyn Conversation>;

    /// Replace the pending user prompt (used to begin a freshly-forked child
    /// or set a continuation/reminder prompt).
    fn set_next_prompt(&mut self, prompt: &str);

    /// Inject operator text from an interactive `Inject` command.
    fn inject_operator(&mut self, text: &str);

    /// Record a rejected `Done` turn (assistant turn + tool results + next prompt).
    fn done_rejected(
        &mut self,
        text: &str,
        reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        tool_calls: &[rig_core::completion::message::ToolCall],
        results: Vec<rig_core::completion::Message>,
        next_prompt: rig_core::completion::Message,
    );

    /// The full durable message transcript (history + pending user prompt).
    fn messages(&self) -> Vec<rig_core::completion::Message>;

    /// Update the preserve-reasoning flag. Called by `LoopState::new` so that
    /// conversations built from raw messages (chat continuation) inherit the
    /// run's actual preserve-reasoning setting rather than a hardcoded `false`.
    fn set_preserve_reasoning(&mut self, _preserve: bool) {}

    /// Observe one tool result across all contexts and return per-context
    /// extra annotations (for inline visibility alongside raw output).
    fn observe_tool_results(
        &mut self,
        _turn: usize,
        _name: &str,
        _args: &str,
        _output: &str,
    ) -> Vec<Option<String>> {
        Vec::new()
    }

    /// Join a child conversation's contexts back into this conversation,
    /// returning any newly-observed text blocks. This is the preferred path
    /// for Task tool context joining — it avoids exposing raw
    /// [`ConversationContext`] objects to callers.
    fn join_child_conversation(&mut self, _child: &dyn Conversation) -> Vec<String> {
        Vec::new()
    }

    /// Call `finalize_turn` on every context at a turn boundary.
    fn finalize_contexts(&mut self, _turn: usize) {}

    /// Return file observations from the first context that provides them.
    fn file_observations(&self) -> Option<FileObservations> {
        None
    }

    /// Seed the conversation with default tool-ledger contexts if none are
    /// present. Used by tests that bypass the executor's normal context setup.
    fn ensure_contexts(&mut self) {}

    /// Reset per-turn transcript state on every context so a child loop
    /// starts with a clean slate.
    fn clear_context_transcripts(&mut self) {}
}

// `Box<dyn Conversation>` participates in struct cloning; delegate to `clone_box`.
impl Clone for Box<dyn Conversation> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// `CompletedRun` derives `Debug`; the trait object needs a `Debug` impl.
impl std::fmt::Debug for dyn Conversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Conversation")
    }
}