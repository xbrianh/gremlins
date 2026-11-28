//! Gather live facts for the pure exact-input binding rules.
use std::sync::Arc;

use serde_json::Value;

use super::input_rules::{self, Decision, Effect, OwnerSnapshot, Snapshot};
use super::input_wire::Descriptor;
use super::run_registry::{ControllerClaim, RunRegistry};
use super::session_task::SessionTaskHandle;
use crate::clients::execution_input::{ExecutionIdentity, InputSender};

pub(crate) struct BoundInput {
    pub(crate) sender: InputSender,
    pub(crate) identity: ExecutionIdentity,
    _claim: ControllerClaim,
}

pub(super) async fn bind_wire(
    request: &Value,
    registry: &Arc<RunRegistry>,
    owner: Option<&SessionTaskHandle>,
) -> Result<BoundInput, Value> {
    input_rules::validate_wire(request)?;
    bind(request, registry, owner).await
}

pub(super) async fn bind(
    request: &Value,
    registry: &Arc<RunRegistry>,
    owner: Option<&SessionTaskHandle>,
) -> Result<BoundInput, Value> {
    let descriptor = input_rules::descriptor(request)
        .map_err(|message| input_rules::error(request, &message))?;
    let mut snapshot = Snapshot {
        live: false,
        kind: String::new(),
        owner: None,
        resolved: None,
        target_stage_run_id: None,
        target_ended: false,
        target_accepts_commands: false,
        sender_closed: false,
        claim_free: true,
    };
    let mut sender = None;
    if let Some((interactive, store, _)) = registry.get_run_interactive(descriptor.run_id).await {
        snapshot.live = true;
        snapshot.kind = store.read_str("kind");
        if snapshot.kind == "chat" {
            if let Some(owner) = owner {
                let closed = owner.is_closed().await;
                snapshot.owner = Some(OwnerSnapshot {
                    closed,
                    chat_ids: if closed {
                        Vec::new()
                    } else {
                        owner.get_chat_ids().await
                    },
                });
            }
        }
        if let Some((identity, resolved_sender)) =
            interactive.resolve_input(descriptor.run_id, descriptor.stage_run_id, descriptor.stage)
        {
            snapshot.resolved = Some((identity, resolved_sender.nonce()));
            sender = Some(resolved_sender);
        } else {
            let target = interactive.target();
            snapshot.target_stage_run_id = target.stage_run_id().map(str::to_owned);
            snapshot.target_ended = target.end_signal().is_ended();
            snapshot.target_accepts_commands = target.accepts_commands();
        }
    }
    acquire_binding(request, &descriptor, snapshot, sender, |identity| {
        registry
            .claim_controller(&identity.run_id, &identity.stage_run_id)
            .ok()
    })
    .map(|(sender, identity, claim)| BoundInput {
        sender,
        identity,
        _claim: claim,
    })
    .map_err(|decision| decision.frame)
}

