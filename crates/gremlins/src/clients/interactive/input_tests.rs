use super::*;
use crate::clients::execution_input::{Delivery, InputRejected, INPUT_CAPACITY};
use tokio::sync::mpsc::error::TryRecvError;

fn identity(stage: &str, stage_run: &str) -> ExecutionIdentity {
    ExecutionIdentity {
        run_id: "run".into(),
        stage_name: stage.into(),
        stage_run_id: stage_run.into(),
    }
}

fn receive(session: &mut InteractiveSession) -> crate::clients::execution_input::Correction {
    match session.try_recv_input().unwrap() {
        crate::clients::execution_input::InputCommand::Correction(correction) => correction,
        crate::clients::execution_input::InputCommand::Control(_) => {
            panic!("expected correction, received control")
        }
    }
}

#[test]
fn overlapping_factories_route_by_identity_without_debugger_or_event_consumption() {
    let registry = InputRegistry::default();
    let (mut factory_a, handle_a) = InteractiveSessionFactory::for_pipeline();
    let (mut factory_b, handle_b) = InteractiveSessionFactory::for_pipeline();
    let (mut factory_c, handle_c) = InteractiveSessionFactory::for_pipeline();
    for handle in [&handle_a, &handle_b, &handle_c] {
        handle.set_input_registry(registry.clone());
    }
    let a = identity("A", "a-1");
    let b = identity("B", "b-1");
    let c = identity("C", "c-1");
    let mut session_a = factory_a.next_session_for_stage("a-1", true, None).unwrap();
    let mut session_b = factory_b.next_session_for_stage("b-1", true, None).unwrap();
    let mut session_c = factory_c
        .next_session_for_stage("c-1", false, None)
        .unwrap();
    session_a.bind_input(&registry, a.clone(), true, None);
    session_b.bind_input(&registry, b.clone(), true, None);
    session_c.bind_input(&registry, c.clone(), false, None);
    assert_eq!(session_c.input_identity.as_ref(), Some(&c));
    assert!(matches!(
        session_c.try_recv_input(),
        Err(TryRecvError::Disconnected)
    ));
    for handle in [&handle_a, &handle_b, &handle_c] {
        assert!(handle.input_sender(&c).is_none());
    }

    // Deliberately use the other factory's handle: identity, not the latest
    // published debugger target, selects the inbox. No event stream is polled.
    let mut receipt_a = handle_b
        .input_sender(&a)
        .unwrap()
        .try_send("request-a".into(), "only A".into())
        .unwrap();
    assert!(matches!(
        session_b.try_recv_input(),
        Err(TryRecvError::Empty)
    ));
    let mut receipt_b = handle_a
        .input_sender(&b)
        .unwrap()
        .try_send("request-b".into(), "only B".into())
        .unwrap();
    let correction_b = receive(&mut session_b);
    assert_eq!(
        (&*correction_b.request_id, &*correction_b.text),
        ("request-b", "only B")
    );
    correction_b.incorporate(|_, _| {});
    assert_eq!(receipt_b.try_recv(), Ok(Delivery::Delivered));
    assert_eq!(
        receipt_a.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    );
    let correction_a = receive(&mut session_a);
    assert_eq!(
        (&*correction_a.request_id, &*correction_a.text),
        ("request-a", "only A")
    );
    correction_a.incorporate(|_, _| {});
    assert_eq!(receipt_a.try_recv(), Ok(Delivery::Delivered));
    assert!(matches!(
        session_a.try_recv_input(),
        Err(TryRecvError::Empty)
    ));
    assert!(matches!(
        session_b.try_recv_input(),
        Err(TryRecvError::Empty)
    ));
    assert!(!session_a.is_attached());
    assert!(!session_b.is_attached());
}

