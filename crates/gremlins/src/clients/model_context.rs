use std::any::Any;

/// A per-turn mutable context attached to the agent loop.
///
/// Contexts observe tool calls and produce per-turn transcript blocks that are
/// merged into user messages during request assembly. They never see [`Message`]
/// and never produce durable history — only ephemeral transcript blocks that
/// `compose_model_transcript` weaves into the outgoing request.
#[allow(dead_code)]
pub trait MutableModelContext: Any + Send + Sync {
    /// Whether this context propagates into nested Task (Fork/Scout/Implement)
    /// loops. `ToolLedger` propagates; futures like `FindingsLedger` may not.
    fn propagates_to_task(&self) -> bool;

    /// Downcast to a concrete type.
    fn as_any(&self) -> &dyn Any;

    /// Downcast to a concrete type (mutable).
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Clone this context into a new heap allocation.
    fn clone_box(&self) -> Box<dyn MutableModelContext>;

    /// Record a successful tool call observation for the given turn.
    ///
    /// Called once per tool after the tool returns `"ok"`.  Must be
    /// idempotent across replayed turns.
    fn observe(&mut self, turn: usize, tool: &str, args: &str, output: &str);

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
    fn join(&mut self, _other: &dyn MutableModelContext) {
        // Default: no-op.  Concrete contexts that need merging override this.
    }
}

// `Box<dyn MutableModelContext>` participates in `#[derive(Clone)]` structs
// (e.g. `AgentLoopRuntime`); delegate to the trait's `clone_box`.
impl Clone for Box<dyn MutableModelContext> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// `CompletedRun` derives `Debug`; the trait object needs a `Debug` impl, but
// its contents are not worth printing.
impl std::fmt::Debug for dyn MutableModelContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MutableModelContext")
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
    contexts: &[Box<dyn MutableModelContext>],
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
/// The algorithm for turn 0:
///   1. Emit all history messages unchanged.
///   2. Emit User(next_prompt + context[0]) as the prompt tail.
///
/// For turn ≥ 1:
///   1. Emit User(stage_prompt + context[0]) as the bootstrap.
///   2. Walk history in (assistant, results*) pairs indexed by pair_idx.
///      After each pair, if pair_idx + 1 < turn and context[pair_idx+1]
///      is non-empty, emit a synthesized User(context[pair_idx+1] +
///      "Continue.") as the inter-turn prompt.
///   3. Emit User(next_prompt + context[turn]) as the prompt tail.
///
/// Returns the composed messages ready for the model request.
pub(crate) fn compose_model_transcript(
    stage_prompt: &str,
    history: &[rig_core::completion::Message],
    next_prompt: &rig_core::completion::Message,
    contexts: &[Box<dyn MutableModelContext>],
    turn: usize,
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

    // 1. Bootstrap: stage_prompt + context[0].
    let bootstrap = if blocks_by_turn[0].is_empty() {
        rig_core::completion::Message::user(stage_prompt.to_string())
    } else {
        let body = blocks_by_turn[0].join("\n\n");
        rig_core::completion::Message::user(format!("{body}\n\n---\n\n{stage_prompt}"))
    };
    out.push(bootstrap);

    // 2. Walk history in (assistant, results*) pairs.
    let mut idx = 0;
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

    // 3. Prompt tail.
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
