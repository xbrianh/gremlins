//! Persistent conversation transport. UI targets and generations stay with the caller.

use std::sync::Arc;

use super::{socket, BufReader, DaemonClient, Execution, Selector};
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub(crate) struct Frame {
    pub execution: Option<Execution>,
    pub value: Value,
}

pub(crate) struct Transport {
    pub commands: mpsc::UnboundedSender<Value>,
    pub frames: mpsc::UnboundedReceiver<Frame>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DaemonClient {
    pub(crate) fn start_attachment(
        self: &Arc<Self>,
        run: String,
        stage: Option<String>,
        mode: &'static str,
    ) -> Transport {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (tx, frames) = mpsc::unbounded_channel();
        let client = Arc::clone(self);
        let task = tokio::spawn(async move {
            let selector = match stage.as_deref() {
                Some(stage) => Selector::Stage { run: &run, stage },
                None => Selector::Run { run: &run },
            };
            let mut execution = None;
            if let Err(reason) = client
                .run_attachment(selector, mode, command_rx, &tx, &mut execution)
                .await
            {
                let _ = tx.send(Frame {
                    execution,
                    value: json!({"type":"transport_error", "reason":reason}),
                });
            }
        });
        Transport {
            commands,
            frames,
            task,
        }
    }

    async fn run_attachment(
        &self,
        selector: Selector<'_>,
        mode: &str,
        mut commands: mpsc::UnboundedReceiver<Value>,
        tx: &mpsc::UnboundedSender<Frame>,
        execution: &mut Option<Execution>,
    ) -> Result<(), String> {
        let session = self.session_id().await;
        let stream =
            socket::connect_session_socket(&gremlins::config::state_root(), &session).await?;
        let mut reader = BufReader::new(stream);
        let mut request = selector.request(&session);
        request["mode"] = json!(mode);
        socket::write_json_line(reader.get_mut(), &request).await?;
        let ack = read_ack(&mut reader).await?;
        *execution = Some(selector.observed_binding(&ack)?);
        if tx
            .send(Frame {
                execution: execution.clone(),
                value: ack,
            })
            .is_err()
        {
            return Ok(());
        }
        let pinned = execution.as_ref().expect("acknowledgement pins execution");
        loop {
            let value = tokio::select! {
                frame = socket::read_json_line(&mut reader) => frame?.ok_or("attachment disconnected")?,
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()) };
                    socket::write_json_line(reader.get_mut(), &command).await?;
                    continue;
                }
            };
            if is_broadcast(&value)
                || value
                    .get("execution_id")
                    .is_some_and(|v| v != &pinned.execution_id)
                || value
                    .get("nonce")
                    .is_some_and(|v| v.as_u64() != Some(pinned.nonce))
            {
                continue;
            }
            let terminal = is_terminal(&value);
            if tx
                .send(Frame {
                    execution: execution.clone(),
                    value,
                })
                .is_err()
                || terminal
            {
                return Ok(());
            }
        }
    }
}

pub(super) async fn read_ack(
    reader: &mut BufReader<tokio::net::UnixStream>,
) -> Result<Value, String> {
    loop {
        let value = socket::read_json_line(reader)
            .await?
            .ok_or("attachment disconnected")?;
        if !is_broadcast(&value) {
            return Ok(value);
        }
    }
}

fn is_broadcast(value: &Value) -> bool {
    match value["type"].as_str() {
        Some(t) if t.starts_with("run_") => true,
        Some("event" | "stage_transition" | "token_usage" | "log_line" | "bail") => true,
        _ => false,
    }
}

pub(crate) fn is_terminal(value: &Value) -> bool {
    matches!(value["type"].as_str(), Some("ended" | "detached" | "done"))
        && !(value["type"] == "ended" && value["reason"] == "resumed")
}