fn acquire_binding<C>(
    request: &Value,
    descriptor: &Descriptor<'_>,
    mut snapshot: Snapshot,
    sender: Option<InputSender>,
    acquire: impl FnOnce(&ExecutionIdentity) -> Option<C>,
) -> Result<(InputSender, ExecutionIdentity, C), Decision> {
    snapshot.claim_free = true;
    snapshot.sender_closed = false;
    let eligible = input_rules::bind_descriptor(request, descriptor, &snapshot);
    let Effect::Bind(identity) = eligible.effect else {
        return Err(eligible);
    };
    let claim = acquire(&identity);
    snapshot.claim_free = claim.is_some();
    if claim.is_some() {
        snapshot.sender_closed = sender
            .as_ref()
            .expect("binding rules require a resolved sender")
            .is_closed();
    }
    let decision = input_rules::bind_descriptor(request, descriptor, &snapshot);
    match decision.effect {
        Effect::Bind(identity) => Ok((
            sender.expect("binding rules require a resolved sender"),
            identity,
            claim.expect("binding rules require a reserved claim"),
        )),
        Effect::Reject => Err(decision),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::execution_input::InputRegistry;
    use serde_json::json;
    use std::cell::Cell;

    struct Claim<'a>(&'a Cell<bool>);

    impl Drop for Claim<'_> {
        fn drop(&mut self) {
            assert!(self.0.replace(false));
        }
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
            target_stage_run_id: None,
            target_ended: false,
            target_accepts_commands: true,
            sender_closed: false,
            claim_free: true,
        }
    }

    fn requests() -> [Value; 2] {
        [
            json!({"op":"input", "req_id":"input-race", "id":"run",
                "stage_run_id":"invocation", "nonce":7}),
            json!({"op":"chat", "req_id":"chat-race", "text":"fix",
                "target":{"id":"run", "stage_run_id":"invocation", "nonce":7}}),
        ]
    }

    #[test]
    fn internal_binding_without_req_id_acquires_and_releases_claim() {
        let mut request = requests()[0].clone();
        request.as_object_mut().unwrap().remove("req_id");
        let descriptor = input_rules::descriptor(&request).unwrap();
        let state = snapshot();
        let inputs = InputRegistry::default();
        let identity = &state.resolved.as_ref().unwrap().0;
        let (_receiver, _revoker) = inputs.register_with_nonce(identity.clone(), Some(7), |_| {});
        let sender = inputs.sender(identity).unwrap();
        let held = Cell::new(false);
        let binding = acquire_binding(&request, &descriptor, state, Some(sender), |_| {
            assert!(!held.replace(true));
            Some(Claim(&held))
        })
        .unwrap();
        assert!(held.get());
        drop(binding);
        assert!(!held.get());
    }

    #[test]
    fn revocation_during_successful_acquisition_rejects_and_releases_claim() {
        for request in requests() {
            let state = snapshot();
            let identity = &state.resolved.as_ref().unwrap().0;
            let inputs = InputRegistry::default();
            let (_receiver, revoker) =
                inputs.register_with_nonce(identity.clone(), Some(7), |_| {});
            let sender = inputs.sender(identity).unwrap();
            let held = Cell::new(false);
            assert!(!sender.is_closed());
            let result = acquire_binding(
                &request,
                &input_rules::descriptor(&request).unwrap(),
                state,
                Some(sender),
                |actual| {
                    assert_eq!(actual, &snapshot().resolved.unwrap().0);
                    assert!(!held.replace(true));
                    revoker.revoke();
                    Some(Claim(&held))
                },
            );
            let Err(decision) = result else {
                panic!("revoked input was bound")
            };
            assert_eq!(decision.effect, Effect::Reject);
            assert_eq!(
                decision.frame,
                json!({"type":"error",
                "message":"execution input is revoked", "req_id":request["req_id"]})
            );
            assert!(!held.get(), "rejected binding retained its claim");
        }
    }

    #[test]
    fn busy_precedes_revocation_during_acquisition() {
        for request in requests() {
            let state = snapshot();
            let identity = &state.resolved.as_ref().unwrap().0;
            let inputs = InputRegistry::default();
            let (_receiver, revoker) =
                inputs.register_with_nonce(identity.clone(), Some(7), |_| {});
            let sender = inputs.sender(identity).unwrap();
            let result = acquire_binding::<()>(
                &request,
                &input_rules::descriptor(&request).unwrap(),
                state,
                Some(sender),
                |_| {
                    revoker.revoke();
                    None
                },
            );
            let Err(decision) = result else {
                panic!("busy input was bound")
            };
            assert_eq!(
                decision.frame,
                json!({"type":"error",
                "message":"controller_busy", "req_id":request["req_id"]})
            );
        }
    }

    #[test]
    fn ineligible_requests_never_acquire_claims() {
        let mut foreign = snapshot();
        foreign.kind = "chat".into();
        let mut stale = snapshot();
        stale.resolved.as_mut().unwrap().1 = Some(8);
        let mut stale_nonce = requests()[0].clone();
        stale_nonce["nonce"] = json!(8);
        for (request, state) in [
            (requests()[0].clone(), foreign),
            (requests()[1].clone(), stale),
            (stale_nonce, snapshot()),
        ] {
            let result = acquire_binding::<()>(
                &request,
                &input_rules::descriptor(&request).unwrap(),
                state,
                None,
                |_| panic!("ineligible request attempted claim"),
            );
            assert!(matches!(
                result,
                Err(Decision {
                    effect: Effect::Reject,
                    ..
                })
            ));
        }
    }
}
