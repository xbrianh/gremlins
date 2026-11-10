//! The default [`Conversation`] implementation used by the agent loop.
//!
//! `StandardConversation` owns the durable message history and the pending user
//! prompt. On each turn the pending prompt is committed into history before the
//! assistant message, so `history + next_prompt` is always the complete
//! transcript. Findings content is prepended to the ephemeral
//! turn prompt so the system preamble stays cache-stable across turns.
//!
//! `render_turn` stashes the rendered user message (with prefixes) so
//! `record_turn`, `done_rejected`, and `finalize` can commit the exact
//! message the model saw rather than the raw `next_prompt`. A
//! `prompt_committed` flag prevents `messages()` from appending a
//! duplicate when `record_turn` has already committed the prompt.
//!
//! ## Compaction
//!
//! [`StandardConversation::compact`] is an on-demand method that
//! replaces the oldest history messages with a signed compaction
//! block (server) or a plain summary (local). The most recent
//! `KEEP_RECENT` user+assistant pairs are preserved verbatim.
//! It is called by the `/compact` TUI command handler; there is no
//! automatic compaction.

use async_trait::async_trait;
use rig_core::completion::message::{Reasoning, Sealed, ToolCall};
use rig_core::completion::Message;
use std::sync::Arc;

use crate::clients::backend::Backend;
use crate::clients::compaction::compaction_block_as_message;
use crate::clients::conversation::{Conversation, RunArtifact, TurnScript, TurnScriptError};
use crate::clients::done_helpers::done_user_message;
use crate::clients::findings::FindingsLedger;
use crate::clients::format::assistant_tool_message;
use crate::conversation::compaction::{
    compaction_cut_index, run_local_compaction, CompactionReport, CompactionStrategy,
};

/// Default number of most-recent turns to keep verbatim during compaction.
const KEEP_RECENT: usize = 10;

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
    /// The user message `render_turn` hands to the model. May carry
    /// findings prefixes that `next_prompt` lacks. Used by
    /// `record_turn`, `done_rejected`, and `finalize` so the committed
    /// transcript matches what the model saw.
    rendered_prompt: Option<Message>,
    /// When `true`, `record_turn` has already committed the prompt into
    /// `history` so `messages()` must not append a duplicate. Reset by
    /// `pop_last_assistant_turn` and `begin_next_turn`.
    prompt_committed: bool,
    /// Declarative findings extracted from assistant prose and surfaced in the
    /// per-turn prompt so the model does not re-derive facts it already stated.
    findings_ledger: FindingsLedger,
    preserve_reasoning_in_history: bool,
    last_text: String,
    /// Snapshot produced by the most recent [`Conversation::finalize`] call.
    /// Once set, [`Conversation::messages`] returns this final transcript so
    /// persistence consumers see the exact durable message list.
    finalized_messages: Option<Vec<Message>>,
    /// Backend for model construction and server-side API calls.
    backend: Option<Arc<dyn Backend>>,
}

impl StandardConversation {
    pub(crate) fn new(
        prompt: &str,
        preserve_reasoning: bool,
        prepopulated_messages: Option<Vec<Message>>,
        backend: Option<Arc<dyn Backend>>,
    ) -> Self {
        let history = prepopulated_messages.unwrap_or_default();
        Self {
            history,
            next_prompt: Message::user(prompt.to_string()),
            rendered_prompt: None,
            prompt_committed: false,
            findings_ledger: FindingsLedger::new(),
            preserve_reasoning_in_history: preserve_reasoning,
            last_text: String::new(),
            finalized_messages: None,
            backend,
        }
    }

    /// Roll prompt-forward bookkeeping after a tool batch.
    fn begin_next_turn(&mut self) {
        self.next_prompt = Message::user("Continue.".to_string());
        self.rendered_prompt = None;
        self.prompt_committed = false;
    }

