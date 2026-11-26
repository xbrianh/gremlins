use gremlins_magic_bus::{Bus, CorrId, DispatchEvent, Recv, SubscriberRegistration, TryRecvError};

#[derive(Clone, Debug, PartialEq)]
enum Payload {
    Started,
    Chunk,
    Done,
    Noise,
}

fn event(run: &str, invocation: Option<&str>, payload: Payload) -> DispatchEvent<Payload> {
    let mut corr = CorrId::new().with_run(run);
    if let Some(invocation) = invocation {
        corr = corr.with_stage("agent").with_stage_run_id(invocation);
    }
    DispatchEvent::new(corr, payload)
}

fn selected(event: &DispatchEvent<Payload>) -> bool {
    matches!(event.kind(), Payload::Started | Payload::Done)
        || (event.kind() == &Payload::Chunk && event.corr_id().stage_run_id() == Some("selected"))
}

async fn publish(bus: &Bus<Payload>, event: DispatchEvent<Payload>, asynchronous: bool) {
    if asynchronous {
        bus.emit_async(event).await;
    } else {
        bus.emit(event);
    }
}

#[tokio::test]
async fn filtered_history_and_live_keep_selected_events_and_lifecycle_in_sequence() {
    for asynchronous in [false, true] {
        let bus = Bus::new();
        let batch = || {
            vec![
                event("run", None, Payload::Started),
                event("run", Some("sibling"), Payload::Chunk),
                event("run", Some("selected"), Payload::Chunk),
                event("other", None, Payload::Done),
                event("run", None, Payload::Noise),
                event("run:child", None, Payload::Done),
            ]
        };
        for event in batch() {
            publish(&bus, event, asynchronous).await;
        }
        let (_sub, mut rx, history) = bus
            .subscribe_with_history_filtered(
                SubscriberRegistration::lossless(CorrId::new().with_run("run")),
                selected,
            )
            .unwrap();
        assert!(history.complete);
        assert_eq!(history.through_sequence, 6);
        assert_eq!(history.evicted_through_sequence, 0);
        assert_eq!(
            history
                .events
                .iter()
                .map(|e| (e.sequence(), e.kind().clone()))
                .collect::<Vec<_>>(),
            vec![
                (1, Payload::Started),
                (3, Payload::Chunk),
                (6, Payload::Done)
            ]
        );
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        for event in batch() {
            publish(&bus, event, asynchronous).await;
        }
        for (sequence, payload) in [
            (7, Payload::Started),
            (9, Payload::Chunk),
            (12, Payload::Done),
        ] {
            let Recv::Event(event) = rx.recv().await else {
                panic!("expected event")
            };
            assert_eq!(event.sequence(), sequence);
            assert_eq!(event.kind(), &payload);
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }
}

#[tokio::test]
async fn filtered_sibling_flood_does_not_lag_one_ordered_mailbox() {
    for asynchronous in [false, true] {
        let bus = Bus::new();
        let (sub, mut rx) = bus
            .subscribe_filtered(
                SubscriberRegistration::lag_tolerant(CorrId::new().with_run("run"), 3),
                selected,
            )
            .unwrap();
        publish(&bus, event("run", None, Payload::Started), asynchronous).await;
        for _ in 0..1000 {
            publish(
                &bus,
                event("run", Some("sibling"), Payload::Chunk),
                asynchronous,
            )
            .await;
            publish(&bus, event("other", None, Payload::Done), asynchronous).await;
        }
        publish(
            &bus,
            event("run", Some("selected"), Payload::Chunk),
            asynchronous,
        )
        .await;
        publish(&bus, event("run", None, Payload::Done), asynchronous).await;
        for (sequence, payload) in [
            (1, Payload::Started),
            (2002, Payload::Chunk),
            (2003, Payload::Done),
        ] {
            let Recv::Event(event) = rx.recv().await else {
                panic!("flood caused lag or closure")
            };
            assert_eq!(event.sequence(), sequence);
            assert_eq!(event.kind(), &payload);
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        bus.unsubscribe(&sub);
        drop(sub);
        assert!(matches!(rx.recv().await, Recv::Closed));
    }
}

#[tokio::test]
async fn filtered_rejected_events_do_not_wait_for_bounded_capacity() {
    let bus = Bus::new();
    let (_sub, mut rx) = bus
        .subscribe_filtered(
            SubscriberRegistration::backpressured(CorrId::new().with_run("run"), 1),
            selected,
        )
        .unwrap();
    bus.emit(event("run", None, Payload::Started));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        bus.emit_async(event("run", Some("sibling"), Payload::Chunk))
            .await;
    })
    .await
    .expect("rejected event must not reserve bounded capacity");
    let Recv::Event(event) = rx.recv().await else {
        panic!("expected start")
    };
    assert_eq!(event.kind(), &Payload::Started);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}
