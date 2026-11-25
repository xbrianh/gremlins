//! Exact execution input. No display subscription or debugger lease is involved.
use std::sync::Arc;

use futures::{stream::FuturesUnordered, StreamExt};
use gremlins_magic_bus::connection::SharedWriter;
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc;

use super::error_response;
use super::run_registry::{ControllerClaim, RunRegistry};
use super::session_task::SessionTaskHandle;
use crate::clients::execution_input::{Delivery, ExecutionIdentity, InputControl, InputSender};

pub(crate) struct BoundInput {
    pub sender: InputSender,
    pub identity: ExecutionIdentity,
    _claim: ControllerClaim,
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing or invalid '{key}'"))
}

fn descriptor(request: &Value) -> Result<&Value, String> {
    let target = request.get("target").unwrap_or(request);
    if !target.is_object() || target.get("target").is_some() {
        return Err("target must be a flat descriptor object".into());
    }
    if request.get("target").is_some() {
        for key in ["id", "stage", "stage_run_id", "nonce"] {
            if request.get(key).is_some() {
                return Err(format!("'{key}' conflicts with target descriptor"));
            }
        }
    }
    Ok(target)
}

/// Resolve once, then acquire exclusive control of that exact invocation.
/// `owner` must come from the verified socket/session context, never the request.
pub(crate) async fn bind(
    request: &Value,
    registry: &Arc<RunRegistry>,
    owner: Option<&SessionTaskHandle>,
) -> Result<BoundInput, String> {
    if let Some(error) = super::attachment::retired_target_error(request) {
        return Err(error["message"]
            .as_str()
            .unwrap_or("retired target field")
            .into());
    }
    let target = descriptor(request)?;
    let nonce = target
        .get("nonce")
        .map(|value| {
            value
                .as_u64()
                .filter(|nonce| *nonce != 0)
                .ok_or_else(|| "missing or invalid 'nonce'".to_string())
        })
        .transpose()?;
    let run_id = required(target, "id")?;
    crate::executor::gremlin::validate_gremlin_id(run_id).map_err(|e| e.to_string())?;
    let stage_run_id = required(target, "stage_run_id")?;
    let stage = if target.get("stage").is_some() {
        Some(required(target, "stage")?)
    } else {
        None
    };
    let (interactive, store, _) = registry
        .get_run_interactive(run_id)
        .await
        .ok_or_else(|| "run is not live".to_string())?;
    if store.read_str("kind") == "chat" {
        let owner = owner.ok_or_else(|| "chat input requires its owning session".to_string())?;
        if owner.is_closed().await || !owner.get_chat_ids().await.iter().any(|id| id == run_id) {
            return Err("chat input requires its owning session".into());
        }
    }
    let (identity, sender) = interactive
        .resolve_input(run_id, stage_run_id, stage)
        .ok_or_else(|| {
            // Diagnostic only: a debugger snapshot can neither authorize input
            // nor rule out a sibling registration. Consult it only after failure.
            let target = interactive.target();
            if target.stage_run_id() == Some(stage_run_id) && !target.end_signal().is_ended() {
                if target.accepts_commands() {
                    "execution input is unavailable: input registration is not ready (execution may be preparing)".to_string()
                } else {
                    "execution input is unavailable: execution/backend does not support input".to_string()
                }
            } else {
                "execution input is unavailable or stale: no matching live input registration (execution may be preparing or backend may not support input)".to_string()
            }
        })?;
    if nonce.is_some_and(|nonce| sender.nonce() != Some(nonce)) {
        return Err("execution has been replaced; target is stale".into());
    }
    let claim = registry.claim_controller(&identity.run_id, &identity.stage_run_id)?;
    if sender.is_closed() {
        return Err("execution input is revoked".into());
    }
    Ok(BoundInput {
        sender,
        identity,
        _claim: claim,
    })
}

fn command(
    request: &Value,
    sender: &InputSender,
) -> Result<crate::clients::execution_input::Receipt, String> {
    if let Some(error) = super::attachment::retired_target_error(request) {
        return Err(error["message"]
            .as_str()
            .unwrap_or("retired target field")
            .into());
    }
    // A bound connection cannot be redirected, even to a same-name successor.
    for key in ["target", "id", "stage", "stage_run_id", "nonce"] {
        if request.get(key).is_some() {
            return Err(format!("'{key}' is not allowed on a bound input command"));
        }
    }
    let req_id = required(request, "req_id")?.to_owned();
    match required(request, "cmd")? {
        "message" => sender.try_send(req_id, required(request, "text")?.to_owned()),
        "pause" => sender.try_control(req_id, InputControl::Pause),
        "step" => sender.try_control(req_id, InputControl::Step),
        "resume" => sender.try_control(req_id, InputControl::Resume),
        "bail" => sender.try_control(req_id, InputControl::Bail),
        _ => return Err("unknown input command".into()),
    }
    .map_err(|e| e.to_string())
}

