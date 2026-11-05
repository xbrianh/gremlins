// ── conversation machinery ──────────────────────────────────────────────────

use async_trait::async_trait;

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
#[async_trait]
pub(crate) trait Conversation: Send + Sync {
    /// Clone this conversation into a new heap allocation.
    fn clone_box(&self) -> Box<dyn Conversation>;

    /// Note assistant prose without committing an assistant turn to history
    /// (used to update the findings ledger and `last_text` before completion
    /// classification decides whether to record or reject the turn).
    fn record_prose(&mut self, text: &str);

    /// Record the API-reported input token count for this turn.
    /// Called immediately after every response so compaction decisions see
    /// the latest usage even when the turn continues before `record_turn`.
    fn record_input_tokens(&mut self, _input_tokens: Option<u64>) {}

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
        input_tokens: Option<u64>,
    );

    /// Record tool-result messages into history.
    fn record_tool_results(&mut self, results: Vec<rig_core::completion::Message>);

    /// Drop the most recent assistant turn from history so the turn can be
    /// re-executed cleanly.
    fn pop_last_assistant_turn(&mut self);

    /// Build the outgoing request for the current turn.
    ///
    /// May trigger compaction before building the turn script when
    /// token thresholds are exceeded.
    async fn render_turn(
        &mut self,
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
    fn fork(&self) -> Box<dyn Conversation>;

    /// Create a fresh child conversation with a clean message history and the
    /// given prompt. Used for Scout/Implement tasks.
    fn spawn_child(&self, _prompt: &str) -> Box<dyn Conversation>;

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

    /// True when a server-issued signed compaction block is active and must
    /// be sent with the beta header on ordinary completion requests.
    #[allow(dead_code)] // wired when agent_loop conditionally attaches beta header
    fn has_signed_compaction_block(&self) -> bool {
        false
    }

    /// Compact digest of the Read/Grep evidence still present in the durable
    /// transcript. Fan-out tools fold this into the parent's tool result so a
    /// child loop's discoveries survive after the child is gone.
    fn tool_evidence(&self) -> String {
        collect_tool_evidence(&self.messages())
    }
}

/// Upper bound on [`collect_tool_evidence`] output, so a child that read a
/// large file cannot flood the parent's transcript.
const MAX_EVIDENCE_BYTES: usize = 8_000;

