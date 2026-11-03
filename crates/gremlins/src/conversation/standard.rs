//! The default [`Conversation`] implementation used by the agent loop.
//!
//! `StandardConversation` owns the durable message history and the pending user
//! prompt. On each turn the pending prompt is committed into history before the
//! assistant message, so `history + next_prompt` is always the complete
//! transcript. Findings and scratchpad content travel through the system
//! prompt instead of being spliced into ephemeral user prompts.

use rig_core::completion::message::{Reasoning, Sealed, ToolCall};
use rig_core::completion::Message;

use crate::clients::conversation::{Conversation, RunArtifact, TurnScript};
use crate::clients::done_helpers::done_user_message;
use crate::clients::findings::FindingsLedger;
use crate::clients::format::assistant_tool_message;
use crate::conversation::context::ConversationContext;
use crate::conversation::contexts::landscape::LandscapeContext;
use crate::landscape::LandscapeData;
use crate::task_kind::TaskKind;

/// The standard [`Conversation`] implementation.
pub(crate) struct StandardConversation {
    /// Verbatim durable message history. User prompts are committed before each
    /// assistant message, so `history` alone is the complete transcript of all
    /// completed turns.
    history: Vec<Message>,
    /// The harness's next user message — committed into [`history`] by
    /// [`record_turn`] and [`done_rejected`], replaced by [`record_tool_results`]
    /// and [`set_next_prompt`].
    next_prompt: Message,
    /// Declarative findings extracted from assistant prose and surfaced in the
    /// system prompt so the model does not re-derive facts it already stated.
    findings_ledger: FindingsLedger,
    preserve_reasoning_in_history: bool,
    last_text: String,
    /// Snapshot produced by the most recent [`Conversation::finalize`] call.
    /// Once set, [`Conversation::messages`] returns this final transcript so
    /// persistence consumers see the exact durable message list.
    finalized_messages: Option<Vec<Message>>,
    /// Mutable model contexts that observe tool calls and contribute system-
    /// prompt blocks. Owned here so the agent loop routes every context
    /// operation through the conversation instead of carrying a parallel list.
    contexts: Vec<Box<dyn ConversationContext>>,
}

impl StandardConversation {
    pub(crate) fn new(
        prompt: &str,
        preserve_reasoning: bool,
        prepopulated_messages: Option<Vec<Message>>,
    ) -> Self {
        let history = prepopulated_messages.unwrap_or_default();
        Self {
            history,
            next_prompt: Message::user(prompt.to_string()),
            findings_ledger: FindingsLedger::new(),
            preserve_reasoning_in_history: preserve_reasoning,
            last_text: String::new(),
            finalized_messages: None,
            contexts: Vec::new(),
        }
    }

    /// Seed mutable model contexts from raw landscape data.
    ///
    /// Builds a [`LandscapeContext`] when `landscape_data` is present.  The
    /// context is cloned via `clone_box()` so the conversation owns an
    /// independent copy.
    /// Called before the conversation is boxed and handed off to the agent loop.
    pub(crate) fn seed_from_landscape(&mut self, landscape_data: Option<LandscapeData>) {
        let mut contexts: Vec<Box<dyn ConversationContext>> = Vec::new();

        if let Some(ld) = landscape_data {
            contexts.push(Box::new(LandscapeContext::new(ld)));
        }

        self.contexts = contexts.into_iter().map(|c| c.clone_box()).collect();
    }

    /// Build the combined system prompt from contexts, findings, scratchpad,
    /// and the stage-supplied system prompt.
    fn build_system_prompt(
        &mut self,
        nested: bool,
        scratchpad: Option<String>,
        system_prompt: Option<String>,
    ) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();

        // Context system messages (landscape seed, signal map).
        for ctx in &self.contexts {
            if let Some(msg) = ctx.system_message() {
                parts.push(msg);
            }
        }

        if !nested {
            // Scratchpad (ephemeral — never in history).
            if let Some(content) = scratchpad {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    parts.push(format!(
                        "## Scratchpad\n\nThe following is your persistent scratchpad \
                         content:\n\n{trimmed}"
                    ));
                }
            }

            // Findings from the ledger (recomputed per turn).
            let findings = self.findings_ledger.format();
            if !findings.is_empty() {
                parts.push(findings);
            }
        }

        if let Some(sys) = system_prompt {
            parts.push(sys);
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    /// Roll prompt-forward bookkeeping after a tool batch.
    fn begin_next_turn(&mut self) {
        self.next_prompt = Message::user("Continue.".to_string());
    }

    fn feed_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.last_text = text.to_string();
        }
        self.findings_ledger.feed(text);
    }
}

