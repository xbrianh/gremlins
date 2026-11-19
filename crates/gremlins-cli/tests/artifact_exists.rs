//! `gremlins artifact <id> --exists` through the real CLI and executor socket.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gremlins::executor::state::FileSystemStateStore;

/// Fixtures set a process-wide env var, so tests must not overlap.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    _serial: std::sync::MutexGuard<'static, ()>,
    root: tempfile::TempDir,
    artifacts: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", root.path());
        let mut initial = serde_json::Map::new();
        initial.insert("id".into(), "gr-1".into());
        let store = FileSystemStateStore::create("gr-1", &initial, false).unwrap();
        let artifacts = store.root_dir().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        Fixture {
            _serial: serial,
            root,
            artifacts,
        }
    }

    fn store(&self) -> FileSystemStateStore {
        FileSystemStateStore::open("gr-1").unwrap()
    }

    async fn bind(&self, name: &str, path: &Path) {
        let key = format!("artifact://{name}");
        self.store()
            .commit(&key, path.to_str().unwrap())
            .await
            .unwrap();
    }

    fn cli(&self, id: &str, args: &[&str]) -> Output {
        self.command()
            .args(["artifact", id])
            .args(args)
            .output()
            .unwrap()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gremlins"));
        cmd.env_clear()
            .env("HOME", self.root.path())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("GREMLINS_SANDBOX_ROOT", self.root.path())
            .current_dir(self.root.path());
        cmd
    }

    /// Every file and directory under the sandbox, sorted, with sizes.
    fn tree(&self) -> Vec<(PathBuf, u64)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "sock" || e == "lock") {
                    continue;
                }
                let meta = entry.metadata().unwrap();
                out.push((path.clone(), meta.len()));
                if meta.is_dir() {
                    walk(&path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self.store().root_dir(), &mut out);
        out.sort();
        out
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn exists_contract_on_persisted_run() {
    let fx = Fixture::new();
    let marker = fx.artifacts.join("marker");
    std::fs::write(&marker, "").unwrap();
    let moved = fx.root.path().join("elsewhere.txt");
    std::fs::write(&moved, "data").unwrap();
    fx.bind("marker", &marker).await;
    fx.bind("moved", &moved).await;
    // Orphan at an unbound key's default destination.
    std::fs::write(fx.artifacts.join("orphan"), "x").unwrap();
    // Bound but deleted, and bound to a directory.
    let deleted = fx.artifacts.join("deleted");
    std::fs::write(&deleted, "").unwrap();
    fx.bind("deleted", &deleted).await;
    std::fs::remove_file(deleted).unwrap();
    std::fs::create_dir(fx.artifacts.join("dir")).unwrap();
    fx.bind("dir", &fx.artifacts.join("dir")).await;

    let ok = fx.cli(
        "gr-1",
        &["--exists", "artifact://marker", "artifact://moved"],
    );
    assert_eq!(ok.status.code(), Some(0), "{}", stderr(&ok));
    assert!(ok.stdout.is_empty() && ok.stderr.is_empty());

    let registry = fx.store().root_dir().join("registry.json");
    let bindings = std::fs::read(&registry).unwrap();
    let before = fx.tree();
    let mixed = fx.cli(
        "gr-1",
        &[
            "--exists",
            "artifact://orphan",
            "artifact://marker",
            "artifact://deleted",
            "artifact://dir",
            "artifact://deleted",
            "artifact://nested/never/made.txt",
        ],
    );
    assert_eq!(mixed.status.code(), Some(1), "{}", stderr(&mixed));
    assert_eq!(
        stderr(&mixed).lines().collect::<Vec<_>>(),
        [
            "artifact://orphan",
            "artifact://deleted",
            "artifact://dir",
            "artifact://nested/never/made.txt"
        ]
    );
    assert!(mixed.stdout.is_empty());
    assert_eq!(before, fx.tree(), "query must not write");
    assert_eq!(bindings, std::fs::read(registry).unwrap());

    // Operational failures never exit 1.
    let bad_uri = fx.cli("gr-1", &["--exists", "artifact://orphan", "http://nope"]);
    let unknown = fx.cli("gr-nope", &["--exists", "artifact://marker"]);
    let conflict = fx.cli("gr-1", &["--exists", "artifact://marker", "--list"]);
    let conflict_put = fx.cli("gr-1", &["--exists", "artifact://marker", "--put", "x"]);
    let conflict_get = fx.cli("gr-1", &["artifact://a", "--exists", "artifact://b"]);
    let conflict_json = fx.cli("gr-1", &["--json-path", "foo", "--exists", "artifact://b"]);
    let empty = fx.cli("gr-1", &["--exists"]);
    for out in [
        &bad_uri,
        &unknown,
        &conflict,
        &conflict_put,
        &conflict_get,
        &conflict_json,
        &empty,
    ] {
        assert!(out.status.code().is_some_and(|c| c >= 2), "{}", stderr(out));
        assert!(!stderr(out).is_empty());
    }

    // A corrupt registry is an error, not "missing".
    std::fs::write(fx.store().root_dir().join("registry.json"), "{not json").unwrap();
    let corrupt = fx.cli("gr-1", &["--exists", "artifact://marker"]);
    assert!(
        corrupt.status.code().is_some_and(|c| c >= 2),
        "{}",
        stderr(&corrupt)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_token_must_be_valid_and_known() {
    use std::os::unix::ffi::OsStrExt;
    let fx = Fixture::new();
    let marker = fx.artifacts.join("marker");
    std::fs::write(&marker, "").unwrap();
    fx.bind("marker", &marker).await;

    let run = |token: &std::ffi::OsStr| {
        fx.command()
            .args(["artifact", "gr-1", "--exists", "artifact://marker"])
            .env("GREMLINS_ARTIFACT_CHECK_SCOPE", token)
            .output()
            .unwrap()
    };
    let non_utf8 = run(std::ffi::OsStr::from_bytes(&[0xff, 0xfe]));
    let guessed = run(std::ffi::OsStr::new("guessed-token"));
    for out in [&non_utf8, &guessed] {
        assert!(out.status.code().is_some_and(|c| c >= 2), "{}", stderr(out));
        assert!(!stderr(out).is_empty());
        assert!(!stderr(out).contains("guessed-token"));
        assert!(out.stdout.is_empty());
    }
    // Registered reads must ignore the token entirely.
    let read = fx
        .command()
        .args(["artifact", "gr-1", "artifact://marker"])
        .env("GREMLINS_ARTIFACT_CHECK_SCOPE", "guessed-token")
        .output()
        .unwrap();
    assert_eq!(read.status.code(), Some(0), "{}", stderr(&read));
}

#[test]
fn invalid_config_is_not_a_missing_artifact() {
    let fx = Fixture::new();
    let config = fx.root.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.yaml"), "invalid: [").unwrap();
    let out = fx.cli("gr-1", &["--exists", "artifact://a"]);
    assert!(out.status.code().is_some_and(|c| c >= 2));
    assert!(out.stdout.is_empty() && !out.stderr.is_empty());
}

#[test]
fn unavailable_socket_is_not_a_missing_artifact() {
    let fx = Fixture::new();
    // Prevent the CLI's normal bind-or-connect fallback from starting a daemon.
    std::fs::create_dir(gremlins::executor::socket::GremlinsDaemonLock::lock_path(
        &gremlins::config::state_root(),
    ))
    .unwrap();
    let out = fx.cli("gr-1", &["--exists", "artifact://a"]);
    assert!(out.status.code().is_some_and(|c| c >= 2));
    assert!(out.stdout.is_empty() && !out.stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_reply_from_executor_is_not_a_missing_artifact() {
    use gremlins::executor::socket::{bind_socket, read_json_line, write_json_line};
    use serde_json::json;

    let fx = Fixture::new();
    let listener = bind_socket(&gremlins::config::state_root()).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = tokio::io::BufReader::new(read);
        let version = read_json_line(&mut read).await.unwrap().unwrap();
        assert_eq!(version["op"], "version");
        write_json_line(
            &mut write,
            &json!({"type":"ok", "version":gremlins::VERSION}),
        )
        .await
        .unwrap();
        let request = read_json_line(&mut read).await.unwrap().unwrap();
        assert_eq!(request["exists"], json!(["artifact://a"]));
        assert_eq!(request["scope"], "opaque-check-token");
        write_json_line(
            &mut write,
            &json!({"type":"ok", "id":"gr-1", "missing":["artifact://other"]}),
        )
        .await
        .unwrap();
    });
    let out = fx
        .command()
        .args(["artifact", "gr-1", "--exists", "artifact://a"])
        .env("GREMLINS_ARTIFACT_CHECK_SCOPE", "opaque-check-token")
        .output()
        .unwrap();
    assert!(out.status.code().is_some_and(|c| c >= 2));
    assert!(out.stdout.is_empty() && stderr(&out).contains("protocol mismatch"));
    assert!(!stderr(&out).contains("opaque-check-token"));
    server.await.unwrap();
}

async fn bounded_output(cmd: Command) -> Output {
    let mut cmd = tokio::process::Command::from(cmd);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        cmd.kill_on_drop(true).output(),
    )
    .await
    .expect("CLI blocked while producer was held")
    .unwrap()
}

async fn check_pending(fx: &Fixture, id: &str, token: Option<&str>, uri: &str, expected: i32) {
    let mut cmd = fx.command();
    cmd.args(["artifact", id, "--exists", uri]);
    if let Some(token) = token {
        cmd.env("GREMLINS_ARTIFACT_CHECK_SCOPE", token);
    }
    let out = bounded_output(cmd).await;
    let code = out.status.code().expect("CLI was signalled");
    let diagnostic = stderr(&out);
    assert!(out.stdout.is_empty());
    assert!(!diagnostic.contains(fx.root.path().to_str().unwrap()));
    if let Some(token) = token {
        assert!(!diagnostic.contains(token));
    }
    match expected {
        0 => {
            assert_eq!(code, 0, "{diagnostic}");
            assert!(diagnostic.is_empty());
        }
        1 => {
            assert_eq!(code, 1, "{diagnostic}");
            assert_eq!(diagnostic, format!("{uri}\n"));
        }
        _ => {
            assert!(code >= 2, "{diagnostic}");
            assert!(!diagnostic.is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_scopes_for_a_live_producer() {
    use gremlins::executor::supervisor::ArtifactTestSupervisor;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};
    let fx = Fixture::new();
    let root = fx.root.path();
    let supervisor = ArtifactTestSupervisor::start();
    let defs = root.join(".gremlins");
    std::fs::create_dir_all(&defs).unwrap();
    std::fs::write(defs.join("held.json5"), r#"{
      default_model: 'cmd/echo', stages: [{name:'hold', type:'exec', options:{cmds:[
        'touch "$GREMLINS_PROJECT_ROOT/ready"; for i in $(seq 1 600); do test -f "$GREMLINS_PROJECT_ROOT/release" && break; sleep 0.1; done'
      ]}}]
    }"#).unwrap();
    let mut launch = fx.command();
    launch
        .env("GREMLINS_PROJECT_ROOT", root)
        .args(["launch", "held"]);
    let launch = bounded_output(launch).await;
    assert!(launch.status.success(), "{}", stderr(&launch));
    let id = std::str::from_utf8(&launch.stdout).unwrap().trim();
    timeout(Duration::from_secs(5), async {
        while !root.join("ready").exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("producer did not enter exec stage");

    let uri = "artifact://marker";
    let (a, b) = (root.join("checkout-a"), root.join("checkout-b"));
    for dir in [&a, &b] {
        std::fs::create_dir_all(dir.join("sub")).unwrap();
    }
    let open =
        |dir: &Path| supervisor.open(id, dir, vec![(uri.into(), dir.join("sub/nondefault"))]);
    let (token_a, guard_a) = open(&a);
    let (token_b, _guard_b) = open(&b);
    let owner = tokio::spawn(async move {
        let _guard = guard_a;
        std::future::pending::<()>().await;
    });
    let marker = a.join("sub/nondefault");
    std::fs::write(&marker, "").unwrap();
    check_pending(&fx, id, Some(&token_a), uri, 0).await;
    check_pending(&fx, id, Some(&token_b), uri, 1).await;
    check_pending(&fx, id, None, uri, 1).await;
    check_pending(&fx, id, Some(&token_a), "artifact://undeclared", 2).await;
    check_pending(&fx, "gr-1", Some(&token_a), uri, 2).await;
    check_pending(&fx, id, Some("guessed"), uri, 2).await;
    std::fs::remove_dir(b.join("sub")).unwrap();
    std::os::unix::fs::symlink(a.join("sub"), b.join("sub")).unwrap();
    check_pending(&fx, id, Some(&token_b), uri, 2).await;
    std::fs::remove_file(b.join("sub")).unwrap();

    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    let (retry, guard_retry) = open(&a);
    assert!(retry != token_a);
    check_pending(&fx, id, Some(&token_a), uri, 2).await;
    check_pending(&fx, id, Some(&retry), uri, 0).await;
    drop(guard_retry);
    check_pending(&fx, id, Some(&retry), uri, 2).await;
    FileSystemStateStore::open(id)
        .unwrap()
        .commit(uri, marker.to_str().unwrap())
        .await
        .unwrap();
    check_pending(&fx, id, None, uri, 0).await;
    check_pending(&fx, id, Some(&token_b), uri, 1).await;
    std::fs::write(root.join("release"), "").unwrap();
}
