use crate::definition::{ChatDefinition, GremlinDefinition};
use gremlins_magic_bus::connection::BusSubGuard;
use gremlins_magic_bus::{emit_corr_event, subscribe_run, CorrId, EventKind, ReceiverHalf};

#[tokio::test]
async fn definition_checkpoint_restores_unexecuted_slot_and_interrupt_notice() {
    let model = crate::model_uri::ModelURI::parse("openai/gpt-4").unwrap();
    let mut definition: Box<dyn GremlinDefinition> = Box::new(ChatDefinition::new(
        "chat".into(),
        ".".into(),
        model.clone(),
        String::new(),
    ));
    definition
        .as_any_mut()
        .unwrap()
        .downcast_mut::<ChatDefinition>()
        .unwrap()
        .mark_interrupted();
    let checkpoint = definition.clone_box();
    definition
        .as_any_mut()
        .unwrap()
        .downcast_mut::<ChatDefinition>()
        .unwrap()
        .set_pending("abandoned", &model, "");
    assert_eq!(definition.next_stage().await.unwrap().name(), "chat-0001");
    definition = checkpoint;
    let chat = definition
        .as_any_mut()
        .unwrap()
        .downcast_mut::<ChatDefinition>()
        .unwrap();
    assert_eq!(chat.turn_count(), 0);
    assert!(chat.interrupted());
    chat.set_pending("retry", &model, "");
    assert_eq!(definition.next_stage().await.unwrap().name(), "chat-0001");
}

#[test]
fn bookkeeping_subscription_is_lossless_and_detached_drain_is_finite() {
    let id = format!("turn-bookkeeping-{}", crate::executor::state::token_hex(16));
    let (sub, mut rx) = subscribe_run(&id);
    let guard = BusSubGuard::new(sub);
    let corr = CorrId::new().with_run(&id);
    for _ in 0..128 {
        emit_corr_event(
            &corr,
            EventKind::StreamChunk {
                text: "chunk".into(),
            },
        );
    }
    emit_corr_event(
        &corr,
        EventKind::Done {
            text: String::new(),
            summary: String::new(),
            usage: None,
        },
    );
    drop(guard);
    emit_corr_event(
        &corr,
        EventKind::StreamChunk {
            text: "late".into(),
        },
    );
    let pending = match &rx {
        ReceiverHalf::Lossless(rx) => rx.len(),
        _ => panic!("bookkeeping must be lossless"),
    };
    assert_eq!(pending, 129);
    let mut saw_done = false;
    for _ in 0..pending {
        saw_done |= matches!(rx.try_recv().unwrap().into_kind(), EventKind::Done { .. });
    }
    assert!(saw_done);
    assert!(rx.try_recv().is_err());
}
