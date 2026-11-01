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

use crate::clients::findings::FindingsLedger;
use crate::clients::format::assistant_tool_message;
use crate::clients::conversation::{Conversation, RunArtifact, TurnScript};
use crate::conversation::context::{compose_model_transcript, ConversationContext};
use crate::clients::protocol::FileObservations;
use crate::conversation::contexts::{
    landscape::LandscapeContext, signal::SignalMapContext, tool_ledger::ToolLedger,
};
use crate::conversation::done::{done_user_message, user_message_text};
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

    fn pop_last_two(&mut self) {
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
            contexts: self.contexts.iter().map(|c| c.clone_box()).collect(),
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
        let propagating = self.contexts
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

    fn ensure_contexts(&mut self) {
        if self.contexts.is_empty() {
            use crate::conversation::contexts::tool_ledger::ToolLedger;
            let mut ledger = ToolLedger::new();
            ledger.capture_turn_transcript(0);
            self.contexts.push(Box::new(ledger) as Box<dyn ConversationContext>);
        }
    }
}
