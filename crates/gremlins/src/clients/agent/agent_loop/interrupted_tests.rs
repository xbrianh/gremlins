use crate::clients::agent::agent_loop::{loop_opts, run_agent_loop, test_ctx, RunContext};
use crate::clients::cancel::CancelToken;
use crate::clients::interactive::InteractiveSession;
use crate::conversation::new_blank_conversation;
use rig_core::completion::message::{AssistantContent, AssistantMessage, ToolCall, ToolFunction};
use rig_core::completion::Message;
use rig_core::message::ToolName;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent as Event};
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::time::timeout;

const PROMPT: &str = "Describe the old implementation.";
const FIRST: &str = "  First block. ";
const SECOND: &str = "Second block.\n";
const WAIT: Duration = Duration::from_secs(5);

fn two_blocks() -> Vec<Event> {
    vec![
        Event::text(FIRST),
        Event::text_start("second", None), // closes the first block
        Event::text(SECOND),               // delta only until the next block or EOF
    ]
}

fn finish() -> Vec<Event> {
    vec![
        Event::tool_call("finish", "Done", json!({"summary": "finished"})),
        Event::final_response_with_default_usage(),
    ]
}

/// Preserve a previous, answered tool call in every test: committing or rolling
/// back the current prompt must never remove the preceding result.
fn earlier_messages() -> Vec<Message> {
    let read = ToolCall::from_wire(
        "prior-read",
        ToolFunction::new(ToolName::new("Read").unwrap(), json!({"file_path": "old"})),
    );
    vec![
        Message::user("Read the old file."),
        Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
            read.clone(),
        )])),
        Message::tool_result(read.id, read.function.name, "old contents"),
    ]
}

fn standard_ctx(cwd: &Path, id: &str) -> RunContext {
    let mut ctx = test_ctx(Some(cwd.to_path_buf()), None);
    ctx.idle_timeout = 30.0; // only the operator, never the idle timeout, ends the held stream
    ctx.params.idle_timeout = Some(ctx.idle_timeout);
    ctx.params.gremlin_id = Some(id.to_owned());
    ctx.prepopulated_conversation = new_blank_conversation(PROMPT, false, Some(earlier_messages()));
    ctx
}

#[tokio::test]
async fn uninterrupted_text_blocks_are_finalized_once() {
    let id = "history-uninterrupted";
    let dir = tempfile::tempdir().unwrap();
    let mut events = two_blocks();
    events.extend(finish());
    let model = MockCompletionModel::from_stream_turns([events]);
    let run = timeout(
        WAIT,
        run_agent_loop(
            model.clone().erase(),
            PROMPT,
            standard_ctx(dir.path(), id),
            CancelToken::new(),
            loop_opts(None),
            InteractiveSession::unattached(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(model.request_count(), 1);
    let messages = run.messages().unwrap();
    let [prompt, Message::Assistant(msg), _done_result] = &messages[earlier_messages().len()..]
    else {
        panic!("unexpected finalized transcript: {messages:?}");
    };
    assert_eq!(prompt, &Message::user(PROMPT));
    let [AssistantContent::Text(text), AssistantContent::ToolCall(_done)] = msg.content.as_slice()
    else {
        panic!("unexpected completed response: {msg:?}");
    };
    assert_eq!(text.text, format!("{FIRST}{SECOND}"));
}
