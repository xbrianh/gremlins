use std::process::{Command, Output};

fn list_models(backend: &str) -> Output {
    let root = tempfile::tempdir().unwrap();
    // An unusable credential store: a file where the config directory should be.
    std::fs::write(root.path().join("config"), "not a directory").unwrap();
    Command::new(env!("CARGO_BIN_EXE_gremlins"))
        .args(["list-models", backend])
        .current_dir(root.path())
        .env("GREMLINS_SANDBOX_ROOT", root.path())
        .env_remove("CHATGPT_ACCESS_TOKEN")
        .output()
        .unwrap()
}

fn assert_rejected(backend: &str, diagnostic: &str) {
    let output = list_models(backend);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{backend}");
    assert!(output.stdout.is_empty(), "{backend}");
    assert!(stderr.contains(diagnostic), "{backend}: {stderr}");
    assert!(!stderr.contains("executor"), "{backend}: {stderr}");
}

#[test]
fn registered_backends_without_discovery_are_unsupported() {
    for backend in gremlins::clients::provider_names()
        .filter(|name| !gremlins::clients::model_catalog_names().any(|catalog| catalog == *name))
    {
        assert_rejected(backend, "not supported");
    }
}

#[test]
fn unknown_backend_lists_registered_names() {
    assert_rejected("chatgtp", "unknown backend 'chatgtp'");
    let stderr = String::from_utf8_lossy(&list_models("chatgtp").stderr).into_owned();
    for name in gremlins::clients::provider_names() {
        assert!(stderr.contains(name), "{name}: {stderr}");
    }
}
