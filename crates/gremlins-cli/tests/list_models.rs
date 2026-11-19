use std::process::{Command, Output};

fn list_models(backend: &str) -> Output {
    let root = tempfile::tempdir().unwrap();
    // An unusable credential store: a file where the config directory should be.
    std::fs::write(root.path().join("config"), "not a directory").unwrap();
    Command::new(env!("CARGO_BIN_EXE_gremlins"))
        .args(["list-models", backend])
        .env_clear()
        .env("HOME", root.path())
        .current_dir(root.path())
        .env("GREMLINS_SANDBOX_ROOT", root.path())
        .output()
        .unwrap()
}

fn assert_rejected(backend: &str, diagnostic: &str) -> String {
    let output = list_models(backend);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{backend}");
    assert!(output.stdout.is_empty(), "{backend}");
    assert!(stderr.contains(diagnostic), "{backend}: {stderr}");
    assert!(!stderr.contains("executor"), "{backend}: {stderr}");
    stderr.into_owned()
}

#[test]
fn registered_backends_without_discovery_are_unsupported() {
    for backend in gremlins::clients::provider_names()
        .filter(|name| !gremlins::clients::model_catalog_names().any(|catalog| catalog == *name))
    {
        let message = assert_rejected(backend, "not supported");
        for enabled in ["chatgpt", "copilot"] {
            assert!(message.contains(&format!("'{enabled}'")), "{message}");
        }
    }
}

#[test]
fn unknown_backend_lists_registered_names() {
    let stderr = assert_rejected("chatgtp", "unknown backend 'chatgtp'");
    for name in gremlins::clients::provider_names() {
        assert!(stderr.contains(name), "{name}: {stderr}");
    }
}

/// Exercise the full CLI path for Copilot listing with no credentials:
/// exit code nonzero, stdout empty, stderr names a credential remedy without
/// leaking secrets, and does not mention the executor.
#[test]
fn copilot_missing_credentials_reports_remedy() {
    assert_rejected("copilot", "no Copilot credentials");
}
