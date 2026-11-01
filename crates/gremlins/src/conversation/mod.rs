pub(crate) mod context;
pub(crate) mod contexts;
pub(crate) mod done;
pub(crate) mod standard;

use rig_core::completion::Message;

/// Create a blank [`crate::clients::conversation::Conversation`] via
/// the standard implementation, without naming the concrete type.
pub(crate) fn new_blank_conversation(
    prompt: &str,
    preserve_reasoning: bool,
    prepopulated_messages: Option<Vec<Message>>,
) -> Box<dyn crate::clients::conversation::Conversation> {
    Box::new(standard::StandardConversation::new(
        prompt,
        preserve_reasoning,
        prepopulated_messages,
    ))
}