impl Conversation for StandardConversation {
    fn clone_box(&self) -> Box<dyn Conversation> {
        Box::new(Self {
            history: self.history.clone(),
            next_prompt: self.next_prompt.clone(),
            findings_ledger: self.findings_ledger.clone(),
            preserve_reasoning_in_history: self.preserve_reasoning_in_history,
            last_text: self.last_text.clone(),
            finalized_messages: self.finalized_messages.clone(),
            contexts: self.contexts.iter().map(|c| c.clone_box()).collect(),
        })
    }

    fn record_prose(&mut self, text: &str) {
        self.feed_text(text);
    }

    fn record_turn(
        &mut self,
        text: &str,
        reasoning_items: &[Sealed<Reasoning>],
        tool_calls: &[ToolCall],
    ) {
        // record_prose already fed the findings ledger and updated last_text;
        // do NOT call feed_text here to avoid double-counting findings.

        // Commit the pending user prompt into history before the assistant
        // message. This is the static-conversation invariant: every user prompt
        // that the model sees lives in history, verbatim.
        let prompt = self.next_prompt.clone();
        self.history.push(prompt);
        self.history.push(assistant_tool_message(
            text,
            reasoning_items,
            tool_calls,
            self.preserve_reasoning_in_history,
        ));
    }

    fn record_tool_results(&mut self, results: Vec<Message>) {
        self.history.extend(results);
        self.begin_next_turn();
    }

    fn pop_last_assistant_turn(&mut self) {
        // Pop the assistant message.
        if matches!(self.history.last(), Some(Message::Assistant { .. })) {
            self.history.pop();
        }
        // Pop the preceding user prompt (the one committed by record_turn)
        // and restore it as next_prompt for clean turn re-execution.
        if let Some(Message::User { .. }) = self.history.last() {
            let prompt = self.history.pop().unwrap();
            self.next_prompt = prompt;
        }
    }

    fn render_turn(
        &mut self,
        nested: bool,
        scratchpad: Option<String>,
        system_prompt: Option<String>,
    ) -> TurnScript {
        let combined_system = self.build_system_prompt(nested, scratchpad, system_prompt);

        TurnScript {
            messages: self.history.clone(),
            system_prompt: combined_system,
            next_user_prompt: self.next_prompt.clone(),
        }
    }

