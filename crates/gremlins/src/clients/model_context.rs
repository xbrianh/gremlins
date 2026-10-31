use std::any::Any;

use super::protocol::FileObservations;

/// A per-turn mutable context attached to the agent loop.
///
/// Contexts observe tool calls and produce per-turn transcript blocks that are
/// merged into user messages during request assembly. They never see [`Message`]
/// and never produce durable history — only ephemeral transcript blocks that
/// `compose_model_transcript` weaves into the outgoing request.
#[allow(dead_code)]
pub trait ConversationContext: Any + Send + Sync {
    /// Whether this context propagates into nested Task (Fork/Scout/Implement)
    /// loops. `ToolLedger` propagates; futures like `FindingsLedger` may not.
    fn propagates_to_task(&self) -> bool;

    /// Downcast to a concrete type.
    fn as_any(&self) -> &dyn Any;

    /// Downcast to a concrete type (mutable).
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Clone this context into a new heap allocation.
    fn clone_box(&self) -> Box<dyn ConversationContext>;

    /// Serve a tool result directly from this context, short-circuiting the
    /// normal handler dispatch. Called after arg validation and sandbox
    /// checking but before the `match name { ... }` dispatch in
    /// [`invoke_with_contexts`].
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

    /// Return optional text to annotate this tool's result message in
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
    fn finalize_turn(&mut self, turn: usize);

    /// Return per-turn transcript blocks.
    ///
    /// Index `i` corresponds to turn `i`.  `None` means the context produced
    /// no block for that turn.  The returned vector must have length exactly
    /// `turn + 1` (the current turn number plus one for the seeded index).
    fn transcript(&self) -> Vec<Option<String>>;

    /// Return the current context content as a system-prompt block.
    ///
    /// This content is trusted — it is always accurate and should be
    /// treated as ground truth by the model.  It replaces per-turn
    /// transcript blocks for contexts that have moved their content
    /// into the system prompt.
    fn system_message(&self) -> Option<String> {
        None
    }

    /// Return file observations (Read/Grep + mutation paths) for cross-stage
    /// pruning and reporting. `ToolLedger` overrides this; other contexts
    /// return `None`.
    fn file_observations(&self) -> Option<FileObservations> {
        None
    }

    /// Reset per-turn transcript state so the context starts fresh in a
    /// child loop.  Seeded parent contexts call this before the child's
    /// first turn; the child's own turns then populate new transcript
    /// entries indexed from 0, independent of the parent's turn numbering.
    fn clear_transcript(&mut self) {
        // Default: no-op.  Concrete contexts with transcript state override.
    }

    /// Merge another context of the same concrete type into `self`.
    ///
    /// Used after a parallel fork to reconcile observations from sibling
    /// branches.  Not yet wired by any caller.
    fn join(&mut self, _other: &dyn ConversationContext) {
        // Default: no-op.  Concrete contexts that need merging override this.
    }

