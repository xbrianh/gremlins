use std::{fs, process::Command};

use serde_json::{json, Value};

#[test]
fn status_validates_optional_token_strings_without_printing_them() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("config/credentials");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("chatgpt.json");
    let run = |record: Value| {
        fs::write(&path, record.to_string()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_gremlins"))
            .args(["auth", "chatgpt", "status"])
            .env("GREMLINS_SANDBOX_ROOT", root.path())
            .env_remove("CHATGPT_ACCESS_TOKEN")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!text.contains("secret-marker"));
        (output.status.success(), text)
    };
    let (ok, text) = run(json!({"access_token":"secret-marker", "id_token":null}));
    assert!(
        ok && text.contains("Source: stored OAuth sign-in"),
        "{text}"
    );
    for (field, invalid) in [
        ("access_token", json!(123)),
        ("id_token", json!(["secret-marker"])),
        ("refresh_token", json!({"key":"secret-marker"})),
    ] {
        let (ok, text) = run(json!({(field):invalid}));
        assert!(
            !ok && text.contains("OAuth record: unreadable"),
            "{field}: {text}"
        );
        assert!(text.contains("Source: none"), "{field}: {text}");
    }
}