    fn finalize(
        &mut self,
        turn: usize,
        text: &str,
        reasoning_items: &[Sealed<Reasoning>],
        done: Option<&ToolCall>,
    ) -> RunArtifact {
        let mut messages = self.history.clone();
        // The pending prompt for this Done turn was never committed by
        // record_turn, so append it here.
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
        let tool_slice: &[ToolCall] = done.map(std::slice::from_ref).unwrap_or(&[]);
        messages.push(assistant_tool_message(
            text,
            reasoning_items,
            tool_slice,
            self.preserve_reasoning_in_history,
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
            messages.push(done_user_message(
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

    fn fork(&self, kind: TaskKind) -> Box<dyn Conversation> {
        let _ = kind;
        let mut history = self.history.clone();
        // The caller appends the current-turn assistant tool-call to history
        // before forking. Carrying it into the child without its still-unknown
        // tool results would leave an unmatched assistant tool-use, so drop it.
        // Also drop the preceding user prompt that was committed by record_turn
        // for this same turn — the child receives its own fork prompt instead.
        let had_assistant = matches!(history.last(), Some(Message::Assistant { .. }));
        if had_assistant {
            history.pop();
            // Only pop the preceding user prompt when we actually removed an
            // assistant — the prompt is the one committed by record_turn for
            // the same turn.
            if let Some(Message::User { .. }) = history.last() {
                history.pop();
            }
        }
        Box::new(Self {
            history,
            next_prompt: Message::user(String::new()),
            // Fork children start with a clean FindingsLedger and empty
            // last_text — matching the old prepopulated_messages semantics
            // where only raw messages were carried forward.
            findings_ledger: FindingsLedger::new(),
            preserve_reasoning_in_history: self.preserve_reasoning_in_history,
            last_text: String::new(),
            finalized_messages: None,
            contexts: self
                .contexts
                .iter()
                .filter(|c| c.propagates_to_task())
                .map(|c| c.clone_box())
                .collect(),
        })
    }

    fn serve_tool(&self, name: &str, args: &serde_json::Value) -> Option<String> {
        for ctx in &self.contexts {
            if let Some(output) = ctx.serve_tool(name, args) {
                return Some(output);
            }
        }
        None
    }

    fn empty_child_with_contexts(&self, prompt: &str) -> Box<dyn Conversation> {
        let mut child = Self::new(prompt, self.preserve_reasoning_in_history, None);
        let propagating = self
            .contexts
            .iter()
            .filter(|c| c.propagates_to_task())
            .map(|c| c.clone_box())
            .collect::<Vec<_>>();
        child.contexts = propagating;
        Box::new(child)
    }

    fn set_next_prompt(&mut self, prompt: &str) {
        self.next_prompt = Message::user(prompt.to_string());
    }

    fn set_preserve_reasoning(&mut self, preserve: bool) {
        self.preserve_reasoning_in_history = preserve;
    }

    fn inject_operator(&mut self, text: &str) {
        if let Message::User { content: existing } = self.next_prompt.clone() {
            use rig_core::completion::message::{Text, UserContent};
            let has_tool_result = existing
                .iter()
                .any(|c| matches!(c, UserContent::ToolResult(_)));
            if has_tool_result {
                let mut merged = existing;
                merged.push(UserContent::Text(Text {
                    text: format!("[operator]: {text}"),
                    additional_params: None,
                }));
                self.next_prompt = Message::User { content: merged };
            } else {
                self.history
                    .push(Message::user(format!("[operator]: {text}")));
                self.next_prompt = Message::user(text);
            }
        } else {
            self.history
                .push(Message::user(format!("[operator]: {text}")));
            self.next_prompt = Message::user(text);
        }
    }

    fn done_rejected(
        &mut self,
        text: &str,
        reasoning_items: &[Sealed<Reasoning>],
        tool_calls: &[ToolCall],
        results: Vec<Message>,
        next_prompt: Message,
    ) {
        // record_prose already fed the findings ledger and updated last_text;
        // do NOT call feed_text here to avoid double-counting findings.

        // Commit the pending user prompt into history before the assistant
        // message, matching the record_turn invariant.
        let prompt = std::mem::replace(&mut self.next_prompt, next_prompt);
        self.history.push(prompt);
        self.history.push(assistant_tool_message(
            text,
            reasoning_items,
            tool_calls,
            self.preserve_reasoning_in_history,
        ));
        self.history.extend(results);
    }

    fn messages(&self) -> Vec<Message> {
        if let Some(ref finalized) = self.finalized_messages {
            return finalized.clone();
        }
        let mut all = self.history.clone();
        all.push(self.next_prompt.clone());
        all
    }

    fn observe_tool_results(
        &mut self,
        turn: usize,
        name: &str,
        args: &str,
        output: &str,
    ) -> Vec<Option<String>> {
        let mut extra = Vec::with_capacity(self.contexts.len());
        for ctx in &mut self.contexts {
            ctx.observe_tool(turn, name, args, output);
            extra.push(ctx.tool_result_extra(turn, name, args, output));
        }
        extra
    }

    fn finalize_contexts(&mut self, turn: usize) {
        for ctx in &mut self.contexts {
            ctx.finalize_turn(turn);
        }
    }
}

#[cfg(test)]
mod tests {
    use rig_core::completion::Message;
    use rig_core::message::{AssistantContent, Issuer, UserContent};

    use crate::clients::agent_loop::*;
    use crate::clients::cancel::CancelToken;
    use crate::conversation::new_blank_conversation;

    fn user_message_text(message: &Message) -> String {
        match message {
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    }

    // Override test_ctx to use StandardConversation (context-aware) instead of
    // the ConversationStub that agent_loop tests use.
    fn test_ctx(
        cwd: Option<std::path::PathBuf>,
        raw_path: Option<std::path::PathBuf>,
    ) -> crate::clients::agent_loop::RunContext {
        let mut ctx = crate::clients::agent_loop::test_ctx(cwd, raw_path);
        ctx.prepopulated_conversation = new_blank_conversation("hi", false, None);
        ctx
    }

    /// A parent agent-loop turn with prior assistant/tool-result history that
    /// emits `Task(kind="Fork")` must hand the child the snapshot of that
    /// history (including the current user prompt), followed by the Fork
    /// prompt, in exact order. This exercises the agent-loop snapshot path and
    /// the `tools::invoke` handoff rather than calling the task runner directly.
    #[tokio::test]
    async fn fork_child_receives_parent_history_then_fork_prompt() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("note.txt");
        std::fs::write(&note, "hello from parent").unwrap();

        let read_call = MockStreamEvent::tool_call(
            "r1",
            "Read",
            serde_json::json!({ "file_path": note.to_str().unwrap() }),
        );
        let fork_call = MockStreamEvent::tool_call(
            "f1",
            "Task",
            serde_json::json!({"kind": "Fork", "prompt": "finish as fork"}),
        );
        let done_call = |id: &str, summary: &str| {
            MockStreamEvent::tool_call(id, "Done", serde_json::json!({ "summary": summary }))
        };

        // One model drives every loop: parent Read turn, parent Fork turn,
        // forked child turn, then parent wrap-up turn.
        let model = MockCompletionModel::from_stream_turns([
            vec![
                read_call,
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                fork_call,
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("fork reply"),
                done_call("d2", "fork reply"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("parent done"),
                done_call("d1", "parent done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);

        run_agent_loop(
            gm(model.clone().erase()),
            "parent go",
            ctx,
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        let requests = model.requests();
        let fork_req = requests
            .iter()
            .find(|req| {
                matches!(
                    req.chat_history.first(),
                    Some(Message::System { content })
                        if content.contains("This is a forked child agent")
                )
            })
            .expect("the forked child must issue its own request");

        let history = &fork_req.chat_history;
        // The child inherits the parent's conversation as static history:
        // [system prompt] + [User(stage_prompt), Assistant(tool_call), User(tool_result)]
        // followed by the fork prompt. That's 5 messages: system + 4 (3 inherited + 1 fork).
        assert_eq!(history.len(), 5, "unexpected fork history: {history:#?}");

        // Fork system prompt leads the child history.
        assert!(matches!(
            &history[0],
            Message::System { content }
                if content.contains("<tools>") && content.contains("Do not call")
        ));

        // Inherited parent conversation — the fork prompt is the last message.
        assert!(
            history
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. })),
            "history must contain an assistant message"
        );
        assert!(
            history.iter().any(|m| matches!(m, Message::User { content } if content.iter().any(|c| matches!(c, UserContent::ToolResult(_))))),
            "history must contain a tool result"
        );
        // The Fork prompt is the last message in chat_history.
        assert!(
            user_message_text(history.last().unwrap()).contains("finish as fork"),
            "last message should be fork prompt: {:?}",
            history.last()
        );
    }

    /// A reasoning-only turn (reasoning deltas but no text and no tool call)
    /// must still record an assistant message carrying the structured
    /// reasoning item before nudging with "Continue.". Dropping it would both
    /// omit the item from the next request and leave two consecutive user
    /// messages — breaking the replay invariant the provider relies on.
    #[tokio::test]
    async fn loop_replays_reasoning_item_on_reasoning_only_turn() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-reasoning-only-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Turn 0: reasoning only (rs_0), no text, no tool call — the loop
        // records the reasoning item and continues with "Continue.".
        // Turn 1: Done. The second request must carry rs_0 as an assistant
        // message, not as two consecutive user messages.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::reasoning_delta_with_id("rs_0", "thinking"),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({ "summary": "done" }),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        let result = run_agent_loop(
            gm(model.clone().erase()),
            "think then finish",
            ctx,
            cancel,
            loop_opts_preserve_reasoning(None),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code, 0);

        let reqs = model.requests();
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];

        // The reasoning-only turn must be replayed as an assistant message.
        let assistant = second
            .chat_history
            .iter()
            .find(|m| matches!(m, Message::Assistant { .. }))
            .expect("second request carries the prior assistant turn");
        let Message::Assistant { content, .. } = assistant else {
            unreachable!()
        };
        let reasoning = content.iter().find_map(|c| match c {
            AssistantContent::Reasoning(sealed) => {
                sealed.open(&Issuer::from("mock")).map(|r| r.id.clone())
            }
            _ => None,
        });
        assert_eq!(
            reasoning.flatten().as_deref(),
            Some("rs_0"),
            "reasoning-only turn's structured item must survive into the replay"
        );

        // No two consecutive user messages: the assistant turn (with its
        // reasoning item) must sit between the original prompt and "Continue.".
        let mut prev_user = false;
        for m in &second.chat_history {
            match m {
                Message::User { .. } => {
                    assert!(
                        !prev_user,
                        "replayed history must not contain consecutive user messages"
                    );
                    prev_user = true;
                }
                _ => prev_user = false,
            }
        }
    }
}
