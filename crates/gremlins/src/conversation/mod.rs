pub(crate) mod compaction;
pub(crate) mod standard;
pub(crate) mod tool_specs;
pub(crate) mod tools;

#[cfg(test)]
use rig_core::completion::Message;

/// Test-only convenience: build a fresh [`StandardConversation`] and box it
/// so tests don't have to spell out the concrete type at every call site.
#[cfg(test)]
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
