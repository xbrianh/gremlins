use rig_core::completion::Message;

/// Extract the concatenated text blocks from a user message.
pub(crate) fn user_message_text(message: &Message) -> String {
    use rig_core::completion::message::UserContent;
    match message {
        Message::User { content } => content
            .iter()
            .filter_map(|item| match item {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

/// Build the merged Done action-ledger + tool_result user message.
pub(crate) fn done_user_message(
    ledger: &str,
    call_id: rig_core::completion::message::CallId,
    tool_name: rig_core::completion::message::ToolName,
    result_text: &str,
) -> Message {
    use rig_core::completion::message::{Text, ToolResult, ToolResultContent, UserContent};
    Message::User {
        content: vec![
            UserContent::ToolResult(ToolResult {
                call: call_id,
                name: tool_name,
                content: vec![ToolResultContent::Text(Text {
                    text: result_text.to_string(),
                    additional_params: None,
                })],
            }),
            UserContent::Text(Text {
                text: ledger.to_string(),
                additional_params: None,
            }),
        ],
    }
}