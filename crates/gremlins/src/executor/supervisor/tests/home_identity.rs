use super::*;

async fn reserved_session(
    registry: &TestRegistry,
    sessions: &super::super::SessionRegistry,
) -> (serde_json::Value, SessionTaskHandle) {
    let response =
        super::super::ops::handle_create_session(&serde_json::json!({}), registry, sessions).await;
    assert_eq!(response["type"], "ok");
    let session_id = response["session_id"].as_str().unwrap();
    let session = sessions.lock().unwrap().get(session_id).unwrap().clone();
    assert_eq!(
        session.get_home_run_id().await.as_deref(),
        response["home_run_id"].as_str()
    );
    (response, session)
}

#[tokio::test]
async fn closed_session_chat_setup_keeps_home_reservation_until_teardown() {
    let (_sandbox, project_root, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let sessions = Default::default();
    let (response, session) = reserved_session(&registry, &sessions).await;
    let home_id = response["home_run_id"].as_str().unwrap().to_string();
    assert_eq!(session.get_home_run_id().await, Some(home_id.clone()));
    session.mark_closed().await;

    let result = super::super::chat::resolve_chat_handle(
        &registry,
        &session,
        &model("cmd/echo"),
        "helpful",
        &project_root,
        &HashMap::new(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(session.get_home_run_id().await, Some(home_id.clone()));
    assert_eq!(session.get_chat_ids().await, vec![home_id.clone()]);
    assert!(!registry.contains_run(&home_id).await);
    teardown_session(&session).await;
    assert!(session.get_home_run_id().await.is_none());
    assert!(session.get_chat_ids().await.is_empty());
}

#[tokio::test]
async fn rejected_chat_spawn_does_not_commit_home_reservation() {
    let (_sandbox, project_root, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let sessions = Default::default();
    let (response, session) = reserved_session(&registry, &sessions).await;
    let home_id = response["home_run_id"].as_str().unwrap().to_string();
    let gremlin = create_chat_gremlin(
        &home_id,
        &model("cmd/echo"),
        "helpful",
        &project_root,
        &HashMap::new(),
    )
    .unwrap();
    session.mark_closed().await;
    assert!(super::super::chat::spawn_chat_gremlin(
        gremlin,
        Arc::clone(&registry),
        session.clone(),
    )
    .await
    .is_none());
    assert_eq!(session.get_home_run_id().await, Some(home_id.clone()));
    assert!(!registry.contains_run(&home_id).await);
    assert!(session.get_chat_gremlin_keys().await.is_empty());
    teardown_session(&session).await;
    assert!(session.get_home_run_id().await.is_none());
}

#[tokio::test]
async fn reserved_home_identity_survives_failures_then_resolves_reuses_and_replaces() {
    let (_sandbox, project_root, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let sessions: super::super::SessionRegistry = Default::default();
    let (response, session) = reserved_session(&registry, &sessions).await;
    let home_id = response["home_run_id"].as_str().unwrap();
    assert_eq!(session.get_chat_ids().await, vec![home_id.to_string()]);
    assert!(!registry.contains_run(home_id).await);
    assert!(session.get_chat_gremlin_keys().await.is_empty());
    assert_ne!(registry.reserve_chat_id().await, home_id);

    let chat_model = model("cmd/echo");
    let env = HashMap::new();
    for _ in 0..2 {
        let result = super::super::chat::resolve_chat_handle(
            &registry,
            &session,
            &model("fake/m"),
            "helpful",
            &project_root,
            &env,
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("unknown provider should fail chat setup"),
        };
        assert!(error.to_string().contains("unknown provider"));
        assert_eq!(session.get_home_run_id().await.as_deref(), Some(home_id));
        assert_eq!(session.get_chat_ids().await, vec![home_id.to_string()]);
        assert!(!registry.contains_run(home_id).await);
        assert!(session.get_chat_gremlin_keys().await.is_empty());
        assert!(session.get_chat_handle().await.is_none());
    }

    let handle = super::super::chat::resolve_chat_handle(
        &registry,
        &session,
        &chat_model,
        "helpful",
        &project_root,
        &env,
    )
    .await
    .unwrap();
    assert_eq!(handle.id, home_id);
    assert!(session.get_home_run_id().await.is_none());
    assert!(registry.contains_run(home_id).await);
    let reused = super::super::chat::resolve_chat_handle(
        &registry,
        &session,
        &chat_model,
        "helpful",
        &project_root,
        &env,
    )
    .await
    .unwrap();
    assert_eq!(reused.id, home_id);

    registry.stop_run(home_id).await;
    assert!(!registry.contains_run(home_id).await);
    let fresh = super::super::chat::resolve_chat_handle(
        &registry,
        &session,
        &chat_model,
        "helpful",
        &project_root,
        &env,
    )
    .await
    .unwrap();
    assert_ne!(fresh.id, home_id);
    teardown_session(&session).await;
    assert!(!registry.contains_run(&fresh.id).await);
    assert!(!registry.contains_run(home_id).await);
    assert!(sessions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_session_unused_home_reservation_is_owned_and_torn_down() {
    let (_sandbox, _, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let sessions: super::super::SessionRegistry = Default::default();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (response, session) = reserved_session(&registry, &sessions).await;
        let id = response["home_run_id"].as_str().unwrap().to_string();
        assert_eq!(session.get_chat_ids().await, vec![id.clone()]);
        teardown_session(&session).await;
        assert!(session.get_chat_ids().await.is_empty());
        assert!(session.get_home_run_id().await.is_none());
        assert!(!registry.contains_run(&id).await);
        assert!(!crate::config::state_root()
            .join(response["socket"].as_str().unwrap())
            .exists());
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
    assert!(sessions.lock().unwrap().is_empty());
}
