//! The default [`Conversation`] implementation used by the agent loop.
//!
//! `StandardConversation` owns the durable message history, the pending user
//! prompt, and the findings ledger. It assembles each turn's outgoing model
//! transcript by injecting findings/scratchpad into the pending prompt and then
//! interleaving per-turn context blocks via [`compose_model_transcript`].

use std::any::Any;
use std::hash::{Hash, Hasher};

use rig_core::completion::message::{Reasoning, Sealed, ToolCall};
use rig_core::completion::Message;

use crate::clients::conversation::{Conversation, RunArtifact, TurnScript};
use crate::clients::done_helpers::{done_user_message, user_message_text};
use crate::clients::findings::FindingsLedger;
use crate::clients::format::assistant_tool_message;
use crate::clients::protocol::FileObservations;
use crate::conversation::context::{compose_model_transcript, ConversationContext};
use crate::conversation::contexts::{
    landscape::LandscapeContext, signal::SignalMapContext, tool_ledger::ToolLedger,
};
use crate::landscape::{signals::SignalProfile, LandscapeData};
use crate::task_kind::TaskKind;

/// The standard [`Conversation`] implementation.
pub(crate) struct StandardConversation {
    /// Verbatim durable message history (assistant tool-call messages followed
    /// by their tool results). Non-fork inherited history precedes this loop's
    /// own messages; `current_turn_start` marks that boundary.
    history: Vec<Message>,
    /// The harness's next user message (the stage prompt on turn 0, then
    /// `"Continue."` or a findings/nudge variant).
    next_prompt: Message,
    /// Number of history messages that predate this loop (fork/chat continuation).
    current_turn_start: usize,
    /// Declarative findings extracted from assistant prose and re-injected so
    /// the model does not re-derive facts it already stated.
    findings_ledger: FindingsLedger,
    turn_findings_injected: bool,
    turn_scratchpad_injected: bool,
    last_scratchpad_hash: Option<u64>,
    preserve_reasoning_in_history: bool,
    last_text: String,
    /// Snapshot produced by the most recent [`Conversation::finalize`] call.
    /// Once set, [`Conversation::messages`] returns this final transcript so
    /// persistence consumers see the exact durable message list.
    finalized_messages: Option<Vec<Message>>,
    /// Mutable model contexts that observe tool calls and contribute per-turn
    /// transcript blocks. Owned here so the agent loop routes every context
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
        let current_turn_start = history.len();
        Self {
            history,
            next_prompt: Message::user(prompt.to_string()),
            current_turn_start,
            findings_ledger: FindingsLedger::new(),
            turn_findings_injected: false,
            turn_scratchpad_injected: false,
            last_scratchpad_hash: None,
            preserve_reasoning_in_history: preserve_reasoning,
            last_text: String::new(),
            finalized_messages: None,
            contexts: Vec::new(),
        }
    }

    /// Seed mutable model contexts from raw landscape data.
    ///
    /// Builds a [`ToolLedger`] (always), plus a [`SignalMapContext`] and
    /// [`LandscapeContext`] when `landscape_data` is present.  Contexts are
    /// cloned via `clone_box()` so the conversation owns independent copies.
    /// Called before the conversation is boxed and handed off to the agent loop.
    pub(crate) fn seed_from_landscape(
        &mut self,
        landscape_data: Option<LandscapeData>,
        user_prompt: &str,
        profile: SignalProfile,
    ) {
        let mut contexts: Vec<Box<dyn ConversationContext>> = Vec::new();

        // ToolLedger always goes first — it's the primary per-turn observation context.
        let mut ledger = ToolLedger::new();
        ledger.capture_turn_transcript(0);
        contexts.push(Box::new(ledger) as Box<dyn ConversationContext>);

        if let Some(mut ld) = landscape_data {
            let signal_sections = std::mem::take(&mut ld.signal_sections);
            let symbol_index = std::mem::take(&mut ld.symbol_index);
            let has_signals = !signal_sections.is_empty() || !symbol_index.is_empty();
            if has_signals {
                contexts.push(Box::new(SignalMapContext::new(
                    signal_sections,
                    Some(user_prompt.to_string()),
                    ld.root.clone(),
                    symbol_index,
                    profile,
                )));
            }
            contexts.push(Box::new(LandscapeContext::new(ld)));
        }

        self.contexts = contexts.into_iter().map(|c| c.clone_box()).collect();
    }

    /// Re-inject findings into `next_prompt` when they are missing from the
    /// outgoing request. No-op for nested loops.
    fn inject_findings_if_missing(&mut self, request_messages: &[Message], nested: bool) {
        if nested || self.turn_findings_injected {
            return;
        }
        let findings_block = self.findings_ledger.format();
        if findings_block.is_empty() {
            return;
        }
        let in_request = request_messages
            .iter()
            .any(|msg| user_message_text(msg).contains("## Findings"))
            || user_message_text(&self.next_prompt).contains("## Findings");
        if in_request {
            return;
        }
        let next_text = user_message_text(&self.next_prompt);
        let combined = format!("{next_text}\n\n{findings_block}");
        self.next_prompt = Message::user(combined);
        self.turn_findings_injected = true;
    }

    /// Re-inject the scratchpad into `next_prompt` when its content changed
    /// since the last turn. No-op for nested loops.
    fn inject_scratchpad_if_missing(
        &mut self,
        request_messages: &[Message],
        nested: bool,
        scratchpad: Option<String>,
    ) {
        if nested || self.turn_scratchpad_injected {
            return;
        }
        let Some(content) = scratchpad else {
            return;
        };
        if content.trim().is_empty() {
            self.last_scratchpad_hash = None;
            return;
        }
        let in_request = request_messages
            .iter()
            .any(|msg| user_message_text(msg).contains("## Scratchpad"))
            || user_message_text(&self.next_prompt).contains("## Scratchpad");
        if in_request {
            return;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let current_hash = hasher.finish();
        if self.last_scratchpad_hash == Some(current_hash) {
            return;
        }
        self.last_scratchpad_hash = Some(current_hash);
        let next_text = user_message_text(&self.next_prompt);
        let combined = format!(
            "## Scratchpad\n\nThe following is your persistent scratchpad content:\n\n{content}\n\n---\n\n{next_text}"
        );
        self.next_prompt = Message::user(combined);
        self.turn_scratchpad_injected = true;
    }

    /// Roll prompt-forward bookkeeping after a tool batch.
    fn begin_next_turn(&mut self) {
        let findings = self.findings_ledger.format();
        let mut next_text = "Continue.".to_string();
        if !findings.is_empty() {
            next_text = format!("{next_text}\n\n{findings}");
            self.turn_findings_injected = true;
        } else {
            self.turn_findings_injected = false;
        }
        self.next_prompt = Message::user(next_text);
        self.turn_scratchpad_injected = false;
    }

    fn feed_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.last_text = text.to_string();
        }
        self.findings_ledger.feed(text);
    }
}

