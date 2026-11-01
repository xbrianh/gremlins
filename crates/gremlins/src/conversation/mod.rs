pub(crate) mod context;
pub(crate) mod contexts;
pub(crate) mod standard;

#[cfg(test)]
use rig_core::completion::Message;

/// Test-only convenience: build a fresh [`StandardConversation`], seed the
/// default tool-ledger context (as the executor would), and box it so tests
/// don't have to spell out the concrete type or context setup at every call site.
#[cfg(test)]
pub(crate) fn new_blank_conversation(
    prompt: &str,
    preserve_reasoning: bool,
    prepopulated_messages: Option<Vec<Message>>,
) -> Box<dyn crate::clients::conversation::Conversation> {
    let mut conv =
        standard::StandardConversation::new(prompt, preserve_reasoning, prepopulated_messages);
    conv.seed_from_landscape(None, "", crate::landscape::signals::SignalProfile::Full);
    Box::new(conv)
}