    /// Text for observations that arrived via [`join`] since the last call.
    /// The agent loop drains this after a Task child's contexts are merged
    /// back into the parent so newly discovered file contents reach the
    /// parent model. Default: empty.
    fn new_observations_text(&mut self) -> String {
        String::new()
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

    /// Seed the conversation's mutable model contexts. Cloned from the
    /// caller's seed list via `clone_box()`.
    fn set_contexts(&mut self, _ctxs: Vec<Box<dyn ConversationContext>>) {}

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

    /// Join child contexts (returned from a Task) back into the parent
    /// contexts, returning any newly-observed text blocks.
    fn join_child_contexts(&mut self, _children: &[Box<dyn ConversationContext>]) -> Vec<String> {
        Vec::new()
    }

    /// Call `finalize_turn` on every context at a turn boundary.
    fn finalize_contexts(&mut self, _turn: usize) {}

    /// Return clones of every context that propagates into nested tasks.
    fn propagating_contexts(&self) -> Vec<Box<dyn ConversationContext>> {
        Vec::new()
    }

    /// Snapshot all contexts via `clone_box()`.
    fn context_snapshot(&self) -> Vec<Box<dyn ConversationContext>> {
        Vec::new()
    }

    /// Return file observations from the first context that provides them.
    fn file_observations(&self) -> Option<FileObservations> {
        None
    }

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

// ── composition machinery ──────────────────────────────────────────────────

/// Zip per-turn transcripts from every context, asserting uniform length.
///
/// Returns a `Vec` where each element (one per turn index) is itself a `Vec`
/// of per-context blocks for that turn.  `None` blocks are omitted.
///
/// A context whose transcript is shorter than `expected_len` (missing turns)
/// contributes nothing for the absent indices; one that is longer (a parent
/// context seeded into a nested loop, whose extra entries belong to the
/// parent's own turn numbering) has its surplus entries ignored.  Both cases
/// are benign for rendering: the child's inherited history already carries
/// the parent's earlier turns.
pub(crate) fn zip_context_transcripts(
    contexts: &[Box<dyn ConversationContext>],
    expected_len: usize,
) -> Vec<Vec<String>> {
    let mut by_turn: Vec<Vec<String>> = vec![Vec::new(); expected_len];

    for ctx in contexts {
        let tx = ctx.transcript();
        for (i, block) in tx.into_iter().enumerate().take(expected_len) {
            if let Some(text) = block {
                by_turn[i].push(text);
            }
        }
    }

    by_turn
}

/// Build request messages by interleaving context transcripts with history.
///
/// `stage_prompt` is the original stage/task prompt — it persists across all
/// turns and is emitted as the bootstrap user message (merged with
/// `context[0]`).
///
/// `history` is the durable message history — assistant tool-call messages
/// followed by their tool results.  No user prompts are stored in history;
/// context blocks replace them ephemerally.
///
/// `next_prompt` is the harness's next user message (the stage prompt on
/// turn 0, then `"Continue."` or a findings/nudge variant).
///
/// `inherited_count` is the number of history messages that predate this loop
/// (fork/chat continuation). Those are emitted verbatim before the bootstrap
/// so older user prompts stay ahead of the current instruction.
///
/// The algorithm for turn 0:
///   1. Emit all history messages unchanged.
///   2. Emit User(next_prompt + context[0]) as the prompt tail.
///
/// For turn ≥ 1:
///   1. Emit the first `inherited_count` history messages unchanged.
///   2. Emit User(stage_prompt + context[0]) as the bootstrap.
///   3. Walk the remaining history in (assistant, results*) pairs indexed by
///      pair_idx. After each pair, if pair_idx + 1 < turn and
///      context[pair_idx+1] is non-empty, emit a synthesized
///      User(context[pair_idx+1] + "Continue.") as the inter-turn prompt.
///   4. Emit User(next_prompt + context[turn]) as the prompt tail.
///
/// Returns the composed messages ready for the model request.
pub(crate) fn compose_model_transcript(
    stage_prompt: &str,
    history: &[rig_core::completion::Message],
    next_prompt: &rig_core::completion::Message,
    contexts: &[Box<dyn ConversationContext>],
    turn: usize,
    inherited_count: usize,
) -> Vec<rig_core::completion::Message> {
    let expected = turn + 1;
    let blocks_by_turn = zip_context_transcripts(contexts, expected);

    // turn 0: no bootstrap — the stage prompt *is* next_prompt.
    // Just emit history + prompt tail.
    if turn == 0 {
        let mut out: Vec<rig_core::completion::Message> = Vec::with_capacity(history.len() + 1);
        out.extend(history.iter().cloned());
        let tail = if blocks_by_turn[0].is_empty() {
            next_prompt.clone()
        } else {
            merge_context_blocks(next_prompt, &blocks_by_turn[0])
        };
        out.push(tail);
        return out;
    }

    // turn ≥ 1: full interleaving with bootstrap.
    let mut out: Vec<rig_core::completion::Message> =
        Vec::with_capacity(history.len() + expected + 1);

    // 1. Inherited (pre-loop) history stays ahead of the bootstrap so its
    //    earlier user prompts are not reordered after the current stage prompt.
    let inherited_end = inherited_count.min(history.len());
    for msg in &history[..inherited_end] {
        out.push(msg.clone());
    }

    // 2. Bootstrap: stage_prompt + context[0].
    let bootstrap = if blocks_by_turn[0].is_empty() {
        rig_core::completion::Message::user(stage_prompt.to_string())
    } else {
        let body = blocks_by_turn[0].join("\n\n");
        rig_core::completion::Message::user(format!("{body}\n\n---\n\n{stage_prompt}"))
    };
    out.push(bootstrap);

    // 3. Walk this loop's history in (assistant, results*) pairs.
    let mut idx = inherited_end;
    let mut pair_idx: usize = 0;
    while idx < history.len() {
        match &history[idx] {
            rig_core::completion::Message::Assistant { .. } => {
                out.push(history[idx].clone());
                idx += 1;
                // Collect all following tool results for this assistant.
                while idx < history.len() && is_tool_result(&history[idx]) {
                    out.push(history[idx].clone());
                    idx += 1;
                }
                // After this pair, insert inter-turn context block if
                // there's a next turn and context for it.
                if pair_idx + 1 < turn && !blocks_by_turn[pair_idx + 1].is_empty() {
                    let body = blocks_by_turn[pair_idx + 1].join("\n\n");
                    out.push(rig_core::completion::Message::user(format!(
                        "{body}\n\nContinue."
                    )));
                }
                pair_idx += 1;
            }
            _ => {
                out.push(history[idx].clone());
                idx += 1;
            }
        }
    }

    // 4. Prompt tail.
    let final_prompt = if blocks_by_turn[turn].is_empty() {
        next_prompt.clone()
    } else {
        merge_context_blocks(next_prompt, &blocks_by_turn[turn])
    };
    out.push(final_prompt);

    out
}

// ── internal helpers ───────────────────────────────────────────────────────

/// True when `msg` is a tool-result user message.
fn is_tool_result(msg: &rig_core::completion::Message) -> bool {
    use rig_core::completion::message::UserContent;
    matches!(msg, rig_core::completion::Message::User { content } if content.iter().any(|c| matches!(c, UserContent::ToolResult(_))))
}

/// Merge context blocks into a user message by prepending them as text.
fn merge_context_blocks(
    msg: &rig_core::completion::Message,
    blocks: &[String],
) -> rig_core::completion::Message {
    use rig_core::completion::message::{Text, UserContent};

    let context_text = blocks.join("\n\n");

    match msg {
        rig_core::completion::Message::User { content } => {
            let mut new_content: Vec<UserContent> = Vec::with_capacity(content.len() + 2);

            new_content.push(UserContent::Text(Text {
                text: context_text,
                additional_params: None,
            }));

            new_content.push(UserContent::Text(Text {
                text: "\n\n---\n\n".to_string(),
                additional_params: None,
            }));

            new_content.extend(content.clone());
            rig_core::completion::Message::User {
                content: new_content,
            }
        }
        _ => msg.clone(),
    }
}