#[test]
fn independent_handles_keep_queued_requests_and_receipts_after_one_disconnects() {
    let registry = InputRegistry::default();
    let (mut factory, first_handle) = InteractiveSessionFactory::for_pipeline();
    first_handle.set_input_registry(registry.clone());
    let second_handle = first_handle.clone();
    let id = identity("A", "a-1");
    let mut session = factory.next_session_for_stage("a-1", true, None).unwrap();
    session.bind_input(&registry, id.clone(), true, None);
    let first_sender = first_handle.input_sender(&id).unwrap();
    let second_sender = second_handle.input_sender(&id).unwrap();
    let mut first_receipt = first_sender.try_send("first".into(), "one".into()).unwrap();
    let mut second_receipt = second_sender
        .try_send("second".into(), "two".into())
        .unwrap();
    drop(first_sender);
    drop(first_handle);

    let first = receive(&mut session);
    let second = receive(&mut session);
    assert_eq!((&*first.request_id, &*first.text), ("first", "one"));
    assert_eq!((&*second.request_id, &*second.text), ("second", "two"));
    second.incorporate(|_, _| {});
    assert_eq!(second_receipt.try_recv(), Ok(Delivery::Delivered));
    assert_eq!(
        first_receipt.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    );
    drop(first);
    assert_eq!(first_receipt.try_recv(), Ok(Delivery::NotDelivered));

    let mut receipts = Vec::new();
    for index in 0..INPUT_CAPACITY {
        receipts.push(
            second_sender
                .try_send(format!("queued-{index}"), format!("text-{index}"))
                .unwrap(),
        );
    }
    assert!(matches!(
        second_sender.try_send("overflow".into(), "lost".into()),
        Err(InputRejected::Full)
    ));
    for (index, receipt) in receipts.iter_mut().enumerate() {
        let correction = receive(&mut session);
        assert_eq!(correction.request_id, format!("queued-{index}"));
        assert_eq!(correction.text, format!("text-{index}"));
        correction.incorporate(|_, _| {});
        assert_eq!(receipt.try_recv(), Ok(Delivery::Delivered));
    }
    assert!(matches!(session.try_recv_input(), Err(TryRecvError::Empty)));
}

#[test]
fn cancellation_during_backend_selection_never_publishes_accepting_input() {
    for cancel_before_session in [false, true] {
        let registry = InputRegistry::default();
        let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
        handle.set_input_registry(registry.clone());
        let cancel = CancelToken::new();
        if cancel_before_session {
            cancel.cancel();
        }
        let id = identity("A", "a-1");
        let mut session = factory
            .next_session_for_stage("a-1", true, Some(&cancel))
            .unwrap();
        if !cancel_before_session {
            cancel.cancel();
        }
        assert!(handle.input_sender(&id).is_none());
        session.bind_input(&registry, id.clone(), true, Some(&cancel));
        assert_eq!(session.input_identity.as_ref(), Some(&id));
        assert!(handle.input_sender(&id).is_none());
        assert!(matches!(
            session.try_recv_input(),
            Err(TryRecvError::Disconnected)
        ));
    }
}

#[test]
fn ended_execution_never_publishes_accepting_input() {
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("A", "a-1");
    let mut session = factory.next_session_for_stage("a-1", true, None).unwrap();
    session
        .slot
        .end_execution(session.nonce, &session.end_signal);
    session.bind_input(&registry, id.clone(), true, None);
    assert!(handle.input_sender(&id).is_none());
    assert!(matches!(
        session.try_recv_input(),
        Err(TryRecvError::Disconnected)
    ));
}

#[test]
fn late_bind_after_factory_drop_never_publishes_accepting_input() {
    for chat in [false, true] {
        let registry = InputRegistry::default();
        let (mut factory, handle) = if chat {
            InteractiveSessionFactory::for_chat()
        } else {
            InteractiveSessionFactory::for_pipeline()
        };
        handle.set_input_registry(registry.clone());
        let id = identity("A", "a-1");
        let mut session = factory.next_session_for_stage("a-1", true, None).unwrap();
        drop(factory);

        assert!(session.end_signal.is_ended());
        session.bind_input(&registry, id.clone(), true, None);
        assert!(handle.input_sender(&id).is_none());
        assert!(matches!(
            session.try_recv_input(),
            Err(TryRecvError::Disconnected)
        ));
    }
}

#[test]
fn cancellation_settles_queued_and_polled_input_without_consumer_progress() {
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let cancel = CancelToken::new();
    let old_id = identity("A", "a-1");
    let mut old = factory
        .next_session_for_stage("a-1", true, Some(&cancel))
        .unwrap();
    old.bind_input(&registry, old_id.clone(), true, Some(&cancel));
    let sender = handle.input_sender(&old_id).unwrap();
    let mut delivered = sender.try_send("delivered".into(), "done".into()).unwrap();
    receive(&mut old).incorporate(|_, _| {});
    let mut polled_receipt = sender.try_send("polled".into(), "held".into()).unwrap();
    let polled = receive(&mut old);
    let mut queued_receipt = sender.try_send("queued".into(), "waiting".into()).unwrap();

    cancel.cancel();
    // Neither the receiver nor the outstanding correction progresses here.
    assert_eq!(polled_receipt.try_recv(), Ok(Delivery::NotDelivered));
    assert_eq!(queued_receipt.try_recv(), Ok(Delivery::NotDelivered));
    assert_eq!(delivered.try_recv(), Ok(Delivery::Delivered));
    assert!(handle.input_sender(&old_id).is_none());
    assert!(matches!(
        sender.try_send("late".into(), "stale".into()),
        Err(InputRejected::Revoked)
    ));
    polled.incorporate(|_, _| {});

    let new_id = identity("A", "a-2");
    let mut successor = factory.next_session_for_stage("a-2", true, None).unwrap();
    successor.bind_input(&registry, new_id.clone(), true, None);
    assert_eq!(successor.input_identity.as_ref(), Some(&new_id));
    assert!(matches!(
        successor.try_recv_input(),
        Err(TryRecvError::Empty)
    ));
    drop(old);
    let mut fresh = handle
        .input_sender(&new_id)
        .unwrap()
        .try_send("fresh".into(), "new".into())
        .unwrap();
    let correction = receive(&mut successor);
    assert_eq!(correction.request_id, "fresh");
    correction.incorporate(|_, _| {});
    assert_eq!(fresh.try_recv(), Ok(Delivery::Delivered));
}

