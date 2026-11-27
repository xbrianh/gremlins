use super::{app::App, protocol::Execution, reconcile_session_chat_follow, Attachment, Target};

#[test]
fn identity_ack_retires_selected_passive_follow_without_retiring_session_owner() {
    let mut app = App::new();
    let previous = app.session_chat_target();
    let destination = Target::Run {
        run_id: "replacement".into(),
    };
    app.select_composer(destination.clone());
    app.replace_input("destination draft");
    app.conversations.begin_session_chat_turn("turn".into());
    let execution = Execution {
        run_id: "replacement".into(),
        stage_run_id: "stage".into(),
        nonce: 7,
    };
    app.conversations
        .session_chat_turn_started("turn", Some(execution.clone()));
    let mut attachment = Attachment {
        target: Some(destination.clone()),
        generation: 4,
        ..Attachment::default()
    };
    app.migrate_chat_run_id("replacement".into());

    reconcile_session_chat_follow(&mut app, &mut attachment, &previous);

    assert!(attachment.target.is_none());
    assert_eq!(attachment.generation, 5);
    assert_eq!(app.selected_target, destination);
    assert_eq!(app.input.text(), "destination draft");
    assert_eq!(app.conversations.session_chat_turn(), Some("turn"));
    assert_eq!(
        app.conversations.snapshot_execution(&destination),
        Some(execution)
    );
}
