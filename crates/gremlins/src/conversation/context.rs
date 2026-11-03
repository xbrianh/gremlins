/// A per-turn mutable context attached to the agent loop.
///
/// Contexts observe tool calls, contribute system-prompt blocks (e.g. landscape
/// seed, signal map, findings), and rotate per-turn state (e.g. the `[seen]`
/// dedup window).  They never see [`Message`] and never produce durable
/// history — their output is always recomputed per turn.
#[allow(dead_code)]
pub(crate) trait ConversationContext: Send + Sync {
    /// Whether this context propagates into nested Task (Fork/Scout/Implement)
    /// loops. The landscape contexts do; futures like `FindingsLedger` may not.
    fn propagates_to_task(&self) -> bool;

    /// Clone this context into a new heap allocation.
    fn clone_box(&self) -> Box<dyn ConversationContext>;

    /// Serve a tool result directly from this context, short-circuiting the
    /// normal handler dispatch. Called after arg validation and sandbox
    /// checking but before the `match name { ... }` dispatch in
    /// [`invoke_with_conversation`].
    ///
    /// Returns `Some(output)` when this context handles the tool; `None`
    /// falls through to the normal handler. Read-only (`&self`) — context
    /// state mutation happens separately through [`observe_tool`].
    fn serve_tool(&self, _name: &str, _args: &serde_json::Value) -> Option<String> {
        None
    }

    /// Record a successful tool call observation for the given turn.
    ///
    /// Called once per tool after the tool returns `"ok"`.  Must be
    /// idempotent across replayed turns.
    fn observe_tool(&mut self, turn: usize, tool: &str, args: &str, output: &str);

    /// Return optional text to annotate this tool's tool-result message in
    /// history, for inline visibility alongside the raw tool output.
    ///
    /// Called immediately after `observe_tool` for the same tool.
    /// Read-only.  Default: `None`.
    fn tool_result_extra(
        &self,
        _turn: usize,
        _tool: &str,
        _args: &str,
        _output: &str,
    ) -> Option<String> {
        None
    }

    /// Finalize the turn — apply any deferred mutations or invalidation
    /// that must wait until all concurrent tool batches complete.
    fn finalize_turn(&mut self, _turn: usize) {}

    /// Return the current context content as a system-prompt block.
    ///
    /// This content is trusted — it is always accurate and should be
    /// treated as ground truth by the model.
    fn system_message(&self) -> Option<String> {
        None
    }
}

// `Box<dyn ConversationContext>` participates in `#[derive(Clone)]` structs
// (e.g. `AgentLoopRuntime`); delegate to the trait's `clone_box`.
impl Clone for Box<dyn ConversationContext> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// `CompletedRun` derives `Debug`; the trait object needs a `Debug` impl, but
// its contents are not worth printing.
impl std::fmt::Debug for dyn ConversationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConversationContext")
    }
}
