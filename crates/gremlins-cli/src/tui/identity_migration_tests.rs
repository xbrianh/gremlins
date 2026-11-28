use super::{
    app::{App, ConversationStatus, Target},
    screen::LiveArea,
    session_chat_events::{apply_session_chat_event, apply_session_chat_identity},
    SessionChatInterrupt,
};
use ratatui::backend::TestBackend;
use serde_json::json;

fn run(id: &str) -> Target {
    Target::Run { run_id: id.into() }
}

fn live_area(app: &App) -> LiveArea<TestBackend> {
    let mut live = LiveArea::with_backend(TestBackend::new(80, 24), 80, 24);
    live.sync_app_state(app);
    live
}

fn rendered(app: &App, live: &mut LiveArea<TestBackend>) -> String {
    live.compose(app).unwrap();
    let buffer = live.backend().buffer();
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn late_earlier_turn_done_does_not_end_current_turn() {
    let mut app = App::new();
    let target = app.session_chat_target();
    let mut live = live_area(&app);
    app.conversations.begin_session_chat_turn("earlier".into());
    app.conversations.retire_session_chat_turn();
    app.conversations.begin_session_chat_turn("current".into());
    app.set_conversation_status(target.clone(), ConversationStatus::Running);
    app.active_request = true;
    live.submit_prompt("current prompt".into());
    live.push_response("current response");

    apply_session_chat_event(
        &mut app,
        &mut live,
        &mut SessionChatInterrupt::default(),
        gremlins_magic_bus::EventKind::Done {
            text: "stale completion".into(),
            summary: "stale summary".into(),
            usage: None,
        },
        Some("earlier".into()),
        None,
    )
    .unwrap();

    assert_eq!(app.conversations.session_chat_turn(), Some("current"));
    assert!(app.active_request);
    assert_eq!(
        app.conversation_status(&target),
        ConversationStatus::Running
    );
    assert!(live.target_has_current_turn(&target));
    let text = rendered(&app, &mut live);
    assert!(text.contains("current prompt"));
    assert!(text.contains("current response"));
    assert!(!text.contains("stale completion"));
    assert!(!text.contains("stale summary"));
}

#[test]
fn turnless_warning_is_displayed_independent_of_active_turn() {
    for active in [false, true] {
        let mut app = App::new();
        let target = app.session_chat_target();
        let mut live = live_area(&app);
        if active {
            app.conversations.begin_session_chat_turn("current".into());
            app.set_conversation_status(target.clone(), ConversationStatus::Running);
            app.active_request = true;
            live.submit_prompt("current prompt".into());
            live.push_response("current response");
        }
        let status = app.conversation_status(&target);
        live.select_conversation(&mut app, run("other"));
        app.replace_input("other draft");

        apply_session_chat_event(
            &mut app,
            &mut live,
            &mut SessionChatInterrupt::default(),
            gremlins_magic_bus::EventKind::SystemWarning {
                message: crate::daemon_client::chat::DISCONTINUITY_NOTICE.into(),
                turn: None,
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(app.selected_target, run("other"));
        assert_eq!(app.input.text(), "other draft");
        assert!(
            !rendered(&app, &mut live).contains(crate::daemon_client::chat::DISCONTINUITY_NOTICE)
        );
        assert_eq!(
            app.conversations.session_chat_turn(),
            active.then_some("current")
        );
        assert_eq!(app.conversation_status(&target), status);
        assert_eq!(live.target_has_current_turn(&target), active);
        live.select_conversation(&mut app, target);
        let text = rendered(&app, &mut live);
        assert!(
            text.contains(crate::daemon_client::chat::DISCONTINUITY_NOTICE),
            "{text}"
        );
        if active {
            assert!(app.active_request);
            assert!(text.contains("current response"));
        }
    }
}

#[test]
fn app_preinstall_identity_is_nonempty_and_has_consistent_ownership() {
    let app = App::new();
    let id = app.chat_run_id.as_deref().expect("preinstall identity");
    assert!(!id.is_empty());
    let target = run(id);
    assert_eq!(app.selected_target, target);
    assert_eq!(app.session_chat_target(), target);
    assert_eq!(app.conversations.session_chat_target(), &target);
    assert!(app.is_session_chat(&target));
    assert_eq!(app.conversation_status(&target), ConversationStatus::Idle);
}

#[test]
fn background_app_migration_collision_preserves_destination_draft() {
    let mut app = App::new();
    let source = app.session_chat_target();
    let destination = run("replacement");
    let other = run("other");
    app.conversations.begin_session_chat_turn("turn".into());
    app.set_conversation_status(source.clone(), ConversationStatus::Running);
    app.replace_input("source draft");
    app.input_cursor = 3;
    app.select_composer(destination.clone());
    app.replace_input("destination draft");
    app.input_cursor = 5;
    app.select_composer(other.clone());
    app.replace_input("other draft");
    app.input_cursor = 2;

    app.migrate_chat_run_id("replacement".into());

    assert_eq!(app.selected_target, other);
    assert_eq!(app.input.text(), "other draft");
    assert_eq!(app.input_cursor, 2);
    assert_eq!(app.session_chat_target(), destination);
    assert_eq!(app.conversations.session_chat_turn(), Some("turn"));
    assert_eq!(
        app.conversation_status(&destination),
        ConversationStatus::Running
    );
    app.select_composer(destination);
    assert_eq!(app.input.text(), "destination draft");
    assert_eq!(app.input_cursor, 5);
    app.select_composer(source);
    assert!(app.input.is_empty());
    assert_eq!(app.input_cursor, 0);
}

#[test]
fn selected_destination_migration_loads_source_transcript_status_and_model_not_draft() {
    let mut app = App::new();
    let source = app.session_chat_target();
    let destination = run("replacement");
    let mut live = live_area(&app);
    app.conversations.begin_session_chat_turn("turn".into());
    app.set_conversation_status(source.clone(), ConversationStatus::Running);
    app.active_request = true;
    app.chat_model = Some("source-model".into());
    app.replace_input("source draft");
    app.input_cursor = 3;
    live.push_transcript_line("source transcript".into());
    live.select_conversation(&mut app, destination.clone());
    app.chat_model = Some("destination-model".into());
    app.replace_input("destination draft");
    app.input_cursor = 5;
    live.push_transcript_line("destination transcript".into());
    assert!(!app.active_request);

    apply_session_chat_identity(
        &mut app,
        &mut live,
        &json!({"type":"session_chat_run_identity", "generation":7,
            "turn":"turn", "id":"replacement"}),
        7,
    );

    assert_eq!(app.selected_target, destination);
    assert_eq!(app.session_chat_target(), destination);
    assert_eq!(app.chat_run_id.as_deref(), Some("replacement"));
    assert_eq!(app.conversations.session_chat_turn(), Some("turn"));
    assert!(app.active_request);
    assert_eq!(
        app.conversation_status(&destination),
        ConversationStatus::Running
    );
    assert_eq!(app.chat_model.as_deref(), Some("source-model"));
    assert_eq!(app.input.text(), "destination draft");
    assert_eq!(app.input_cursor, 5);
    let text = rendered(&app, &mut live);
    assert!(text.contains("source transcript"));
    assert!(!text.contains("destination transcript"));

    live.select_conversation(&mut app, run("other"));
    live.select_conversation(&mut app, destination);
    assert_eq!(app.input.text(), "destination draft");
    assert_eq!(app.input_cursor, 5);
    assert!(app.active_request);
    assert_eq!(app.chat_model.as_deref(), Some("source-model"));
    live.select_conversation(&mut app, source);
    assert!(app.input.is_empty());
    assert!(!rendered(&app, &mut live).contains("source transcript"));
}

#[test]
fn selected_source_migration_keeps_preexisting_destination_draft() {
    let mut app = App::new();
    let source = app.session_chat_target();
    let destination = run("replacement");
    app.select_composer(destination.clone());
    app.replace_input("destination draft");
    app.input_cursor = 5;
    app.select_composer(source);
    app.replace_input("source draft");

    app.migrate_chat_run_id("replacement".into());

    assert_eq!(app.selected_target, destination);
    assert_eq!(app.input.text(), "destination draft");
    assert_eq!(app.input_cursor, 5);
}

#[test]
fn migration_rekeys_only_the_session_watch_selection() {
    for follows_session in [true, false] {
        let mut app = App::new();
        let old = app.chat_run_id.clone().unwrap();
        let highlighted = if follows_session { old } else { "other".into() };
        app.watch_selection = Some(highlighted.clone());
        app.select_composer(run("selected-other"));

        app.migrate_chat_run_id("replacement".into());

        let expected = if follows_session {
            "replacement"
        } else {
            &highlighted
        };
        assert_eq!(app.watch_selection.as_deref(), Some(expected));
        assert_eq!(app.selected_target, run("selected-other"));
    }
}

#[test]
fn parked_chat_model_reply_is_restored_on_conversation_selection() {
    let mut app = App::new();
    let session = app.session_chat_target();
    let other = run("other");
    let mut live = live_area(&app);
    app.chat_model = Some("old-model".into());
    live.select_conversation(&mut app, other.clone());
    app.chat_model = Some("other-model".into());
    app.replace_input("other draft");
    app.input_cursor = 4;

    live.set_target_chat_model(&session, Some("queried-model".into()));

    assert_eq!(app.selected_target, other);
    assert_eq!(app.chat_model.as_deref(), Some("other-model"));
    assert_eq!(app.input.text(), "other draft");
    live.select_conversation(&mut app, session.clone());
    assert_eq!(app.chat_model.as_deref(), Some("queried-model"));
    live.select_conversation(&mut app, other);
    assert_eq!(app.chat_model.as_deref(), Some("other-model"));
    assert_eq!(app.input.text(), "other draft");
    assert_eq!(app.input_cursor, 4);
    live.set_target_chat_model(&session, None);
    live.select_conversation(&mut app, session);
    assert_eq!(app.chat_model, None);
}