    fn feed_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.last_text = text.to_string();
        }
        self.findings_ledger.feed(text);
    }

    /// The committed durable transcript only — excludes the unsubmitted
    /// pending prompt that [`messages`](Self::messages) appends. Persistence
    /// callers (e.g. the `/compact` handler) want exactly what was committed,
    /// not a synthetic placeholder prompt.
    pub(crate) fn committed_history(&self) -> Vec<Message> {
        self.history.clone()
    }

    /// Run compaction on the current history. Called by the `/compact` TUI
    /// command handler.
    ///
    /// Returns `None` when there is nothing to compact or every path fails.
    pub(crate) async fn compact(
        &mut self,
        system_prompt: Option<&str>,
        instructions: Option<&str>,
    ) -> Option<CompactionReport> {
        let backend = match &self.backend {
            Some(b) => b,
            None => {
                log::error!("Compaction unavailable: no backend");
                return None;
            }
        };
        let cut = compaction_cut_index(&self.history, KEEP_RECENT);
        if cut == 0 {
            return None;
        }
        let kept = self.history[cut..].to_vec();
        let to_compact = self.history[..cut].to_vec();

        // ── Server-side compaction ──
        if backend.supports_compaction() {
            match backend
                .compact(&to_compact, system_prompt, instructions)
                .await
            {
                Ok(Some(r)) if r.block.is_some() => {
                    let signed = r.block.unwrap();
                    let mut new_history = Vec::with_capacity(1 + kept.len());
                    new_history.push(compaction_block_as_message(&signed));
                    new_history.extend_from_slice(&kept);
                    self.history = new_history;
                    return Some(CompactionReport {
                        strategy: CompactionStrategy::Server,
                        messages_compacted: to_compact.len(),
                        messages_kept: kept.len(),
                        compaction_input_tokens: r.input_tokens,
                        compaction_output_tokens: r.output_tokens,
                        compaction_block: Some(signed),
                    });
                }
                Ok(_) => {
                    log::warn!("Server compaction returned no block; falling back to local");
                }
                Err(e) => {
                    log::warn!("Server compaction failed: {e}; falling back to local");
                }
            }
        }

        // ── Local compaction ──
        let spec = format!("{}/{}", backend.name(), backend.model());
        if let Some((summary_msg, input_tokens, output_tokens)) =
            run_local_compaction(&spec, &to_compact, instructions, system_prompt).await
        {
            let mut new_history = Vec::with_capacity(1 + kept.len());
            new_history.push(summary_msg);
            new_history.extend_from_slice(&kept);
            self.history = new_history;
            return Some(CompactionReport {
                strategy: CompactionStrategy::Local,
                messages_compacted: to_compact.len(),
                messages_kept: kept.len(),
                compaction_input_tokens: input_tokens,
                compaction_output_tokens: output_tokens,
                compaction_block: None,
            });
        }

        None
    }
}

