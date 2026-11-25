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

async fn home_frame(reader: &mut tokio::io::BufReader<tokio::net::UnixStream>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), socket::read_json_line(reader))
        .await
        .expect("home socket response timed out")
        .unwrap()
        .expect("home socket closed")
}

#[tokio::test]
async fn announced_home_execution_resolves_input_and_shares_legacy_controller_claim() {
    use crate::executor::supervisor::test_provider::pending_request;
    use serde_json::json;
    use tokio::{io::BufReader, net::UnixStream};

    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key": "test", "base-url": format!("http://{}", provider.local_addr().unwrap())
        }})
        .to_string(),
    );
    std::fs::write(
        sandbox.join("config/settings.yaml"),
        "default-model: openrouter/test-home-input\n",
    )
    .unwrap();
    let (registry, _) = spawn_test_actor().await;
    let sessions = Default::default();
    let (created, session) = reserved_session(&registry, &sessions).await;
    let path = crate::config::state_root().join(created["socket"].as_str().unwrap());
    let mut main = BufReader::new(UnixStream::connect(&path).await.unwrap());
    socket::write_json_line(
        main.get_mut(),
        &json!({"op":"chat", "session_id":created["session_id"],
            "text":"Review draft", "turn":"home-input", "project_root":sandbox.path()}),
    )
    .await
    .unwrap();
    let started = loop {
        let event = home_frame(&mut main).await;
        assert_ne!(event["type"], "error", "{event}");
        if event["type"] == "turn_started" {
            break event;
        }
    };
    assert_eq!(started["id"], created["home_run_id"]);
    // The provider request is a barrier after run_agent has bound its inbox.
    let (_, held) = pending_request(&provider).await;
    let run_id = started["id"].as_str().unwrap();
    let execution_id = started["stage_run_id"].as_str().unwrap();
    let (interactive, _, _) = registry.get_run_interactive(run_id).await.unwrap();
    let (identity, sender) = interactive
        .resolve_input(run_id, execution_id, None)
        .expect("TurnStarted identity must resolve to the running home inbox");
    assert_eq!(identity.run_id, run_id);
    // The announced descriptor aliases the invocation's canonical identity.
    let (canonical, canonical_sender) = interactive
        .resolve_input(run_id, &identity.stage_run_id, Some(&identity.stage_name))
        .expect("the invocation's exact identity must resolve too");
    assert_eq!(canonical, identity);
    assert!(!canonical_sender.is_closed());
    assert!(!sender.is_closed());
    assert_eq!(interactive.target().stage_run_id(), Some(execution_id));
    assert_eq!(started["nonce"], interactive.target().nonce());

    let mut exact = BufReader::new(UnixStream::connect(&path).await.unwrap());
    socket::write_json_line(
        exact.get_mut(),
        &json!({"op":"input", "session_id":created["session_id"],
            "id":run_id, "stage_run_id":execution_id, "req_id":"exact"}),
    )
    .await
    .unwrap();
    let bound = home_frame(&mut exact).await;
    assert_eq!(bound["type"], "input_bound", "{bound}");
    assert_eq!(bound["stage_run_id"], identity.stage_run_id);
    assert_eq!(bound["stage"], identity.stage_name);

    let mut legacy = BufReader::new(UnixStream::connect(&path).await.unwrap());
    socket::write_json_line(
        legacy.get_mut(),
        &json!({"op":"attach", "session_id":created["session_id"],
            "scope":"conversation", "mode":"control", "id":run_id,
            "stage_run_id":execution_id, "nonce":started["nonce"], "req_id":"legacy"}),
    )
    .await
    .unwrap();
    let busy = home_frame(&mut legacy).await;
    assert_eq!(busy["type"], "error", "{busy}");
    assert_eq!(busy["req_id"], "legacy");
    assert!(busy.to_string().contains("controller_busy"), "{busy}");

    socket::write_json_line(exact.get_mut(), &json!({"cmd":"detach"}))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), socket::read_json_line(&mut exact))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    session.cancel_turn(None).await.unwrap();
    drop(held);
    teardown_session(&session).await;
    assert!(sender.is_closed());
    assert!(interactive
        .resolve_input(run_id, execution_id, None)
        .is_none());
}
