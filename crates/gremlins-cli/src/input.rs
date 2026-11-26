//! Exact, connection-bound native input transport. No retries are performed.
use gremlins::executor::socket;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::BufReader;
use tokio::net::{unix::OwnedWriteHalf, UnixStream};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

type Reply = oneshot::Sender<Result<(), String>>;

#[derive(Default)]
struct State {
    pending: HashMap<String, Reply>,
    failure: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    // A separate descriptor permits synchronous shutdown even during a write.
    shutdown: std::os::unix::net::UnixStream,
}

impl Shared {
    fn fail(&self, reason: impl std::fmt::Display) {
        let error = format!("delivery uncertain: {reason}");
        let mut state = self.state.lock().unwrap();
        if state.failure.is_none() {
            state.failure = Some(error.clone());
            for (_, reply) in state.pending.drain() {
                let _ = reply.send(Err(error.clone()));
            }
        }
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }

    fn receipt(&self, frame: Value) -> Result<(), String> {
        let kind = required(&frame, "type")?;
        if kind == "ended" {
            return Err("input ended without confirmation".into());
        }
        if !matches!(kind, "queued" | "delivered" | "dropped" | "error") {
            return Err("unexpected input receipt".into());
        }
        let id = required(&frame, "req_id")?;
        let mut state = self.state.lock().unwrap();
        if !state.pending.contains_key(id) {
            return Err("unknown or mismatched input receipt".into());
        }
        if kind == "queued" {
            return Ok(());
        }
        let result = match kind {
            "delivered" => Ok(()),
            "dropped" => Err("input not delivered (dropped)".into()),
            "error" => Err(required(&frame, "message")?.to_owned()),
            _ => unreachable!(),
        };
        let reply = state.pending.remove(id).unwrap();
        let _ = reply.send(result);
        Ok(())
    }
}

pub(crate) struct InputClient {
    shared: Arc<Shared>,
    writer: Arc<AsyncMutex<OwnedWriteHalf>>,
    reader: JoinHandle<()>,
}

impl InputClient {
    pub(crate) async fn bind(mut stream: UnixStream, request: Value) -> Result<Self, String> {
        validate_bind(&request)?;
        socket::write_json_line(&mut stream, &request).await?;
        let mut reader = BufReader::new(stream);
        let ack = socket::read_json_line(&mut reader)
            .await?
            .ok_or("input closed before binding acknowledgment")?;
        if ack["type"] == "error" {
            return Err(ack["message"]
                .as_str()
                .unwrap_or("input binding rejected")
                .into());
        }
        if ack["type"] != "input_bound"
            || ["id", "stage_run_id", "req_id"]
                .iter()
                .any(|key| ack.get(*key) != request.get(*key))
        {
            return Err("input binding acknowledgment mismatch".into());
        }
        // Preserve any receipts already buffered after the acknowledgment.
        let buffered = reader.buffer().to_vec();
        let stream = reader.into_inner().into_std().map_err(|e| e.to_string())?;
        let shutdown = stream.try_clone().map_err(|e| e.to_string())?;
        let stream = UnixStream::from_std(stream).map_err(|e| e.to_string())?;
        let (read, write) = stream.into_split();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            shutdown,
        });
        let task_shared = shared.clone();
        let reader = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let prefix = std::io::Cursor::new(buffered);
            let mut reader = BufReader::new(prefix.chain(read));
            loop {
                let result = match socket::read_json_line(&mut reader).await {
                    Ok(Some(frame)) => task_shared.receipt(frame),
                    Ok(None) => Err("input disconnected without confirmation".into()),
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    task_shared.fail(error);
                    break;
                }
            }
        });
        Ok(Self {
            shared,
            writer: Arc::new(AsyncMutex::new(write)),
            reader,
        })
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.shared.state.lock().unwrap().failure.is_some()
    }

    pub(crate) fn close(&self) {
        self.shared.fail("input client closed");
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.shared.state.lock().unwrap().pending.is_empty()
    }

    pub(crate) fn send(
        &self,
        command: Value,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + use<> {
        let registered = self.register(command);
        async move {
            registered?
                .await
                .unwrap_or_else(|_| Err("delivery uncertain: receipt task closed".into()))
        }
    }

    fn register(&self, command: Value) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let id = validate_command(&command)?.to_owned();
        let (reply, received) = oneshot::channel();
        {
            let mut state = self.shared.state.lock().unwrap();
            if let Some(error) = &state.failure {
                return Err(error.clone());
            }
            if state.pending.contains_key(&id) {
                return Err("duplicate inflight input req_id".into());
            }
            state.pending.insert(id, reply);
        }
        // Cancellation must not interrupt a JSON line or remove its receipt slot.
        let writer = self.writer.clone();
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let mut writer = writer.lock().await;
            if shared.state.lock().unwrap().failure.is_some() {
                return;
            }
            if let Err(error) = socket::write_json_line(&mut *writer, &command).await {
                shared.fail(error);
            }
        });
        Ok(received)
    }
}

