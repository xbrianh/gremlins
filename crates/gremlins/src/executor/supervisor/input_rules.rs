//! Pure exact-input validation, binding decisions, and receipt bookkeeping.
use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::input_wire::{self, Command, Descriptor};
use crate::clients::execution_input::{Delivery, ExecutionIdentity};

#[derive(Debug, Clone)]
pub(super) struct OwnerSnapshot {
    pub(super) closed: bool,
    pub(super) chat_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct Snapshot {
    pub(super) live: bool,
    pub(super) kind: String,
    pub(super) owner: Option<OwnerSnapshot>,
    pub(super) resolved: Option<(ExecutionIdentity, Option<u64>)>,
    pub(super) target_stage_run_id: Option<String>,
    pub(super) target_ended: bool,
    pub(super) target_accepts_commands: bool,
    pub(super) sender_closed: bool,
    pub(super) claim_free: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    Bind(ExecutionIdentity),
    Reject,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Decision {
    pub(super) frame: Value,
    pub(super) effect: Effect,
}

fn is_chat(request: &Value) -> bool {
    request.get("op").and_then(Value::as_str) == Some("chat")
}

pub(super) fn chat_preflight(request: &Value) -> Result<(), String> {
    if [
        "id",
        "nonce",
        "stage",
        "stage_id",
        "execution_id",
        "stage_run_id",
    ]
    .iter()
    .any(|key| request.get(key).is_some())
    {
        return Err("top-level stage-run selectors are not supported; use target with id, nonce, and stage_run_id".into());
    }
    if let Some(message) = input_wire::retired_target_message(request) {
        return Err(message);
    }
    if request
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err("missing 'text' field".into());
    }
    Ok(())
}

pub(super) fn descriptor(request: &Value) -> Result<Descriptor<'_>, String> {
    if is_chat(request) {
        chat_preflight(request)?;
        if let Some(target) = request.get("target") {
            let target = target.as_object().ok_or("target must be an object")?;
            if target
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
            {
                return Err("target.id is required".into());
            }
            if target.get("nonce").and_then(Value::as_u64).unwrap_or(0) == 0 {
                return Err("target.nonce is required".into());
            }
            if target
                .get("stage_run_id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .is_none()
            {
                return Err("target.stage_run_id is required".into());
            }
        }
    }
    input_wire::descriptor(request, |id| {
        crate::executor::gremlin::validate_gremlin_id(id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
}

pub(super) fn bound_error(request: &Value, message: &str) -> Value {
    json!({"type":"error", "message":message, "req_id":request.get("req_id")})
}

pub(super) fn error(request: &Value, message: &str) -> Value {
    let mut frame = json!({"type":"error", "message":message});
    if !is_chat(request) || request.get("req_id").is_some() {
        frame["req_id"] = request.get("req_id").cloned().unwrap_or(Value::Null);
    }
    frame
}

pub(super) fn enqueue_error(request: &Value, message: &str) -> Value {
    error(request, message)
}

pub(super) fn input_bound(identity: &ExecutionIdentity, request: &Value) -> Value {
    json!({"type":"input_bound", "id":identity.run_id,
        "stage":identity.stage_name, "stage_run_id":identity.stage_run_id,
        "req_id":request.get("req_id")})
}

/// Requires a targeted chat request accepted by `descriptor` and the binding rules.
pub(super) fn chat_queued(request: &Value, identity: &ExecutionIdentity) -> Value {
    super::ok_response(json!({"id":identity.run_id, "status":"queued",
        "nonce":request["target"]["nonce"],
        "stage_run_id":request["target"]["stage_run_id"],
        "req_id":request.get("req_id").and_then(Value::as_str)}))
}

/// Requires the same validated targeted request used for `chat_queued`.
pub(super) fn chat_settled(
    request: &Value,
    identity: &ExecutionIdentity,
    delivery: Delivery,
) -> Value {
    let mut frame = json!({"id":identity.run_id,
        "stage_run_id":request["target"]["stage_run_id"],
        "req_id":request.get("req_id").and_then(Value::as_str)});
    if delivery == Delivery::Delivered {
        frame["type"] = json!("delivered");
        frame["nonce"] = request["target"]["nonce"].clone();
    } else {
        frame["type"] = json!("correction_dropped");
        frame["reason"] = json!("agent exited before incorporation");
    }
    frame
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommandAction<'a> {
    Detach,
    Close,
    Enqueue(Command<'a>),
    Reject(Value),
}

pub(super) fn command_action<'a>(
    request: &'a Value,
    pending: &PendingReceipts,
    output_capacity: usize,
) -> CommandAction<'a> {
    if request.get("cmd").and_then(Value::as_str) == Some("detach") {
        return CommandAction::Detach;
    }
    if output_capacity < 2 {
        return CommandAction::Close;
    }
    match pending.command(request) {
        Ok(command) => CommandAction::Enqueue(command),
        Err(frame) => CommandAction::Reject(frame),
    }
}

fn reject(request: &Value, message: &str) -> Decision {
    Decision {
        frame: error(request, message),
        effect: Effect::Reject,
    }
}

pub(super) fn validate_wire(request: &Value) -> Result<(), Value> {
    input_wire::required(request, "req_id")
        .map(|_| ())
        .map_err(|message| bound_error(request, &message))
}

#[cfg(test)]
fn bind_wire(request: &Value, snapshot: &Snapshot) -> Decision {
    if let Err(frame) = validate_wire(request) {
        return Decision {
            frame,
            effect: Effect::Reject,
        };
    }
    bind(request, snapshot)
}

#[cfg(test)]
pub(super) fn bind(request: &Value, snapshot: &Snapshot) -> Decision {
    let descriptor = match descriptor(request) {
        Ok(descriptor) => descriptor,
        Err(message) => return reject(request, &message),
    };
    bind_descriptor(request, &descriptor, snapshot)
}

pub(super) fn bind_descriptor(
    request: &Value,
    descriptor: &Descriptor<'_>,
    snapshot: &Snapshot,
) -> Decision {
    if !snapshot.live {
        return reject(request, "run is not live");
    }
    if snapshot.kind == "chat"
        && !snapshot.owner.as_ref().is_some_and(|owner| {
            !owner.closed && owner.chat_ids.iter().any(|id| id == descriptor.run_id)
        })
    {
        let message = if is_chat(request) {
            format!("target {}: not owned by this session", descriptor.run_id)
        } else {
            "chat input requires its owning session".into()
        };
        return reject(request, &message);
    }
    let Some((identity, nonce)) = &snapshot.resolved else {
        let message = if snapshot.target_stage_run_id.as_deref() == Some(descriptor.stage_run_id)
            && !snapshot.target_ended
        {
            if snapshot.target_accepts_commands {
                "execution input is unavailable: input registration is not ready (execution may be preparing)"
            } else {
                "execution input is unavailable: execution/backend does not support input"
            }
        } else {
            "execution input is unavailable or stale: no matching live input registration (execution may be preparing or backend may not support input)"
        };
        return reject(request, message);
    };
    if descriptor
        .nonce
        .is_some_and(|expected| Some(expected) != *nonce)
    {
        return reject(request, "execution has been replaced; target is stale");
    }
    if !snapshot.claim_free {
        return reject(request, "controller_busy");
    }
    if snapshot.sender_closed {
        return reject(request, "execution input is revoked");
    }
    Decision {
        frame: input_bound(identity, request),
        effect: Effect::Bind(identity.clone()),
    }
}

#[derive(Debug, Default)]
pub(super) struct PendingReceipts {
    next_token: u64,
    pending: BTreeMap<u64, Value>,
}

impl PendingReceipts {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn len(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn command<'a>(&self, request: &'a Value) -> Result<Command<'a>, Value> {
        if self.len() >= 32 {
            return Err(bound_error(request, "too many pending input receipts"));
        }
        input_wire::command(request).map_err(|message| bound_error(request, &message))
    }

    pub(super) fn queued(&mut self, req_id: Value) -> (u64, Value) {
        let token = self.next_token;
        self.next_token = self
            .next_token
            .checked_add(1)
            .expect("input receipt token exhausted");
        self.pending.insert(token, req_id.clone());
        (token, json!({"type":"queued", "req_id":req_id}))
    }

    pub(super) fn settled(&mut self, token: u64, delivery: Delivery) -> Option<Value> {
        let req_id = self.pending.remove(&token)?;
        let kind = if delivery == Delivery::Delivered {
            "delivered"
        } else {
            "dropped"
        };
        Some(json!({"type":kind, "req_id":req_id}))
    }

    /// The adapter must drain original receipt results before revoking.
    pub(super) fn revoke(&mut self) -> Vec<Value> {
        let mut frames: Vec<_> = std::mem::take(&mut self.pending)
            .into_values()
            .map(|req_id| json!({"type":"dropped", "req_id":req_id}))
            .collect();
        frames.push(json!({"type":"ended"}));
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_binding_does_not_require_wire_correlation() {
        for req_id in [None, Some(Value::Null), Some(json!(42)), Some(json!(" "))] {
            let mut request = request();
            request.as_object_mut().unwrap().remove("req_id");
            if let Some(req_id) = req_id {
                request["req_id"] = req_id;
            }
            assert!(matches!(
                bind(&request, &snapshot()).effect,
                Effect::Bind(_)
            ));
            let decision = bind_wire(&request, &snapshot());
            assert_eq!(decision.effect, Effect::Reject);
            assert_eq!(
                decision.frame,
                bound_error(&request, "missing or invalid 'req_id'")
            );
        }
    }

    #[test]
    fn wire_correlation_validation_precedes_descriptor_validation() {
        let request = json!({"stage_id":null});
        assert_eq!(
            bind_wire(&request, &snapshot()).frame,
            json!({"type":"error", "req_id":null, "message":"missing or invalid 'req_id'"})
        );
        assert_ne!(
            bind(&request, &snapshot()).frame["message"],
            "missing or invalid 'req_id'"
        );
    }

    #[test]
    fn ineligible_binding_snapshots_are_rejected() {
        let mut foreign = snapshot();
        foreign.kind = "chat".into();
        let mut stale = snapshot();
        stale.resolved.as_mut().unwrap().1 = Some(8);
        let mut missing_id = request();
        missing_id.as_object_mut().unwrap().remove("req_id");
        for (request, state) in [
            (request(), foreign),
            (request(), stale),
            (missing_id, snapshot()),
        ] {
            let decision = bind_wire(&request, &state);
            assert_eq!(decision.effect, Effect::Reject);
        }
    }

    fn request() -> Value {
        json!({"op":"input", "req_id":"r", "id":"run", "stage_run_id":"invocation", "nonce":7})
    }

    fn chat(target: Value) -> Value {
        json!({"op":"chat", "text":"fix", "req_id":"r", "target":target})
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            live: true,
            kind: "agent".into(),
            owner: None,
            resolved: Some((
                ExecutionIdentity {
                    run_id: "run".into(),
                    stage_name: "build".into(),
                    stage_run_id: "invocation".into(),
                },
                Some(7),
            )),
            target_stage_run_id: Some("invocation".into()),
            target_ended: false,
            target_accepts_commands: true,
            sender_closed: false,
            claim_free: true,
        }
    }

    fn rejection(request: &Value, snapshot: &Snapshot, message: &str) {
        let decision = bind_wire(request, snapshot);
        assert_eq!(decision.effect, Effect::Reject);
        assert_eq!(decision.frame, error(request, message));
    }

    #[test]
    fn binding_rejection_table_and_precedence() {
        let request = request();
        let mut state = snapshot();
        rejection(
            &json!({"stage_id":null}),
            &state,
            "missing or invalid 'req_id'",
        );
        state.live = false;
        state.kind = "chat".into();
        state.resolved = None;
        state.claim_free = false;
        state.sender_closed = true;
        rejection(&request, &state, "run is not live");
        state.live = true;
        rejection(&request, &state, "chat input requires its owning session");
        for owner in [
            None,
            Some(OwnerSnapshot {
                closed: true,
                chat_ids: vec!["run".into()],
            }),
            Some(OwnerSnapshot {
                closed: false,
                chat_ids: vec!["other".into()],
            }),
        ] {
            state.owner = owner;
            rejection(&request, &state, "chat input requires its owning session");
            rejection(
                &chat(json!({"id":"run","stage_run_id":"invocation","nonce":7})),
                &state,
                "target run: not owned by this session",
            );
        }
        state.owner = Some(OwnerSnapshot {
            closed: false,
            chat_ids: vec!["run".into()],
        });
        for (target_id, ended, accepts, message) in [
            (Some("invocation"), false, true, "execution input is unavailable: input registration is not ready (execution may be preparing)"),
            (Some("invocation"), false, false, "execution input is unavailable: execution/backend does not support input"),
            (Some("other"), false, true, "execution input is unavailable or stale: no matching live input registration (execution may be preparing or backend may not support input)"),
            (Some("invocation"), true, true, "execution input is unavailable or stale: no matching live input registration (execution may be preparing or backend may not support input)"),
            (None, false, false, "execution input is unavailable or stale: no matching live input registration (execution may be preparing or backend may not support input)"),
        ] {
            state.target_stage_run_id = target_id.map(str::to_owned);
            state.target_ended = ended;
            state.target_accepts_commands = accepts;
            rejection(&request, &state, message);
        }
        state.resolved = snapshot().resolved;
        for nonce in [None, Some(8)] {
            state.resolved.as_mut().unwrap().1 = nonce;
            rejection(
                &request,
                &state,
                "execution has been replaced; target is stale",
            );
        }
        state.resolved = snapshot().resolved;
        rejection(&request, &state, "controller_busy");
        state.claim_free = true;
        rejection(&request, &state, "execution input is revoked");
        state.sender_closed = false;
        assert!(matches!(bind(&request, &state).effect, Effect::Bind(_)));
    }

    #[test]
    fn required_binding_rejections_are_correlated() {
        let mut cases = Vec::new();
        let mut state = snapshot();
        state.live = false;
        cases.push((state, "run is not live"));
        for owner in [
            None,
            Some(OwnerSnapshot {
                closed: true,
                chat_ids: vec!["run".into()],
            }),
            Some(OwnerSnapshot {
                closed: false,
                chat_ids: vec!["foreign".into()],
            }),
        ] {
            let mut state = snapshot();
            state.kind = "chat".into();
            state.owner = owner;
            cases.push((state, "chat input requires its owning session"));
        }
        for (target_id, accepts, message) in [
            (Some("invocation"), true, "execution input is unavailable: input registration is not ready (execution may be preparing)"),
            (Some("invocation"), false, "execution input is unavailable: execution/backend does not support input"),
            (None, false, "execution input is unavailable or stale: no matching live input registration (execution may be preparing or backend may not support input)"),
        ] {
            let mut state = snapshot();
            state.resolved = None;
            state.target_stage_run_id = target_id.map(str::to_owned);
            state.target_accepts_commands = accepts;
            cases.push((state, message));
        }
        let mut stale = snapshot();
        stale.resolved.as_mut().unwrap().1 = Some(8);
        cases.push((stale, "execution has been replaced; target is stale"));
        let mut busy = snapshot();
        busy.claim_free = false;
        cases.push((busy, "controller_busy"));
        let mut revoked = snapshot();
        revoked.sender_closed = true;
        cases.push((revoked, "execution input is revoked"));
        let mut cases: Vec<_> = cases
            .into_iter()
            .map(|(state, message)| (request(), state, message))
            .collect();
        let mut malformed = request();
        malformed["stage_run_id"] = Value::Null;
        let mut retired = request();
        retired["target"] = json!({"execution_id":null});
        let mut top_level = chat(json!({"id":"run", "nonce":7, "stage_run_id":"invocation"}));
        top_level["stage_run_id"] = json!("invocation");
        cases.extend([
            (malformed, snapshot(), "missing or invalid 'stage_run_id'"),
            (retired, snapshot(), "target.execution_id is retired; use target.stage_run_id"),
            (top_level, snapshot(), "top-level stage-run selectors are not supported; use target with id, nonce, and stage_run_id"),
        ]);
        assert_eq!(cases.len(), 13);
        for (index, (mut request, state, message)) in cases.into_iter().enumerate() {
            let req_id = json!(format!("rejection-{index}"));
            request["req_id"] = req_id.clone();
            let decision = bind(&request, &state);
            assert_eq!(decision.effect, Effect::Reject, "{message}");
            assert_eq!(decision.frame["type"], "error");
            assert_eq!(decision.frame["message"], message);
            assert_eq!(decision.frame["req_id"], req_id);
        }
    }

    #[test]
    fn chat_alias_frames_echo_client_selector_not_canonical_invocation() {
        let identity = snapshot().resolved.unwrap().0;
        for req_id in [json!(" r "), Value::Null, json!(42)] {
            let mut request = chat(json!({"id":"run", "stage_run_id":"build", "nonce":7}));
            request["req_id"] = req_id.clone();
            let expected_id = req_id.as_str().map(str::to_owned);
            assert_eq!(
                bind(&request, &snapshot()).effect,
                Effect::Bind(identity.clone())
            );
            assert_eq!(
                input_bound(&identity, &request)["stage_run_id"],
                "invocation"
            );
            assert_eq!(
                chat_queued(&request, &identity),
                json!({"type":"ok", "status":"queued",
                "id":"run", "nonce":7, "stage_run_id":"build", "req_id":expected_id})
            );
            assert_eq!(
                chat_settled(&request, &identity, Delivery::Delivered),
                json!({"type":"delivered", "id":"run", "nonce":7,
                    "stage_run_id":"build", "req_id":expected_id})
            );
            assert_eq!(
                chat_settled(&request, &identity, Delivery::NotDelivered),
                json!({"type":"correction_dropped", "id":"run", "stage_run_id":"build",
                    "req_id":expected_id, "reason":"agent exited before incorporation"})
            );
        }
    }

    #[test]
    fn command_actions_preserve_detach_capacity_limit_validation_precedence() {
        let mut pending = PendingReceipts::new();
        let request = json!({"cmd":"message", "req_id":"r", "text":"fix"});
        for capacity in [2, 64] {
            assert_eq!(
                command_action(&request, &pending, capacity),
                CommandAction::Enqueue(Command::Message {
                    req_id: "r",
                    text: "fix"
                })
            );
        }
        assert!(pending.is_empty());
        for i in 0..32 {
            pending.queued(json!(i));
        }
        assert_eq!(
            command_action(&json!({"cmd":"detach", "stage_id":null}), &pending, 0),
            CommandAction::Detach
        );
        for capacity in [0, 1] {
            assert_eq!(
                command_action(&request, &pending, capacity),
                CommandAction::Close
            );
        }
        assert_eq!(
            command_action(&json!({"req_id":"r"}), &pending, 2),
            CommandAction::Reject(
                json!({"type":"error", "req_id":"r", "message":"too many pending input receipts"})
            )
        );
        let empty = PendingReceipts::new();
        let invalid = json!({"cmd":"other", "req_id":"r"});
        assert_eq!(
            command_action(&invalid, &empty, 2),
            CommandAction::Reject(empty.command(&invalid).unwrap_err())
        );
        assert_eq!(pending.len(), 32);
    }

    #[test]
    fn bound_commands_cannot_spoof_chat_error_correlation() {
        let request = json!({"op":"chat", "cmd":"pause"});
        let mut pending = PendingReceipts::new();
        assert_eq!(
            command_action(&request, &pending, 2),
            CommandAction::Reject(json!({"type":"error", "req_id":null,
                "message":"missing or invalid 'req_id'"}))
        );
        for i in 0..32 {
            pending.queued(json!(i));
        }
        assert_eq!(
            command_action(&request, &pending, 2),
            CommandAction::Reject(json!({"type":"error", "req_id":null,
                "message":"too many pending input receipts"}))
        );
        let mut actual_chat = chat(Value::Null);
        actual_chat.as_object_mut().unwrap().remove("req_id");
        let decision = bind(&actual_chat, &snapshot());
        assert_eq!(decision.effect, Effect::Reject);
        assert_eq!(
            decision.frame,
            json!({"type":"error", "message":"target must be an object"})
        );
    }

    #[test]
    fn enqueue_failure_emits_only_error_and_allocates_no_receipt() {
        let pending = PendingReceipts::new();
        for request in [
            json!({"cmd":"message", "req_id":"r", "text":"fix"}),
            chat(json!({"id":"run", "nonce":7, "stage_run_id":"build"})),
        ] {
            assert_eq!(
                enqueue_error(&request, "execution input inbox is full"),
                json!({"type":"error", "message":"execution input inbox is full", "req_id":"r"})
            );
            assert!(pending.is_empty());
        }
        assert_eq!(
            enqueue_error(&json!({"op":"chat"}), "full"),
            json!({"type":"error", "message":"full"})
        );
        assert_eq!(
            enqueue_error(&json!({"cmd":"pause"}), "full"),
            json!({"type":"error", "message":"full", "req_id":null})
        );
    }

    #[test]
    fn successful_routes_use_resolved_identity_and_optional_nonce() {
        let state = snapshot();
        let target = json!({"id":"run", "stage_run_id":"build", "nonce":7});
        for request in [
            json!({"op":"input", "req_id":"r", "id":"run", "stage_run_id":"build", "nonce":7}),
            json!({"op":"input", "req_id":"r", "target":target}),
            chat(target),
        ] {
            let decision = bind(&request, &state);
            assert_eq!(
                decision.effect,
                Effect::Bind(state.resolved.as_ref().unwrap().0.clone())
            );
            assert_eq!(
                decision.frame,
                json!({"type":"input_bound", "req_id":"r", "id":"run",
                "stage":"build", "stage_run_id":"invocation"})
            );
        }
        let mut request = request();
        request.as_object_mut().unwrap().remove("nonce");
        let mut state = snapshot();
        state.resolved.as_mut().unwrap().1 = None;
        assert!(matches!(bind(&request, &state).effect, Effect::Bind(_)));
    }

    #[test]
    fn chat_preflight_preserves_precedence_even_without_target() {
        for key in [
            "id",
            "nonce",
            "stage",
            "stage_id",
            "execution_id",
            "stage_run_id",
        ] {
            let mut request = chat(json!({"execution_id":null}));
            request[key] = Value::Null;
            request["text"] = Value::Null;
            assert_eq!(chat_preflight(&request).unwrap_err(),
                "top-level stage-run selectors are not supported; use target with id, nonce, and stage_run_id");
        }
        for text in [
            Value::Null,
            json!(false),
            json!(3),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert_eq!(
                chat_preflight(&json!({"op":"chat", "text":text})).unwrap_err(),
                "missing 'text' field"
            );
        }
        assert!(chat_preflight(&json!({"op":"chat", "text":" \n "})).is_ok());
        assert!(chat_preflight(&json!({"op":"chat", "text":"fix", "context":"x"})).is_ok());
        assert_eq!(
            descriptor(&chat(Value::Null)).unwrap_err(),
            "target must be an object"
        );
        assert_eq!(
            descriptor(&chat(json!({}))).unwrap_err(),
            "target.id is required"
        );
        assert_eq!(
            descriptor(&chat(json!({"id":"bad/id"}))).unwrap_err(),
            "target.nonce is required"
        );
        assert_eq!(
            descriptor(&chat(json!({"id":"bad/id", "nonce":7}))).unwrap_err(),
            "target.stage_run_id is required"
        );
    }

    #[test]
    fn retired_chat_errors_match_attachment_frames() {
        for key in ["stage_id", "execution_id"] {
            let mut request = chat(json!({}));
            request["target"][key] = Value::Null;
            let mut expected = super::super::attachment::retired_target_error(&request).unwrap();
            expected["req_id"] = request["req_id"].clone();
            assert_eq!(
                enqueue_error(&request, &chat_preflight(&request).unwrap_err()),
                expected
            );
        }
    }

    #[test]
    fn retired_aliases_on_every_route() {
        for key in ["stage_id", "execution_id"] {
            let top = format!("retired '{key}' field; use 'stage_run_id'");
            let nested = format!("target.{key} is retired; use target.stage_run_id");
            let mut inline = request();
            inline[key] = Value::Null;
            assert_eq!(descriptor(&inline).unwrap_err(), top);
            let mut targeted = json!({"op":"input", "target":{}});
            targeted["target"][key] = Value::Null;
            assert_eq!(descriptor(&targeted).unwrap_err(), nested);
            targeted["op"] = json!("chat");
            assert_eq!(chat_preflight(&targeted).unwrap_err(), nested);
            assert_eq!(descriptor(&targeted).unwrap_err(), nested);
            for request in [inline, targeted] {
                let error = PendingReceipts::new().command(&request).unwrap_err();
                assert_eq!(
                    error["message"],
                    input_wire::retired_target_message(&request).unwrap()
                );
            }
        }
    }

    #[test]
    fn descriptor_validation_table_and_canonical_id_precedence() {
        for (request, message) in [
            (json!(null), "input request must be an object"),
            (json!({"extra":true}), "unsupported input field 'extra'"),
            (
                json!({"target":null}),
                "target must be a flat descriptor object",
            ),
            (
                json!({"target":{"target":{}}}),
                "target must be a flat descriptor object",
            ),
            (
                json!({"target":{},"id":null}),
                "'id' conflicts with target descriptor",
            ),
            (
                json!({"target":{"extra":true}}),
                "unsupported input target field 'extra'",
            ),
            (json!({"nonce":0}), "missing or invalid 'nonce'"),
            (json!({}), "missing or invalid 'id'"),
            (json!({"id":"run"}), "missing or invalid 'stage_run_id'"),
            (
                json!({"id":"run", "stage_run_id":"s", "stage":null}),
                "missing or invalid 'stage'",
            ),
        ] {
            assert_eq!(descriptor(&request).unwrap_err(), message, "{request}");
        }
        let expected = crate::executor::gremlin::validate_gremlin_id("bad/id")
            .unwrap_err()
            .to_string();
        for request in [
            json!({"id":"bad/id"}),
            json!({"target":{"id":"bad/id"}}),
            chat(json!({"id":"bad/id", "nonce":7, "stage_run_id":"s"})),
        ] {
            assert_eq!(descriptor(&request).unwrap_err(), expected);
        }
    }

    #[test]
    fn pending_limit_precedes_command_validation_and_settlement_frees_capacity() {
        let mut pending = PendingReceipts::new();
        assert!(pending.is_empty());
        let request = json!({"cmd":"message", "req_id":"r", "text":" fix "});
        for i in 0..32 {
            assert!(pending.command(&request).is_ok());
            assert_eq!(
                pending.queued(json!(i)),
                (i, json!({"type":"queued", "req_id":i}))
            );
        }
        assert_eq!(pending.len(), 32);
        for request in [request.clone(), json!({"stage_id":null})] {
            assert_eq!(
                pending.command(&request).unwrap_err(),
                error(&request, "too many pending input receipts")
            );
        }
        assert_eq!(
            pending.settled(0, Delivery::Delivered),
            Some(json!({"type":"delivered", "req_id":0}))
        );
        assert!(pending.command(&request).is_ok());
        assert_eq!(pending.len(), 31);
        for token in 1..32 {
            assert_eq!(
                pending.settled(token, Delivery::Delivered),
                Some(json!({"type":"delivered", "req_id":token}))
            );
        }
        assert!(pending.is_empty());
    }

    #[test]
    fn duplicate_request_ids_settle_independently_and_revoke_does_not_retarget() {
        let mut pending = PendingReceipts::default();
        let (delivered, _) = pending.queued(json!("same"));
        let (dropped, _) = pending.queued(json!("same"));
        let (unresolved, _) = pending.queued(Value::Null);
        let (also_unresolved, _) = pending.queued(json!("held"));
        assert_ne!(delivered, dropped);
        assert_eq!(
            pending.settled(delivered, Delivery::Delivered),
            Some(json!({"type":"delivered", "req_id":"same"}))
        );
        assert_eq!(
            pending.settled(dropped, Delivery::NotDelivered),
            Some(json!({"type":"dropped", "req_id":"same"}))
        );
        assert_eq!(pending.settled(delivered, Delivery::NotDelivered), None);
        assert_eq!(
            pending.revoke(),
            vec![
                json!({"type":"dropped", "req_id":null}),
                json!({"type":"dropped", "req_id":"held"}),
                json!({"type":"ended"})
            ]
        );
        assert!(pending.is_empty());
        assert_eq!(pending.settled(unresolved, Delivery::Delivered), None);
        let (successor, _) = pending.queued(json!("successor"));
        assert!(successor > also_unresolved);
        assert_eq!(pending.settled(also_unresolved, Delivery::Delivered), None);
        assert_eq!(pending.settled(unresolved, Delivery::Delivered), None);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending.revoke(),
            vec![
                json!({"type":"dropped", "req_id":"successor"}),
                json!({"type":"ended"})
            ]
        );
    }

    #[test]
    fn command_decoding_and_errors_are_wire_exact() {
        let pending = PendingReceipts::new();
        for cmd in ["message", "pause", "step", "resume", "bail"] {
            let request = json!({"cmd":cmd, "req_id":" r ", "text":" fix "});
            assert_eq!(
                pending.command(&request).unwrap(),
                input_wire::command(&request).unwrap()
            );
        }
        for request in [
            json!(null),
            json!({}),
            json!({"req_id":"r"}),
            json!({"req_id":"r", "cmd":"message"}),
            json!({"req_id":"r", "cmd":"other"}),
        ] {
            assert_eq!(
                pending.command(&request).unwrap_err(),
                error(&request, &input_wire::command(&request).unwrap_err())
            );
        }
        for key in ["target", "id", "stage", "stage_run_id", "nonce"] {
            let mut request = json!({"cmd":"pause", "req_id":"r"});
            request[key] = Value::Null;
            assert_eq!(
                pending.command(&request).unwrap_err()["message"],
                format!("'{key}' is not allowed on a bound input command")
            );
        }
    }
}