#[async_trait]
impl Conversation for StandardConversation {
    fn clone_box(&self) -> Box<dyn Conversation> {
        Box::new(Self {
            history: self.history.clone(),
            next_prompt: self.next_prompt.clone(),
            rendered_prompt: self.rendered_prompt.clone(),
            prompt_committed: self.prompt_committed,
            findings_ledger: self.findings_ledger.clone(),
            preserve_reasoning_in_history: self.preserve_reasoning_in_history,
            last_text: self.last_text.clone(),
            finalized_messages: self.finalized_messages.clone(),
            backend: self.backend.clone(),
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
        _input_tokens: Option<u64>,
    ) {
        // record_prose already fed the findings ledger and updated last_text;
        // do NOT call feed_text here to avoid double-counting findings.

        // Commit the exact rendered user prompt (with findings
        // prefixes) so the transcript matches what the model saw.
        let prompt = self
            .rendered_prompt
            .take()
            .unwrap_or_else(|| self.next_prompt.clone());
        self.history.push(prompt);
        self.prompt_committed = true;
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
        self.rendered_prompt = None;
        self.prompt_committed = false;
    }

    async fn render_turn(
        &mut self,
        nested: bool,
        system_prompt: Option<String>,
    ) -> Result<TurnScript, TurnScriptError> {
        let combined_system = system_prompt;

        let next_user_prompt = if nested {
            self.next_prompt.clone()
        } else {
            // Prepend per-turn mutable content (findings) to the
            // ephemeral user prompt so the system preamble stays cache-stable
            // across turns.
            let mut prefix = Vec::new();

            let findings = self.findings_ledger.format();
            if !findings.is_empty() {
                prefix.push(findings);
            }

            if prefix.is_empty() {
                self.next_prompt.clone()
            } else {
                // Prefixes ride as text blocks ahead of the real content.
                let mut content = Vec::new();
                let separator = "\n\n---\n\n";
                for p in prefix {
                    content.push(rig_core::completion::message::UserContent::Text(
                        rig_core::completion::message::Text {
                            text: p,
                            additional_params: None,
                        },
                    ));
                }
                content.push(rig_core::completion::message::UserContent::Text(
                    rig_core::completion::message::Text {
                        text: separator.to_string(),
                        additional_params: None,
                    },
                ));
                match self.next_prompt.clone() {
                    Message::User {
                        content: mut existing,
                    } => {
                        content.append(&mut existing);
                        Message::User { content }
                    }
                    other => {
                        // Non-user next_prompt: the content blocks won't merge
                        // correctly, so return the prompt as-is.
                        other
                    }
                }
            }
        };

        // Coarse prompt-size gate: sum text byte lengths of all messages
        // (including system prompt) and reject oversized turns before the
        // model ever sees them.
        let limit = crate::config::max_prompt_bytes();
        let sys_bytes = combined_system
            .as_ref()
            .map(|s| s.len() as u64)
            .unwrap_or(0);
        let actual: u64 = sys_bytes
            + self
                .history
                .iter()
                .chain(std::iter::once(&next_user_prompt))
                .map(crate::clients::conversation::message_text_byte_len)
                .sum::<u64>();
        if actual > limit {
            return Err(TurnScriptError::TooLarge { limit, actual });
        }

        // Stash the rendered prompt so record_turn/done_rejected/finalize
        // can commit the exact message the model saw.
        self.rendered_prompt = Some(next_user_prompt.clone());

        Ok(TurnScript {
            messages: self.history.clone(),
            system_prompt: combined_system,
            next_user_prompt,
        })
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
        // record_turn, so append the rendered prompt (with findings
        // prefixes) if available, otherwise `next_prompt`.
        let prompt = self
            .rendered_prompt
            .take()
            .unwrap_or_else(|| self.next_prompt.clone());
        messages.push(prompt);
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

    fn fork(&self) -> Box<dyn Conversation> {
        let mut history = self.history.clone();
        // The caller appends the current-turn assistant tool-call to history
        // before forking. Carrying it into the child without its still-unknown
        // tool results would leave an unmatched assistant tool-use, so drop only
        // the assistant message — the preceding user prompt (the original task)
        // must stay so the child receives the promised parent context.
        if matches!(history.last(), Some(Message::Assistant { .. })) {
            history.pop();
        }
        Box::new(Self {
            history,
            next_prompt: Message::user(String::new()),
            rendered_prompt: None,
            prompt_committed: false,
            // Fork children start with a clean FindingsLedger and empty
            // last_text — matching the old prepopulated_messages semantics
            // where only raw messages were carried forward.
            findings_ledger: FindingsLedger::new(),
            preserve_reasoning_in_history: self.preserve_reasoning_in_history,
            last_text: String::new(),
            finalized_messages: None,
            backend: self.backend.clone(),
        })
    }

    fn spawn_child(&self, prompt: &str) -> Box<dyn Conversation> {
        Box::new(Self::new(
            prompt,
            self.preserve_reasoning_in_history,
            None,
            self.backend.clone(),
        ))
    }

    fn set_next_prompt(&mut self, prompt: &str) {
        self.next_prompt = Message::user(prompt.to_string());
        self.rendered_prompt = None;
        self.prompt_committed = false;
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
                // Append to the pending tool-result user message.
                let mut merged = existing;
                merged.push(UserContent::Text(Text {
                    text: format!("[operator]: {text}"),
                    additional_params: None,
                }));
                self.next_prompt = Message::User { content: merged };
            } else {
                // Append the operator note to the pending user prompt so
                // the original task is not discarded.
                let mut merged = existing;
                merged.push(UserContent::Text(Text {
                    text: format!("\n\n[operator]: {text}"),
                    additional_params: None,
                }));
                self.history
                    .push(Message::user(format!("[operator]: {text}")));
                self.next_prompt = Message::User { content: merged };
            }
            self.rendered_prompt = None;
        } else {
            self.history
                .push(Message::user(format!("[operator]: {text}")));
            self.next_prompt = Message::user(text);
            self.rendered_prompt = None;
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

        // Commit the rendered prompt (with prefixes) before the assistant
        // message, matching the record_turn invariant.
        let prompt = self
            .rendered_prompt
            .take()
            .unwrap_or_else(|| self.next_prompt.clone());
        self.next_prompt = next_prompt;
        self.history.push(prompt);
        self.prompt_committed = true;
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
        if !self.prompt_committed {
            all.push(self.next_prompt.clone());
        }
        all
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
            model.clone().erase(),
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
        // [system prompt] + [User(stage_prompt), Assistant(tool_call), User(tool_result),
        // User(Continue.)] followed by the fork prompt.
        // That's 6 messages: system + 5 (4 inherited + 1 fork).
        assert_eq!(history.len(), 6, "unexpected fork history: {history:#?}");

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

    /// A Scout child that reads a file must have those contents attached to
    /// the Task tool result in the parent's durable history, not only the
    /// child's own transcript. After a further tool round the evidence must
    /// still be present.
    #[tokio::test]
    async fn scout_read_observations_ride_the_task_tool_result() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        use std::collections::HashMap;
        use std::sync::Arc;

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
                model: child_handle.clone().erase(),
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
            parent.clone().erase(),
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
        let requests = parent.requests();
        let parent_history = &requests.last().unwrap().chat_history;
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
            "child Read evidence must ride the Task tool result, got: {task_result}"
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
            model.clone().erase(),
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