#[test]
fn execution_drop_and_factory_teardown_revoke_retained_senders() {
    for teardown_factory in [false, true] {
        let registry = InputRegistry::default();
        let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
        handle.set_input_registry(registry.clone());
        let id = identity("A", "a-1");
        let mut session = factory.next_session_for_stage("a-1", true, None).unwrap();
        session.bind_input(&registry, id.clone(), true, None);
        let sender = handle.input_sender(&id).unwrap();
        let mut polled_receipt = sender.try_send("polled".into(), "held".into()).unwrap();
        let polled = receive(&mut session);
        let mut queued_receipt = sender.try_send("queued".into(), "waiting".into()).unwrap();
        if teardown_factory {
            drop(factory);
            // The execution is still alive; factory teardown alone must revoke.
            assert!(matches!(
                session.try_recv_input(),
                Err(TryRecvError::Disconnected)
            ));
        } else {
            drop(session);
        }
        assert_eq!(polled_receipt.try_recv(), Ok(Delivery::NotDelivered));
        assert_eq!(queued_receipt.try_recv(), Ok(Delivery::NotDelivered));
        assert!(handle.input_sender(&id).is_none());
        assert!(matches!(
            sender.try_send("late".into(), "stale".into()),
            Err(InputRejected::Revoked)
        ));
        polled.incorporate(|_, _| {});
    }
}

#[tokio::test]
async fn factory_drop_wakes_next_input_on_paused_session_without_cancellation() {
    paused_next_input_ends(true).await;
}

#[tokio::test]
async fn replacement_wakes_next_input_on_paused_session_without_cancellation() {
    paused_next_input_ends(false).await;
}

async fn paused_next_input_ends(drop_factory: bool) {
    use std::future::{poll_fn, Future};
    use std::task::Poll;
    use std::time::Duration;

    let registry = InputRegistry::default();
    let (mut factory, _handle) = InteractiveSessionFactory::for_pipeline();
    let mut session = factory.next_session_for_stage("a-1", true, None).unwrap();
    session.bind_input(&registry, identity("A", "a-1"), true, None);
    session.set_inbox_paused(true);
    {
        let next = session.next_input();
        tokio::pin!(next);
        poll_fn(|cx| {
            assert!(next.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        let _successor = if drop_factory {
            drop(factory);
            None
        } else {
            Some(factory.next_session_for_stage("a-2", true, None).unwrap())
        };
        assert!(tokio::time::timeout(Duration::from_secs(1), next)
            .await
            .expect("execution end must wake paused input without Resume")
            .is_none());
    }
    assert!(session.inbox_paused());
    tokio::time::timeout(Duration::from_secs(1), session.wait_execution_end())
        .await
        .expect("execution end remains latched");
    assert!(session.next_input().await.is_none());
}

#[test]
fn replacement_revokes_previous_input_even_while_previous_session_is_alive() {
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let old_id = identity("A", "a-1");
    let mut old = factory.next_session_for_stage("a-1", true, None).unwrap();
    old.bind_input(&registry, old_id.clone(), true, None);
    let old_sender = handle.input_sender(&old_id).unwrap();
    let mut receipt = old_sender.try_send("old".into(), "pending".into()).unwrap();

    let new_id = identity("A", "a-2");
    let mut successor = factory.next_session_for_stage("a-2", true, None).unwrap();
    successor.bind_input(&registry, new_id.clone(), true, None);
    assert_eq!(receipt.try_recv(), Ok(Delivery::NotDelivered));
    assert!(handle.input_sender(&old_id).is_none());
    assert!(matches!(
        old_sender.try_send("late".into(), "stale".into()),
        Err(InputRejected::Revoked)
    ));
    assert!(matches!(
        old.try_recv_input(),
        Err(TryRecvError::Disconnected)
    ));
    assert!(matches!(
        successor.try_recv_input(),
        Err(TryRecvError::Empty)
    ));
    drop(old);
    let mut receipt = handle
        .input_sender(&new_id)
        .unwrap()
        .try_send("new".into(), "current".into())
        .unwrap();
    receive(&mut successor).incorporate(|_, _| {});
    assert_eq!(receipt.try_recv(), Ok(Delivery::Delivered));
}
