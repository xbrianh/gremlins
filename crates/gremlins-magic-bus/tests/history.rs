use gremlins_magic_bus::{
    emit_corr_event, emit_corr_event_async, emit_event, emit_run_event_async, init_bus,
    subscribe_run_with_history, CorrId, EventKind, TryRecvError,
};

#[tokio::test]
async fn concrete_emitters_retain_history_before_explicit_initialization() {
    // This integration test runs in its own process with a fresh global bus.
    let id = "early-history";
    emit_event(EventKind::RunStopped { id: id.into() });
    let child = CorrId::new()
        .with_run("early-history:child")
        .with_stage("build");
    emit_corr_event(
        &child,
        EventKind::RunStopped {
            id: "child-sync".into(),
        },
    );
    emit_corr_event_async(
        &child,
        EventKind::RunStopped {
            id: "child-async".into(),
        },
    )
    .await;
    emit_run_event_async(
        id,
        EventKind::RunStopped {
            id: "run-async".into(),
        },
    )
    .await;
    init_bus();

    let (sub, mut rx, history) = subscribe_run_with_history(id, 16);
    assert_eq!(history.events.len(), 4);
    assert!(history.complete);
    assert_eq!(history.through_sequence, 4);
    assert_eq!(history.evicted_through_sequence, 0);
    let labels: Vec<_> = history
        .events
        .iter()
        .map(|event| match event.kind() {
            EventKind::RunStopped { id } => id.as_str(),
            other => panic!("unexpected event: {other:?}"),
        })
        .collect();
    assert_eq!(labels, [id, "child-sync", "child-async", "run-async"]);
    assert_eq!(history.events[0].corr_id(), &CorrId::new().with_run(id));
    assert_eq!(history.events[3].corr_id(), &CorrId::new().with_run(id));
    assert_eq!(history.events[1].corr_id(), &child);
    assert_eq!(history.events[2].corr_id(), &child);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    // Prompt-debug payloads are live-only, but still advance the global cut.
    emit_event(EventKind::PromptRequest {
        id: id.into(),
        json: r#"{"messages":[]}"#.into(),
    });
    let prompt = rx.try_recv().unwrap();
    assert!(
        matches!(prompt.kind(), EventKind::PromptRequest { id: run, json }
        if run == id && json == r#"{"messages":[]}"#)
    );
    assert_eq!(prompt.corr_id().run(), Some(id));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    let (omitted_sub, mut omitted_rx, omitted) = subscribe_run_with_history(id, 16);
    assert_eq!(omitted.events.len(), 4);
    assert!(omitted
        .events
        .iter()
        .all(|event| !matches!(event.kind(), EventKind::PromptRequest { .. })));
    assert!(!omitted.complete);
    assert_eq!(prompt.sequence(), 5);
    assert_eq!(omitted.through_sequence, 5);
    assert_eq!(omitted.evicted_through_sequence, 5);
    assert!(matches!(omitted_rx.try_recv(), Err(TryRecvError::Empty)));

    emit_event(EventKind::RunStopped { id: id.into() });
    let (unrelated_sub, _, unrelated) = subscribe_run_with_history("unrelated-history", 16);
    assert!(unrelated.events.is_empty());
    assert!(!unrelated.complete);
    assert_eq!(unrelated.through_sequence, 6);
    assert_eq!(unrelated.evicted_through_sequence, 5);

    init_bus().unsubscribe(&omitted_sub);
    init_bus().unsubscribe(&unrelated_sub);
    init_bus().unsubscribe(&sub);
}