impl Drop for InputClient {
    fn drop(&mut self) {
        self.close();
        self.reader.abort();
    }
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("input requires nonempty string '{key}'"))
}

fn validate_bind(request: &Value) -> Result<(), String> {
    if required(request, "op")? != "input" {
        return Err("input binding requires op input".into());
    }
    for key in ["id", "stage_run_id", "req_id"] {
        required(request, key)?;
    }
    if let Some(nonce) = request.get("nonce") {
        if !nonce.as_u64().is_some_and(|nonce| nonce != 0) {
            return Err("input requires nonzero integer 'nonce'".into());
        }
    }
    Ok(())
}

fn validate_command(command: &Value) -> Result<&str, String> {
    let id = required(command, "req_id")?;
    // Only command payload fields are allowed; selectors can never redirect a binding.
    let object = command
        .as_object()
        .ok_or("input command must be an object")?;
    let operation = required(command, "cmd")?;
    for key in object.keys() {
        let allowed = matches!(key.as_str(), "cmd" | "req_id")
            || (operation == "message" && key == "text")
            || (operation == "bail" && key == "reason");
        if !allowed {
            return Err(format!("'{key}' is not allowed on a bound input command"));
        }
    }
    match operation {
        "message" => {
            required(command, "text")?;
        }
        "bail" => {
            if command.get("reason").is_some() {
                required(command, "reason")?;
            }
        }
        "pause" | "step" | "resume" => {}
        _ => return Err("unknown input command".into()),
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::time::{timeout, Duration};

    fn request() -> Value {
        json!({"op":"input","id":"run","stage_run_id":"execution","req_id":"bind","nonce":9})
    }

    fn ack() -> Value {
        json!({"type":"input_bound","id":"run","stage_run_id":"execution","req_id":"bind","stage":"agent"})
    }

    async fn frame(peer: &mut BufReader<UnixStream>) -> Value {
        timeout(Duration::from_secs(2), socket::read_json_line(peer))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    async fn emit(peer: &mut BufReader<UnixStream>, value: Value) {
        socket::write_json_line(peer.get_mut(), &value)
            .await
            .unwrap();
    }

    async fn bound() -> (Arc<InputClient>, BufReader<UnixStream>) {
        let (client, peer) = UnixStream::pair().unwrap();
        let task = tokio::spawn(InputClient::bind(client, request()));
        let mut peer = BufReader::new(peer);
        assert_eq!(frame(&mut peer).await, request());
        emit(&mut peer, ack()).await;
        (Arc::new(task.await.unwrap().unwrap()), peer)
    }

    fn send(client: &Arc<InputClient>, cmd: &str, id: &str) -> JoinHandle<Result<(), String>> {
        let client = client.clone();
        let mut command = json!({"cmd":cmd,"req_id":id});
        if cmd == "message" {
            command["text"] = json!("hello");
        }
        tokio::spawn(async move { client.send(command).await })
    }

    async fn result(task: JoinHandle<Result<(), String>>) -> Result<(), String> {
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn binding_requires_exact_native_ack_and_reports_busy() {
        for key in ["type", "id", "stage_run_id", "req_id"] {
            for replacement in [Value::Null, json!("wrong"), json!(42)] {
                let (client, mut peer) = UnixStream::pair().unwrap();
                let task = tokio::spawn(InputClient::bind(client, request()));
                let mut reader = BufReader::new(&mut peer);
                socket::read_json_line(&mut reader).await.unwrap();
                let mut response = ack();
                response[key] = replacement;
                socket::write_json_line(&mut peer, &response).await.unwrap();
                assert!(task.await.unwrap().is_err(), "{key}");
            }
        }
        let (client, peer) = UnixStream::pair().unwrap();
        let task = tokio::spawn(InputClient::bind(client, request()));
        let mut peer = BufReader::new(peer);
        frame(&mut peer).await;
        emit(
            &mut peer,
            json!({"type":"error","req_id":"bind","message":"controller busy"}),
        )
        .await;
        assert_eq!(task.await.unwrap().err().unwrap(), "controller busy");
    }

    #[tokio::test]
    async fn queued_message_does_not_block_controls_and_connection_is_reusable() {
        let (client, mut peer) = bound().await;
        let message = send(&client, "message", "m");
        assert_eq!(frame(&mut peer).await["req_id"], "m");
        emit(&mut peer, json!({"type":"queued","req_id":"m"})).await;
        for cmd in ["pause", "resume", "step", "bail"] {
            let control = send(&client, cmd, cmd);
            assert_eq!(frame(&mut peer).await["cmd"], cmd);
            emit(&mut peer, json!({"type":"queued","req_id":cmd})).await;
            emit(&mut peer, json!({"type":"delivered","req_id":cmd})).await;
            assert!(result(control).await.is_ok());
            assert!(!message.is_finished());
        }
        emit(&mut peer, json!({"type":"delivered","req_id":"m"})).await;
        assert!(result(message).await.is_ok());
    }

    #[tokio::test]
    async fn command_busy_and_dropped_are_terminal_not_connection_failures() {
        let (client, mut peer) = bound().await;
        for (kind, expected) in [("error", "controller busy"), ("dropped", "dropped")] {
            let task = send(&client, "pause", "same");
            frame(&mut peer).await;
            emit(
                &mut peer,
                json!({"type":kind,"req_id":"same","message":"controller busy"}),
            )
            .await;
            assert!(result(task).await.unwrap_err().contains(expected));
            assert!(!client.is_failed());
        }
        let task = send(&client, "resume", "same");
        frame(&mut peer).await;
        emit(&mut peer, json!({"type":"delivered","req_id":"same"})).await;
        assert!(result(task).await.is_ok());
    }

    #[tokio::test]
    async fn disconnect_ended_and_malformed_receipts_fail_all_pending() {
        for response in [
            None,
            Some(json!({"type":"ended"})),
            Some(json!({"type":"delivered","req_id":"unknown"})),
            Some(json!({"type":"delivered","req_id":42})),
            Some(json!({"type":"mystery","req_id":"a"})),
            Some(json!({"type":"error","req_id":"a"})),
            Some(json!({"type":"queued"})),
        ] {
            let (client, mut peer) = bound().await;
            let a = send(&client, "message", "a");
            frame(&mut peer).await;
            emit(&mut peer, json!({"type":"queued","req_id":"a"})).await;
            let b = send(&client, "resume", "b");
            frame(&mut peer).await;
            if let Some(response) = response {
                emit(&mut peer, response).await;
            }
            drop(peer);
            assert!(result(a).await.unwrap_err().contains("uncertain"));
            assert!(result(b).await.unwrap_err().contains("uncertain"));
            assert!(client.is_failed());
            assert!(client
                .send(json!({"cmd":"pause","req_id":"c"}))
                .await
                .unwrap_err()
                .contains("uncertain"));
        }
    }

    #[tokio::test]
    async fn canceled_requests_keep_their_slot_until_terminal_receipt() {
        let (client, mut peer) = bound().await;
        let a = send(&client, "message", "a");
        frame(&mut peer).await;
        a.abort();
        let _ = a.await;
        assert!(client
            .send(json!({"cmd":"pause","req_id":"a"}))
            .await
            .unwrap_err()
            .contains("duplicate"));
        emit(&mut peer, json!({"type":"queued","req_id":"a"})).await;
        let b = send(&client, "resume", "b");
        frame(&mut peer).await;
        emit(&mut peer, json!({"type":"delivered","req_id":"a"})).await;
        emit(&mut peer, json!({"type":"delivered","req_id":"b"})).await;
        assert!(result(b).await.is_ok());
        let again = send(&client, "step", "a");
        frame(&mut peer).await;
        emit(&mut peer, json!({"type":"delivered","req_id":"a"})).await;
        assert!(result(again).await.is_ok());
    }

    #[test]
    fn command_fields_are_operation_specific() {
        for operation in ["message", "bail", "pause", "step", "resume"] {
            let mut command = json!({"cmd":operation,"req_id":"x"});
            if operation == "message" {
                command["text"] = json!("hello");
            }
            assert!(validate_command(&command).is_ok());
            for field in ["text", "reason", "extra"] {
                if (operation == "message" && field == "text")
                    || (operation == "bail" && field == "reason")
                {
                    continue;
                }
                for value in [json!("extraneous"), Value::Null, json!(42)] {
                    let mut invalid = command.clone();
                    invalid[field] = value;
                    assert!(validate_command(&invalid).is_err(), "{invalid}");
                }
            }
        }
        assert!(validate_command(&json!({"cmd":"bail","req_id":"x","reason":"stop"})).is_ok());
        for value in [Value::Null, json!(42), json!(""), json!("  ")] {
            for (operation, field) in [("message", "text"), ("bail", "reason")] {
                let mut invalid = json!({"cmd":operation,"req_id":"x"});
                invalid[field] = value.clone();
                assert!(validate_command(&invalid).is_err(), "{invalid}");
            }
        }
    }

    #[tokio::test]
    async fn close_fails_waiters_and_shuts_down_socket() {
        let (client, mut peer) = bound().await;
        assert!(!client.is_failed());
        let message = send(&client, "message", "m");
        frame(&mut peer).await;
        emit(&mut peer, json!({"type":"queued","req_id":"m"})).await;
        let control = send(&client, "pause", "p");
        frame(&mut peer).await;
        assert!(client.has_pending());
        client.close();
        assert!(client.is_failed());
        assert!(!client.has_pending());
        assert!(result(message).await.unwrap_err().contains("uncertain"));
        assert!(result(control).await.unwrap_err().contains("uncertain"));
        client.close();
        assert!(client
            .send(json!({"cmd":"resume","req_id":"r"}))
            .await
            .unwrap_err()
            .contains("uncertain"));
        assert!(
            timeout(Duration::from_secs(2), socket::read_json_line(&mut peer))
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn drop_closes_socket_and_validation_rejects_redirects() {
        let (client, mut peer) = bound().await;
        for key in [
            "id",
            "stage_run_id",
            "stage",
            "nonce",
            "target",
            "op",
            "run_id",
        ] {
            let mut cmd = json!({"cmd":"pause","req_id":"x"});
            cmd[key] = json!("redirect");
            assert!(client.send(cmd).await.is_err());
        }
        for cmd in [
            json!({"cmd":"detach","req_id":"x"}),
            json!({"cmd":"message","req_id":"x"}),
            json!({"cmd":"pause","req_id":1}),
            json!({"cmd":"pause","req_id":""}),
        ] {
            assert!(client.send(cmd).await.is_err());
        }
        drop(client);
        assert!(
            timeout(Duration::from_secs(2), socket::read_json_line(&mut peer))
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        for key in ["op", "id", "stage_run_id", "req_id"] {
            let mut req = request();
            req[key] = json!("");
            assert!(validate_bind(&req).is_err());
        }
    }
}
