pub(crate) mod context;
pub(crate) mod contexts;
pub(crate) mod done;
pub(crate) mod standard;

use crate::clients::conversation::Conversation;
use rig_core::completion::Message;

/// Create a blank [`Conversation`] via the standard implementation,
/// without naming the concrete type.
pub(crate) fn new_blank_conversation(
    prompt: &str,
    preserve_reasoning: bool,
    prepopulated_messages: Option<Vec<Message>>,
) -> Box<dyn Conversation> {
    standard::StandardConversation::blank(prompt, preserve_reasoning, prepopulated_messages)
}