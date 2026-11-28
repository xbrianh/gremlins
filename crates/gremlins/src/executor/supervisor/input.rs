//! Exact execution input. No display subscription or debugger lease is involved.
use std::{future::Future, sync::Arc};

#[cfg(test)]
#[path = "input_adapter_tests.rs"]
mod adapter_tests;

use futures::{stream::FuturesUnordered, StreamExt};
use gremlins_magic_bus::connection::SharedWriter;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc;

use super::input_flow::{acknowledge, WRITE_TIMEOUT};
use super::input_rules::{self, bound_error, CommandAction, PendingReceipts};
pub(crate) use super::input_state::BoundInput;
use super::input_wire::Command;
use super::run_registry::RunRegistry;
use super::session_task::SessionTaskHandle;
use crate::clients::execution_input::{Delivery, InputControl, InputSender};

/// Descriptor-only lookup retained for internal callers; wire routes use `bind_wire`.
#[cfg(test)]
pub(crate) async fn bind(
    request: &Value,
    registry: &Arc<RunRegistry>,
    owner: Option<&SessionTaskHandle>,
) -> Result<BoundInput, String> {
    super::input_state::bind(request, registry, owner)
        .await
        .map_err(|frame| frame["message"].as_str().unwrap_or_default().to_owned())
}

fn command(
    command: Command<'_>,
    sender: &InputSender,
) -> Result<crate::clients::execution_input::Receipt, String> {
    let (req_id, control) = match command {
        Command::Message { req_id, text } => {
            return sender
                .try_send(req_id.to_owned(), text.to_owned())
                .map_err(|e| e.to_string());
        }
        Command::Pause { req_id } => (req_id, InputControl::Pause),
        Command::Step { req_id } => (req_id, InputControl::Step),
        Command::Resume { req_id } => (req_id, InputControl::Resume),
        Command::Bail { req_id } => (req_id, InputControl::Bail),
    };
    sender
        .try_control(req_id.to_owned(), control)
        .map_err(|e| e.to_string())
}

struct WriterTask(tokio::task::JoinHandle<()>);
impl Drop for WriterTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) async fn handle_input(
    request: &Value,
    registry: &Arc<RunRegistry>,
    owner: Option<&SessionTaskHandle>,
    reader: BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let bound = match super::input_state::bind_wire(request, registry, owner).await {
        Ok(bound) => bound,
        Err(error) => {
            let _ = writer.write_json_line(&error).await;
            return;
        }
    };
    serve_bound(request, bound, reader, writer).await;
}

async fn serve_bound(
    request: &Value,
    bound: BoundInput,
    reader: BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let writer = writer.clone();
    serve_bound_io(request, bound, reader, move |frame| {
        let writer = writer.clone();
        async move { writer.write_json_line(&frame).await }
    })
    .await;
}

async fn serve_bound_io<R, W, F>(request: &Value, bound: BoundInput, mut reader: R, write: W)
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: Fn(Value) -> F + Clone + Send + 'static,
    F: Future<Output = Result<(), String>> + Send,
{
    let ack = input_rules::input_bound(&bound.identity, request);
    // Never dispatch pipelined commands until binding has reached the peer.
    let sender = bound.sender.clone();
    let Some(bound) = acknowledge(bound, sender.closed(), write(ack)).await else {
        return;
    };
    // Socket backpressure must not prevent EOF/revocation from releasing control.
    let (output, mut frames) = mpsc::channel::<Value>(64);
    let write = write.clone();
    let mut writer_task = WriterTask(tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            if !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, write(frame)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    }));
    let mut pending = FuturesUnordered::new();
    let mut receipts = PendingReceipts::new();
    loop {
        tokio::select! {
            biased;
            result = pending.next(), if !receipts.is_empty() => {
                if let Some((token, delivery)) = result {
                    if let Some(frame) = receipts.settled(token, delivery) {
                        if output.try_send(frame).is_err() { break; }
                    }
                }
            }
            _ = bound.sender.closed() => {
                // Closure settles every receipt, but incorporation may have won
                // the race. Preserve the original result instead of guessing.
                while let Some((token, delivery)) = pending.next().await {
                    if let Some(frame) = receipts.settled(token, delivery) {
                        if output.try_send(frame).is_err() { break; }
                    }
                }
                for frame in receipts.revoke() {
                    if output.try_send(frame).is_err() { break; }
                }
                break;
            }
            _ = output.closed() => break,
            result = crate::executor::socket::read_json_line(&mut reader) => {
                let request = match result { Ok(Some(request)) => request, _ => break };
                let decoded = match input_rules::command_action(&request, &receipts, output.capacity()) {
                    CommandAction::Detach | CommandAction::Close => break,
                    CommandAction::Enqueue(command) => command,
                    CommandAction::Reject(frame) => {
                        if output.try_send(frame).is_err() { break; }
                        continue;
                    }
                };
                let req_id = request.get("req_id").cloned().unwrap_or(Value::Null);
                match command(decoded, &bound.sender) {
                    Ok(receipt) => {
                        let (token, frame) = receipts.queued(req_id);
                        if output.try_send(frame).is_err() { break; }
                        pending.push(async move { (token, receipt.await.unwrap_or(Delivery::NotDelivered)) });
                    }
                    Err(message) => {
                        if output.try_send(bound_error(&request, &message)).is_err() { break; }
                    }
                }
            }
        }
    }
    drop(bound);
    drop(output);
    // Best-effort bounded drain; the claim is already released.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), &mut writer_task.0).await;
}