const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    if let Err(message) = required(request, "req_id") {
        let mut error = error_response(&message);
        error["req_id"] = request.get("req_id").cloned().unwrap_or(Value::Null);
        let _ = writer.write_json_line(&error).await;
        return;
    }
    let bound = match bind(request, registry, owner).await {
        Ok(bound) => bound,
        Err(message) => {
            let mut error = error_response(&message);
            error["req_id"] = request.get("req_id").cloned().unwrap_or(Value::Null);
            let _ = writer.write_json_line(&error).await;
            return;
        }
    };
    serve_bound(request, bound, reader, writer).await;
}

async fn serve_bound(
    request: &Value,
    bound: BoundInput,
    mut reader: BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let identity = &bound.identity;
    let ack = json!({"type":"input_bound", "id":identity.run_id,
        "stage":identity.stage_name, "stage_run_id":identity.stage_run_id,
        "req_id":request.get("req_id")});
    // Never dispatch pipelined commands until binding has reached the peer.
    tokio::select! {
        biased;
        _ = bound.sender.closed() => return,
        result = tokio::time::timeout(WRITE_TIMEOUT, writer.write_json_line(&ack)) => {
            if !matches!(result, Ok(Ok(()))) { return; }
        }
    }
    // Socket backpressure must not prevent EOF/revocation from releasing control.
    let (output, mut frames) = mpsc::channel::<Value>(64);
    let socket_writer = writer.clone();
    let mut writer_task = WriterTask(tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            if !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, socket_writer.write_json_line(&frame)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    }));
    let mut pending = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            result = pending.next(), if !pending.is_empty() => {
                if let Some((req_id, delivery)) = result {
                    let kind = if delivery == Delivery::Delivered { "delivered" } else { "dropped" };
                    if output.try_send(json!({"type":kind,"req_id":req_id})).is_err() { break; }
                }
            }
            _ = bound.sender.closed(), if pending.is_empty() => {
                let _ = output.try_send(json!({"type":"ended"}));
                break;
            }
            _ = output.closed() => break,
            result = crate::executor::socket::read_json_line(&mut reader) => {
                let request = match result { Ok(Some(request)) => request, _ => break };
                if request.get("cmd").and_then(Value::as_str) == Some("detach") { break; }
                // Bound both receipt tracking and output independently of the socket.
                if output.capacity() < 2 { break; }
                let req_id = request.get("req_id").cloned().unwrap_or(Value::Null);
                if pending.len() >= 32 {
                    if output.try_send(json!({"type":"error", "message":"too many pending input receipts", "req_id":req_id})).is_err() { break; }
                    continue;
                }
                match command(&request, &bound.sender) {
                    Ok(receipt) => {
                        if output.try_send(json!({"type":"queued","req_id":req_id})).is_err() { break; }
                        pending.push(async move { (req_id, receipt.await.unwrap_or(Delivery::NotDelivered)) });
                    }
                    Err(message) => {
                        if output.try_send(json!({"type":"error","message":message,"req_id":req_id})).is_err() { break; }
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

#[cfg(test)]
mod tests {
    use super::*;
    fn inbox() -> (
        InputSender,
        crate::clients::execution_input::InputReceiver,
        Arc<crate::clients::execution_input::InputRevoker>,
    ) {
        let registry = crate::clients::execution_input::InputRegistry::default();
        let identity = ExecutionIdentity {
            run_id: "run".into(),
            stage_name: "build".into(),
            stage_run_id: "nonce".into(),
        };
        let (receiver, revoker) = registry.register(identity.clone(), |_| {});
        (registry.sender(&identity).unwrap(), receiver, revoker)
    }

    #[tokio::test]
    async fn message_receipt_requires_consumer_confirmation() {
        let (sender, mut receiver, _revoker) = inbox();
        let mut receipt = command(
            &json!({"cmd":"message", "text":"fix", "req_id":"one"}),
            &sender,
        )
        .unwrap();
        assert!(matches!(
            receipt.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let crate::clients::execution_input::InputCommand::Correction(correction) =
            receiver.recv().await.unwrap()
        else {
            panic!("expected correction")
        };
        assert!(correction.incorporate(|id, text| {
            assert_eq!(id, "one");
            assert_eq!(text, "fix");
        }));
        assert_eq!(receipt.await.unwrap(), Delivery::Delivered);
    }

    #[tokio::test]
    async fn controls_have_receipts_and_revocation_drops_them() {
        let (sender, mut receiver, revoker) = inbox();
        for cmd in ["pause", "step", "resume", "bail"] {
            let receipt = command(&json!({"cmd":cmd, "req_id":cmd}), &sender).unwrap();
            let crate::clients::execution_input::InputCommand::Control(control) =
                receiver.recv().await.unwrap()
            else {
                panic!("expected control")
            };
            assert!(control.incorporate(|_, _| {}));
            assert_eq!(receipt.await.unwrap(), Delivery::Delivered);
        }
        let receipt = command(
            &json!({"cmd":"message", "text":"fix", "req_id":"last"}),
            &sender,
        )
        .unwrap();
        revoker.revoke();
        assert_eq!(receipt.await.unwrap(), Delivery::NotDelivered);
    }

    #[test]
    fn bound_commands_reject_retargeting_and_retired_fields() {
        let (sender, mut receiver, _revoker) = inbox();
        for key in [
            "target",
            "id",
            "stage",
            "stage_run_id",
            "stage_id",
            "execution_id",
        ] {
            let mut request = json!({"cmd":"message", "text":"fix", "req_id":"one"});
            request[key] = json!("other");
            assert!(command(&request, &sender).is_err(), "{key}");
        }
        assert!(receiver.try_recv().is_err());
    }

    fn registry() -> Arc<RunRegistry> {
        Arc::new(RunRegistry::new(tokio::sync::watch::channel(false).0))
    }

    #[tokio::test]
    async fn bind_rejections_echo_request_id_and_require_it() {
        let registry = registry();
        for req_id in [
            Value::Null,
            json!(3),
            json!(""),
            json!("  "),
            json!("bind-1"),
        ] {
            let request =
                json!({"op":"input", "id":"run", "stage_run_id":"nonce", "req_id":req_id});
            let (server, client) = tokio::net::UnixStream::pair().unwrap();
            let (reader, writer) = server.into_split();
            let writer = SharedWriter::new(writer);
            handle_input(&request, &registry, None, BufReader::new(reader), &writer).await;
            let response = crate::executor::socket::read_json_line(&mut BufReader::new(client))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response["type"], "error");
            assert_eq!(response["req_id"], req_id);
            let message = response["message"].as_str().unwrap();
            assert!(message.contains(if req_id == json!("bind-1") {
                "not live"
            } else {
                "req_id"
            }));
        }
        // The shared lookup also serves legacy chat, whose req_id is optional.
        assert!(bind(
            &json!({"id":"run", "stage_run_id":"nonce"}),
            &registry,
            None
        )
        .await
        .err()
        .unwrap()
        .contains("not live"));
    }

    #[tokio::test]
    async fn failed_bind_ack_releases_claim_while_reader_is_open() {
        use std::time::Duration;
        let registry = registry();
        use tokio::io::AsyncWriteExt;
        let (sender, mut receiver, _revoker) = inbox();
        let independent = sender.clone();
        let bound = BoundInput {
            sender,
            identity: ExecutionIdentity {
                run_id: "run".into(),
                stage_name: "build".into(),
                stage_run_id: "nonce".into(),
            },
            _claim: registry.claim_controller("run", "nonce").unwrap(),
        };
        // The read side stays open: only ACK write failure can end the handler.
        let (read_server, mut read_peer) = tokio::net::UnixStream::pair().unwrap();
        read_peer
            .write_all(b"{\"cmd\":\"bail\",\"req_id\":\"pipelined\"}\n")
            .await
            .unwrap();
        let (reader, _unused_writer) = read_server.into_split();
        let (write_server, write_peer) = tokio::net::UnixStream::pair().unwrap();
        let (_unused_reader, writer) = write_server.into_split();
        drop(write_peer);
        let writer = SharedWriter::new(writer);
        tokio::time::timeout(
            Duration::from_secs(1),
            serve_bound(
                &json!({"req_id":"bind"}),
                bound,
                BufReader::new(reader),
                &writer,
            ),
        )
        .await
        .expect("failed ACK retained claim");
        assert!(registry.claim_controller("run", "nonce").is_ok());
        assert!(receiver.try_recv().is_err(), "failed ACK dispatched bail");
        assert!(!independent.is_closed());
    }

    #[tokio::test]
    async fn stalled_ack_times_out_without_dispatching_pipelined_bail() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let registry = registry();
        let (sender, mut receiver, _revoker) = inbox();
        let bound = BoundInput {
            sender,
            identity: ExecutionIdentity {
                run_id: "run".into(),
                stage_name: "build".into(),
                stage_run_id: "nonce".into(),
            },
            _claim: registry.claim_controller("run", "nonce").unwrap(),
        };
        let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
        peer.write_all(b"{\"cmd\":\"bail\",\"req_id\":\"pipelined\"}\n")
            .await
            .unwrap();
        let (reader, writer) = server.into_split();
        let writer = SharedWriter::new(writer);
        let task = tokio::spawn(async move {
            serve_bound(
                &json!({"req_id":"x".repeat(4 * 1024 * 1024)}),
                bound,
                BufReader::new(reader),
                &writer,
            )
            .await;
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert!(registry.claim_controller("run", "nonce").is_err());
        assert!(receiver.try_recv().is_err());
        tokio::time::pause();
        tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .expect("stalled ACK timeout retained claim")
            .unwrap();
        assert!(registry.claim_controller("run", "nonce").is_ok());
        assert!(
            receiver.try_recv().is_err(),
            "timed-out ACK dispatched bail"
        );
    }

    #[tokio::test]
    async fn pending_limit_rejects_excess_without_losing_held_receipts() {
        use crate::clients::execution_input::InputCommand;
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let registry = registry();
        let (sender, mut receiver, _revoker) = inbox();
        let bound = BoundInput {
            sender,
            identity: ExecutionIdentity {
                run_id: "run".into(),
                stage_name: "build".into(),
                stage_run_id: "nonce".into(),
            },
            _claim: registry.claim_controller("run", "nonce").unwrap(),
        };
        let (server, peer) = tokio::net::UnixStream::pair().unwrap();
        let (reader, writer) = server.into_split();
        let writer = SharedWriter::new(writer);
        let (peer_reader, mut peer_writer) = peer.into_split();
        let mut peer_reader = BufReader::new(peer_reader);
        let task = tokio::spawn(async move {
            serve_bound(
                &json!({"req_id":"bind"}),
                bound,
                BufReader::new(reader),
                &writer,
            )
            .await;
        });
        async fn frame(reader: &mut BufReader<OwnedReadHalf>) -> Value {
            tokio::time::timeout(
                Duration::from_secs(2),
                crate::executor::socket::read_json_line(reader),
            )
            .await
            .unwrap()
            .unwrap()
            .expect("connection closed")
        }
        assert_eq!(frame(&mut peer_reader).await["type"], "input_bound");
        let mut held = Vec::new();
        for n in 0..32 {
            let req_id = format!("held-{n}");
            let request = json!({"cmd":"message", "text":"fix", "req_id":req_id});
            peer_writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            assert_eq!(
                frame(&mut peer_reader).await,
                json!({"type":"queued", "req_id":req_id})
            );
            let InputCommand::Correction(correction) = receiver.try_recv().unwrap() else {
                panic!("expected correction")
            };
            held.push(correction);
        }
        for req_id in ["excess-1", "excess-2"] {
            let request = json!({"cmd":"bail", "req_id":req_id});
            peer_writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let response = frame(&mut peer_reader).await;
            assert_eq!(response["type"], "error");
            assert_eq!(response["req_id"], req_id);
            assert_eq!(response["message"], "too many pending input receipts");
            assert!(receiver.try_recv().is_err());
            assert!(!task.is_finished());
            assert!(registry.claim_controller("run", "nonce").is_err());
        }
        for (n, correction) in held.into_iter().enumerate() {
            let req_id = format!("held-{n}");
            assert!(correction.incorporate(|id, text| {
                assert_eq!(id, req_id);
                assert_eq!(text, "fix");
            }));
            assert_eq!(
                frame(&mut peer_reader).await,
                json!({"type":"delivered", "req_id":req_id})
            );
        }
        peer_writer
            .write_all(b"{\"cmd\":\"resume\",\"req_id\":\"after-limit\"}\n")
            .await
            .unwrap();
        assert_eq!(
            frame(&mut peer_reader).await,
            json!({"type":"queued", "req_id":"after-limit"})
        );
        let InputCommand::Control(control) = receiver.try_recv().unwrap() else {
            panic!("expected control")
        };
        assert!(control.incorporate(|_, control| assert_eq!(control, InputControl::Resume)));
        assert_eq!(
            frame(&mut peer_reader).await,
            json!({"type":"delivered", "req_id":"after-limit"})
        );
        peer_writer
            .write_all(b"{\"cmd\":\"detach\"}\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(registry.claim_controller("run", "nonce").is_ok());
    }

    #[tokio::test]
    async fn blocked_ack_does_not_block_revocation_or_claim_release() {
        use std::time::Duration;
        let registry = registry();
        let (sender, _receiver, revoker) = inbox();
        let bound = BoundInput {
            sender,
            identity: ExecutionIdentity {
                run_id: "run".into(),
                stage_name: "build".into(),
                stage_run_id: "nonce".into(),
            },
            _claim: registry.claim_controller("run", "nonce").unwrap(),
        };
        let (server, _unread_peer) = tokio::net::UnixStream::pair().unwrap();
        let (reader, writer) = server.into_split();
        let writer = SharedWriter::new(writer);
        // Exceeds the socket buffer; the peer deliberately never reads the ACK.
        let request = json!({"req_id":"x".repeat(4 * 1024 * 1024)});
        let task = tokio::spawn(async move {
            serve_bound(&request, bound, BufReader::new(reader), &writer).await;
        });
        tokio::task::yield_now().await;
        revoker.revoke();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("slow peer blocked revocation")
            .unwrap();
        assert!(registry.claim_controller("run", "nonce").is_ok());
    }

    #[tokio::test]
    async fn exact_descriptors_validate_sibling_nonce_and_reject_conflicts() {
        use crate::clients::execution_input::InputRegistry;
        let dir = tempfile::tempdir().unwrap();
        let gremlin = super::super::chat::create_chat_gremlin(
            "descriptor-run",
            &crate::clients::client::ModelURI::parse("cmd/echo").unwrap(),
            "helpful",
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        gremlin
            .state
            .patch(&[], json!({"kind":"pipeline"}).as_object().unwrap());
        let registry = registry();
        let _gremlin = registry
            .register_chat_gremlin(
                "descriptor-run".into(),
                gremlin,
                crate::clients::cancel::CancelToken::new(),
            )
            .await;
        let inputs = InputRegistry::default();
        let (interactive, _, _) = registry
            .get_run_interactive("descriptor-run")
            .await
            .unwrap();
        interactive.set_input_registry(inputs.clone());
        let identity = ExecutionIdentity {
            run_id: "descriptor-run".into(),
            stage_name: "sibling".into(),
            stage_run_id: "exact".into(),
        };
        let (mut receiver, revoker) =
            inputs.register_with_nonce(identity.clone(), Some(42), |_| {});
        assert_ne!(interactive.target().nonce(), 42);
        let target =
            json!({"id":"descriptor-run","stage":"sibling","stage_run_id":"exact","nonce":42});
        let bound = bind(&json!({"target":target}), &registry, None)
            .await
            .unwrap();
        assert_eq!(bound.identity, identity);
        drop(bound);
        for key in ["stage_id", "execution_id"] {
            let mut retired = target.clone();
            retired[key] = json!("retired");
            let error = bind(&json!({"target":retired}), &registry, None)
                .await
                .err()
                .expect("nested retired selector accepted alongside stage_run_id");
            assert!(error.contains(key) && error.contains("retired"), "{error}");
            drop(
                bind(&json!({"target":target}), &registry, None)
                    .await
                    .unwrap(),
            );
        }
        for key in ["nonce", "id", "stage", "stage_run_id"] {
            let mut bad = target.clone();
            bad[key] = if key == "nonce" {
                json!(43)
            } else {
                json!("wrong")
            };
            assert!(
                bind(&json!({"target":bad}), &registry, None).await.is_err(),
                "{key}"
            );
            let mut conflict = json!({"target":target});
            conflict[key] = target[key].clone();
            assert!(bind(&conflict, &registry, None).await.is_err(), "{key}");
        }
        let mut nested = target.clone();
        nested["target"] = target.clone();
        assert!(bind(&json!({"target":nested}), &registry, None)
            .await
            .is_err());
        assert!(receiver.try_recv().is_err());
        let session = super::super::session_task::spawn_session_task(
            dir.path().to_path_buf(),
            "sess-descriptor".into(),
            registry.clone(),
            None,
        );
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let (_, writer) = server.into_split();
        let writer = SharedWriter::new(writer);
        let mut client = BufReader::new(client);
        for descriptor in [nested, {
            let mut stale = target.clone();
            stale["nonce"] = json!(41);
            stale
        }] {
            super::super::chat::handle_chat(
                &json!({"text":"fix", "target":descriptor}),
                &writer,
                &registry,
                &session,
            )
            .await;
            let response = crate::executor::socket::read_json_line(&mut client)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response["type"], "error");
            assert!(receiver.try_recv().is_err());
        }
        super::super::chat::handle_chat(
            &json!({"text":"fix", "target":target}),
            &writer,
            &registry,
            &session,
        )
        .await;
        let response = crate::executor::socket::read_json_line(&mut client)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response["status"], "queued", "{response}");
        let crate::clients::execution_input::InputCommand::Correction(correction) =
            receiver.recv().await.unwrap()
        else {
            panic!("correction")
        };
        assert!(correction.incorporate(|_, text| assert_eq!(text, "fix")));
        session.teardown(true).await;
        revoker.revoke();
        let (_replacement, _revoker) = inputs.register_with_nonce(identity, Some(43), |_| {});
        assert!(bind(&json!({"target":target}), &registry, None)
            .await
            .is_err());
        registry.stop_run("descriptor-run").await.unwrap();
    }

    #[tokio::test]
    async fn stalled_post_enqueue_writer_does_not_block_execution_or_disconnect() {
        stalled_post_enqueue_writer(false).await;
    }

    #[tokio::test]
    async fn stalled_post_enqueue_writer_times_out_and_releases_claim() {
        stalled_post_enqueue_writer(true).await;
    }

    async fn stalled_post_enqueue_writer(wait_for_timeout: bool) {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let registry = registry();
        let (sender, mut receiver, _revoker) = inbox();
        let independent = sender.clone();
        let identity = ExecutionIdentity {
            run_id: "run".into(),
            stage_name: "build".into(),
            stage_run_id: "nonce".into(),
        };
        let bound = BoundInput {
            sender,
            identity,
            _claim: registry.claim_controller("run", "nonce").unwrap(),
        };
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let (reader, writer) = server.into_split();
        let writer = SharedWriter::new(writer);
        let (client_reader, mut client_writer) = client.into_split();
        let mut client_reader = BufReader::new(client_reader);
        let task = tokio::spawn(async move {
            serve_bound(
                &json!({"req_id":"bind"}),
                bound,
                BufReader::new(reader),
                &writer,
            )
            .await;
        });
        let ack = crate::executor::socket::read_json_line(&mut client_reader)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ack["type"], "input_bound");
        // The queued response, not the bind ACK, exceeds the unread socket buffer.
        let request = json!({"cmd":"message","text":"fix","req_id":"x".repeat(4 * 1024 * 1024)});
        client_writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let command = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let crate::clients::execution_input::InputCommand::Correction(correction) = command else {
            panic!("correction")
        };
        assert!(correction.incorporate(|_, text| assert_eq!(text, "fix")));
        assert!(
            !task.is_finished(),
            "handler must remain live while responses stall"
        );
        if wait_for_timeout {
            tokio::time::pause();
            tokio::time::timeout(Duration::from_secs(6), task)
                .await
                .expect("stalled writer timeout retained control")
                .unwrap();
            tokio::time::resume();
        } else {
            drop(client_reader);
            drop(client_writer);
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("disconnect retained control")
                .unwrap();
        }
        let _claim = registry.claim_controller("run", "nonce").unwrap();
        let receipt = independent
            .try_send("independent".into(), "next".into())
            .unwrap();
        let crate::clients::execution_input::InputCommand::Correction(correction) =
            receiver.recv().await.unwrap()
        else {
            panic!("correction")
        };
        assert!(correction.incorporate(|_, text| assert_eq!(text, "next")));
        assert_eq!(receipt.await.unwrap(), Delivery::Delivered);
    }

    #[test]
    fn exact_selector_fields_must_be_nonempty_strings() {
        for value in [Value::Null, json!(3), json!(""), json!("  ")] {
            assert!(required(&json!({"stage_run_id":value}), "stage_run_id").is_err());
        }
        assert_eq!(
            required(&json!({"stage_run_id":"nonce"}), "stage_run_id").unwrap(),
            "nonce"
        );
    }
}