impl Conversation for StandardConversation {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn clone_box(&self) -> Box<dyn Conversation> {
        Box::new(Self {
            history: self.history.clone(),
            next_prompt: self.next_prompt.clone(),
            current_turn_start: self.current_turn_start,
            findings_ledger: self.findings_ledger.clone(),
            turn_findings_injected: self.turn_findings_injected,
            turn_scratchpad_injected: self.turn_scratchpad_injected,
            last_scratchpad_hash: self.last_scratchpad_hash,
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
        // The pending user prompt is a separate field, not in history.
        // Only the assistant message (and any trailing results from the
        // same turn) need removal for clean turn re-execution.
        if matches!(self.history.last(), Some(Message::Assistant { .. })) {
            self.history.pop();
        }
    }

    fn render_turn(
        &mut self,
        stage_prompt: &str,
        turn: usize,
        nested: bool,
        scratchpad: Option<String>,
        system_prompt: Option<String>,
    ) -> TurnScript {
        let request_messages = self.history.clone();
        self.inject_findings_if_missing(&request_messages, nested);
        self.inject_scratchpad_if_missing(&request_messages, nested, scratchpad);

        let composed = compose_model_transcript(
            stage_prompt,
            &request_messages,
            &self.next_prompt,
            &self.contexts,
            turn,
            self.current_turn_start,
        );
        let (history_msgs, final_prompt) = composed
            .split_last()
            .map(|(last, rest)| (rest.to_vec(), last.clone()))
            .unwrap_or_else(|| (vec![], self.next_prompt.clone()));

        let context_system_msgs: Vec<String> = self
            .contexts
            .iter()
            .filter_map(|ctx| ctx.system_message())
            .collect();
        let combined_system = match (context_system_msgs.is_empty(), system_prompt) {
            (true, None) => None,
            (true, Some(sys)) => Some(sys),
            (false, None) => Some(context_system_msgs.join("\n\n")),
            (false, Some(sys)) => Some(format!("{}\n\n{}", context_system_msgs.join("\n\n"), sys)),
        };

        TurnScript {
            messages: history_msgs,
            system_prompt: combined_system,
            // Pass the final message verbatim so ToolResult blocks survive.
            next_user_prompt: final_prompt,
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
        if matches!(history.last(), Some(Message::Assistant { .. })) {
            history.pop();
        }
        let current_turn_start = history.len();
        Box::new(Self {
            history,
            next_prompt: Message::user(String::new()),
            current_turn_start,
            // Fork children start with a clean FindingsLedger and empty
            // last_text — matching the old prepopulated_messages semantics
            // where only raw messages were carried forward.
            findings_ledger: FindingsLedger::new(),
            turn_findings_injected: false,
            turn_scratchpad_injected: false,
            last_scratchpad_hash: None,
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
        self.history.push(assistant_tool_message(
            text,
            reasoning_items,
            tool_calls,
            self.preserve_reasoning_in_history,
        ));
        self.history.extend(results);
        self.next_prompt = next_prompt;
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

    fn join_child_conversation(&mut self, child: &dyn Conversation) -> Vec<String> {
        // Downcast is safe: StandardConversation is the only Conversation impl.
        let child = child
            .as_any()
            .downcast_ref::<StandardConversation>()
            .expect("join_child_conversation: child must be StandardConversation");
        let snapshot: Vec<Box<dyn ConversationContext>> =
            child.contexts.iter().map(|c| c.clone_box()).collect();
        if snapshot.is_empty() {
            return Vec::new();
        }
        // Match child contexts to parent contexts by TypeId and join.
        let mut task_observations = Vec::new();
        for child_ctx in &snapshot {
            let child_tid = child_ctx.as_any().type_id();
            for parent_ctx in &mut self.contexts {
                if parent_ctx.as_any().type_id() == child_tid {
                    parent_ctx.join(child_ctx.as_ref());
                }
            }
        }
        for parent_ctx in &mut self.contexts {
            let text = parent_ctx.new_observations_text();
            if !text.is_empty() {
                task_observations.push(text);
            }
        }
        task_observations
    }

    fn finalize_contexts(&mut self, turn: usize) {
        for ctx in &mut self.contexts {
            ctx.finalize_turn(turn);
        }
    }

    fn file_observations(&self) -> Option<FileObservations> {
        self.contexts.iter().find_map(|ctx| ctx.file_observations())
    }

    fn clear_context_transcripts(&mut self) {
        for ctx in &mut self.contexts {
            ctx.clear_transcript();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use rig_core::completion::Message;
    use rig_core::message::{AssistantContent, Issuer, UserContent};

    use crate::clients::agent_loop::*;
    use crate::clients::cancel::CancelToken;
    use crate::clients::done_helpers::user_message_text;
    use crate::clients::format::ledger_key_arg;
    use crate::conversation::new_blank_conversation;

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
        // The child inherits the parent's conversation but no longer sees a
        // separate [seen] block — context content travels through
        // compose_model_transcript.  The inherited ledger is cleared via
        // clear_transcript so the context blocks are empty.
        assert_eq!(history.len(), 4, "unexpected fork history: {history:#?}");

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

    #[tokio::test]
    async fn loop_ledger_follows_all_tool_results() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-ledger-multi-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, "alpha").unwrap();
        std::fs::write(&f2, "beta").unwrap();
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({"file_path": f1.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c2",
                    "Read",
                    serde_json::json!({"file_path": f2.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        run_agent_loop(
            gm(model.clone().erase()),
            "read both",
            ctx.clone(),
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        let reqs = model.requests();
        let history: Vec<&Message> = reqs[1].chat_history.iter().collect();
        // The [seen] block travels through user-message transcript blocks
        // (The ledger's system_message() returns None to keep the system
        // prompt cache-stable).
        assert!(history.len() >= 4, "unexpected history: {history:#?}");
        let seen_msg = history
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content: _ } => {
                    let text = user_message_text(m);
                    if text.contains("[seen]") {
                        Some(text)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("[seen] block not found in any user message: {history:#?}"));
        assert!(
            seen_msg.contains("a.txt") && seen_msg.contains("b.txt"),
            "ledger must list both actions: {seen_msg}"
        );
    }

    #[tokio::test]
    async fn loop_ledger_injected_into_history() {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-oa-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, "content").unwrap();

        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Read",
                    serde_json::json!({"file_path": f.to_str().unwrap()}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                rig_core::test_utils::MockStreamEvent::text("done"),
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": "done"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut ctx = test_ctx(Some(dir.clone()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        let cancel = CancelToken::new();
        run_agent_loop(
            gm(model.clone().erase()),
            "read",
            ctx.clone(),
            cancel,
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        let reqs = model.requests();
        assert_eq!(reqs.len(), 2);
        let history: Vec<&Message> = reqs[1].chat_history.iter().collect();
        let key = ledger_key_arg(&serde_json::json!({"file_path": f.to_str().unwrap()}));
        assert!(key.contains("x.txt"), "ledger must name the file: {key}");
        // The [seen] block travels through user-message transcript blocks
        // (The ledger's system_message() returns None to keep the system
        // prompt cache-stable).
        assert!(history.len() >= 3, "unexpected history: {history:#?}");
        let seen_msg = history
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content: _ } => {
                    let text = user_message_text(m);
                    if text.contains("[seen]") {
                        Some(text)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("[seen] block not found in any user message: {history:#?}"));
        assert!(
            seen_msg.contains(&key),
            "ledger must name the file: {seen_msg}"
        );
        assert!(
            seen_msg.contains("R("),
            "ledger must show Read indicator: {seen_msg}"
        );
        // The history must contain a tool result.
        let has_result = history.iter().any(|m| {
            matches!(m, Message::User { content } if content.iter().any(|c| matches!(c, UserContent::ToolResult(_))))
        });
        assert!(has_result, "history must contain a tool result");
        // The history must contain an assistant message.
        let has_assistant = history
            .iter()
            .any(|m| matches!(m, Message::Assistant { .. }));
        assert!(has_assistant, "history must contain an assistant message");
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

    /// Regression: a Scout child that reads a file must have those contents
    /// attached to the Task tool result in the parent's durable history, not
    /// only the ephemeral next_prompt. After a further tool round the evidence
    /// must still be present even though the ledger marks the file as read.
    #[tokio::test]
    async fn scout_read_observations_ride_the_task_tool_result() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("note.txt");
        std::fs::write(&note, "evidence-from-child").unwrap();

        let task_call = MockStreamEvent::tool_call(
            "t1",
            "Task",
            serde_json::json!({ "kind": "Scout", "prompt": "inspect note.txt" }),
        );
        let done_call = |id: &str, summary: &str| {
            MockStreamEvent::tool_call(id, "Done", serde_json::json!({ "summary": summary }))
        };

        // Parent: delegate, then finish.
        let parent = MockCompletionModel::from_stream_turns([
            vec![
                task_call,
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                done_call("d1", "outer done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        // Child (Scout): Read the file, then finish.
        let child = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call(
                    "r1",
                    "Read",
                    serde_json::json!({ "file_path": note.to_str().unwrap() }),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("scout result"),
                done_call("d2", "scout result"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);

        let child_handle = child.clone();
        let factory: crate::clients::task::TaskModelFactory = Arc::new(move |_spec: &str| {
            Ok(crate::clients::task::TaskModel {
                model: gm(child_handle.clone().erase()),
                temperature: None,
                max_tokens: None,
                extra: None,
                preserve_reasoning: None,
            })
        });
        let selector = crate::clients::task::TaskModelSelector::new(
            HashMap::from([(crate::task_kind::TaskKind::Scout, "openai/mini".to_string())]),
            factory,
        )
        .expect("configured");
        let mut ctx = test_ctx(Some(dir.path().to_path_buf()), None);
        ctx.idle_timeout = 5.0;
        ctx.params.idle_timeout = Some(5.0);
        ctx.params.task_models = Some(selector);

        run_agent_loop(
            gm(parent.clone().erase()),
            "go",
            ctx,
            CancelToken::new(),
            loop_opts(None),
            None,
        )
        .await
        .unwrap();

        // The parent's second request must carry the child's Read evidence
        // inside the Task tool result (durable history).
        let reqs = parent.requests();
        let parent_history = &reqs.last().unwrap().chat_history;
        let task_result = parent_history
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::ToolResult(r) => Some(
                            r.content
                                .iter()
                                .filter_map(|rc| match rc {
                                    rig_core::completion::message::ToolResultContent::Text(t) => {
                                        Some(t.text.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .find(|t| t.contains("scout result"))
            .expect("task result must reach the parent history");
        assert!(
            task_result.contains("evidence-from-child"),
            "joined Read observations must ride the Task tool result, got: {task_result}"
        );
    }
}
