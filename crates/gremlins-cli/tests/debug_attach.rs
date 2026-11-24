//! Standalone debug uses the conversation protocol, without a terminal UI.
use gremlins::executor::socket;
use serde_json::json;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::BufReader;

#[tokio::test]
async fn standalone_debug_uses_conversation_attach_in_debugger_mode() {
    let root = tempfile::Builder::new()
        .prefix("debug-")
        .tempdir_in("/tmp")
        .unwrap();
    std::env::set_var("GREMLINS_SANDBOX_ROOT", root.path());
    let listener = socket::bind_socket(&gremlins::config::state_root()).unwrap();
    let path = root.path().to_owned();
    let child = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_gremlins"))
            .args(["debug", "run"])
            .env_clear()
            .env("HOME", &path)
            .env("GREMLINS_SANDBOX_ROOT", &path)
            .current_dir(&path)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        let (stream, _) = listener.accept().await.unwrap();
        let mut peer = BufReader::new(stream);
        let version = socket::read_json_line(&mut peer).await.unwrap().unwrap();
        assert_eq!(version["op"], "version");
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"ok", "version":gremlins::VERSION}),
        )
        .await
        .unwrap();
        assert_eq!(
            socket::read_json_line(&mut peer).await.unwrap().unwrap(),
            json!({"op":"attach", "scope":"conversation", "id":"run", "mode":"debugger"})
        );
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"attached", "scope":"conversation", "id":"run", "stage_run_id":"advertised", "nonce":9, "capabilities":["message"]}),
        )
        .await
        .unwrap();
        // Only stage_run_id differs: neither frame belongs to this attachment.
        for frame in [
            json!({"type":"ready", "scope":"conversation", "id":"run", "stage_run_id":"replacement", "nonce":9}),
            json!({"type":"error", "scope":"conversation", "id":"run", "stage_run_id":"replacement", "nonce":9, "message":"wrong invocation"}),
        ] {
            socket::write_json_line(peer.get_mut(), &frame).await.unwrap();
        }
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"error", "scope":"conversation", "id":"run", "stage_run_id":"advertised", "nonce":9, "message":"fixture stopped"}),
        )
        .await
        .unwrap();
        let output = child.await.unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("attached (conversation)"), "{stderr}");
        assert!(stderr.contains("fixture stopped"), "{stderr}");
        assert!(!stderr.contains("connected to gremlin"), "{stderr}");
        assert!(!stderr.contains("wrong invocation"), "{stderr}");
        assert!(!stderr.contains("TTY"), "{stderr}");
    })
    .await
    .unwrap();
}
