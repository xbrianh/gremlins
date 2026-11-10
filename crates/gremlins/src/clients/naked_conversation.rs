//! A concrete, production [`Conversation`] implementation for shell-backed
//! models.
//!
//! [`NakedConversation`] tracks flat message history without a findings
//! ledger. It is purpose-built for single-turn use via [`run_naked`] — no
//! tool layer, no multi-turn loops, no fork/spawn.

use async_trait::async_trait;

use super::conversation::message_text_byte_len;
use super::conversation::{Conversation, RunArtifact, TurnScript, TurnScriptError};

/// A minimal [`Conversation`] implementation for shell-backed models.
///
/// It owns the same durable [`Message`] history and pending-prompt state as
/// the test stub, but lives in the production `clients/` surface so the
/// `cmd` backend (whose model is a shell command) can participate in the
/// agent-loop transcript machinery without depending on
/// `crate::conversation`.
pub(crate) struct NakedConversation {
    history: Vec<rig_core::completion::Message>,
    next_prompt: rig_core::completion::Message,
    preserve_reasoning: bool,
    last_text: String,
    finalized_messages: Option<Vec<rig_core::completion::Message>>,
}

impl NakedConversation {
    pub(crate) fn with_history(prompt: &str, history: Vec<rig_core::completion::Message>) -> Self {
        Self {
            history,
            next_prompt: rig_core::completion::Message::user(prompt.to_string()),
            preserve_reasoning: false,
            last_text: String::new(),
            finalized_messages: None,
        }
    }

    fn box_self(&self) -> Box<dyn Conversation> {
        Box::new(Self {
            history: self.history.clone(),
            next_prompt: self.next_prompt.clone(),
            preserve_reasoning: self.preserve_reasoning,
            last_text: self.last_text.clone(),
            finalized_messages: self.finalized_messages.clone(),
        })
    }
}

#[async_trait]
impl Conversation for NakedConversation {
    fn clone_box(&self) -> Box<dyn Conversation> {
        self.box_self()
    }

    fn record_prose(&mut self, text: &str) {
        self.last_text = text.to_string();
    }

    fn record_turn(
        &mut self,
        _text: &str,
        _reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        _tool_calls: &[rig_core::completion::message::ToolCall],
        _input_tokens: Option<u64>,
    ) {
        unreachable!("NakedConversation: record_turn called in multi-turn path")
    }

    fn record_tool_results(&mut self, _results: Vec<rig_core::completion::Message>) {
        unreachable!("NakedConversation: record_tool_results called in multi-turn path")
    }

    fn pop_last_assistant_turn(&mut self) {
        unreachable!("NakedConversation: pop_last_assistant_turn called in retry path")
    }

    async fn render_turn(
        &mut self,
        _nested: bool,
        system_prompt: Option<String>,
    ) -> Result<TurnScript, TurnScriptError> {
        let limit = crate::config::max_prompt_bytes();
        let sys_bytes = system_prompt.as_ref().map(|s| s.len() as u64).unwrap_or(0);
        let actual: u64 = sys_bytes
            + self
                .history
                .iter()
                .chain(std::iter::once(&self.next_prompt))
                .map(message_text_byte_len)
                .sum::<u64>();
        if actual > limit {
            return Err(TurnScriptError::TooLarge { limit, actual });
        }
        Ok(TurnScript {
            messages: self.history.clone(),
            system_prompt,
            next_user_prompt: self.next_prompt.clone(),
        })
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
        unreachable!("NakedConversation: fork called in task/fork path")
    }

    fn spawn_child(&self, _prompt: &str) -> Box<dyn Conversation> {
        unreachable!("NakedConversation: spawn_child called in task path")
    }

    fn set_next_prompt(&mut self, prompt: &str) {
        self.next_prompt = rig_core::completion::Message::user(prompt.to_string());
    }

    fn inject_operator(&mut self, _text: &str) {
        unreachable!("NakedConversation: inject_operator called in interactive path")
    }

    fn done_rejected(
        &mut self,
        _text: &str,
        _reasoning_items: &[rig_core::completion::message::Sealed<
            rig_core::completion::message::Reasoning,
        >],
        _tool_calls: &[rig_core::completion::message::ToolCall],
        _results: Vec<rig_core::completion::Message>,
        _next_prompt: rig_core::completion::Message,
    ) {
        unreachable!("NakedConversation: done_rejected called in multi-turn path")
    }

    fn messages(&self) -> Vec<rig_core::completion::Message> {
        if let Some(ref msgs) = self.finalized_messages {
            return msgs.clone();
        }
        let mut msgs = self.history.clone();
        msgs.push(self.next_prompt.clone());
        msgs
    }

    fn set_preserve_reasoning(&mut self, preserve: bool) {
        self.preserve_reasoning = preserve;
    }
}