/// Summarize the Read/Grep results still present in `messages` as
/// `label\n<output>` blocks, newest last, capped at [`MAX_EVIDENCE_BYTES`].
fn collect_tool_evidence(messages: &[rig_core::completion::Message]) -> String {
    use rig_core::completion::message::{AssistantContent, ToolResultContent, UserContent};

    let mut entries: Vec<String> = Vec::new();
    let mut pending: Vec<(rig_core::completion::message::CallId, String)> = Vec::new();

    for message in messages {
        match message {
            rig_core::completion::Message::Assistant { content, .. } => {
                pending = content
                    .iter()
                    .filter_map(|item| match item {
                        AssistantContent::ToolCall(call) => {
                            let name = call.function.name.as_str();
                            if name != "Read" && name != "Grep" {
                                return None;
                            }
                            let target = call
                                .function
                                .arguments
                                .get("file_path")
                                .or_else(|| call.function.arguments.get("path"))
                                .and_then(|v| v.as_str());
                            let target = if name == "Grep" {
                                target.unwrap_or(".")
                            } else {
                                target?
                            };
                            Some((call.id.clone(), format!("{name}: {target}")))
                        }
                        _ => None,
                    })
                    .collect();
            }
            rig_core::completion::Message::User { content } => {
                if pending.is_empty() {
                    continue;
                }
                for item in content {
                    let UserContent::ToolResult(result) = item else {
                        continue;
                    };
                    let Some(position) = pending.iter().position(|(id, _)| *id == result.call)
                    else {
                        continue;
                    };
                    let text: String = result
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            ToolResultContent::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    if !text.is_empty() && !text.starts_with("Error:") {
                        entries.push(format!("{}\n{text}", pending[position].1));
                    }
                    pending.remove(position);
                }
            }
            _ => {}
        }
    }

    let mut out = String::new();
    for entry in entries {
        let extra = entry.len() + if out.is_empty() { 0 } else { 2 };
        if out.len() + extra > MAX_EVIDENCE_BYTES {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&entry);
    }
    out
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

// ── test stub ──────────────────────────────────────────────────────────────

/// A minimal [`Conversation`] implementation for use in `clients/` tests.
///
/// Tracks flat message history (no findings ledger) so that agent-loop and
/// tool-registry tests can run without depending on `crate::conversation`
/// (which lives in a different package boundary). `record_turn`,
/// `done_rejected`, `pop_last_assistant_turn`, and `fork` all mirror
/// `StandardConversation` — including the prompt-commit invariant — so the
/// "no consecutive user messages" guard tests exercise the shipped semantics.
#[cfg(test)]
pub(crate) struct ConversationStub {
    history: Vec<rig_core::completion::Message>,
    next_prompt: rig_core::completion::Message,
    prompt_committed: bool,
    preserve_reasoning: bool,
    last_text: String,
    finalized_messages: Option<Vec<rig_core::completion::Message>>,
}

#[cfg(test)]
impl ConversationStub {
    pub(crate) fn new(prompt: &str) -> Self {
        Self {
            history: Vec::new(),
            next_prompt: rig_core::completion::Message::user(prompt.to_string()),
            prompt_committed: false,
            preserve_reasoning: false,
            last_text: String::new(),
            finalized_messages: None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_history(prompt: &str, history: Vec<rig_core::completion::Message>) -> Self {
        Self {
            history,
            next_prompt: rig_core::completion::Message::user(prompt.to_string()),
            prompt_committed: false,
            preserve_reasoning: false,
            last_text: String::new(),
            finalized_messages: None,
        }
    }

    fn begin_next_turn(&mut self) {
        self.next_prompt = rig_core::completion::Message::user("Continue.".to_string());
        self.prompt_committed = false;
    }

    fn box_self(&self) -> Box<dyn Conversation> {
        Box::new(Self {
            history: self.history.clone(),
            next_prompt: self.next_prompt.clone(),
            prompt_committed: self.prompt_committed,
            preserve_reasoning: self.preserve_reasoning,
            last_text: self.last_text.clone(),
            finalized_messages: self.finalized_messages.clone(),
        })
    }
}

#[cfg(test)]
#[async_trait]
impl Conversation for ConversationStub {
    fn clone_box(&self) -> Box<dyn Conversation> {
        self.box_self()
    }

    fn record_prose(&mut self, text: &str) {
        self.last_text = text.to_string();
    }

    fn record_turn(
        &mut self,
        text: &str,
        reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        tool_calls: &[rig_core::completion::message::ToolCall],
        _input_tokens: Option<u64>,
    ) {
        // Commit the pending user prompt before the assistant message.
        let prompt = self.next_prompt.clone();
        self.history.push(prompt);
        self.prompt_committed = true;
        self.history.push(super::format::assistant_tool_message(
            text,
            reasoning_items,
            tool_calls,
            self.preserve_reasoning,
        ));
    }

    fn record_tool_results(&mut self, results: Vec<rig_core::completion::Message>) {
        self.history.extend(results);
        self.begin_next_turn();
    }

    fn pop_last_assistant_turn(&mut self) {
        // Pop the assistant message.
        if matches!(
            self.history.last(),
            Some(rig_core::completion::Message::Assistant { .. })
        ) {
            self.history.pop();
        }
        // Pop the preceding user prompt and restore it as next_prompt.
        if let Some(rig_core::completion::Message::User { .. }) = self.history.last() {
            let prompt = self.history.pop().unwrap();
            self.next_prompt = prompt;
        }
        self.prompt_committed = false;
    }

    async fn render_turn(
        &mut self,
        _nested: bool,
        _scratchpad: Option<String>,
        system_prompt: Option<String>,
    ) -> TurnScript {
        TurnScript {
            messages: self.history.clone(),
            system_prompt,
            next_user_prompt: self.next_prompt.clone(),
        }
    }

    fn finalize(
        &mut self,
        turn: usize,
        text: &str,
        reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        done: Option<&rig_core::completion::message::ToolCall>,
    ) -> RunArtifact {
        let mut messages = self.history.clone();
        messages.push(self.next_prompt.clone());
        let final_text = if text.trim().is_empty() {
            done.and_then(|tc| {
                tc.function
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| self.last_text.clone())
        } else {
            text.to_string()
        };
        let tool_slice: &[rig_core::completion::message::ToolCall] =
            done.map(std::slice::from_ref).unwrap_or(&[]);
        messages.push(super::format::assistant_tool_message(
            text,
            reasoning_items,
            tool_slice,
            self.preserve_reasoning,
        ));
        if let Some(done_tc) = done {
            let done_ledger = format!(
                "[turn {}]: Done {}",
                turn,
                done_tc
                    .function
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            );
            messages.push(super::done_helpers::done_user_message(
                &done_ledger,
                done_tc.id.clone(),
                done_tc.function.name.clone(),
                "Done accepted.",
            ));
        }
        self.finalized_messages = Some(messages.clone());
        RunArtifact {
            messages,
            final_text,
        }
    }

    fn fork(&self) -> Box<dyn Conversation> {
        let mut history = self.history.clone();
        // Drop the in-flight assistant turn: its tool results are still
        // unknown. Keep the preceding user prompt (the original task)
        // so the child receives the promised parent context.
        if matches!(
            history.last(),
            Some(rig_core::completion::Message::Assistant { .. })
        ) {
            history.pop();
        }
        Box::new(Self {
            history,
            next_prompt: rig_core::completion::Message::user(String::new()),
            prompt_committed: false,
            preserve_reasoning: self.preserve_reasoning,
            last_text: String::new(),
            finalized_messages: None,
        })
    }

    fn spawn_child(&self, prompt: &str) -> Box<dyn Conversation> {
        Box::new(Self::new(prompt))
    }

    fn set_next_prompt(&mut self, prompt: &str) {
        self.next_prompt = rig_core::completion::Message::user(prompt.to_string());
    }

    fn inject_operator(&mut self, text: &str) {
        self.next_prompt = rig_core::completion::Message::user(text.to_string());
    }

    fn done_rejected(
        &mut self,
        text: &str,
        reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        tool_calls: &[rig_core::completion::message::ToolCall],
        results: Vec<rig_core::completion::Message>,
        next_prompt: rig_core::completion::Message,
    ) {
        // Commit the pending prompt before the assistant message, exactly as
        // `StandardConversation::done_rejected` does.
        let prompt = std::mem::replace(&mut self.next_prompt, next_prompt);
        self.history.push(prompt);
        self.prompt_committed = true;
        self.history.push(super::format::assistant_tool_message(
            text,
            reasoning_items,
            tool_calls,
            self.preserve_reasoning,
        ));
        self.history.extend(results);
    }

    fn messages(&self) -> Vec<rig_core::completion::Message> {
        if let Some(ref msgs) = self.finalized_messages {
            return msgs.clone();
        }
        let mut msgs = self.history.clone();
        if !self.prompt_committed {
            msgs.push(self.next_prompt.clone());
        }
        msgs
    }

    fn set_preserve_reasoning(&mut self, preserve: bool) {
        self.preserve_reasoning = preserve;
    }

    fn has_signed_compaction_block(&self) -> bool {
        false
    }
}
